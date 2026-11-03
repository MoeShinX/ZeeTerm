use std::time::{Duration, Instant};

use super::*;
use crate::ai::agent::builtin::conversation::FailureInfo;
use crate::ai::agent::builtin::provider::{ErrorKind, TransportError};
use crate::ai::agent::builtin::tokenway::testsupport::Stub;

const TOKEN_URL: &str = "https://tokenwayapi.com/api/connect/token";
const STATUS_URL: &str = "https://tokenwayapi.com/api/connect/status";
const REVOKE_URL: &str = "https://tokenwayapi.com/api/connect/revoke";

fn policy() -> UrlPolicy {
    UrlPolicy::production()
}

fn request() -> TokenRequest<'static> {
    TokenRequest {
        code: "the-code",
        code_verifier: "the-verifier-the-verifier-the-verifier-0123",
        redirect_uri: "http://127.0.0.1:50000/seg/cb",
        device: "Windows 台式机",
    }
}

// ---------------------------------------------------------------------------
// 错误码映射
// ---------------------------------------------------------------------------

fn body(code: &str, message: &str) -> Vec<u8> {
    serde_json::json!({"error": {"code": code, "message": message}})
        .to_string()
        .into_bytes()
}

#[test]
fn http_errors_map_to_kinds_with_chinese_messages() {
    let table: &[(u16, &str, ApiErrorKind)] = &[
        (401, "invalid_token", ApiErrorKind::InvalidToken),
        (401, "", ApiErrorKind::InvalidToken),
        (400, "invalid_grant", ApiErrorKind::InvalidGrant),
        (429, "rate_limited", ApiErrorKind::RateLimited),
        (429, "", ApiErrorKind::RateLimited),
        (503, "unavailable", ApiErrorKind::Unavailable),
        (500, "server_error", ApiErrorKind::Unavailable),
        (502, "status_unavailable", ApiErrorKind::Unavailable),
        (400, "invalid_request", ApiErrorKind::Rejected),
        (404, "", ApiErrorKind::Rejected),
    ];
    for (status, code, kind) in table {
        let err = error_from_response(*status, &body(code, "服务端的话"));
        assert_eq!(err.kind, *kind, "{status} {code}");
        assert_eq!(err.status, Some(*status));
        assert!(
            err.message.chars().any(|c| c as u32 > 0x4e00),
            "要有中文原因：{}",
            err.message
        );
    }
    // 授权失效的提示就是卡里要的那句。
    let err = error_from_response(401, &body("invalid_token", "x"));
    assert!(err.message.contains("Tokenway 授权已失效"));
    assert!(err.message.contains("重新连接"));
    // 错误码原文保留给界面（不是拿文字匹配）。
    assert_eq!(
        error_from_response(400, &body("invalid_grant", "x"))
            .code
            .as_deref(),
        Some("invalid_grant")
    );
}

#[test]
fn server_text_is_cleaned_before_it_reaches_the_screen() {
    let err = error_from_response(418, &body("teapot", "第一行\n第二行\u{7}\u{1b}[31m红"));
    assert!(
        !err.message.chars().any(char::is_control),
        "{:?}",
        err.message
    );
    let long = "长".repeat(500);
    let err = error_from_response(418, &body("x", &long));
    assert!(
        err.message.chars().count() < 200,
        "{}",
        err.message.chars().count()
    );
    // 响应体不是 JSON / 为空。
    for raw in [&b"<html>bad gateway</html>"[..], &b""[..], &b"null"[..]] {
        let err = error_from_response(418, raw);
        assert_eq!(err.kind, ApiErrorKind::Rejected);
        assert!(err.message.contains("HTTP 418"));
    }
    assert_eq!(
        error_from_response(502, b"<html>").kind,
        ApiErrorKind::Unavailable
    );
}

fn failure(kind: ErrorKind, status: Option<u16>, code: Option<&str>) -> FailureInfo {
    FailureInfo {
        kind,
        code: code.map(str::to_owned),
        status,
    }
}

