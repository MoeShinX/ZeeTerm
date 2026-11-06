//! 内置助手接口的同步（V2-A3）：往返、版本号随内容走、老格式兼容、合并 / 冲突 / 删除语义、
//! 导入校验、备份恢复换 uid、防误删计数。
//!
//! 数据库里的 Key 凭据是真 DPAPI 密文，所以引擎用 [`DpapiSealer`]（两个"电脑"共用本机 DPAPI，
//! 同步包里的明文照样走一遍主密码加密）。

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::apply::{apply_snapshot, local_counts};
use super::engine::{SyncEngine, SyncStatus};
use super::guard::{deletion_plan, DeletionDecision};
use super::merge::merge;
use super::package::{
    AiEndpointRecord, IdentityRecord, SyncSnapshot, Tombstone, ENTITY_AI_ENDPOINT,
    SNAPSHOT_VERSION_BUILTIN_AI,
};
use super::sealer::DpapiSealer;
use super::settings::validate_value;
use super::storage::MemoryStorage;
use crate::ai::agent::builtin::catalog::CapsOverride;
use crate::ai::agent::builtin::config::{AiEndpointDraft, EndpointKind, OpenAiOptionsConfig, Tri};
use crate::store::Database;

const PASSWORD: &str = "test-master-password";
const NOW: i64 = 1_760_000_000;

fn temp_dir(name: &str) -> crate::test_util::TestDir {
    crate::test_util::TestDir::new(&format!("sync-builtin-{name}"))
}

fn draft(name: &str, key: Option<&str>) -> AiEndpointDraft {
    AiEndpointDraft {
        id: None,
        name: name.to_owned(),
        kind: EndpointKind::OpenAiChat,
        base_url: "https://api.example.com/v1".to_owned(),
        api_key: key.map(str::to_owned),
        default_model: Some("model-a".to_owned()),
        models: vec!["model-a".to_owned(), "model-b".to_owned()],
        overrides: BTreeMap::from([(
            "model-b".to_owned(),
            CapsOverride {
                context_window: Some(64_000),
                ..CapsOverride::default()
            },
        )]),
        openai_options: OpenAiOptionsConfig {
            echo_reasoning_content: Tri::On,
            ..OpenAiOptionsConfig::default()
        },
        preset_id: Some("tokenway".to_owned()),
    }
}

fn package_json(storage: &MemoryStorage) -> Value {
    let bytes = storage.bytes().expect("云端有包");
    let plaintext = super::crypto::open(&bytes, PASSWORD).expect("解开");
    serde_json::from_slice(&plaintext).expect("JSON")
}

fn record(uid: &str, name: &str, updated: i64, device: &str) -> AiEndpointRecord {
    AiEndpointRecord {
        uid: uid.to_owned(),
        name: name.to_owned(),
        kind: "openai_chat".to_owned(),
        base_url: "https://api.example.com/v1".to_owned(),
        key_identity_uid: None,
        default_model: None,
        models: Vec::new(),
        overrides: json!({}),
        openai_options: json!({}),
        preset_id: None,
        created_at: 1,
        updated_at: updated,
        device_id: device.to_owned(),
    }
}

fn identity(uid: &str, secret: &str) -> IdentityRecord {
    IdentityRecord {
        uid: uid.to_owned(),
        name: format!("{uid} 的 API Key"),
        kind: "password".to_owned(),
        key_path: None,
        secret: Some(secret.to_owned()),
        updated_at: 10,
        device_id: "dev-a".to_owned(),
    }
}

