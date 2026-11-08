//! 多语句写操作的原子性（审核 W10）：用触发器让"第二步"失败，断言第一步没有留下东西，
//! 也没有发出"数据变了"的通知。都在内存库里做，不碰真实数据目录。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::ai::agent::builtin::config::{AiEndpointDraft, EndpointKind, OpenAiOptionsConfig};
use crate::model::{
    AuthMethod, HostDraft, IdentityDraft, IdentityKind, Protocol, ProtocolSettings, ProxyDraft,
    ProxyKind, ProxyMode,
};
use crate::store::Database;

fn db() -> Database {
    Database::open_in_memory().expect("内存库")
}

/// 故障注入：执行一段 SQL（建触发器用）。
fn inject(db: &Database, sql: &str) {
    db.with_conn(|conn| {
        conn.execute_batch(sql)?;
        Ok(())
    })
    .expect("注入故障");
}

/// 登记变更回调，返回被通知的次数。
fn count_notifications(db: &Database) -> Arc<AtomicUsize> {
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&counter);
    db.set_change_listener(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    });
    counter
}

fn endpoint_draft(key: Option<&str>) -> AiEndpointDraft {
    AiEndpointDraft {
        id: None,
        name: "测试接口".to_owned(),
        kind: EndpointKind::OpenAiChat,
        base_url: "https://api.example.com/v1".to_owned(),
        api_key: key.map(str::to_owned),
        default_model: None,
        models: Vec::new(),
        overrides: BTreeMap::new(),
        openai_options: OpenAiOptionsConfig::default(),
        preset_id: None,
    }
}

fn host_draft(name: &str) -> HostDraft {
    HostDraft {
        protocol: None,
        id: None,
        group_id: None,
        name: name.to_owned(),
        host: "example.internal".to_owned(),
        port: 22,
        username: "deploy".to_owned(),
        auth_method: AuthMethod::Password,
        identity_id: None,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: ProxyMode::None,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    }
}

