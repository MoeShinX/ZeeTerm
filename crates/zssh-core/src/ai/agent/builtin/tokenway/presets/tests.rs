use std::sync::Mutex;

use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{json, Value};

use super::*;
use crate::ai::agent::builtin::provider::{ByteStream, HttpResponse, TransportError};
use crate::ssh::BoxFuture;

/// Tokenway 侧 `zeeterm-presets.mjs`（同一套规范化和签名算法）用一把一次性测试密钥签出来的清单，
/// 私钥签完即丢、不在仓库里；夹具里只有公钥、清单、签名和 Node 算出来的规范化文本。
const NODE_FIXTURE: &str = include_str!("../fixtures/node_signed.json");

fn fixture() -> Value {
    serde_json::from_str(NODE_FIXTURE).expect("夹具是合法 JSON")
}

fn fixture_key() -> PublicKey {
    PublicKey::from_base64(fixture()["public_key"].as_str().unwrap()).expect("夹具公钥")
}

fn case(name: &str) -> Value {
    fixture()["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("夹具里没有 {name}"))
        .clone()
}

/// 夹具签发时间之后一小时。
fn after_issue() -> i64 {
    parse_utc_timestamp("2026-11-03T09:00:00Z").unwrap()
}

/// 用固定种子的测试密钥在 Rust 里签（测试地址白名单、过期等 Node 夹具覆盖不到的情形）。
fn sign_here(mut doc: Value) -> (PublicKey, String) {
    let pair = Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap();
    let key = PublicKey::from_bytes(<[u8; 32]>::try_from(pair.public_key().as_ref()).unwrap());
    doc.as_object_mut().unwrap().remove("signature");
    // 含无法规范化的数字时签个空消息：验的那边同样规范化不了，一样会被拒绝。
    let message = canonical_json(&doc).unwrap_or_default();
    let signature = STANDARD.encode(pair.sign(message.as_bytes()).as_ref());
    doc.as_object_mut()
        .unwrap()
        .insert("signature".to_owned(), Value::String(signature));
    (key, doc.to_string())
}

fn good_doc() -> Value {
    serde_json::from_str(include_str!("../presets.json")).unwrap()
}

// ---------------------------------------------------------------------------
// 内置清单
// ---------------------------------------------------------------------------

#[test]
fn the_builtin_manifest_is_valid_and_offers_tokenway_first() {
    let manifest = builtin_manifest();
    assert_eq!(manifest.version, 1);
    assert!(manifest.issued_at_unix > 0);
    let tokenway = manifest.tokenway().expect("内置清单里有 Tokenway");
    assert!(tokenway.recommended);
    assert!(tokenway.tagline.as_deref().is_some_and(|t| !t.is_empty()));
    let urls = tokenway.authorize_urls().expect("四个授权地址齐全");
    let policy = UrlPolicy::production();
    for url in [urls.connect, urls.token, urls.status, urls.revoke] {
        policy.check(url).expect(url);
        assert!(url.starts_with("https://tokenwayapi.com/"), "{url}");
    }
    // 接入地址和注册 / 充值页也都在白名单里。
    for url in [
        tokenway.openai_base.as_deref().unwrap(),
        tokenway.anthropic_base.as_deref().unwrap(),
        tokenway.register_url.as_deref().unwrap(),
        tokenway.topup_url.as_deref().unwrap(),
    ] {
        policy.check(url).expect(url);
    }
    // 内置清单里不写死邀请码。
    assert!(!tokenway.register_url.as_deref().unwrap().contains("aff="));
}

// ---------------------------------------------------------------------------
// 跨语言验签（Node 签 → Rust 验）
// ---------------------------------------------------------------------------

#[test]
fn rust_canonical_json_is_byte_identical_to_the_node_one() {
    for name in ["basic", "tricky"] {
        let case = case(name);
        let mut manifest = case["manifest"].clone();
        manifest.as_object_mut().unwrap().remove("signature");
        assert_eq!(
            canonical_json(&manifest).unwrap(),
            case["canonical"].as_str().unwrap(),
            "{name}：Rust 和 Node 的规范化文本必须逐字节一致"
        );
    }
}

#[test]
fn a_manifest_signed_by_node_verifies_in_rust() {
    let key = fixture_key();
    for name in ["basic", "tricky"] {
        verify_signature(&case(name)["manifest"], &key).unwrap_or_else(|e| panic!("{name}：{e}"));
    }
    let text = case("basic")["manifest"].to_string();
    let manifest = verify_manifest(&text, &key, &UrlPolicy::production(), after_issue(), None)
        .expect("基本清单通过全部检查");
    assert_eq!(manifest.issued_at, "2026-11-03T08:00:00Z");
    assert_eq!(manifest.tokenway().unwrap().name, "Tokenway");
    // 刁钻的那份（键序、转义、emoji、整数形式的浮点、未知字段）也通过。
    let text = case("tricky")["manifest"].to_string();
    verify_manifest(&text, &key, &UrlPolicy::production(), after_issue(), None).expect("tricky");
}

#[test]
fn tampering_with_a_node_signed_manifest_fails_verification() {
    let key = fixture_key();
    let policy = UrlPolicy::production();
    let now = after_issue();

    // 改了内容。
    let mut doc = case("basic")["manifest"].clone();
    doc["presets"][0]["token_url"] = json!("https://tokenwayapi.com/api/connect/other");
    assert_eq!(
        verify_manifest(&doc.to_string(), &key, &policy, now, None).unwrap_err(),
        PresetError::BadSignature
    );
    // 多加一个字段也不行。
    let mut doc = case("tricky")["manifest"].clone();
    doc["extra"] = json!(1);
    assert_eq!(
        verify_manifest(&doc.to_string(), &key, &policy, now, None).unwrap_err(),
        PresetError::BadSignature
    );
    // 签名被改了一位。
    let mut doc = case("basic")["manifest"].clone();
    let sig = doc["signature"].as_str().unwrap().to_owned();
    let swapped = if &sig[10..11] == "A" { "B" } else { "A" };
    doc["signature"] = json!(format!("{}{}{}", &sig[..10], swapped, &sig[11..]));
    assert_eq!(
        verify_manifest(&doc.to_string(), &key, &policy, now, None).unwrap_err(),
        PresetError::BadSignature
    );
    // 没有签名 / 签名不是 base64 / 空串。
    for signature in [None, Some("!!!not base64"), Some("")] {
        let mut doc = case("basic")["manifest"].clone();
        match signature {
            None => {
                doc.as_object_mut().unwrap().remove("signature");
            }
            Some(text) => doc["signature"] = json!(text),
        }
        let err = verify_manifest(&doc.to_string(), &key, &policy, now, None).unwrap_err();
        assert!(
            matches!(err, PresetError::NoSignature | PresetError::BadSignature),
            "{err:?}"
        );
    }
    // 换一把公钥。
    let other = PublicKey::from_bytes([9u8; 32]);
    assert_eq!(
        verify_manifest(
            &case("basic")["manifest"].to_string(),
            &other,
            &policy,
            now,
            None
        )
        .unwrap_err(),
        PresetError::BadSignature
    );
}

#[test]
fn the_placeholder_public_key_never_verifies_anything() {
    assert!(
        production_key().is_none(),
        "占位公钥必须解析不出来；换成真公钥之后这条测试要改成断言它有值"
    );
}

#[test]
fn public_keys_must_be_exactly_32_bytes_of_base64() {
    assert!(PublicKey::from_base64("").is_none());
    assert!(PublicKey::from_base64("AAAA").is_none());
    assert!(PublicKey::from_base64(&STANDARD.encode([1u8; 31])).is_none());
    assert!(PublicKey::from_base64(&STANDARD.encode([1u8; 33])).is_none());
    assert!(PublicKey::from_base64(&STANDARD.encode([1u8; 32])).is_some());
}

// ---------------------------------------------------------------------------
// 域名白名单
// ---------------------------------------------------------------------------

#[test]
fn the_policy_accepts_the_allowed_domain_and_its_subdomains_over_https_only() {
    let policy = UrlPolicy::production();
    for ok in [
        "https://tokenwayapi.com/connect/zeeterm",
        "https://api.tokenwayapi.com/v1",
        "https://a.b.tokenwayapi.com/x?y=1",
        "https://TokenwayAPI.com/up",
        "https://tokenwayapi.com:443/x",
    ] {
        policy.check(ok).unwrap_or_else(|e| panic!("{ok}：{e}"));
    }
    for bad in [
        "http://tokenwayapi.com/x",
        "https://tokenwayapi.com.evil.example/x",
        "https://evil-tokenwayapi.com/x",
        "https://eviltokenwayapi.com/x",
        "https://evil.example/tokenwayapi.com",
        "https://tokenwayapi.com@evil.example/x",
        "https://user:pw@tokenwayapi.com/x",
        "https://tokenwayapi.com:8443/x",
        "https://tokenwayapi.com./x",
        "https://127.0.0.1/x",
        "http://127.0.0.1:8080/x",
        "ftp://tokenwayapi.com/x",
        "file:///C:/x",
        "javascript:alert(1)",
        "tokenwayapi.com/x",
        "https://tokenwayapi.com/a b",
        "",
    ] {
        assert!(policy.check(bad).is_err(), "{bad} 应当被拒绝");
    }
    // 测试用的宽松规则只多放行回环的 http，别的照旧。
    let loose = UrlPolicy::with_http_loopback_for_tests();
    loose.check("http://127.0.0.1:5555/x").expect("回环 http");
    loose.check("http://[::1]:5555/x").expect("回环 http v6");
    assert!(loose.check("http://example.com/x").is_err());
    assert!(loose.check("http://localhost:5555/x").is_err());
    assert!(loose.check("https://evil.example/x").is_err());
}

type Edit = Box<dyn Fn(&mut Value)>;

#[test]
fn a_correctly_signed_manifest_with_a_foreign_or_insecure_address_is_still_dropped() {
    let now = after_issue();
    let cases: Vec<(&str, Edit)> = vec![
        (
            "域名不在白名单",
            Box::new(|d| {
                d["presets"][0]["token_url"] = json!("https://evil.example/api/connect/token")
            }),
        ),
        (
            "http",
            Box::new(|d| {
                d["presets"][0]["status_url"] = json!("http://tokenwayapi.com/api/connect/status")
            }),
        ),
        (
            "接入地址换了域名",
            Box::new(|d| d["presets"][0]["openai_base"] = json!("https://api.evil.example/v1")),
        ),
        (
            "新增字段里的地址也查",
            Box::new(|d| d["presets"][0]["support_url"] = json!("https://evil.example/")),
        ),
        (
            "嵌套里的 *_base 也查",
            Box::new(|d| {
                d["presets"][0]["extra"] = json!({"mirror_base": "https://evil.example/"})
            }),
        ),
        (
            "地址不是字符串",
            Box::new(|d| d["presets"][0]["topup_url"] = json!(5)),
        ),
    ];
    for (what, edit) in cases {
        let mut doc = good_doc();
        doc["issued_at"] = json!("2026-11-03T08:00:00Z");
        edit(&mut doc);
        let (key, text) = sign_here(doc);
        let err = verify_manifest(&text, &key, &UrlPolicy::production(), now, None).unwrap_err();
        assert!(matches!(err, PresetError::BadUrl(_)), "{what}：{err:?}");
    }
}

// ---------------------------------------------------------------------------
// 时间与版本
// ---------------------------------------------------------------------------

#[test]
fn expired_future_and_rolled_back_manifests_are_rejected() {
    let key = fixture_key();
    let policy = UrlPolicy::production();
    let text = case("basic")["manifest"].to_string();
    let issued = parse_utc_timestamp("2026-11-03T08:00:00Z").unwrap();
    // 刚好一年内：通过；超过一年：过期。
    verify_manifest(&text, &key, &policy, issued + MAX_AGE_SECS - 1, None).expect("还没过期");
    assert!(matches!(
        verify_manifest(&text, &key, &policy, issued + MAX_AGE_SECS + 1, None),
        Err(PresetError::Expired(_))
    ));
    // 签发时间在未来：一天内的时钟误差放行，更远的拒绝。
    verify_manifest(&text, &key, &policy, issued - 3600, None).expect("时钟误差");
    assert!(matches!(
        verify_manifest(
            &text,
            &key,
            &policy,
            issued - MAX_FUTURE_SKEW_SECS - 60,
            None
        ),
        Err(PresetError::FromTheFuture(_))
    ));
    // 比正在用的旧：回放；一样新：放行。
    assert!(matches!(
        verify_manifest(&text, &key, &policy, issued + 60, Some(issued + 1)),
        Err(PresetError::Rollback(_))
    ));
    verify_manifest(&text, &key, &policy, issued + 60, Some(issued)).expect("同一份");
}

#[test]
fn an_explicit_expires_at_is_honoured() {
    let now = after_issue();
    let mut doc = good_doc();
    doc["issued_at"] = json!("2026-11-03T08:00:00Z");
    doc["expires_at"] = json!("2026-11-03T08:30:00Z");
    let (key, text) = sign_here(doc);
    assert!(matches!(
        verify_manifest(&text, &key, &UrlPolicy::production(), now, None),
        Err(PresetError::Expired(_))
    ));
}

#[test]
fn malformed_manifests_are_rejected_with_a_reason() {
    let now = after_issue();
    let cases: Vec<(&str, Edit)> = vec![
        ("版本不认识", Box::new(|d| d["version"] = json!(2))),
        (
            "没有 issued_at",
            Box::new(|d| {
                d.as_object_mut().unwrap().remove("issued_at");
            }),
        ),
        (
            "issued_at 不是时间",
            Box::new(|d| d["issued_at"] = json!("yesterday")),
        ),
        ("presets 为空", Box::new(|d| d["presets"] = json!([]))),
        (
            "预设没有 id",
            Box::new(|d| {
                d["presets"][0].as_object_mut().unwrap().remove("id");
            }),
        ),
        (
            "id 含空格",
            Box::new(|d| d["presets"][0]["id"] = json!("a b")),
        ),
        (
            "name 含控制字符",
            Box::new(|d| d["presets"][0]["name"] = json!("x\u{7}y")),
        ),
        ("含无法规范化的数字", Box::new(|d| d["weird"] = json!(1.5))),
    ];
    for (what, edit) in cases {
        let mut doc = good_doc();
        if what != "没有 issued_at" && what != "issued_at 不是时间" {
            doc["issued_at"] = json!("2026-11-03T08:00:00Z");
        }
        edit(&mut doc);
        let (key, text) = sign_here(doc);
        assert!(
            verify_manifest(&text, &key, &UrlPolicy::production(), now, None).is_err(),
            "{what}"
        );
    }
    let (key, _) = sign_here(good_doc());
    assert!(matches!(
        verify_manifest("not json", &key, &UrlPolicy::production(), now, None),
        Err(PresetError::NotJson(_))
    ));
    let huge = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_MANIFEST_BYTES));
    assert_eq!(
        verify_manifest(&huge, &key, &UrlPolicy::production(), now, None).unwrap_err(),
        PresetError::TooLarge
    );
}

