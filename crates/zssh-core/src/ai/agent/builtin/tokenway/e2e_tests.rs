//! 端到端（本机假 Tokenway，不联网）：连接 → 存保险库 → 取模型 → 对话 → 余额 → 需充值 → 授权失效 → 断开。
//! 全部走真实的 `ReqwestTransport` 和本机回环 HTTP；界面层的同一条流程见原生应用的端到端测试。

use std::time::Duration;

use tokio::sync::Notify;

use super::api::{
    classify_chat_failure, fetch_status, ChatFailureAction, RefreshGate, BALANCE_MIN_INTERVAL,
};
use super::connect::{authorize, AuthorizeParams};
use super::endpoint::{disconnect, save_authorized};
use super::fake::{FakeBrowser, FakeTokenway};
use super::presets::UrlPolicy;
use crate::ai::agent::builtin::conversation::FailureInfo;
use crate::ai::agent::builtin::probe::{fetch_models, test_endpoint, MODELS_TIMEOUT};
use crate::ai::agent::builtin::provider::{ApiKey, ProviderError, ReqwestTransport};
use crate::store::Database;

fn failure_of(error: &ProviderError) -> FailureInfo {
    FailureInfo {
        kind: error.kind,
        code: error.code.clone(),
        status: error.status,
    }
}

#[tokio::test]
async fn connect_chat_balance_topup_revocation_and_disconnect_against_a_local_fake() {
    let fake = FakeTokenway::start().await.unwrap();
    let transport = ReqwestTransport::build(None).unwrap();
    let policy = UrlPolicy::with_http_loopback_for_tests();
    let preset = fake.preset();
    let db = Database::open_in_memory().unwrap();

    // 1 浏览器授权（假浏览器 = 授权页直接 302 回调）。
    let authorized = authorize(
        AuthorizeParams {
            urls: preset.authorize_urls().unwrap(),
            device: "e2e 设备",
            aff: None,
            policy: &policy,
            timeout: Duration::from_secs(20),
        },
        &FakeBrowser::new(),
        &transport,
        &Notify::new(),
        &|_| {},
    )
    .await
    .expect("授权");

    // 2 用新令牌取模型清单，存成接口（Key 进保险库）。
    let probe_config = |key: &str, base: &str| {
        crate::ai::agent::builtin::provider::ProviderConfig::new(
            crate::ai::agent::builtin::provider::ProviderKind::OpenAiChat,
            base,
            ApiKey::new(key),
        )
    };
    let base = format!("{}/v1", fake.origin());
    let listed = fetch_models(
        &transport,
        &probe_config(authorized.api_key.expose(), &base),
        MODELS_TIMEOUT,
    )
    .await
    .expect("模型清单");
    let ids: Vec<String> = listed.into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids.len(), 3);
    let endpoint = save_authorized(&db, &preset, &authorized, Some(&ids), &policy).unwrap();
    assert_eq!(endpoint.default_model.as_deref(), Some("fake-chat-pro"));
    let key = db.ai_endpoint_api_key(endpoint.id).unwrap().unwrap();
    assert_eq!(Some(key.clone()), fake.last_key());

    // 3 对话：用保险库里取出的 Key，经真实的 provider 层。
    let config = endpoint.provider_config(ApiKey::new(key.clone()));
    let probe = test_endpoint(&transport, &config, "fake-chat-pro").await;
    let reply = probe.reply.expect("对话成功");
    assert!(reply.text.contains("收到"), "{}", reply.text);

    // 4 余额：充值余额显示；节流最多每 60 秒一次。
    let mut gate = RefreshGate::new(BALANCE_MIN_INTERVAL);
    let now = std::time::Instant::now();
    assert!(gate.try_start(now));
    let status_url = preset.status_url.as_deref().unwrap();
    let balance = fetch_status(&transport, &policy, status_url, &ApiKey::new(key.clone()))
        .await
        .unwrap();
    assert_eq!(balance.label(), "余额 ¥12.34");
    assert!(!gate.try_start(now + Duration::from_secs(30)));

    // 5 需充值：状态变成"需充值"，对话回 402 + 错误码，界面据此给"去充值"。
    fake.set_needs_topup(true);
    let balance = fetch_status(&transport, &policy, status_url, &ApiKey::new(key.clone()))
        .await
        .unwrap();
    assert!(balance.needs_topup);
    assert_eq!(balance.label(), "需充值");
    let probe = test_endpoint(&transport, &config, "fake-chat-pro").await;
    let error = probe.reply.expect_err("402");
    assert_eq!(error.status, Some(402));
    assert_eq!(
        classify_chat_failure(&failure_of(&error)),
        Some(ChatFailureAction::RequiresTopup)
    );
    fake.set_needs_topup(false);
    fake.set_balance(500, 700);

    // 6 授权失效（用户在网站改了密码 / 停用了授权）：对话 401、状态 401，界面给"重新连接"，本地配置不删。
    fake.revoke_all();
    let probe = test_endpoint(&transport, &config, "fake-chat-pro").await;
    let error = probe.reply.expect_err("401");
    assert_eq!(error.status, Some(401));
    assert_eq!(
        classify_chat_failure(&failure_of(&error)),
        Some(ChatFailureAction::AuthorizationInvalid)
    );
    let err = fetch_status(&transport, &policy, status_url, &ApiKey::new(key.clone()))
        .await
        .unwrap_err();
    assert_eq!(err.kind, super::api::ApiErrorKind::InvalidToken);
    assert_eq!(db.ai_endpoint_list().unwrap().len(), 1, "不自动删本地配置");

    // 7 重新连接：原地换 Key，之后又能聊了。
    let again = authorize(
        AuthorizeParams {
            urls: preset.authorize_urls().unwrap(),
            device: "e2e 设备",
            aff: None,
            policy: &policy,
            timeout: Duration::from_secs(20),
        },
        &FakeBrowser::new(),
        &transport,
        &Notify::new(),
        &|_| {},
    )
    .await
    .expect("重新授权");
    let endpoint = save_authorized(&db, &preset, &again, None, &policy).unwrap();
    assert_eq!(db.ai_endpoint_list().unwrap().len(), 1);
    let key = db.ai_endpoint_api_key(endpoint.id).unwrap().unwrap();
    let config = endpoint.provider_config(ApiKey::new(key.clone()));
    assert!(test_endpoint(&transport, &config, "fake-chat-pro")
        .await
        .ok());

    // 8 断开：服务端停用 + 删接口 + 删保险库里的 Key。
    let report = disconnect(
        &db,
        &transport,
        preset.revoke_url.as_deref(),
        &policy,
        endpoint.id,
    )
    .await
    .unwrap();
    assert!(report.revoked.is_ok());
    assert_eq!(fake.key_is_revoked(&key), Some(true));
    assert!(db.ai_endpoint_list().unwrap().is_empty());
    assert!(db.identity_list().unwrap().is_empty());

    // 只请求过约定的这些路径：没有任何使用统计 / 遥测。
    let known = [
        "GET /connect/zeeterm",
        "POST /api/connect/token",
        "GET /api/connect/status",
        "POST /api/connect/revoke",
        "GET /v1/models",
        "POST /v1/chat/completions",
    ];
    for request in fake.requests() {
        assert!(known.contains(&request.as_str()), "多余的请求：{request}");
    }
}
