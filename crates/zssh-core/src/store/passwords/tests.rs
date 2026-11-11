use super::*;
use crate::model::{HostDraft, IdentityDraft, IdentityKind, ProxyMode};

fn draft(host: &Host) -> HostDraft {
    HostDraft {
        id: Some(host.id),
        group_id: host.group_id,
        name: host.name.clone(),
        host: host.host.clone(),
        port: host.port,
        username: host.username.clone(),
        auth_method: host.auth_method,
        identity_id: host.identity_id,
        proxy_id: host.proxy_id,
        jump_host_id: host.jump_host_id,
        proxy_mode: host.proxy_mode,
        tags: host.tags.clone(),
        sort: host.sort,
        legacy_algorithms: host.legacy_algorithms,
        protocol: None,
    }
}

fn host(db: &Database, name: &str, identity_id: Option<i64>) -> Host {
    let identity_id = identity_id.or_else(|| {
        Some(
            db.identity_save(&IdentityDraft {
                id: None,
                name: format!("{name} password"),
                kind: crate::model::IdentityKind::Password,
                key_path: None,
                secret: Some("old".into()),
                passphrase: None,
            })
            .expect("identity")
            .id,
        )
    });
    db.host_save(&HostDraft {
        id: None,
        group_id: None,
        name: name.into(),
        host: "example.invalid".into(),
        port: 22,
        username: "ops".into(),
        auth_method: AuthMethod::Password,
        identity_id,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: ProxyMode::None,
        tags: vec![],
        sort: 0,
        legacy_algorithms: false,
        protocol: None,
    })
    .expect("host")
}

fn snapshot(db: &Database, host: &Host) -> PasswordSnapshot {
    db.password_writeback_snapshot(host.id)
        .expect("capture")
        .expect("valid chain")
}
fn replacement(text: &str) -> Option<PasswordReplacement> {
    Some(PasswordReplacement::new(text.into(), "new password".into()))
}
fn password(db: &Database, host: &Host) -> String {
    db.host_saved_password(host.id)
        .expect("password")
        .to_string()
}

#[test]
fn unchanged_chain_updates_only_the_authenticated_hop() {
    let db = Database::open_in_memory().unwrap();
    let jump = host(&db, "jump", None);
    let target = host(&db, "target", None);
    let mut target_draft = draft(&target);
    target_draft.jump_host_id = Some(jump.id);
    let target = db.host_save(&target_draft).unwrap();
    let expected = snapshot(&db, &target);
    assert_eq!(
        expected.hosts().map(|host| host.id).collect::<Vec<_>>(),
        [jump.id, target.id]
    );
    assert_eq!(
        db.save_authenticated_passwords(&expected, &[replacement("new"), None])
            .unwrap(),
        1
    );
    assert_eq!(password(&db, &jump), "new");
    assert_eq!(password(&db, &target), "old");
    assert_eq!(db.host_get(jump.id).unwrap(), jump, "不覆盖主机字段");
    assert_eq!(db.host_get(target.id).unwrap(), target);
}

#[test]
fn replacing_a_jump_with_the_same_address_and_hop_count_writes_nothing() {
    let db = Database::open_in_memory().unwrap();
    let first = host(&db, "first", None);
    let second = host(&db, "second", None);
    let target = host(&db, "target", None);
    let mut edited = draft(&target);
    edited.jump_host_id = Some(first.id);
    let target = db.host_save(&edited).unwrap();
    let expected = snapshot(&db, &target);
    edited.jump_host_id = Some(second.id);
    db.host_save(&edited).unwrap();
    assert_eq!(
        db.save_authenticated_passwords(&expected, &[replacement("first-new"), None])
            .unwrap(),
        0
    );
    assert_eq!(password(&db, &first), "old");
    assert_eq!(password(&db, &second), "old");
    assert_eq!(password(&db, &target), "old");
}