#[test]
fn timestamps_parse_like_rfc3339_utc() {
    assert_eq!(parse_utc_timestamp("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(parse_utc_timestamp("1970-01-02T00:00:01Z"), Some(86_401));
    assert_eq!(
        parse_utc_timestamp("2000-03-01T00:00:00Z"),
        Some(951_868_800)
    );
    assert_eq!(
        parse_utc_timestamp("2026-11-03T08:00:00.123Z"),
        Some(1_793_692_800)
    );
    for bad in [
        "",
        "2026-11-03",
        "2026-13-03T00:00:00Z",
        "2026-11-03T25:00:00Z",
        "2026-11-03T08:00:00+08:00",
        "2026-11-03 08:00:00Z",
    ] {
        assert_eq!(parse_utc_timestamp(bad), None, "{bad}");
    }
}

// ---------------------------------------------------------------------------
// 缓存与定时更新
// ---------------------------------------------------------------------------

struct Fixed {
    status: u16,
    body: String,
}

struct Chunk(Option<Vec<u8>>);

impl ByteStream for Chunk {
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move { Ok(self.0.take()) })
    }
}

/// 只会 GET 的桩：返回固定的状态和文本，记下被请求的地址。
struct StubTransport {
    reply: Mutex<Result<Fixed, String>>,
    urls: Mutex<Vec<String>>,
}

