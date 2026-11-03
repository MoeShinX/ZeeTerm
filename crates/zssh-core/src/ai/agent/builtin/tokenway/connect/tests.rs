use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;

use super::*;
use crate::ai::agent::builtin::provider::ReqwestTransport;
use crate::ai::agent::builtin::tokenway::api::ApiErrorKind;
use crate::ai::agent::builtin::tokenway::fake::{Approve, FakeBrowser, FakeTokenway};
use crate::ai::agent::builtin::tokenway::presets::Preset;
use crate::ai::agent::builtin::tokenway::testsupport::Stub;

fn loose() -> UrlPolicy {
    UrlPolicy::with_http_loopback_for_tests()
}

fn transport() -> ReqwestTransport {
    ReqwestTransport::build(None).expect("传输层")
}

fn no_stage() -> impl Fn(Stage) + Send + Sync {
    |_| {}
}

// ---------------------------------------------------------------------------
// PKCE / state
// ---------------------------------------------------------------------------

#[test]
fn pkce_values_follow_rfc_7636() {
    // RFC 7636 附录 B 的测试向量。
    assert_eq!(
        pkce::challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let a = pkce::generate().unwrap();
    let b = pkce::generate().unwrap();
    assert_ne!(a.verifier, b.verifier, "每次都是新的");
    for pair in [&a, &b] {
        let len = pair.verifier.len();
        assert!((43..=128).contains(&len), "verifier 长度 {len}");
        assert!(
            pair.verifier
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
            "verifier 只能含非保留字符"
        );
        assert_eq!(
            pair.challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(pair.verifier.as_bytes()))
        );
        assert_eq!(pair.challenge.len(), 43, "S256 的 challenge 固定 43 位");
    }
    let state = pkce::random_state().unwrap();
    assert!(state.len() >= 8 && state.len() <= 256);
    assert_ne!(state, pkce::random_state().unwrap());
}

// ---------------------------------------------------------------------------
// 授权页地址 / 设备名 / 邀请码
// ---------------------------------------------------------------------------