#[tokio::test]
async fn endpoints_and_keys_sync_to_the_other_computer_and_the_version_follows_the_content() {
    let dir_a = temp_dir("a");
    let dir_b = temp_dir("b");
    let db_a = Database::open_in_memory().unwrap();
    let db_b = Database::open_in_memory().unwrap();
    let storage = MemoryStorage::new();
    let sealer_a = DpapiSealer::new(db_a.vault_account().to_owned());
    let sealer_b = DpapiSealer::new(db_b.vault_account().to_owned());
    let engine_a = SyncEngine::new(&db_a, &sealer_a, "dev-a", &dir_a, None, None);
    let engine_b = SyncEngine::new(&db_b, &sealer_b, "dev-b", &dir_b, None, None);

    // 没有接口：包是旧格式，没有 ai_endpoints 键。
    db_a.setting_set("ai_default_agent", "\"claude\"").unwrap();
    engine_a.sync(&storage, PASSWORD, None).await.unwrap();
    let before = package_json(&storage);
    assert_eq!(before["schema_version"], 1);
    assert!(before.get("ai_endpoints").is_none(), "没有接口就不写这个键");

    // 加接口 + Key + 默认接口：包升到 3，B 拿到的一个字段不少，Key 能解出来。
    let saved = db_a
        .ai_endpoint_save(&draft("主力", Some("sk-sync-0001")))
        .unwrap();
    db_a.set_ai_builtin_default(Some((saved.id, Some("model-b"))))
        .unwrap();
    db_a.set_ai_default_agent("builtin").unwrap();
    let report = engine_a.sync(&storage, PASSWORD, None).await.unwrap();
    assert_eq!(report.status, SyncStatus::Completed);
    let after = package_json(&storage);
    assert_eq!(after["schema_version"], SNAPSHOT_VERSION_BUILTIN_AI);
    assert_eq!(after["ai_endpoints"][0]["name"], "主力");
    // Key 不在接口记录里，在凭据记录里（和主机密码同一套加密）。
    assert!(!after["ai_endpoints"].to_string().contains("sk-sync-0001"));
    assert!(after["identities"].to_string().contains("sk-sync-0001"));

    let report = engine_b.sync(&storage, PASSWORD, None).await.unwrap();
    assert_eq!(report.status, SyncStatus::Completed);
    let b = db_b.ai_endpoint_list().unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].uid, saved.uid, "uid 跨电脑不变");
    assert_eq!(b[0].name, "主力");
    assert_eq!(b[0].kind, EndpointKind::OpenAiChat);
    assert_eq!(b[0].base_url, saved.base_url);
    assert_eq!(b[0].models, saved.models);
    assert_eq!(b[0].overrides, saved.overrides);
    assert_eq!(b[0].openai_options, saved.openai_options);
    assert_eq!(b[0].preset_id.as_deref(), Some("tokenway"));
    assert!(b[0].has_key);
    assert_eq!(
        db_b.ai_endpoint_api_key(b[0].id).unwrap().as_deref(),
        Some("sk-sync-0001")
    );
    let default = db_b.ai_builtin_default().unwrap().unwrap();
    assert_eq!(default.endpoint_id, b[0].id);
    assert_eq!(default.model.as_deref(), Some("model-b"));
    assert_eq!(db_b.ai_default_agent().unwrap(), "builtin");

    // B 改了 Key 和名字，A 同步后看到。
    let mut change = draft("主力改名", Some("sk-sync-0002"));
    change.id = Some(b[0].id);
    db_b.ai_endpoint_save(&change).unwrap();
    engine_b.sync(&storage, PASSWORD, None).await.unwrap();
    engine_a.sync(&storage, PASSWORD, None).await.unwrap();
    let a = db_a.ai_endpoint_list().unwrap();
    assert_eq!(a[0].name, "主力改名");
    assert_eq!(
        db_a.ai_endpoint_api_key(a[0].id).unwrap().as_deref(),
        Some("sk-sync-0002")
    );

    // 删除语义同主机：A 删了，B 同步后接口、Key 凭据、默认设置都没了。
    db_a.ai_endpoint_delete(a[0].id).unwrap();
    db_a.set_ai_default_agent("claude").unwrap();
    engine_a
        .sync(&storage, PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    engine_b
        .sync(&storage, PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    assert!(db_b.ai_endpoint_list().unwrap().is_empty());
    assert!(db_b.identity_list().unwrap().is_empty(), "Key 凭据跟着删");
    assert_eq!(db_b.ai_builtin_default().unwrap(), None);

    // 内容没了，包回到旧格式（Z-SSH 0.2.14 又能同步）。
    let back = package_json(&storage);
    assert_eq!(back["schema_version"], 1);
    assert!(back.get("ai_endpoints").is_none());
}

#[tokio::test]
async fn a_package_from_an_older_client_has_no_endpoints_and_leaves_local_ones_alone() {
    let dir_old = temp_dir("old");
    let dir_new = temp_dir("new");
    let db_old = Database::open_in_memory().unwrap();
    let db_new = Database::open_in_memory().unwrap();
    let storage = MemoryStorage::new();
    let sealer_old = DpapiSealer::new(db_old.vault_account().to_owned());
    let sealer_new = DpapiSealer::new(db_new.vault_account().to_owned());
    SyncEngine::new(&db_old, &sealer_old, "old", &dir_old, None, None)
        .sync(&storage, PASSWORD, None)
        .await
        .unwrap();
    let engine_new = SyncEngine::new(&db_new, &sealer_new, "new", &dir_new, None, None);
    engine_new.sync(&storage, PASSWORD, None).await.unwrap();
    db_new.ai_endpoint_save(&draft("本机", None)).unwrap();
    engine_new.sync(&storage, PASSWORD, None).await.unwrap();
    assert_eq!(package_json(&storage)["schema_version"], 3);
    engine_new.sync(&storage, PASSWORD, None).await.unwrap();
    assert_eq!(db_new.ai_endpoint_list().unwrap().len(), 1);
}

#[test]
fn parsing_a_v1_package_without_the_key_gives_no_endpoints() {
    let raw = json!({
        "schema_version": 1, "device_id": "old", "created_at": 5,
        "groups": [], "hosts": [], "identities": [], "proxies": [],
        "settings": {}, "known_hosts": [], "tombstones": []
    });
    let snapshot = SyncSnapshot::parse(&serde_json::to_vec(&raw).unwrap()).unwrap();
    assert!(snapshot.ai_endpoints.is_empty());
    assert_eq!(snapshot.required_version(), 1);
    // 写回去仍然没有这个键，字节级和老格式一致。
    let rewritten: Value = serde_json::from_slice(&snapshot.to_json().unwrap()).unwrap();
    assert_eq!(rewritten, raw);
}

#[test]
fn a_newer_package_than_this_version_is_still_refused() {
    let mut raw = json!({
        "schema_version": 4, "device_id": "x", "created_at": 5
    });
    raw["ai_endpoints"] = json!([]);
    let err = SyncSnapshot::parse(&serde_json::to_vec(&raw).unwrap()).unwrap_err();
    assert!(
        matches!(
            err,
            super::SyncError::FormatTooNew {
                found: 4,
                supported: 3
            }
        ),
        "{err:?}"
    );
}

#[test]
fn any_builtin_content_raises_the_version() {
    let mut snapshot = SyncSnapshot::empty("dev-a", NOW);
    assert_eq!(snapshot.required_version(), 1);
    snapshot
        .settings
        .insert("ai_default_agent".to_owned(), json!("claude"));
    assert_eq!(snapshot.required_version(), 1);
    snapshot
        .settings
        .insert("ai_default_agent".to_owned(), json!("builtin"));
    assert_eq!(
        snapshot.required_version(),
        3,
        "默认智能体是 builtin 老客户端不认识"
    );
    snapshot.settings.clear();
    snapshot.ai_endpoints.push(record("e1", "甲", 10, "dev-a"));
    assert_eq!(snapshot.required_version(), 3);
    snapshot.ai_endpoints.clear();
    snapshot.settings.insert(
        "ai_builtin_default".to_owned(),
        json!({"endpoint_uid": "e1", "model": null}),
    );
    assert_eq!(snapshot.required_version(), 3);
}

#[test]
fn endpoints_merge_like_other_records() {
    let mut base = SyncSnapshot::empty("cloud", NOW);
    base.ai_endpoints.push(record("e1", "甲", 10, "cloud"));

    // 单边改：用改了的那边。
    let mut local = base.clone();
    local.device_id = "dev-a".to_owned();
    local.ai_endpoints[0].name = "甲-本地".to_owned();
    local.ai_endpoints[0].updated_at = 20;
    let outcome = merge(Some(&base), &local, &base, NOW).unwrap();
    assert_eq!(outcome.snapshot.ai_endpoints[0].name, "甲-本地");
    assert!(outcome.conflicts.is_empty());

    // 两边都改成不同内容：时间新的赢，另一份进冲突。
    let mut remote = base.clone();
    remote.device_id = "dev-b".to_owned();
    remote.ai_endpoints[0].base_url = "https://other.example.com/v1".to_owned();
    remote.ai_endpoints[0].updated_at = 30;
    remote.ai_endpoints[0].device_id = "dev-b".to_owned();
    let outcome = merge(Some(&base), &local, &remote, NOW).unwrap();
    assert_eq!(
        outcome.snapshot.ai_endpoints[0].base_url,
        "https://other.example.com/v1"
    );
    let conflict = outcome
        .conflicts
        .iter()
        .find(|c| c.entity == ENTITY_AI_ENDPOINT)
        .expect("有冲突");
    assert!(conflict.other.contains("甲-本地"));

    // 本地删、云端改：删除赢，改动留在冲突里，墓碑防复活。
    let mut local = base.clone();
    local.device_id = "dev-a".to_owned();
    local.ai_endpoints.clear();
    let outcome = merge(Some(&base), &local, &remote, NOW).unwrap();
    assert!(outcome.snapshot.ai_endpoints.is_empty());
    assert!(outcome
        .tombstones
        .iter()
        .any(|t| t.entity == ENTITY_AI_ENDPOINT && t.key == "e1"));
    assert!(outcome
        .conflicts
        .iter()
        .any(|c| c.entity == ENTITY_AI_ENDPOINT && c.kept.is_empty()));
}

#[test]
fn dangling_key_and_default_references_are_cleared_after_a_merge() {
    let mut snapshot = SyncSnapshot::empty("dev-a", NOW);
    snapshot.identities.push(identity("k1", "sk-1"));
    let mut with_key = record("e1", "有 Key", 10, "dev-a");
    with_key.key_identity_uid = Some("k1".to_owned());
    let mut lost_key = record("e2", "丢了 Key", 10, "dev-a");
    lost_key.key_identity_uid = Some("gone".to_owned());
    snapshot.ai_endpoints = vec![with_key, lost_key];
    snapshot.settings.insert(
        "ai_builtin_default".to_owned(),
        json!({"endpoint_uid": "e-missing", "model": "m"}),
    );
    let notes = super::merge::null_missing_references(&mut snapshot);
    assert_eq!(
        snapshot.ai_endpoints[0].key_identity_uid.as_deref(),
        Some("k1")
    );
    assert_eq!(snapshot.ai_endpoints[1].key_identity_uid, None);
    assert!(!snapshot.settings.contains_key("ai_builtin_default"));
    assert!(notes.iter().any(|n| n.contains("丢了 Key")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("默认接口")), "{notes:?}");

    // 默认接口存在就保留。
    snapshot.settings.insert(
        "ai_builtin_default".to_owned(),
        json!({"endpoint_uid": "e1", "model": null}),
    );
    super::merge::null_missing_references(&mut snapshot);
    assert!(snapshot.settings.contains_key("ai_builtin_default"));
}

#[test]
fn applying_rejects_unknown_kinds_and_bad_options_without_writing_anything() {
    let db = Database::open_in_memory().unwrap();
    let mut snapshot = SyncSnapshot::empty("dev-a", NOW);
    snapshot
        .ai_endpoints
        .push(record("e1", "好的", 10, "dev-a"));
    let mut future = record("e2", "未来类型", 10, "dev-a");
    future.kind = "openai_responses".to_owned();
    snapshot.ai_endpoints.push(future);
    let err = apply_snapshot(&db, &snapshot, &DpapiSealer::for_test(), None, &[], NOW).unwrap_err();
    assert!(err.to_string().contains("不认识的类型"), "{err}");
    assert!(db.ai_endpoint_list().unwrap().is_empty(), "整次导入回滚");

    snapshot.ai_endpoints.pop();
    snapshot.ai_endpoints[0].overrides = json!([1]);
    let err = apply_snapshot(&db, &snapshot, &DpapiSealer::for_test(), None, &[], NOW).unwrap_err();
    assert!(err.to_string().contains("选项格式"), "{err}");

    snapshot.ai_endpoints[0].overrides = json!({});
    snapshot.ai_endpoints[0].base_url = "ftp://nope".to_owned();
    assert!(apply_snapshot(&db, &snapshot, &DpapiSealer::for_test(), None, &[], NOW).is_err());

    snapshot.ai_endpoints[0].base_url = "https://ok.example.com/v1".to_owned();
    let report = apply_snapshot(&db, &snapshot, &DpapiSealer::for_test(), None, &[], NOW).unwrap();
    assert_eq!(report.identities.inserted, 0);
    assert_eq!(db.ai_endpoint_list().unwrap().len(), 1);
    assert_eq!(local_counts(&db).unwrap()[ENTITY_AI_ENDPOINT], 1);
}

#[test]
fn unknown_option_keys_are_kept_byte_for_byte_through_apply_and_snapshot() {
    let db = Database::open_in_memory().unwrap();
    let mut snapshot = SyncSnapshot::empty("dev-a", NOW);
    let mut future = record("e1", "未来选项", 10, "dev-a");
    future.openai_options = json!({"echo_reasoning_content": "on", "future_switch": 7});
    future.overrides = json!({"m": {"context_window": 1000, "future_field": true}});
    snapshot.ai_endpoints.push(future.clone());
    apply_snapshot(&db, &snapshot, &DpapiSealer::for_test(), None, &[], NOW).unwrap();
    let again =
        super::snapshot::snapshot_local(&db, &DpapiSealer::for_test(), "dev-a", None, NOW).unwrap();
    assert_eq!(again.ai_endpoints[0].openai_options, future.openai_options);
    assert_eq!(again.ai_endpoints[0].overrides, future.overrides);
}

#[test]
fn restoring_a_backup_gives_deleted_endpoints_a_new_uid_and_fixes_the_default() {
    let mut snapshot = SyncSnapshot::empty("dev-a", NOW);
    snapshot.identities.push(identity("k1", "sk-1"));
    let mut dead = record("e-dead", "删过的", 10, "dev-a");
    dead.key_identity_uid = Some("k1".to_owned());
    snapshot.ai_endpoints.push(dead);
    snapshot
        .ai_endpoints
        .push(record("e-live", "活的", 10, "dev-a"));
    snapshot.settings.insert(
        "ai_builtin_default".to_owned(),
        json!({"endpoint_uid": "e-dead", "model": null}),
    );
    let tombstones = vec![Tombstone {
        entity: ENTITY_AI_ENDPOINT.to_owned(),
        key: "e-dead".to_owned(),
        deleted_at: 20,
        device_id: "dev-b".to_owned(),
    }];
    let renamed = super::backup::renew_deleted_uids(&mut snapshot, &tombstones);
    let new_uid = renamed.get("e-dead").expect("换了 uid").clone();
    assert!(snapshot.ai_endpoints.iter().any(|e| e.uid == new_uid));
    assert!(snapshot.ai_endpoints.iter().any(|e| e.uid == "e-live"));
    assert_eq!(
        snapshot.settings["ai_builtin_default"]["endpoint_uid"],
        json!(new_uid)
    );
}

#[test]
fn deleting_endpoints_counts_in_the_mass_delete_guard() {
    let mut local = SyncSnapshot::empty("dev-a", NOW);
    local.ai_endpoints.push(record("e1", "甲", 10, "dev-a"));
    local.ai_endpoints.push(record("e2", "乙", 10, "dev-a"));
    let target = SyncSnapshot::empty("dev-b", NOW);
    let plan = deletion_plan(&local, &target);
    assert_eq!(plan.deleted_records, 2);
    assert!(plan.needs_confirmation());
    assert!(plan.samples.iter().any(|s| s.entity == ENTITY_AI_ENDPOINT));
}

// 这条靠"另一台设备封的密钥本机也能解"（Windows 同一用户的 DPAPI）。
// macOS 每个资料库一把密钥，别的设备封的密文本机按设计解不开，
// 恢复时要求用户重新填密钥。这里只在 Windows 上验证。
#[cfg(windows)]
#[tokio::test]
async fn the_conflict_list_can_restore_a_deleted_endpoint_as_a_new_record() {
    let dir_a = temp_dir("conflict-a");
    let dir_b = temp_dir("conflict-b");
    let db_a = Database::open_in_memory().unwrap();
    let db_b = Database::open_in_memory().unwrap();
    let storage = MemoryStorage::new();
    let sealer_a = DpapiSealer::new(db_a.vault_account().to_owned());
    let sealer_b = DpapiSealer::new(db_b.vault_account().to_owned());
    let engine_a = SyncEngine::new(&db_a, &sealer_a, "dev-a", &dir_a, None, None);
    let engine_b = SyncEngine::new(&db_b, &sealer_b, "dev-b", &dir_b, None, None);
    let saved = db_a
        .ai_endpoint_save(&draft("甲", Some("sk-c-0001")))
        .unwrap();
    engine_a.sync(&storage, PASSWORD, None).await.unwrap();
    engine_b.sync(&storage, PASSWORD, None).await.unwrap();

    // A 删除，B 改名；A 先同步，B 再同步：删除赢，B 的改动留在冲突里。
    db_a.ai_endpoint_delete(saved.id).unwrap();
    engine_a
        .sync(&storage, PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    let b = db_b.ai_endpoint_list().unwrap().remove(0);
    let mut rename = draft("甲-B", None);
    rename.id = Some(b.id);
    db_b.ai_endpoint_save(&rename).unwrap();
    engine_b
        .sync(&storage, PASSWORD, Some(DeletionDecision::Continue))
        .await
        .unwrap();
    assert!(db_b.ai_endpoint_list().unwrap().is_empty());

    let list = super::conflicts::list(&db_b, &DpapiSealer::for_test(), "dev-b", SyncEngine::now())
        .unwrap();
    let conflict = list
        .iter()
        .find(|c| c.entity == ENTITY_AI_ENDPOINT)
        .expect("冲突列表里有这条接口");
    assert!(conflict.deletion && conflict.can_use_other);
    assert!(
        !conflict.other_summary.contains("sk-c-0001"),
        "摘要不含 Key"
    );
    super::conflicts::use_other(
        &db_b,
        &DpapiSealer::for_test(),
        "dev-b",
        None,
        conflict.id,
        SyncEngine::now(),
    )
    .unwrap();
    let restored = db_b.ai_endpoint_list().unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].name, "甲-B");
    assert_ne!(restored[0].uid, saved.uid, "恢复出来的是新记录");
}

#[test]
fn the_default_setting_and_agent_rules_accept_builtin_and_refuse_junk() {
    assert!(validate_value("ai_default_agent", &json!("builtin")).is_ok());
    assert!(validate_value("ai_default_agent", &json!("nope")).is_err());
    for ok in [
        json!({"endpoint_uid": "abc", "model": "gpt-x"}),
        json!({"endpoint_uid": "abc", "model": null}),
        json!({"endpoint_uid": "abc"}),
    ] {
        assert!(validate_value("ai_builtin_default", &ok).is_ok(), "{ok}");
    }
    for bad in [
        json!("abc"),
        json!({"model": "x"}),
        json!({"endpoint_uid": ""}),
        json!({"endpoint_uid": 5}),
        json!({"endpoint_uid": "a", "model": 5}),
        json!({"endpoint_uid": "a\u{7}"}),
    ] {
        assert!(validate_value("ai_builtin_default", &bad).is_err(), "{bad}");
    }
}