impl StubTransport {
    fn ok(body: String) -> Self {
        Self {
            reply: Mutex::new(Ok(Fixed { status: 200, body })),
            urls: Mutex::new(Vec::new()),
        }
    }

    fn set(&self, reply: Result<Fixed, String>) {
        *self.reply.lock().unwrap() = reply;
    }

    fn calls(&self) -> usize {
        self.urls.lock().unwrap().len()
    }
}

impl Transport for StubTransport {
    fn post<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async { Err(TransportError::Other("桩不做 POST".into())) })
    }

    fn get<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        self.urls.lock().unwrap().push(request.url);
        let reply = match &*self.reply.lock().unwrap() {
            Ok(fixed) => Ok((fixed.status, fixed.body.clone())),
            Err(why) => Err(why.clone()),
        };
        Box::pin(async move {
            let (status, body) = reply.map_err(TransportError::Connect)?;
            Ok(HttpResponse {
                status,
                headers: Vec::new(),
                body: Box::new(Chunk(Some(body.into_bytes()))),
            })
        })
    }
}

fn newer_doc(issued_at: &str) -> Value {
    let mut doc = good_doc();
    doc["issued_at"] = json!(issued_at);
    doc["presets"][0]["tagline"] = json!(format!("更新于 {issued_at}"));
    doc
}

