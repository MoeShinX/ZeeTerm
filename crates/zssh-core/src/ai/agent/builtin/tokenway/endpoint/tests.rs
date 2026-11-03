use std::time::Duration;

use tokio::sync::Notify;

use super::*;
use crate::ai::agent::builtin::provider::ReqwestTransport;
use crate::ai::agent::builtin::tokenway::connect::{authorize, AuthorizeParams};
use crate::ai::agent::builtin::tokenway::fake::{FakeBrowser, FakeTokenway};

fn loose() -> UrlPolicy {
    UrlPolicy::with_http_loopback_for_tests()
}

fn transport() -> ReqwestTransport {
    ReqwestTransport::build(None).expect("传输层")
}

fn db() -> Database {
    Database::open_in_memory().expect("内存库")
}

async fn authorized(fake: &FakeTokenway) -> Authorized {
    let preset = fake.preset();
    let policy = loose();
    authorize(
        AuthorizeParams {
            urls: preset.authorize_urls().unwrap(),
            device: "测试 设备",
            aff: None,
            policy: &policy,
            timeout: Duration::from_secs(20),
        },
        &FakeBrowser::new(),
        &transport(),
        &Notify::new(),
        &|_| {},
    )
    .await
    .expect("授权成功")
}

fn models() -> Vec<String> {
    ["text-embedding-fake", "fake-chat-pro", "fake-chat-mini"]
        .map(str::to_owned)
        .to_vec()
}

#[test]
fn the_default_model_skips_non_chat_models() {
    assert_eq!(
        pick_default_model(&models()).as_deref(),
        Some("fake-chat-pro")
    );
    let only_odd = ["whisper-1".to_owned(), "dall-e-3".to_owned()];
    assert_eq!(pick_default_model(&only_odd).as_deref(), Some("whisper-1"));
    assert_eq!(pick_default_model(&[]), None);
}

#[test]
fn only_the_two_tokenway_presets_count_as_tokenway() {
    assert!(is_tokenway(Some(PRESET_AUTHORIZED)));
    assert!(is_tokenway(Some(PRESET_PASTED)));
    assert!(!is_tokenway(Some("openai")));
    assert!(!is_tokenway(None));
    assert!(is_authorized_token(Some("tokenway")));
    assert!(!is_authorized_token(Some("tokenway_key")));
    assert!(!is_authorized_token(None));
}

#[tokio::test]
async fn the_key_goes_into_the_vault_as_a_new_default_endpoint() {
    let fake = FakeTokenway::start().await.unwrap();
    let auth = authorized(&fake).await;
    let db = db();
    let preset = fake.preset();
    let saved = save_authorized(&db, &preset, &auth, Some(&models()), &loose()).expect("保存");
    assert_eq!(saved.name, "Tokenway");
    assert_eq!(saved.kind, EndpointKind::OpenAiChat);
    assert_eq!(saved.base_url, format!("{}/v1", fake.origin()));
    assert_eq!(saved.preset_id.as_deref(), Some("tokenway"));
    assert_eq!(saved.default_model.as_deref(), Some("fake-chat-pro"));
    assert!(saved.has_key && saved.is_ready());
    // Key 在保险库里，就是服务端发的那一把。
    assert_eq!(db.ai_endpoint_api_key(saved.id).unwrap(), fake.last_key());
    // 没有默认接口时它成了默认接口。
    let default = db.ai_builtin_default().unwrap().expect("设了默认");
    assert_eq!(default.endpoint_id, saved.id);
    // Key 的凭据对主机隐藏（接口存储本来的规则），而且接口表里没有 Key 的痕迹。
    assert_eq!(db.ai_endpoint_key_identity_ids().unwrap().len(), 1);
    assert!(!format!("{saved:?}").contains(&fake.last_key().unwrap()));
}

#[tokio::test]
async fn reconnecting_swaps_the_key_in_place_and_keeps_the_users_settings() {
    let fake = FakeTokenway::start().await.unwrap();
    let db = db();
    let preset = fake.preset();
    let first = save_authorized(
        &db,
        &preset,
        &authorized(&fake).await,
        Some(&models()),
        &loose(),
    )
    .unwrap();
    let first_key = fake.last_key().unwrap();
    // 用户改了名字、挑了别的模型、另有一个自己的默认接口。
    let mut edit = AiEndpointDraft {
        id: Some(first.id),
        name: "我的 Tokenway".into(),
        kind: EndpointKind::OpenAiChat,
        base_url: first.base_url.clone(),
        api_key: None,
        default_model: Some("fake-chat-mini".into()),
        models: vec!["extra-model".into()],
        overrides: Default::default(),
        openai_options: Default::default(),
        preset_id: Some("tokenway".into()),
    };
    edit.openai_options.include_usage = false;
    db.ai_endpoint_save(&edit).unwrap();

    let second = save_authorized(
        &db,
        &preset,
        &authorized(&fake).await,
        Some(&models()),
        &loose(),
    )
    .unwrap();
    assert_eq!(second.id, first.id, "原地更新，不新建第二条");
    assert_eq!(second.uid, first.uid);
    assert_eq!(db.ai_endpoint_list().unwrap().len(), 1);
    assert_eq!(second.name, "我的 Tokenway");
    assert_eq!(second.default_model.as_deref(), Some("fake-chat-mini"));
    assert_eq!(second.models, vec!["extra-model"]);
    assert!(!second.openai_options.include_usage);
    let second_key = db.ai_endpoint_api_key(second.id).unwrap().unwrap();
    assert_ne!(second_key, first_key, "Key 换成了新的");
    assert_eq!(Some(second_key), fake.last_key());
    assert_eq!(db.ai_endpoint_key_identity_ids().unwrap().len(), 1);
}