#[test]
fn editing_connection_fields_or_credential_binding_invalidates_the_snapshot() {
    for field in ["address", "user", "port", "identity", "protocol"] {
        let db = Database::open_in_memory().unwrap();
        let original = host(&db, "host", None);
        let other = host(&db, "other", None);
        let expected = snapshot(&db, &original);
        let mut edited = draft(&original);
        match field {
            "address" => edited.host = "other.invalid".into(),
            "user" => edited.username = "other".into(),
            "port" => edited.port = 2222,
            "identity" => edited.identity_id = other.identity_id,
            "protocol" => {
                let mut protocol = original.protocol.clone();
                protocol.x11_forwarding = !protocol.x11_forwarding;
                edited.protocol = Some(protocol);
            }
            _ => unreachable!(),
        }
        db.host_save(&edited).unwrap();
        assert_eq!(
            db.save_authenticated_passwords(&expected, &[replacement("new")])
                .unwrap(),
            0,
            "{field}"
        );
        assert_eq!(password(&db, &original), "old");
        assert_eq!(password(&db, &other), "old");
    }
}

#[test]
fn changing_the_secret_on_the_same_identity_cannot_be_overwritten() {
    let db = Database::open_in_memory().unwrap();
    let original = host(&db, "host", None);
    let expected = snapshot(&db, &original);
    db.identity_save(&IdentityDraft {
        id: original.identity_id,
        name: "host password".into(),
        kind: IdentityKind::Password,
        key_path: None,
        secret: Some("edited-elsewhere".into()),
        passphrase: None,
    })
    .unwrap();
    assert_eq!(
        db.save_authenticated_passwords(&expected, &[replacement("stale")])
            .unwrap(),
        0
    );
    assert_eq!(password(&db, &original), "edited-elsewhere");
}

#[test]
fn a_shared_credential_is_split_without_changing_other_hosts() {
    let db = Database::open_in_memory().unwrap();
    let first = host(&db, "first", None);
    let second = host(&db, "second", first.identity_id);
    let expected = snapshot(&db, &first);
    assert_eq!(
        db.save_authenticated_passwords(&expected, &[replacement("new")])
            .unwrap(),
        1
    );
    assert_eq!(password(&db, &first), "new");
    assert_eq!(password(&db, &second), "old");
    assert_ne!(
        db.host_get(first.id).unwrap().identity_id,
        second.identity_id
    );
    assert_eq!(db.identity_list().unwrap().len(), 2);
}

#[test]
fn a_failed_later_write_rolls_back_the_entire_chain() {
    let db = Database::open_in_memory().unwrap();
    let jump = host(&db, "jump", None);
    let target = host(&db, "target", None);
    let mut edited = draft(&target);
    edited.jump_host_id = Some(jump.id);
    let target = db.host_save(&edited).unwrap();
    let expected = snapshot(&db, &target);
    db.with_conn(|conn| {
        conn.execute_batch(&format!("CREATE TRIGGER reject_second BEFORE UPDATE ON identities WHEN OLD.id = {} BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END;", target.identity_id.unwrap()))?;
        Ok(())
    }).unwrap();
    assert!(db
        .save_authenticated_passwords(
            &expected,
            &[replacement("jump-new"), replacement("target-new")]
        )
        .is_err());
    assert_eq!(password(&db, &jump), "old");
    assert_eq!(password(&db, &target), "old");
}

#[test]
fn missing_or_unselected_passwords_do_not_write_or_notify() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let db = Database::open_in_memory().unwrap();
    let original = host(&db, "host", None);
    let expected = snapshot(&db, &original);
    let count = Arc::new(AtomicUsize::new(0));
    let listener = count.clone();
    db.set_change_listener(move || {
        listener.fetch_add(1, Ordering::SeqCst);
    });
    for changes in [vec![], vec![None], vec![replacement("")]] {
        assert_eq!(
            db.save_authenticated_passwords(&expected, &changes)
                .unwrap(),
            0
        );
    }
    assert_eq!(password(&db, &original), "old");
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(!format!("{:?}", replacement("do-not-log-this")).contains("do-not-log-this"));
}
