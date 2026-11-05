use std::time::{Duration, Instant};

use zssh_core::ai::agent::builtin::provider::ReqwestTransport;
use zssh_core::ai::agent::builtin::tokenway::fake::{FakeBrowser, FakeTokenway};
use zssh_core::store::Database;

use super::*;

fn ok_balance(usable_cents: i64, needs_topup: bool) -> Balance {
    Balance {
        usable_cents,
        paid_cents: usable_cents,
        total_cents: usable_cents + 200,
        quota_limit_cents: Some(5000),
        used_cents: 125,
        expires_at: None,
        needs_topup,
    }
}

fn failed(kind: ApiErrorKind, message: &str) -> ApiError {
    ApiError {
        kind,
        message: message.to_owned(),
        status: None,
        code: None,
    }
}

// ---------------------------------------------------------------------------
// 余额显示的状态与节流
// ---------------------------------------------------------------------------

#[test]
fn the_balance_is_fetched_when_first_seen_then_only_after_a_turn_and_at_most_once_a_minute() {
    let mut book = BalanceBook::default();
    let t0 = Instant::now();
    assert!(book.view(7).is_none(), "还没取到不显示");
    assert!(book.poll(7, t0), "第一次看到（打开面板）就取");
    assert!(
        !book.poll(7, t0 + Duration::from_secs(1)),
        "取的过程中不重复取"
    );
    book.finish(7, Ok(ok_balance(1234, false)));
    assert_eq!(book.view(7).unwrap().label, "余额 ¥12.34");
    // 没有新的触发：不取。
    assert!(!book.poll(7, t0 + Duration::from_secs(300)));
    // 一轮对话结束（标脏）：不到 60 秒不取，满 60 秒才取。
    book.mark_all_dirty();
    assert!(
        !book.poll(7, t0 + Duration::from_secs(30)),
        "节流：60 秒内不再取"
    );
    assert!(!book.poll(7, t0 + Duration::from_secs(59)));
    assert!(
        book.poll(7, t0 + Duration::from_secs(61)),
        "标脏了、满 60 秒就取"
    );
    book.finish(7, Ok(ok_balance(1000, false)));
    assert_eq!(book.view(7).unwrap().label, "余额 ¥10.00");
    // 刚取完又来一轮：照样等到下一个 60 秒。
    book.mark_all_dirty();
    assert!(!book.poll(7, t0 + Duration::from_secs(100)));
    assert!(book.poll(7, t0 + Duration::from_secs(122)));
}

#[test]
fn the_balance_chip_wording_matches_core_in_chinese_and_has_no_chinese_in_english() {
    // 中文界面（默认）：和核心库原来的措辞逐字一致。
    for balance in [ok_balance(1234, false), ok_balance(0, true)] {
        assert_eq!(balance_label(&balance), balance.label());
        assert_eq!(balance_detail(&balance), balance.detail());
    }
    let mut no_limit = ok_balance(100, false);
    no_limit.quota_limit_cents = None;
    assert_eq!(balance_detail(&no_limit), no_limit.detail());
    // 英文表里不能有中文，占位符要和中文表一一对应。
    for key in [
        TokenwayMsg::BalanceNeedsTopup,
        TokenwayMsg::BalancePrefix,
        TokenwayMsg::BalanceDetail,
        TokenwayMsg::BalanceLimitDetail,
        TokenwayMsg::BalanceTopupNote,
    ] {
        assert!(
            !key.en()
                .chars()
                .any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c)),
            "{key:?}"
        );
        for placeholder in ["{usable}", "{total}", "{paid}", "{limit}", "{used}"] {
            assert_eq!(
                key.zh().contains(placeholder),
                key.en().contains(placeholder),
                "{key:?} {placeholder}"
            );
        }
    }
}

#[test]
fn a_balance_without_paid_funds_shows_topup() {
    let mut book = BalanceBook::default();
    assert!(book.poll(1, Instant::now()));
    book.finish(1, Ok(ok_balance(0, true)));
    let view = book.view(1).unwrap();
    assert_eq!(view.label, "需充值");
    assert!(view.needs_topup);
    assert!(view.detail.contains("赠送额度可在网页和其他客户端试用"));
    assert_eq!(view.problem, None);
}