#[test]
fn the_authorize_url_carries_every_parameter_the_server_expects() {
    let url = build_connect_url(
        "https://tokenwayapi.com/connect/zeeterm",
        "http://127.0.0.1:50123/AbC_-9/cb",
        "state-value",
        "challenge-value",
        "办公室 电脑",
        Some("inv_01"),
    )
    .unwrap();
    let parsed = Url::parse(&url).unwrap();
    assert_eq!(parsed.host_str(), Some("tokenwayapi.com"));
    assert_eq!(parsed.path(), "/connect/zeeterm");
    let get = |name: &str| {
        parsed
            .query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    assert_eq!(
        get("redirect_uri").as_deref(),
        Some("http://127.0.0.1:50123/AbC_-9/cb")
    );
    assert_eq!(get("state").as_deref(), Some("state-value"));
    assert_eq!(get("code_challenge").as_deref(), Some("challenge-value"));
    assert_eq!(get("code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(get("device").as_deref(), Some("办公室 电脑"));
    assert_eq!(get("aff").as_deref(), Some("inv_01"));
    assert!(!url.contains(' '), "空格要编码：{url}");
    // 没有设备名 / 邀请码就不带这两个参数；预设地址里原有的查询参数保留。
    let url = build_connect_url(
        "https://tokenwayapi.com/connect/zeeterm?source=zeeterm",
        "http://127.0.0.1:50123/x/cb",
        "s",
        "c",
        "",
        None,
    )
    .unwrap();
    assert!(url.contains("source=zeeterm"));
    assert!(!url.contains("device=") && !url.contains("aff="));
}

#[test]
fn device_names_never_carry_a_user_name_or_odd_characters() {
    // Windows 默认的随机计算机名没有信息量，换成通用说法。
    assert_eq!(
        friendly_device_name(Some("DESKTOP-7K3FQ2L")),
        "Windows 台式机"
    );
    assert_eq!(
        friendly_device_name(Some("laptop-abc123")),
        "Windows 笔记本"
    );
    // 其余的把 - _ 换成空格；整个字符串当作计算机名处理，不会去读用户名。
    assert_eq!(friendly_device_name(Some("OFFICE-PC_2")), "OFFICE PC 2");
    assert_eq!(friendly_device_name(None), "Windows 电脑");
    assert_eq!(friendly_device_name(Some("   ")), "Windows 电脑");
    assert_eq!(friendly_device_name(Some("张三的电脑")), "张三的电脑");
    let long = "x".repeat(100);
    assert_eq!(friendly_device_name(Some(&long)).chars().count(), 40);
    let cleaned = clean_device("a\u{7}b<c>\"d'e`f\\g\n h");
    assert_eq!(cleaned, "abcdefg h");
}

#[test]
fn the_invite_code_comes_only_from_the_register_url() {
    assert_eq!(
        aff_from_register_url("https://tokenwayapi.com/register?source=zeeterm&aff=ab-12_X")
            .as_deref(),
        Some("ab-12_X")
    );
    for none in [
        "https://tokenwayapi.com/register?source=zeeterm",
        "https://tokenwayapi.com/register?aff=",
        "https://tokenwayapi.com/register?aff=a%20b",
        "https://tokenwayapi.com/register?aff=%3Cscript%3E",
        "not a url",
    ] {
        assert_eq!(aff_from_register_url(none), None, "{none}");
    }
}

// ---------------------------------------------------------------------------
// 完整授权（本机假 Tokenway）
// ---------------------------------------------------------------------------

async fn run_authorize(
    fake: &FakeTokenway,
    opener: &dyn UrlOpener,
    timeout: Duration,
    cancel: &Notify,
) -> Result<Authorized, ConnectError> {
    let preset = fake.preset();
    let urls = preset.authorize_urls().unwrap();
    let policy = loose();
    authorize(
        AuthorizeParams {
            urls,
            device: "测试 设备",
            aff: Some("inv01"),
            policy: &policy,
            timeout,
        },
        opener,
        &transport(),
        cancel,
        &no_stage(),
    )
    .await
}

#[tokio::test]
async fn a_full_authorization_trades_the_code_for_a_key_and_asks_for_nothing_else() {
    let fake = FakeTokenway::start().await.unwrap();
    let browser = FakeBrowser::new();
    let cancel = Notify::new();
    let stages = Arc::new(Mutex::new(Vec::new()));
    let preset = fake.preset();
    let policy = loose();
    let recorded = stages.clone();
    let got = authorize(
        AuthorizeParams {
            urls: preset.authorize_urls().unwrap(),
            device: "测试 设备",
            aff: Some("inv01"),
            policy: &policy,
            timeout: Duration::from_secs(20),
        },
        &browser,
        &transport(),
        &cancel,
        &move |stage| recorded.lock().unwrap().push(stage),
    )
    .await
    .expect("授权成功");
    assert_eq!(Some(got.api_key.expose().to_owned()), fake.last_key());
    assert!(got.api_key.expose().starts_with("sk-test-zt-"));
    assert_eq!(
        got.openai_base.as_deref(),
        Some(format!("{}/v1", fake.origin()).as_str())
    );
    assert_eq!(
        *stages.lock().unwrap(),
        vec![Stage::WaitingForBrowser, Stage::Exchanging]
    );

    // 只发了两个请求：授权页（浏览器）和换令牌，别的什么都没有。
    assert_eq!(
        fake.requests(),
        ["GET /connect/zeeterm", "POST /api/connect/token"]
    );
    assert_eq!(fake.authed_requests(), 0);
    // 授权页收到的参数：设备名（不含用户名）、邀请码来自参数、回调只在本机回环。
    assert_eq!(fake.last_device().as_deref(), Some("测试 设备"));
    let query = fake.last_authorize_query();
    let value = |name: &str| {
        query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    };
    let redirect = value("redirect_uri").unwrap();
    assert!(redirect.starts_with("http://127.0.0.1:"), "{redirect}");
    assert!(redirect.ends_with("/cb"));
    assert_eq!(value("code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(value("aff").as_deref(), Some("inv01"));
    assert_eq!(value("code_challenge").unwrap().len(), 43);
    // 授权结束后回环端口已关闭。
    let port: u16 = redirect
        .split(':')
        .nth(2)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
}

#[tokio::test]
async fn denying_in_the_browser_is_reported_and_no_token_is_requested() {
    let fake = FakeTokenway::start().await.unwrap();
    fake.set_approve(Approve::Deny);
    let err = run_authorize(
        &fake,
        &FakeBrowser::new(),
        Duration::from_secs(20),
        &Notify::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err, ConnectError::Denied);
    assert!(err.is_user_choice());
    assert_eq!(fake.count("/api/connect/token"), 0);
    assert!(err.to_string().contains("拒绝"));
}

/// 把授权页地址里的 `state` 偷偷换掉再去访问：相当于回调的 state 对不上。
struct StateTamperingBrowser;

impl UrlOpener for StateTamperingBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        let tampered = url.replace("state=", "state=x");
        FakeBrowser::new().open(&tampered)
    }
}

#[tokio::test]
async fn a_callback_with_the_wrong_state_fails_without_ever_calling_the_token_endpoint() {
    let fake = FakeTokenway::start().await.unwrap();
    let err = run_authorize(
        &fake,
        &StateTamperingBrowser,
        Duration::from_secs(20),
        &Notify::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(err, ConnectError::Invalid);
    assert_eq!(err.to_string(), "授权失败，请重试");
    assert_eq!(
        fake.count("/api/connect/token"),
        0,
        "state 不对不能拿码去换令牌"
    );
}

#[tokio::test]
async fn waiting_times_out_and_can_be_cancelled_while_the_browser_is_held() {
    let fake = FakeTokenway::start().await.unwrap();
    fake.set_approve(Approve::Hold);
    let browser = FakeBrowser::new();
    let err = run_authorize(&fake, &browser, Duration::from_millis(300), &Notify::new())
        .await
        .unwrap_err();
    assert_eq!(err, ConnectError::Timeout);
    assert!(err.to_string().contains("5 分钟"));
    // 超时后回环端口关了。
    let url = Url::parse(&browser.opened()[0]).unwrap();
    let redirect = url
        .query_pairs()
        .find(|(k, _)| k == "redirect_uri")
        .unwrap()
        .1
        .into_owned();
    let port = Url::parse(&redirect).unwrap().port().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());

    // 取消。
    let cancel = Arc::new(Notify::new());
    let stop = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        stop.notify_one();
    });
    let err = run_authorize(&fake, &FakeBrowser::new(), Duration::from_secs(60), &cancel)
        .await
        .unwrap_err();
    assert_eq!(err, ConnectError::Cancelled);
    assert!(err.is_user_choice());
    assert_eq!(fake.count("/api/connect/token"), 0);
    fake.release();
}

struct BrokenBrowser;

impl UrlOpener for BrokenBrowser {
    fn open(&self, _url: &str) -> Result<(), String> {
        Err("没有默认浏览器".to_owned())
    }
}

#[tokio::test]
async fn a_browser_that_cannot_open_is_reported() {
    let fake = FakeTokenway::start().await.unwrap();
    let err = run_authorize(
        &fake,
        &BrokenBrowser,
        Duration::from_secs(5),
        &Notify::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ConnectError::Browser(_)), "{err:?}");
    assert!(err.to_string().contains("没有默认浏览器"));
}

