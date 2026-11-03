use std::collections::BTreeMap;

use super::*;
use crate::ai::agent::builtin::catalog::CapsOverride;
use crate::ai::agent::builtin::config::{EndpointKind, OpenAiOptionsConfig, Tri};
use crate::model::IdentityKind;

fn db() -> Database {
    Database::open_in_memory().expect("内存库")
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
            max_completion_tokens: Tri::On,
            ..OpenAiOptionsConfig::default()
        },
        preset_id: Some("tokenway".to_owned()),
    }
}

fn identity_names(db: &Database) -> Vec<String> {
    db.identity_list()
        .unwrap()
        .into_iter()
        .map(|i| i.name)
        .collect()
}

#[test]
fn crud_roundtrip_keeps_every_field() {
    let db = db();
    let saved = db
        .ai_endpoint_save(&draft("主力", Some("sk-aaa-0001")))
        .unwrap();
    assert!(saved.id > 0 && !saved.uid.is_empty());
    assert_eq!(saved.kind, EndpointKind::OpenAiChat);
    assert_eq!(saved.models, vec!["model-a", "model-b"]);
    assert_eq!(saved.overrides["model-b"].context_window, Some(64_000));
    assert_eq!(saved.openai_options.max_completion_tokens, Tri::On);
    assert_eq!(saved.preset_id.as_deref(), Some("tokenway"));
    assert!(saved.has_key && saved.key_identity_id.is_some());
    assert_eq!(saved.created_at, saved.updated_at);

    let mut changed = draft("主力改名", None);
    changed.id = Some(saved.id);
    changed.kind = EndpointKind::Anthropic;
    changed.base_url = "https://anthropic.example.com/".to_owned();
    changed.models.clear();
    let updated = db.ai_endpoint_save(&changed).unwrap();
    assert_eq!(updated.id, saved.id);
    assert_eq!(updated.uid, saved.uid, "uid 不变");
    assert_eq!(updated.kind, EndpointKind::Anthropic);
    assert_eq!(updated.base_url, "https://anthropic.example.com");
    assert!(updated.models.is_empty());
    assert_eq!(updated.created_at, saved.created_at);
    assert!(updated.has_key, "api_key=None 不动已存的 Key");

    assert_eq!(db.ai_endpoint_list().unwrap().len(), 1);
    assert_eq!(db.ai_endpoint_get(saved.id).unwrap().name, "主力改名");
    db.ai_endpoint_delete(saved.id).unwrap();
    assert!(db.ai_endpoint_list().unwrap().is_empty());
    assert!(matches!(
        db.ai_endpoint_get(saved.id),
        Err(ZsshError::NotFound { .. })
    ));
}

#[test]
fn the_key_lives_in_the_vault_as_a_password_identity() {
    let db = db();
    let saved = db
        .ai_endpoint_save(&draft("甲", Some("sk-vault-0001")))
        .unwrap();
    let identities = db.identity_list().unwrap();
    assert_eq!(identities.len(), 1);
    assert_eq!(identities[0].name, "甲 的 API Key");
    assert_eq!(identities[0].kind, IdentityKind::Password);
    assert!(identities[0].has_secret);
    assert_eq!(Some(identities[0].id), saved.key_identity_id);
    assert_eq!(
        db.ai_endpoint_api_key(saved.id).unwrap().as_deref(),
        Some("sk-vault-0001")
    );
    // 表里存的是 DPAPI 密文，不含明文。
    let raw: Vec<u8> = db
        .with_conn(|conn| {
            Ok(conn.query_row("SELECT secret_blob FROM identities", [], |r| r.get(0))?)
        })
        .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("sk-vault-0001"));
    // 接口配置表里没有 Key 的任何痕迹。
    let dump: String = db
        .with_conn(|conn| {
            Ok(conn.query_row(
                "SELECT group_concat(name || base_url || models || overrides || openai_options) FROM ai_endpoints",
                [],
                |r| r.get(0),
            )?)
        })
        .unwrap();
    assert!(!dump.contains("sk-vault"));
}