#[test]
fn chat_failures_are_classified_for_topup_reconnect_and_rate_limit() {
    use ChatFailureAction::*;
    assert_eq!(
        classify_chat_failure(&failure(
            ErrorKind::Quota,
            Some(402),
            Some("zeeterm_requires_topup")
        )),
        Some(RequiresTopup)
    );
    // 错误码优先于状态码。
    assert_eq!(
        classify_chat_failure(&failure(
            ErrorKind::Auth,
            Some(401),
            Some("zeeterm_requires_topup")
        )),
        Some(RequiresTopup)
    );
    assert_eq!(
        classify_chat_failure(&failure(ErrorKind::Auth, Some(401), Some("invalid_token"))),
        Some(AuthorizationInvalid)
    );
    assert_eq!(
        classify_chat_failure(&failure(ErrorKind::Auth, Some(401), None)),
        Some(AuthorizationInvalid)
    );
    assert_eq!(
        classify_chat_failure(&failure(ErrorKind::RateLimited, Some(429), None)),
        Some(RateLimited)
    );
    assert_eq!(
        classify_chat_failure(&failure(ErrorKind::RateLimited, None, None)),
        Some(RateLimited)
    );
    // 别的失败不归 Tokenway 管：余额不足（没带错误码的 402）、403、5xx、连不上、没状态码。
    for other in [
        failure(ErrorKind::Quota, Some(402), None),
        failure(ErrorKind::Auth, Some(403), None),
        failure(ErrorKind::Server, Some(500), None),
        failure(ErrorKind::Connect, None, None),
        failure(ErrorKind::Unknown, None, Some("something_else")),
    ] {
        assert_eq!(classify_chat_failure(&other), None, "{other:?}");
    }
}

// ---------------------------------------------------------------------------
// 余额
// ---------------------------------------------------------------------------

#[test]
fn yuan_formatting_uses_two_decimals_and_never_goes_negative() {
    assert_eq!(format_yuan(0), "¥0.00");
    assert_eq!(format_yuan(5), "¥0.05");
    assert_eq!(format_yuan(1234), "¥12.34");
    assert_eq!(format_yuan(123_456), "¥1234.56");
    assert_eq!(format_yuan(-300), "¥0.00");
    // 浮点误差不能漏到界面上。
    assert_eq!(yuan_to_cents(0.1 + 0.2), 30);
    assert_eq!(yuan_to_cents(12.34), 1234);
    assert_eq!(yuan_to_cents(f64::NAN), 0);
}

#[test]
fn the_status_body_becomes_a_balance_with_a_label() {
    let balance = parse_balance(
        br#"{"balance_paid":12.34,"balance_total":14.34,"zeeterm_usable":12.34,
            "quota_limit":50,"used":1.25,"expires_at":"2027-05-01T00:00:00Z","needs_topup":false}"#,
    )
    .unwrap();
    assert_eq!(balance.usable_cents, 1234);
    assert_eq!(balance.paid_cents, 1234);
    assert_eq!(balance.total_cents, 1434);
    assert_eq!(balance.quota_limit_cents, Some(5000));
    assert_eq!(balance.used_cents, 125);
    assert!(!balance.needs_topup);
    assert_eq!(balance.label(), "余额 ¥12.34");
    assert!(balance.detail().contains("账户余额 ¥14.34"));
    assert!(balance.detail().contains("授权上限 ¥50.00"));

    // 没有可用的充值余额：显示"需充值"。
    let topup = parse_balance(
        br#"{"balance_paid":0,"balance_total":2,"zeeterm_usable":0,"quota_limit":null,"used":0,"needs_topup":true}"#,
    )
    .unwrap();
    assert!(topup.needs_topup);
    assert_eq!(topup.label(), "需充值");
    assert_eq!(topup.quota_limit_cents, None);
    assert!(topup.detail().contains("赠送额度可在网页和其他客户端试用"));
    // 没带 needs_topup 时按可用额度推。
    let derived = parse_balance(br#"{"zeeterm_usable":0}"#).unwrap();
    assert!(derived.needs_topup);
    // 缺关键字段 / 不是 JSON。
    for bad in [&br#"{"balance_total":3}"#[..], b"nope", b"[]"] {
        assert_eq!(
            parse_balance(bad).unwrap_err().kind,
            ApiErrorKind::BadResponse
        );
    }
}