#[tokio::test]
async fn addresses_outside_the_policy_never_open_a_browser() {
    let browser = FakeBrowser::new();
    let preset = Preset {
        connect_url: Some("https://evil.example/connect".to_owned()),
        token_url: Some("https://tokenwayapi.com/api/connect/token".to_owned()),
        status_url: Some("https://tokenwayapi.com/s".to_owned()),
        revoke_url: Some("https://tokenwayapi.com/r".to_owned()),
        ..FakeTokenway::start().await.unwrap().preset()
    };
    let policy = UrlPolicy::production();
    let err = authorize(
        AuthorizeParams {
            urls: preset.authorize_urls().unwrap(),
            device: "d",
            aff: None,
            policy: &policy,
            timeout: Duration::from_secs(5),
        },
        &browser,
        &Stub::new(),
        &Notify::new(),
        &no_stage(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, ConnectError::Api(api) if api.kind == ApiErrorKind::Refused),
        "{err:?}"
    );
    assert!(browser.opened().is_empty());
}

#[tokio::test]
async fn an_authorization_code_works_once_and_only_with_its_own_verifier() {
    let fake = FakeTokenway::start().await.unwrap();
    let browser = FakeBrowser::new();
    // 走一遍授权页拿到真实的码，但不去换，自己手动换。
    let preset = fake.preset();
    let verifier = "v".repeat(43);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let redirect = "http://127.0.0.1:59999/seg/cb";
    let url = build_connect_url(
        preset.connect_url.as_deref().unwrap(),
        redirect,
        "state-for-the-test",
        &challenge,
        "d",
        None,
    )
    .unwrap();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let take_code = || async {
        let response = client.get(&url).send().await.unwrap();
        let location = response.headers()["location"].to_str().unwrap().to_owned();
        Url::parse(&location)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "code")
            .unwrap()
            .1
            .into_owned()
    };
    let token_url = preset.token_url.clone().unwrap();
    let try_exchange = |code: String, verifier: String, redirect_uri: &'static str| {
        let token_url = token_url.clone();
        async move {
            exchange_token(
                &transport(),
                &loose(),
                &token_url,
                &TokenRequest {
                    code: &code,
                    code_verifier: &verifier,
                    redirect_uri,
                    device: "d",
                },
            )
            .await
        }
    };
    // 正确的 verifier：成功一次，重放失败。
    let code = take_code().await;
    assert!(try_exchange(code.clone(), verifier.clone(), redirect)
        .await
        .is_ok());
    let replay = try_exchange(code, verifier.clone(), redirect)
        .await
        .unwrap_err();
    assert_eq!(replay.kind, ApiErrorKind::InvalidGrant);
    // verifier 不对（PKCE 不匹配）：失败，而且码也作废了。
    let code = take_code().await;
    let wrong = try_exchange(code.clone(), "w".repeat(43), redirect)
        .await
        .unwrap_err();
    assert_eq!(wrong.kind, ApiErrorKind::InvalidGrant);
    assert_eq!(
        try_exchange(code, verifier.clone(), redirect)
            .await
            .unwrap_err()
            .kind,
        ApiErrorKind::InvalidGrant
    );
    // 回调地址不一致：失败。
    let code = take_code().await;
    assert_eq!(
        try_exchange(code, verifier, "http://127.0.0.1:1111/other/cb")
            .await
            .unwrap_err()
            .kind,
        ApiErrorKind::InvalidGrant
    );
    let _ = browser;
}