#[test]
fn changing_the_key_reuses_the_same_identity_and_clearing_removes_it() {
    let db = db();
    let saved = db
        .ai_endpoint_save(&draft("甲", Some("sk-old-0001")))
        .unwrap();
    let key_id = saved.key_identity_id.unwrap();

    let mut change = draft("甲", Some("sk-new-0002"));
    change.id = Some(saved.id);
    let updated = db.ai_endpoint_save(&change).unwrap();
    assert_eq!(
        updated.key_identity_id,
        Some(key_id),
        "原地更新，不多出凭据"
    );
    assert_eq!(db.identity_list().unwrap().len(), 1);
    assert_eq!(
        db.ai_endpoint_api_key(saved.id).unwrap().as_deref(),
        Some("sk-new-0002")
    );

    // 接口改名，凭据名跟着改。
    let mut rename = draft("乙", None);
    rename.id = Some(saved.id);
    db.ai_endpoint_save(&rename).unwrap();
    assert_eq!(identity_names(&db), vec!["乙 的 API Key"]);

    // 清掉 Key。
    let mut clear = draft("乙", Some(""));
    clear.id = Some(saved.id);
    let cleared = db.ai_endpoint_save(&clear).unwrap();
    assert!(!cleared.has_key && cleared.key_identity_id.is_none());
    assert!(identity_names(&db).is_empty());
    assert_eq!(db.ai_endpoint_api_key(saved.id).unwrap(), None);
}

#[test]
fn an_endpoint_without_a_key_is_fine() {
    let db = db();
    let saved = db.ai_endpoint_save(&draft("本地 Ollama", None)).unwrap();
    assert!(!saved.has_key);
    assert_eq!(db.ai_endpoint_api_key(saved.id).unwrap(), None);
    assert!(db.identity_list().unwrap().is_empty());
}

#[test]
fn marking_no_key_needed_makes_it_ready_drops_a_stored_key_and_survives_a_reload() {
    let db = db();
    // 没填 Key、也没说不需要：存得下来，但不算配好。
    let bare = db.ai_endpoint_save(&draft("本地", None)).unwrap();
    assert!(!bare.is_ready());

    // 先有 Key，再改成"不需要 Key"：Key 凭据一起清掉。
    let keyed = db
        .ai_endpoint_save(&draft("甲", Some("sk-a-0001")))
        .unwrap();
    assert!(keyed.is_ready());
    let mut no_key = draft("甲", None);
    no_key.id = Some(keyed.id);
    no_key.openai_options.no_key = true;
    let saved = db.ai_endpoint_save(&no_key).unwrap();
    assert!(saved.is_ready() && !saved.has_key && saved.key_identity_id.is_none());
    assert_eq!(db.ai_endpoint_api_key(saved.id).unwrap(), None);
    assert!(!identity_names(&db).iter().any(|n| n.starts_with("甲")));

    let reloaded = db.ai_endpoint_get(saved.id).unwrap();
    assert!(reloaded.openai_options.no_key && reloaded.is_ready());

    // Anthropic 不允许。
    let mut anthropic = draft("乙", None);
    anthropic.kind = EndpointKind::Anthropic;
    anthropic.openai_options.no_key = true;
    assert!(db.ai_endpoint_save(&anthropic).is_err());
}

#[test]
fn deleting_the_endpoint_deletes_its_key_unless_a_host_uses_it() {
    let db = db();
    let a = db
        .ai_endpoint_save(&draft("甲", Some("sk-a-0001")))
        .unwrap();
    assert_eq!(db.identity_list().unwrap().len(), 1);
    db.ai_endpoint_delete(a.id).unwrap();
    assert!(db.identity_list().unwrap().is_empty());

    let b = db
        .ai_endpoint_save(&draft("乙", Some("sk-b-0001")))
        .unwrap();
    let key_id = b.key_identity_id.unwrap();
    // 主机保存时不让选 Key 凭据（见下面的测试）；这里模拟"同步来的 / 旧数据里主机已经在用它"。
    let host = db
        .host_save(&crate::store::tests::sample_host(None, None))
        .unwrap();
    db.with_conn_write(|conn| {
        conn.execute(
            "UPDATE hosts SET identity_id = ?1 WHERE id = ?2",
            rusqlite::params![key_id, host.id],
        )?;
        Ok(())
    })
    .unwrap();
    db.ai_endpoint_delete(b.id).unwrap();
    assert_eq!(
        db.identity_list().unwrap().len(),
        1,
        "主机还在用这条凭据：留着，不能让主机失去密码"
    );
}