#[test]
fn the_refresh_gate_allows_one_refresh_per_interval() {
    let mut gate = RefreshGate::new(BALANCE_MIN_INTERVAL);
    let start = Instant::now();
    assert!(gate.try_start(start), "第一次放行");
    assert!(!gate.try_start(start + Duration::from_secs(1)));
    assert!(!gate.try_start(start + Duration::from_secs(59)));
    assert!(
        gate.try_start(start + Duration::from_secs(60)),
        "满 60 秒放行"
    );
    assert!(
        !gate.try_start(start + Duration::from_secs(61)),
        "上一次放行后重新计时"
    );
    gate.reset();
    assert!(
        gate.try_start(start + Duration::from_secs(62)),
        "重新连接后立刻放行"
    );
    assert_eq!(BALANCE_MIN_INTERVAL, Duration::from_secs(60));
}

// ---------------------------------------------------------------------------
// 请求本身
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_token_exchange_posts_the_code_and_verifier_and_parses_the_key() {
    let stub = Stub::new().reply(
        200,
        r#"{"api_key":"sk-test-abcdef123456","token_id":42,"base_url_openai":"https://api.tokenwayapi.com/v1",
            "base_url_anthropic":"https://api.evil.example","expires_at":"2027-05-01T00:00:00Z","quota_limit":50}"#,
    );
    let got = exchange_token(&stub, &policy(), TOKEN_URL, &request())
        .await
        .unwrap();
    assert_eq!(got.api_key.expose(), "sk-test-abcdef123456");
    assert_eq!(got.token_id.as_deref(), Some("42"));
    assert_eq!(
        got.openai_base.as_deref(),
        Some("https://api.tokenwayapi.com/v1")
    );
    assert_eq!(got.anthropic_base, None, "白名单之外的接入地址当没给");
    assert_eq!(got.quota_limit, Some(50.0));

    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    let (is_get, sent) = &requests[0];
    assert!(!is_get);
    assert_eq!(sent.url, TOKEN_URL);
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["grant_type"], "authorization_code");
    assert_eq!(body["code"], "the-code");
    assert_eq!(
        body["code_verifier"],
        "the-verifier-the-verifier-the-verifier-0123"
    );
    assert_eq!(body["redirect_uri"], "http://127.0.0.1:50000/seg/cb");
    assert_eq!(body["device"], "Windows 台式机");
    // 令牌不在 Debug 输出里。
    assert!(!format!("{got:?}").contains("abcdef123456"));
}

#[tokio::test]
async fn token_exchange_errors_are_mapped_and_odd_keys_are_refused() {
    let table: &[(u16, &str, ApiErrorKind)] = &[
        (
            400,
            r#"{"error":{"code":"invalid_grant","message":"x"}}"#,
            ApiErrorKind::InvalidGrant,
        ),
        (
            429,
            r#"{"error":{"code":"rate_limited","message":"x"}}"#,
            ApiErrorKind::RateLimited,
        ),
        (
            503,
            r#"{"error":{"code":"unavailable","message":"x"}}"#,
            ApiErrorKind::Unavailable,
        ),
    ];
    for (status, body, kind) in table {
        let stub = Stub::new().reply(*status, body);
        let err = exchange_token(&stub, &policy(), TOKEN_URL, &request())
            .await
            .unwrap_err();
        assert_eq!(err.kind, *kind);
    }
    for bad in [
        r#"{}"#,
        r#"{"api_key":""}"#,
        r#"{"api_key":"sk with space"}"#,
        r#"{"api_key":"sk-a\nb"}"#,
        r#"{"api_key":5}"#,
        "not json",
    ] {
        let stub = Stub::new().reply(200, bad);
        let err = exchange_token(&stub, &policy(), TOKEN_URL, &request())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::BadResponse, "{bad}");
    }
    let stub = Stub::new().fail(TransportError::Connect("refused".into()));
    let err = exchange_token(&stub, &policy(), TOKEN_URL, &request())
        .await
        .unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::Network);
    assert!(err.message.contains("连不上 Tokenway"));
}