#[test]
fn ai_endpoint_save_leaves_no_orphan_key_when_the_insert_fails() {
    let db = db();
    let notified = count_notifications(&db);
    inject(
        &db,
        "CREATE TRIGGER fail_endpoint_insert BEFORE INSERT ON ai_endpoints \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );

    let err = db
        .ai_endpoint_save(&endpoint_draft(Some("sk-test-0001")))
        .expect_err("第二步被注入的故障打断");
    assert!(err.to_string().contains("injected"), "{err}");

    assert!(
        db.identity_list().unwrap().is_empty(),
        "第一步插入的 Key 凭据必须回滚"
    );
    assert!(db.ai_endpoint_list().unwrap().is_empty());
    assert_eq!(notified.load(Ordering::SeqCst), 0, "失败不能通知数据变了");

    inject(&db, "DROP TRIGGER fail_endpoint_insert;");
    db.ai_endpoint_save(&endpoint_draft(Some("sk-test-0001")))
        .expect("故障解除后能正常保存");
    assert_eq!(db.identity_list().unwrap().len(), 1);
    assert_eq!(db.ai_endpoint_list().unwrap().len(), 1);
    assert_eq!(notified.load(Ordering::SeqCst), 1, "成功提交后通知一次");
}

#[test]
fn ai_endpoint_delete_keeps_everything_when_the_last_step_fails() {
    let db = db();
    let saved = db
        .ai_endpoint_save(&endpoint_draft(Some("sk-test-0002")))
        .unwrap();
    inject(
        &db,
        "CREATE TRIGGER fail_identity_delete BEFORE DELETE ON identities \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    db.ai_endpoint_delete(saved.id).expect_err("删凭据时失败");
    assert_eq!(
        db.ai_endpoint_list().unwrap().len(),
        1,
        "接口那一行的删除要跟着回滚"
    );
}

#[test]
fn host_save_with_protocol_does_not_leave_a_half_saved_host() {
    let db = db();
    let notified = count_notifications(&db);
    // 新建主机的 INSERT 能成功，随后写协议列（UPDATE ... protocol）被打断。
    inject(
        &db,
        "CREATE TRIGGER fail_protocol_write BEFORE UPDATE OF protocol ON hosts \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    let err = db
        .host_save_with_protocol(
            &host_draft("switch-1"),
            &ProtocolSettings::new(Protocol::Telnet),
        )
        .expect_err("写协议设置失败");
    assert!(err.to_string().contains("injected"), "{err}");
    assert!(
        db.host_list().unwrap().is_empty(),
        "主机行不能在协议设置写失败后留下"
    );
    assert_eq!(notified.load(Ordering::SeqCst), 0);
}

#[test]
fn host_protocol_set_failure_keeps_the_old_settings() {
    let db = db();
    let host = db.host_save(&host_draft("web-1")).unwrap();
    inject(
        &db,
        "CREATE TRIGGER fail_protocol_write BEFORE UPDATE OF protocol ON hosts \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    db.host_protocol_set(host.id, &ProtocolSettings::new(Protocol::Telnet))
        .expect_err("写协议设置失败");
    assert_eq!(
        db.host_protocol_get(host.id).unwrap().protocol,
        Protocol::Ssh
    );
}

#[test]
fn proxy_delete_keeps_the_proxy_when_clearing_the_default_reference_fails() {
    let db = db();
    let proxy = db
        .proxy_save(&ProxyDraft {
            id: None,
            name: "本地代理".to_owned(),
            kind: ProxyKind::Socks5,
            host: "127.0.0.1".to_owned(),
            port: 10808,
            username: None,
            secret: None,
        })
        .unwrap();
    db.set_default_proxy_id(Some(proxy.id)).unwrap();
    let notified = count_notifications(&db);
    // 删代理之后要把"默认代理"设置改成 null：让这一步失败。
    inject(
        &db,
        "CREATE TRIGGER fail_settings_update BEFORE UPDATE ON settings \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );

    db.proxy_delete(proxy.id).expect_err("清理默认引用失败");
    assert_eq!(db.proxy_list().unwrap().len(), 1, "代理本身不能先被删掉");
    assert_eq!(db.default_proxy_id().unwrap(), Some(proxy.id));
    assert_eq!(notified.load(Ordering::SeqCst), 0);

    inject(&db, "DROP TRIGGER fail_settings_update;");
    db.proxy_delete(proxy.id).unwrap();
    assert!(db.proxy_list().unwrap().is_empty());
    assert_eq!(db.default_proxy_id().unwrap(), None);
    assert_eq!(notified.load(Ordering::SeqCst), 1);
}

#[test]
fn group_delete_rolls_back_the_group_when_detaching_hosts_fails() {
    let db = db();
    let group = db
        .group_save(&crate::model::GroupDraft {
            id: None,
            name: "组".to_owned(),
            parent_id: None,
            sort: 0,
        })
        .unwrap();
    let mut draft = host_draft("web-1");
    draft.group_id = Some(group.id);
    db.host_save(&draft).unwrap();
    // 外键 ON DELETE SET NULL 也会触发这个触发器，所以删分组本身就会失败；
    // 要点是失败后分组还在、主机还挂在上面。
    inject(
        &db,
        "CREATE TRIGGER fail_host_detach BEFORE UPDATE OF group_id ON hosts \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    db.group_delete(group.id).expect_err("摘主机时失败");
    assert_eq!(db.group_list().unwrap().len(), 1);
    assert_eq!(db.host_list().unwrap()[0].group_id, Some(group.id));
}

#[test]
fn identity_save_failure_changes_nothing() {
    let db = db();
    inject(
        &db,
        "CREATE TRIGGER fail_identity_insert BEFORE INSERT ON identities \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    db.identity_save(&IdentityDraft {
        id: None,
        name: "pw".to_owned(),
        kind: IdentityKind::Password,
        key_path: None,
        secret: Some("s3cret".to_owned()),
        passphrase: None,
    })
    .expect_err("插入被打断");
    assert!(db.identity_list().unwrap().is_empty());
}

#[test]
fn forward_remote_port_clash_is_checked_inside_the_write() {
    use crate::store::forwards::{ForwardKind, PortForwardDraft};

    let db = db();
    let host_id = db.host_save(&host_draft("web-1")).unwrap().id;
    let remote = PortForwardDraft {
        id: None,
        host_id,
        name: "远程".to_owned(),
        kind: ForwardKind::Remote,
        bind_host: String::new(),
        bind_port: 18080,
        dest_host: "localhost".to_owned(),
        dest_port: 80,
        auto_start: false,
    };
    let first = db.forward_save(&remote).unwrap();
    // 新建一条同端口的：被拦；改自己：放行；另一台主机上的同端口：放行；本地转发同端口：放行。
    assert!(db.forward_save(&remote).is_err());
    let mut edit = remote.clone();
    edit.id = Some(first.id);
    db.forward_save(&edit).unwrap();
    let other_host = db.host_save(&host_draft("web-2")).unwrap().id;
    let mut elsewhere = remote.clone();
    elsewhere.host_id = other_host;
    db.forward_save(&elsewhere).unwrap();
    let mut local = remote.clone();
    local.kind = ForwardKind::Local;
    db.forward_save(&local).unwrap();
    assert_eq!(db.forward_list().unwrap().len(), 3);
}