#[test]
fn deleting_the_key_identity_elsewhere_leaves_the_endpoint_without_a_key() {
    let db = db();
    let saved = db
        .ai_endpoint_save(&draft("甲", Some("sk-a-0001")))
        .unwrap();
    db.identity_delete(saved.key_identity_id.unwrap()).unwrap();
    let after = db.ai_endpoint_get(saved.id).unwrap();
    assert!(!after.has_key);
    assert_eq!(after.key_identity_id, None);
    assert_eq!(db.ai_endpoint_api_key(saved.id).unwrap(), None);
    // 重新填 Key 会补一条新的。
    let mut fill = draft("甲", Some("sk-b-0001"));
    fill.id = Some(saved.id);
    let refilled = db.ai_endpoint_save(&fill).unwrap();
    assert!(refilled.has_key);
}

#[test]
fn default_endpoint_and_model_round_trip_and_follow_deletes() {
    let db = db();
    assert_eq!(db.ai_builtin_default().unwrap(), None);
    let a = db.ai_endpoint_save(&draft("甲", None)).unwrap();
    let b = db.ai_endpoint_save(&draft("乙", None)).unwrap();

    db.set_ai_builtin_default(Some((a.id, Some("model-b"))))
        .unwrap();
    let got = db.ai_builtin_default().unwrap().unwrap();
    assert_eq!(got.endpoint_id, a.id);
    assert_eq!(got.endpoint_uid, a.uid);
    assert_eq!(got.model.as_deref(), Some("model-b"));

    db.set_ai_builtin_default(Some((b.id, None))).unwrap();
    assert_eq!(db.ai_builtin_default().unwrap().unwrap().model, None);

    // 删掉默认接口：默认设置一起清掉。
    db.ai_endpoint_delete(b.id).unwrap();
    assert_eq!(db.ai_builtin_default().unwrap(), None);
    assert_eq!(
        db.setting_get(AI_BUILTIN_DEFAULT_KEY).unwrap(),
        None,
        "设置项本身也删了"
    );

    // 设置指着不存在的接口（比如同步来的）按没设置。
    db.setting_set(
        AI_BUILTIN_DEFAULT_KEY,
        r#"{"endpoint_uid":"gone","model":"x"}"#,
    )
    .unwrap();
    assert_eq!(db.ai_builtin_default().unwrap(), None);
    // 坏 JSON 不报错。
    db.setting_set(AI_BUILTIN_DEFAULT_KEY, "not json").unwrap();
    assert_eq!(db.ai_builtin_default().unwrap(), None);

    db.set_ai_builtin_default(Some((a.id, None))).unwrap();
    db.set_ai_builtin_default(None).unwrap();
    assert_eq!(db.ai_builtin_default().unwrap(), None);
    assert!(db.set_ai_builtin_default(Some((9999, None))).is_err());
}

#[test]
fn bad_input_is_rejected_and_nothing_is_written() {
    let db = db();
    let mut bad = draft("x", Some("sk-key-0001"));
    bad.base_url = "not a url".to_owned();
    assert!(db.ai_endpoint_save(&bad).is_err());
    let bad = draft("  ", None);
    assert!(db.ai_endpoint_save(&bad).is_err());
    assert!(db.ai_endpoint_list().unwrap().is_empty());
    assert!(
        db.identity_list().unwrap().is_empty(),
        "校验失败不能留下孤儿凭据"
    );
    let mut missing = draft("x", None);
    missing.id = Some(77);
    assert!(matches!(
        db.ai_endpoint_save(&missing),
        Err(ZsshError::NotFound { .. })
    ));
}

#[test]
fn a_damaged_json_column_does_not_break_the_list() {
    let db = db();
    let saved = db.ai_endpoint_save(&draft("甲", None)).unwrap();
    db.with_conn(|conn| {
        conn.execute(
            "UPDATE ai_endpoints SET models = 'oops', overrides = '[', openai_options = 'x'",
            [],
        )?;
        Ok(())
    })
    .unwrap();
    let list = db.ai_endpoint_list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, saved.id);
    assert!(list[0].models.is_empty() && list[0].overrides.is_empty());
    assert_eq!(list[0].openai_options, OpenAiOptionsConfig::default());
}

#[test]
fn old_databases_upgrade_and_get_the_empty_table() {
    let dir = crate::test_util::TestDir::new("test-ai-endpoint-migration");
    let path = dir.join("legacy.db");
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("legacy-v6.db");
    std::fs::copy(&fixture, &path).expect("复制老库");
    let db = Database::open(&path).expect("老库升级");
    assert_eq!(
        db.schema_version().unwrap(),
        crate::store::db::MIGRATIONS.last().map(|m| m.0).unwrap()
    );
    assert!(db.ai_endpoint_list().unwrap().is_empty());
    assert!(!db.host_list().unwrap().is_empty(), "老数据留着");
    let saved = db
        .ai_endpoint_save(&draft("升级后新建", Some("sk-up-0001")))
        .unwrap();
    assert!(saved.has_key);
    drop(db);
    // 再打开一次，迁移不重复跑，数据还在。
    let db = Database::open(&path).unwrap();
    assert_eq!(db.ai_endpoint_list().unwrap().len(), 1);
}