fn tagline(active: &ActivePresets) -> String {
    active
        .manifest
        .tokenway()
        .and_then(|p| p.tagline.clone())
        .unwrap_or_default()
}

#[tokio::test]
async fn without_a_real_public_key_nothing_is_fetched_and_the_builtin_manifest_is_used() {
    let dir = tempfile::tempdir().unwrap();
    let store = PresetStore::new(dir.path());
    let stub = StubTransport::ok("{}".into());
    let outcome = store
        .refresh(
            &stub,
            PRESETS_URL,
            production_key().as_ref(),
            &UrlPolicy::production(),
            after_issue(),
            false,
        )
        .await;
    assert_eq!(outcome, RefreshOutcome::NoPublicKey);
    assert_eq!(stub.calls(), 0, "占位公钥时连请求都不发");
    let active = store.load(
        production_key().as_ref(),
        &UrlPolicy::production(),
        after_issue(),
    );
    assert_eq!(active.source, PresetSource::Builtin);
}

#[tokio::test]
async fn a_valid_remote_manifest_is_cached_and_loaded_but_only_every_24_hours() {
    let dir = tempfile::tempdir().unwrap();
    let store = PresetStore::new(dir.path());
    let policy = UrlPolicy::production();
    let builtin = builtin_manifest();
    let t0 = builtin.issued_at_unix + 10 * 86_400;
    let (key, text) = sign_here(newer_doc("2026-11-10T00:00:00Z"));
    let stub = StubTransport::ok(text);

    let outcome = store
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t0, false)
        .await;
    assert_eq!(
        outcome,
        RefreshOutcome::Updated {
            issued_at: "2026-11-10T00:00:00Z".into()
        }
    );
    assert_eq!(stub.calls(), 1);
    assert_eq!(stub.urls.lock().unwrap()[0], PRESETS_URL);
    let active = store.load(Some(&key), &policy, t0);
    assert_eq!(active.source, PresetSource::Cached);
    assert!(tagline(&active).contains("2026-11-10"));

    // 23 小时后：不拉。
    let outcome = store
        .refresh(
            &stub,
            PRESETS_URL,
            Some(&key),
            &policy,
            t0 + 23 * 3600,
            false,
        )
        .await;
    assert_eq!(outcome, RefreshOutcome::NotDue);
    assert_eq!(stub.calls(), 1);
    // 25 小时后：拉，内容没变。
    let outcome = store
        .refresh(
            &stub,
            PRESETS_URL,
            Some(&key),
            &policy,
            t0 + 25 * 3600,
            false,
        )
        .await;
    assert_eq!(outcome, RefreshOutcome::Unchanged);
    assert_eq!(stub.calls(), 2);
}