#[tokio::test]
async fn an_existing_default_endpoint_is_not_taken_over() {
    let fake = FakeTokenway::start().await.unwrap();
    let db = db();
    let mine = db
        .ai_endpoint_save(&AiEndpointDraft {
            id: None,
            name: "自己的".into(),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://api.example.com/v1".into(),
            api_key: Some("sk-test-mine-0001".into()),
            default_model: Some("m".into()),
            models: vec![],
            overrides: Default::default(),
            openai_options: Default::default(),
            preset_id: None,
        })
        .unwrap();
    db.set_ai_builtin_default(Some((mine.id, Some("m"))))
        .unwrap();
    let saved = save_authorized(
        &db,
        &fake.preset(),
        &authorized(&fake).await,
        Some(&models()),
        &loose(),
    )
    .unwrap();
    assert_ne!(saved.id, mine.id);
    assert_eq!(
        db.ai_builtin_default().unwrap().unwrap().endpoint_id,
        mine.id
    );
}

#[tokio::test]
async fn a_server_supplied_base_outside_the_policy_is_not_used() {
    let fake = FakeTokenway::start().await.unwrap();
    let mut auth = authorized(&fake).await;
    auth.openai_base = Some("https://api.evil.example/v1".into());
    let db = db();
    let preset = fake.preset();
    // 不合格的接入地址直接拒绝保存，Key 不会被存到别的地方去。
    let strict = UrlPolicy::production();
    assert!(save_authorized(&db, &preset, &auth, None, &strict).is_err());
    assert!(db.ai_endpoint_list().unwrap().is_empty());
    // 没给接入地址（被 api 层当作没给）就用预设里的。
    auth.openai_base = None;
    let saved = save_authorized(&db, &preset, &auth, None, &loose()).unwrap();
    assert_eq!(saved.base_url, preset.openai_base.unwrap());
}

#[tokio::test]
async fn disconnecting_revokes_on_the_server_then_deletes_the_endpoint_and_its_key() {
    let fake = FakeTokenway::start().await.unwrap();
    let db = db();
    let preset = fake.preset();
    let saved = save_authorized(&db, &preset, &authorized(&fake).await, None, &loose()).unwrap();
    let key = fake.last_key().unwrap();
    assert_eq!(fake.key_is_revoked(&key), Some(false));

    let report = disconnect(
        &db,
        &transport(),
        preset.revoke_url.as_deref(),
        &loose(),
        saved.id,
    )
    .await
    .unwrap();
    assert!(report.revoked.is_ok());
    assert_eq!(fake.key_is_revoked(&key), Some(true), "服务端已停用");
    assert_eq!(fake.count("POST /api/connect/revoke"), 1);
    assert!(db.ai_endpoint_list().unwrap().is_empty());
    assert!(
        db.identity_list().unwrap().is_empty(),
        "保险库里的 Key 也删了"
    );
    assert!(db.ai_builtin_default().unwrap().is_none());
}

#[tokio::test]
async fn disconnecting_still_deletes_locally_when_the_server_cannot_be_reached() {
    let fake = FakeTokenway::start().await.unwrap();
    let db = db();
    let preset = fake.preset();
    let saved = save_authorized(&db, &preset, &authorized(&fake).await, None, &loose()).unwrap();
    let revoke_url = preset.revoke_url.clone().unwrap();
    drop(fake); // 服务没了：连不上。
    let report = disconnect(&db, &transport(), Some(&revoke_url), &loose(), saved.id)
        .await
        .unwrap();
    let err = report.revoked.expect_err("没通知到服务端");
    assert_eq!(
        err.kind,
        crate::ai::agent::builtin::tokenway::ApiErrorKind::Network
    );
    assert!(
        db.ai_endpoint_list().unwrap().is_empty(),
        "失败也继续删本地"
    );
    assert!(db.identity_list().unwrap().is_empty());
}

#[tokio::test]
async fn a_pasted_key_endpoint_is_only_deleted_locally() {
    let fake = FakeTokenway::start().await.unwrap();
    let db = db();
    let saved = db
        .ai_endpoint_save(&AiEndpointDraft {
            id: None,
            name: "Tokenway".into(),
            kind: EndpointKind::OpenAiChat,
            base_url: format!("{}/v1", fake.origin()),
            api_key: Some("sk-test-pasted-0001".into()),
            default_model: None,
            models: vec![],
            overrides: Default::default(),
            openai_options: Default::default(),
            preset_id: Some(PRESET_PASTED.into()),
        })
        .unwrap();
    let revoke_url = fake.preset().revoke_url;
    let report = disconnect(&db, &transport(), revoke_url.as_deref(), &loose(), saved.id)
        .await
        .unwrap();
    assert!(report.revoked.is_ok());
    assert_eq!(
        fake.count("/api/connect/revoke"),
        0,
        "自己粘贴的 Key 不通知服务端"
    );
    assert!(db.ai_endpoint_list().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_revoke_address_just_deletes_locally() {
    let fake = FakeTokenway::start().await.unwrap();
    let db = db();
    let saved = save_authorized(
        &db,
        &fake.preset(),
        &authorized(&fake).await,
        None,
        &loose(),
    )
    .unwrap();
    let report = disconnect(&db, &transport(), None, &loose(), saved.id)
        .await
        .unwrap();
    assert!(report.revoked.is_ok());
    assert!(db.ai_endpoint_list().unwrap().is_empty());
}