#[test]
fn upgrading_from_version_twelve_adds_the_table_without_touching_other_data() {
    let dir = crate::test_util::TestDir::new("test-ai-endpoint-v12");
    let path = dir.join("v12.db");
    {
        let conn = rusqlite::Connection::open(&path).expect("建库");
        for (version, sql) in crate::store::db::MIGRATIONS.iter().take(12) {
            conn.execute_batch(sql).expect("跑 1-12 号迁移");
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                [version],
            )
            .expect("记版本号");
        }
        conn.execute(
            "INSERT INTO identities (uid, name, kind, updated_at) VALUES ('u-1', '老凭据', 'password', 1)",
            [],
        )
        .unwrap();
    }
    let db = Database::open(&path).expect("升级");
    assert_eq!(db.schema_version().unwrap(), 13);
    assert_eq!(db.identity_list().unwrap().len(), 1);
    assert!(db.ai_endpoint_list().unwrap().is_empty());
}

fn host_draft_with(
    identity_id: Option<i64>,
    auth: crate::model::AuthMethod,
) -> crate::model::HostDraft {
    crate::model::HostDraft {
        protocol: None,
        id: None,
        group_id: None,
        name: "web-1".to_owned(),
        host: "host.example".to_owned(),
        port: 22,
        username: "deploy".to_owned(),
        auth_method: auth,
        identity_id,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: crate::model::ProxyMode::Default,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    }
}

#[test]
fn the_key_identities_are_listed_and_hosts_cannot_pick_them() {
    let db = db();
    assert!(db.ai_endpoint_key_identity_ids().unwrap().is_empty());
    let saved = db
        .ai_endpoint_save(&draft("主力", Some("sk-test-0001")))
        .unwrap();
    let no_key = db.ai_endpoint_save(&draft("无 Key", None)).unwrap();
    assert!(!no_key.has_key);
    let key_id = saved.key_identity_id.unwrap();
    assert_eq!(db.ai_endpoint_key_identity_ids().unwrap(), vec![key_id]);

    // 主机把它选成密码：被拒，说明是 API Key。
    let err = db
        .host_save(&host_draft_with(
            Some(key_id),
            crate::model::AuthMethod::Password,
        ))
        .expect_err("不能选");
    assert!(err.to_string().contains("API Key"), "{err}");
    assert!(db.host_list().unwrap().is_empty());

    // 普通凭据照常能选。
    let normal = db
        .identity_save(&crate::model::IdentityDraft {
            id: None,
            name: "普通".to_owned(),
            kind: IdentityKind::Password,
            key_path: None,
            secret: Some("pw".to_owned()),
            passphrase: None,
        })
        .unwrap();
    db.host_save(&host_draft_with(
        Some(normal.id),
        crate::model::AuthMethod::Password,
    ))
    .unwrap();

    // 删接口：Key 凭据一起删，不留孤儿。
    db.ai_endpoint_delete(saved.id).unwrap();
    assert!(db.ai_endpoint_key_identity_ids().unwrap().is_empty());
    assert!(!identity_names(&db).iter().any(|n| n.contains("API Key")));
}

#[test]
fn the_models_auto_update_switch_defaults_on_and_is_a_local_setting() {
    let db = db();
    assert!(db.ai_models_auto_update().unwrap());
    db.set_ai_models_auto_update(false).unwrap();
    assert!(!db.ai_models_auto_update().unwrap());
    db.set_ai_models_auto_update(true).unwrap();
    assert!(db.ai_models_auto_update().unwrap());
    // 读不懂的值按开。
    db.setting_set(crate::store::settings::AI_MODELS_AUTO_UPDATE_KEY, "\"x\"")
        .unwrap();
    assert!(db.ai_models_auto_update().unwrap());
    assert!(
        crate::sync::settings::SETTINGS_SYNC_CLASS
            .iter()
            .any(|(key, mode)| {
                *key == "ai_models_auto_update"
                    && *mode == crate::sync::settings::SyncMode::LocalOnly
            }),
        "自动更新开关是本机设置，不进同步包"
    );
}