#[tokio::test]
async fn bad_remote_manifests_are_dropped_and_the_previous_one_stays() {
    let dir = tempfile::tempdir().unwrap();
    let store = PresetStore::new(dir.path());
    let policy = UrlPolicy::production();
    let builtin = builtin_manifest();
    let t0 = builtin.issued_at_unix + 10 * 86_400;
    let t1 = t0 + 2 * 86_400;
    let (key, good) = sign_here(newer_doc("2026-11-10T00:00:00Z"));
    let stub = StubTransport::ok(good.clone());
    store
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t0, false)
        .await;

    // 内容被改（签名失效）。
    let mut tampered: Value = serde_json::from_str(&good).unwrap();
    tampered["presets"][0]["tagline"] = json!("被改过");
    stub.set(Ok(Fixed {
        status: 200,
        body: tampered.to_string(),
    }));
    let outcome = store
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t1, true)
        .await;
    assert!(
        matches!(outcome, RefreshOutcome::Rejected(_)),
        "{outcome:?}"
    );

    // 签名对但域名不在白名单。
    let mut foreign = newer_doc("2026-11-20T00:00:00Z");
    foreign["presets"][0]["token_url"] = json!("https://evil.example/token");
    let (_, foreign_text) = sign_here(foreign);
    stub.set(Ok(Fixed {
        status: 200,
        body: foreign_text,
    }));
    let outcome = store
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t1, true)
        .await;
    assert!(
        matches!(outcome, RefreshOutcome::Rejected(_)),
        "{outcome:?}"
    );

    // 比缓存里的旧（回放）。
    let (_, older) = sign_here(newer_doc("2026-11-05T00:00:00Z"));
    stub.set(Ok(Fixed {
        status: 200,
        body: older,
    }));
    let outcome = store
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t1, true)
        .await;
    assert!(
        matches!(outcome, RefreshOutcome::Rejected(_)),
        "{outcome:?}"
    );

    // 比内置的还旧。
    let (_, ancient) = sign_here(newer_doc("2026-10-01T00:00:00Z"));
    stub.set(Ok(Fixed {
        status: 200,
        body: ancient,
    }));
    let fresh_dir = tempfile::tempdir().unwrap();
    let fresh = PresetStore::new(fresh_dir.path());
    let outcome = fresh
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t0, true)
        .await;
    assert!(
        matches!(outcome, RefreshOutcome::Rejected(_)),
        "{outcome:?}"
    );

    // 网络错误 / 非 200。
    stub.set(Err("连不上".into()));
    let outcome = store
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t1, true)
        .await;
    assert!(matches!(outcome, RefreshOutcome::Failed(_)), "{outcome:?}");
    stub.set(Ok(Fixed {
        status: 500,
        body: "oops".into(),
    }));
    let outcome = store
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t1, true)
        .await;
    assert!(matches!(outcome, RefreshOutcome::Failed(_)), "{outcome:?}");

    // 一路下来，生效的还是第一次拉到的那份。
    let active = store.load(Some(&key), &policy, t1);
    assert_eq!(active.source, PresetSource::Cached);
    assert!(tagline(&active).contains("2026-11-10"));
}