#[tokio::test]
async fn nothing_is_sent_to_an_address_outside_the_policy() {
    let stub = Stub::new().reply(200, "{}");
    for url in [
        "http://tokenwayapi.com/api/connect/token",
        "https://evil.example/api/connect/token",
        "https://tokenwayapi.com.evil.example/x",
        "https://user:pw@tokenwayapi.com/x",
    ] {
        let err = exchange_token(&stub, &policy(), url, &request())
            .await
            .unwrap_err();
        assert_eq!(err.kind, ApiErrorKind::Refused, "{url}");
        let key = ApiKey::new("sk-test-secret-0001");
        assert_eq!(
            fetch_status(&stub, &policy(), url, &key)
                .await
                .unwrap_err()
                .kind,
            ApiErrorKind::Refused
        );
        assert_eq!(
            revoke(&stub, &policy(), url, &key).await.unwrap_err().kind,
            ApiErrorKind::Refused
        );
    }
    assert!(stub.requests().is_empty(), "一个请求都不该发出去");
}

#[tokio::test]
async fn status_sends_the_key_only_in_the_authorization_header() {
    let stub = Stub::new().reply(200, r#"{"zeeterm_usable":3.5,"needs_topup":false}"#);
    let key = ApiKey::new("sk-test-status-key-0001");
    let balance = fetch_status(&stub, &policy(), STATUS_URL, &key)
        .await
        .unwrap();
    assert_eq!(balance.usable_cents, 350);
    let (is_get, sent) = &stub.requests()[0];
    assert!(is_get);
    assert!(!sent.url.contains("sk-test"), "Key 不进地址");
    assert!(sent.body.is_empty());
    let auth = sent
        .headers
        .iter()
        .find(|(n, _)| n == "authorization")
        .unwrap();
    assert_eq!(auth.1, "Bearer sk-test-status-key-0001");

    let stub = Stub::new().reply(401, &String::from_utf8(body("invalid_token", "x")).unwrap());
    let err = fetch_status(&stub, &policy(), STATUS_URL, &key)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ApiErrorKind::InvalidToken);
    assert!(!err.message.contains("sk-test"), "Key 不进错误文字");
    let stub = Stub::new().reply(429, "");
    assert_eq!(
        fetch_status(&stub, &policy(), STATUS_URL, &key)
            .await
            .unwrap_err()
            .kind,
        ApiErrorKind::RateLimited
    );
}

#[tokio::test]
async fn revoke_posts_the_key_in_the_body_and_reports_failures() {
    let stub = Stub::new().reply(200, r#"{"ok":true}"#);
    let key = ApiKey::new("sk-test-revoke-key-0001");
    revoke(&stub, &policy(), REVOKE_URL, &key).await.unwrap();
    let (is_get, sent) = &stub.requests()[0];
    assert!(!is_get);
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["api_key"], "sk-test-revoke-key-0001");
    assert!(!sent.url.contains("sk-test"));

    let stub = Stub::new().reply(401, &String::from_utf8(body_json()).unwrap());
    assert_eq!(
        revoke(&stub, &policy(), REVOKE_URL, &key)
            .await
            .unwrap_err()
            .kind,
        ApiErrorKind::InvalidToken
    );
    let stub = Stub::new().fail(TransportError::Timeout("slow".into()));
    assert_eq!(
        revoke(&stub, &policy(), REVOKE_URL, &key)
            .await
            .unwrap_err()
            .kind,
        ApiErrorKind::Network
    );
}

fn body_json() -> Vec<u8> {
    body("invalid_token", "不是有效的 ZeeTerm 令牌")
}