#[test]
fn a_revoked_token_shows_authorization_invalid_while_other_failures_keep_the_last_number() {
    let mut book = BalanceBook::default();
    let t0 = Instant::now();
    assert!(book.poll(1, t0));
    book.finish(1, Ok(ok_balance(500, false)));
    // 网络 / 限速：继续显示上次的数。
    book.mark_all_dirty();
    assert!(book.poll(1, t0 + Duration::from_secs(61)));
    book.finish(1, Err(failed(ApiErrorKind::Network, "连不上")));
    let view = book.view(1).unwrap();
    assert_eq!(view.label, "余额 ¥5.00");
    assert!(matches!(view.problem, Some(BalanceProblem::Unavailable(_))));
    // 401：令牌已被服务端停用。
    book.mark_all_dirty();
    assert!(book.poll(1, t0 + Duration::from_secs(130)));
    book.finish(1, Err(failed(ApiErrorKind::InvalidToken, "x")));
    let view = book.view(1).unwrap();
    assert_eq!(view.label, "授权失效");
    assert_eq!(view.problem, Some(BalanceProblem::AuthorizationInvalid));
    assert!(view.detail.contains("重新连接"));
    // 重新连接换了令牌（forget）：立刻可以再取，状态清空。
    book.forget(1);
    assert!(book.view(1).is_none());
    assert!(book.poll(1, t0 + Duration::from_secs(131)));
    book.finish(1, Ok(ok_balance(900, false)));
    assert_eq!(book.view(1).unwrap().problem, None);
}

#[test]
fn each_endpoint_has_its_own_balance_and_throttle() {
    let mut book = BalanceBook::default();
    let t0 = Instant::now();
    assert!(book.poll(1, t0));
    assert!(book.poll(2, t0), "另一个接口的节流互不影响");
    book.finish(1, Ok(ok_balance(100, false)));
    book.finish(2, Ok(ok_balance(200, false)));
    assert_eq!(book.view(1).unwrap().label, "余额 ¥1.00");
    assert_eq!(book.view(2).unwrap().label, "余额 ¥2.00");
    book.forget(1);
    assert!(book.view(1).is_none() && book.view(2).is_some());
}

// ---------------------------------------------------------------------------
// 连接流程（本机假 Tokenway）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connecting_authorizes_lists_models_and_saves_the_key_in_the_vault() {
    let fake = FakeTokenway::start().await.unwrap();
    let preset = fake.preset();
    let policy = UrlPolicy::with_http_loopback_for_tests();
    let transport = ReqwestTransport::build(None).unwrap();
    let connected = run_connect(
        &ConnectDeps {
            preset: &preset,
            policy: &policy,
            transport: &transport,
            opener: &FakeBrowser::new(),
            device: "测试 设备",
            timeout: Duration::from_secs(20),
        },
        &Notify::new(),
        &|_| {},
    )
    .await
    .expect("授权");
    assert_eq!(connected.models.as_ref().map(Vec::len), Some(3));
    assert_eq!(fake.last_device().as_deref(), Some("测试 设备"));
    // 请求只有：授权页、换令牌、模型清单。
    assert_eq!(
        fake.requests(),
        [
            "GET /connect/zeeterm",
            "POST /api/connect/token",
            "GET /v1/models"
        ]
    );

    let db = Database::open_in_memory().unwrap();
    let endpoint = finish_connect(&db, &preset, &connected, &policy).expect("保存");
    assert_eq!(endpoint.preset_id.as_deref(), Some("tokenway"));
    assert_eq!(endpoint.default_model.as_deref(), Some("fake-chat-pro"));
    assert_eq!(
        db.ai_endpoint_api_key(endpoint.id).unwrap(),
        fake.last_key()
    );
    assert!(is_authorized_token(endpoint.preset_id.as_deref()));
    // Debug 输出里没有 Key。
    assert!(!format!("{connected:?}").contains(&fake.last_key().unwrap()));
}