#[tokio::test]
async fn a_failed_fetch_is_not_retried_within_an_hour_and_a_non_https_url_is_never_requested() {
    let dir = tempfile::tempdir().unwrap();
    let store = PresetStore::new(dir.path());
    let policy = UrlPolicy::production();
    let (key, _) = sign_here(good_doc());
    let stub = StubTransport::ok(String::new());
    stub.set(Err("没网".into()));
    let t0 = builtin_manifest().issued_at_unix + 86_400;
    assert!(matches!(
        store
            .refresh(&stub, PRESETS_URL, Some(&key), &policy, t0, false)
            .await,
        RefreshOutcome::Failed(_)
    ));
    assert_eq!(
        store
            .refresh(&stub, PRESETS_URL, Some(&key), &policy, t0 + 1800, false)
            .await,
        RefreshOutcome::NotDue
    );
    assert!(matches!(
        store
            .refresh(&stub, PRESETS_URL, Some(&key), &policy, t0 + 3700, false)
            .await,
        RefreshOutcome::Failed(_)
    ));
    assert_eq!(stub.calls(), 2);
    // 地址不合格的根本不发请求。
    let outcome = store
        .refresh(
            &stub,
            "http://tokenwayapi.com/zeeterm/presets.json",
            Some(&key),
            &policy,
            t0 + 10_000,
            true,
        )
        .await;
    assert!(matches!(outcome, RefreshOutcome::Failed(_)));
    assert_eq!(stub.calls(), 2);
}

#[tokio::test]
async fn a_tampered_cache_file_is_ignored_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let store = PresetStore::new(dir.path());
    let policy = UrlPolicy::production();
    let t0 = builtin_manifest().issued_at_unix + 10 * 86_400;
    let (key, text) = sign_here(newer_doc("2026-11-10T00:00:00Z"));
    let stub = StubTransport::ok(text.clone());
    store
        .refresh(&stub, PRESETS_URL, Some(&key), &policy, t0, false)
        .await;
    assert_eq!(
        store.load(Some(&key), &policy, t0).source,
        PresetSource::Cached
    );
    std::fs::write(
        dir.path().join("presets.json"),
        text.replace("Tokenway", "Tokenwey"),
    )
    .unwrap();
    assert_eq!(
        store.load(Some(&key), &policy, t0).source,
        PresetSource::Builtin
    );
    std::fs::write(dir.path().join("presets.json"), "garbage").unwrap();
    assert_eq!(
        store.load(Some(&key), &policy, t0).source,
        PresetSource::Builtin
    );
    // 过了一年的缓存也不再信。
    std::fs::write(dir.path().join("presets.json"), &text).unwrap();
    assert_eq!(
        store.load(Some(&key), &policy, t0 + 400 * 86_400).source,
        PresetSource::Builtin
    );
}