#[tokio::test]
async fn an_unusable_preset_is_refused_before_anything_is_opened() {
    let fake = FakeTokenway::start().await.unwrap();
    let mut preset = fake.preset();
    preset.revoke_url = None;
    let browser = FakeBrowser::new();
    let err = run_connect(
        &ConnectDeps {
            preset: &preset,
            policy: &UrlPolicy::with_http_loopback_for_tests(),
            transport: &ReqwestTransport::build(None).unwrap(),
            opener: &browser,
            device: "d",
            timeout: Duration::from_secs(5),
        },
        &Notify::new(),
        &|_| {},
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("不支持一键授权"), "{err}");
    assert!(browser.opened().is_empty());
    assert_eq!(fake.requests().len(), 0);
}

#[tokio::test]
async fn the_production_policy_refuses_the_local_fake_address() {
    let fake = FakeTokenway::start().await.unwrap();
    let browser = FakeBrowser::new();
    let err = run_connect(
        &ConnectDeps {
            preset: &fake.preset(),
            policy: &UrlPolicy::production(),
            transport: &ReqwestTransport::build(None).unwrap(),
            opener: &browser,
            device: "d",
            timeout: Duration::from_secs(5),
        },
        &Notify::new(),
        &|_| {},
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ConnectError::Api(_)), "{err:?}");
    assert!(browser.opened().is_empty(), "不合格的地址不能打开浏览器");
}

#[test]
fn the_default_device_name_comes_from_the_computer_name_not_the_user_name() {
    // 这台机器上 COMPUTERNAME 是什么都行：结果不为空、不超长、不含路径分隔符。
    let name = default_device_name();
    assert!(!name.is_empty() && name.chars().count() <= 40);
    assert!(!name.contains('\\') && !name.contains('/'));
    if let Ok(user) = std::env::var("USERNAME") {
        let computer = std::env::var("COMPUTERNAME").unwrap_or_default();
        // 计算机名本身带了用户名时由用户在对话框里改；这里只确认我们没有另外去拼用户名。
        if !computer
            .to_ascii_lowercase()
            .contains(&user.to_ascii_lowercase())
        {
            assert!(
                !name
                    .to_ascii_lowercase()
                    .contains(&user.to_ascii_lowercase()),
                "设备名里不能出现用户名"
            );
        }
    }
}

#[test]
fn reconnect_requests_are_taken_once() {
    // 其它用例不会碰这个全局标志。
    let _ = take_reconnect_request();
    assert!(!take_reconnect_request());
    request_reconnect();
    assert!(take_reconnect_request());
    assert!(!take_reconnect_request());
}

#[test]
fn without_the_debug_fake_the_production_policy_and_the_builtin_manifest_are_in_force() {
    // 单元测试里没设 ZSSH_TOKENWAY_FAKE。
    assert!(!fake_active());
    assert_eq!(policy(), UrlPolicy::production());
    let preset = tokenway_preset().expect("内置清单里有 Tokenway");
    assert_eq!(preset.id, "tokenway");
    assert!(
        policy()
            .check(preset.status_url.as_deref().unwrap())
            .is_ok(),
        "清单里的地址通过产品规则"
    );
}

#[test]
fn every_tokenway_message_has_both_languages() {
    use crate::i18n::Msg as _;
    for key in [
        TokenwayMsg::UrlNotAllowedPrefix,
        TokenwayMsg::UrlNotAllowedSuffix,
        TokenwayMsg::NoTopupUrl,
        TokenwayMsg::NoRegisterUrl,
        TokenwayMsg::NoOneClickAuth,
        TokenwayMsg::OpenSettingsDbFailedPrefix,
        TokenwayMsg::AuthorizationInvalidLabel,
        TokenwayMsg::AuthorizationInvalidDetail,
        TokenwayMsg::NotAuthorizedToken,
        TokenwayMsg::NoStatusUrl,
    ] {
        assert!(!key.zh().is_empty(), "{key:?} 缺中文");
        assert!(!key.en().is_empty(), "{key:?} 缺英文");
    }
}
