//! Tokenway 接口预设清单（V2-A5）：内置兜底 + 运行时从 Tokenway 拉取 + ed25519 验签 + 域名白名单。
//!
//! 清单决定"授权 / 换令牌 / 余额 / 停用 / 充值 / 注册"这些地址，用户在界面上改不了它们，所以
//! 来源必须可信：
//!
//! 1. 内置的 `presets.json`（编进程序，不需要签名）；
//! 2. 远端清单（每 24 小时拉一次）：必须带正确的 ed25519 签名（公钥写死在 [`PRESETS_PUBLIC_KEY`]）、
//!    所有 `*_url` / `*_base` 都是 https 且域名在白名单里、没过期、版本没有倒退，任何一条不满足
//!    就整份丢弃，继续用上一份通过验证的缓存或内置清单；
//! 3. 缓存文件每次读出来都重新验一遍，磁盘上被改过的缓存等于没有。
//!
//! 签名规则见 [`super::canonical`]；Tokenway 侧生成方式见它仓库的 `ZEETERM-DEPLOYMENT.md` 第五节。

use std::path::{Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use reqwest::Url;
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::canonical::{canonical_json, CanonicalError};
use crate::ai::agent::builtin::provider::{HttpRequest, Transport};

/// 预设清单的签名公钥（32 字节原始公钥的标准 base64）。
///
/// **上线前必须换成 `node scripts/zeeterm-presets.mjs keygen` 打出来的公钥。** 现在是占位：
/// 占位状态下 [`production_key`] 返回 `None`，远端清单一律视为验签失败（连网络请求都不发），
/// 只用内置清单。
pub const PRESETS_PUBLIC_KEY: &str = "PLACEHOLDER-replace-with-the-zeeterm-presets-public-key";

/// 清单的下载地址。
pub const PRESETS_URL: &str = "https://tokenwayapi.com/zeeterm/presets.json";

/// 允许出现在清单里的域名（本身和它的子域）。
pub const ALLOWED_HOSTS: &[&str] = &["tokenwayapi.com"];

/// Tokenway 预设在清单里的 id。
pub const TOKENWAY_ID: &str = "tokenway";

/// 两次成功拉取之间至少隔多久。
pub const REFRESH_INTERVAL_SECS: i64 = 24 * 3600;
/// 拉取失败后至少隔多久再试（别在网络不通时每次启动都连）。
pub const RETRY_AFTER_FAILURE_SECS: i64 = 3600;
/// 清单的 `issued_at` 超过这么久就当过期（防止拿旧清单回放）。
pub const MAX_AGE_SECS: i64 = 365 * 24 * 3600;
/// `issued_at` 允许比本机时钟超前多久（时钟误差）。
pub const MAX_FUTURE_SKEW_SECS: i64 = 24 * 3600;
/// 清单响应体上限。
pub const MAX_MANIFEST_BYTES: usize = 256 * 1024;

const BUILTIN_JSON: &str = include_str!("presets.json");
const CACHE_FILE: &str = "presets.json";
const META_FILE: &str = "meta.json";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PresetError {
    #[error("清单不是合法的 JSON：{0}")]
    NotJson(String),
    #[error("清单没有签名")]
    NoSignature,
    #[error("清单的签名不对")]
    BadSignature,
    #[error("程序里的清单公钥还没有配置")]
    NoPublicKey,
    #[error("清单已经过期（签发时间 {0}）")]
    Expired(String),
    #[error("清单的签发时间在未来（{0}）")]
    FromTheFuture(String),
    #[error("清单比正在用的更旧（签发时间 {0}），可能是被回放的旧清单")]
    Rollback(String),
    #[error("不认识的清单版本")]
    UnsupportedVersion,
    #[error("清单里的地址不合格：{0}")]
    BadUrl(String),
    #[error("清单格式不对：{0}")]
    Malformed(String),
    #[error("清单太大")]
    TooLarge,
    #[error("{0}")]
    Canonical(#[from] CanonicalError),
}

// ---------------------------------------------------------------------------
// 地址白名单
// ---------------------------------------------------------------------------

/// 清单和授权流程里所有地址要遵守的规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlPolicy {
    hosts: Vec<String>,
    /// 只给测试（本机假 Tokenway）：允许 `http://127.0.0.1` / `[::1]`，任意端口。
    allow_http_loopback: bool,
}

impl UrlPolicy {
    /// 产品用的规则：只有 https，域名是白名单里的或它的子域，不带账号密码，不指定端口。
    pub fn production() -> Self {
        Self {
            hosts: ALLOWED_HOSTS.iter().map(|h| (*h).to_owned()).collect(),
            allow_http_loopback: false,
        }
    }

    /// 测试用：在产品规则之外再放行本机回环的 http 地址。产品代码不得调用（调试版的假 Tokenway
    /// 钩子和测试除外）。
    pub fn with_http_loopback_for_tests() -> Self {
        Self {
            allow_http_loopback: true,
            ..Self::production()
        }
    }

    /// 校验一个地址，通过就返回解析后的 `Url`。
    pub fn check(&self, raw: &str) -> Result<Url, String> {
        if raw.len() > 2048 || raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            return Err("地址里有空白、控制字符或太长".to_owned());
        }
        let url = Url::parse(raw).map_err(|_| "不是合法的地址".to_owned())?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err("地址里不能带账号密码".to_owned());
        }
        let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
        let loopback = matches!(host.as_str(), "127.0.0.1" | "[::1]");
        match url.scheme() {
            "https" => {}
            "http" if self.allow_http_loopback && loopback => return Ok(url),
            _ => return Err("必须是 https".to_owned()),
        }
        if url.port().is_some() {
            return Err("不能指定端口".to_owned());
        }
        let allowed = self
            .hosts
            .iter()
            .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));
        if !allowed {
            return Err(format!("域名 {host} 不在白名单里"));
        }
        Ok(url)
    }
}

// ---------------------------------------------------------------------------
// 清单
// ---------------------------------------------------------------------------

/// 一个预设。除 `id` / `name` 外的字段都可能缺（只有能一键授权的预设才有那四个地址）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    pub id: String,
    pub name: String,
    pub recommended: bool,
    pub tagline: Option<String>,
    pub connect_url: Option<String>,
    pub token_url: Option<String>,
    pub status_url: Option<String>,
    pub revoke_url: Option<String>,
    pub openai_base: Option<String>,
    pub anthropic_base: Option<String>,
    pub register_url: Option<String>,
    pub topup_url: Option<String>,
}

/// 一键授权要用的四个地址。
#[derive(Debug, Clone, Copy)]
pub struct AuthorizeUrls<'a> {
    pub connect: &'a str,
    pub token: &'a str,
    pub status: &'a str,
    pub revoke: &'a str,
}

impl Preset {
    /// 四个授权地址齐全才算能一键授权。
    pub fn authorize_urls(&self) -> Option<AuthorizeUrls<'_>> {
        Some(AuthorizeUrls {
            connect: self.connect_url.as_deref()?,
            token: self.token_url.as_deref()?,
            status: self.status_url.as_deref()?,
            revoke: self.revoke_url.as_deref()?,
        })
    }
}

/// 一份通过验证的清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresetManifest {
    pub version: u32,
    pub issued_at: String,
    pub issued_at_unix: i64,
    pub presets: Vec<Preset>,
}

impl PresetManifest {
    pub fn empty() -> Self {
        Self {
            version: 1,
            issued_at: String::new(),
            issued_at_unix: 0,
            presets: Vec::new(),
        }
    }

    pub fn tokenway(&self) -> Option<&Preset> {
        self.presets.iter().find(|preset| preset.id == TOKENWAY_ID)
    }
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z` → Unix 秒。
pub fn parse_utc_timestamp(text: &str) -> Option<i64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    if d.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    let time = time.split('.').next()?;
    let mut t = time.split(':');
    let (h, mi, s): (i64, i64, i64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
    );
    if t.next().is_some() || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    // Howard Hinnant 的 days_from_civil。
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + s)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 清单里所有以 `_url` / `_base` 结尾的字段（任意层级）都得是合格地址——和 Tokenway 侧签名脚本
/// 的检查一致，免得将来加了新字段却没人检查。
fn check_all_urls(value: &Value, policy: &UrlPolicy, path: &str) -> Result<(), PresetError> {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                check_all_urls(item, policy, &format!("{path}[{index}]"))?;
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                let here = format!("{path}.{key}");
                if key.ends_with("_url") || key.ends_with("_base") {
                    let Some(text) = item.as_str() else {
                        return Err(PresetError::BadUrl(format!("{here} 不是字符串")));
                    };
                    policy
                        .check(text)
                        .map_err(|why| PresetError::BadUrl(format!("{here}：{why}")))?;
                } else {
                    check_all_urls(item, policy, &here)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn text_field(
    map: &serde_json::Map<String, Value>,
    key: &str,
    max_chars: usize,
) -> Result<Option<String>, PresetError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.chars().count() > max_chars || text.chars().any(char::is_control) {
                return Err(PresetError::Malformed(format!("{key} 太长或含控制字符")));
            }
            Ok((!text.is_empty()).then(|| text.to_owned()))
        }
        Some(_) => Err(PresetError::Malformed(format!("{key} 不是字符串"))),
    }
}

/// 把（已经验过签或是内置的）JSON 解析成清单并检查地址。**不验签、不看时间**。
pub fn parse_manifest(value: &Value, policy: &UrlPolicy) -> Result<PresetManifest, PresetError> {
    let Value::Object(root) = value else {
        return Err(PresetError::Malformed("顶层不是对象".to_owned()));
    };
    if root.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(PresetError::UnsupportedVersion);
    }
    check_all_urls(value, policy, "$")?;
    let issued_at = text_field(root, "issued_at", 40)?
        .ok_or_else(|| PresetError::Malformed("缺少 issued_at".to_owned()))?;
    let issued_at_unix = parse_utc_timestamp(&issued_at)
        .ok_or_else(|| PresetError::Malformed("issued_at 不是 UTC 时间".to_owned()))?;
    let Some(Value::Array(items)) = root.get("presets") else {
        return Err(PresetError::Malformed("缺少 presets".to_owned()));
    };
    if items.is_empty() {
        return Err(PresetError::Malformed("presets 是空的".to_owned()));
    }
    let mut presets = Vec::new();
    for item in items {
        let Value::Object(map) = item else {
            return Err(PresetError::Malformed("预设不是对象".to_owned()));
        };
        let id = text_field(map, "id", 64)?
            .filter(|id| {
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
            .ok_or_else(|| PresetError::Malformed("预设缺少合法的 id".to_owned()))?;
        let name = text_field(map, "name", 60)?
            .ok_or_else(|| PresetError::Malformed("预设缺少 name".to_owned()))?;
        presets.push(Preset {
            id,
            name,
            recommended: map.get("recommended").and_then(Value::as_bool) == Some(true),
            tagline: text_field(map, "tagline", 80)?,
            connect_url: text_field(map, "connect_url", 2048)?,
            token_url: text_field(map, "token_url", 2048)?,
            status_url: text_field(map, "status_url", 2048)?,
            revoke_url: text_field(map, "revoke_url", 2048)?,
            openai_base: text_field(map, "openai_base", 2048)?,
            anthropic_base: text_field(map, "anthropic_base", 2048)?,
            register_url: text_field(map, "register_url", 2048)?,
            topup_url: text_field(map, "topup_url", 2048)?,
        });
    }
    Ok(PresetManifest {
        version: 1,
        issued_at,
        issued_at_unix,
        presets,
    })
}

/// 内置兜底清单。
pub fn builtin_manifest() -> PresetManifest {
    serde_json::from_str::<Value>(BUILTIN_JSON)
        .map_err(|err| PresetError::NotJson(err.to_string()))
        .and_then(|value| parse_manifest(&value, &UrlPolicy::production()))
        .unwrap_or_else(|err| {
            // 编进程序的文件，有测试保证它合格；真走到这里也只是没有 Tokenway 这一项。
            tracing::error!(%err, "内置的预设清单不合格");
            PresetManifest::empty()
        })
}

// ---------------------------------------------------------------------------
// 验签
// ---------------------------------------------------------------------------

/// ed25519 公钥（32 字节）。
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey([u8; 32]);

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PublicKey(..)")
    }
}

impl PublicKey {
    pub fn from_base64(text: &str) -> Option<Self> {
        let bytes = STANDARD.decode(text.trim()).ok()?;
        <[u8; 32]>::try_from(bytes.as_slice()).ok().map(Self)
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

/// 程序里写死的公钥；占位（还没配）时是 `None`。
pub fn production_key() -> Option<PublicKey> {
    PublicKey::from_base64(PRESETS_PUBLIC_KEY)
}

/// 去掉顶层 `signature` 后规范化，对这些字节验 ed25519 签名。
pub fn verify_signature(doc: &Value, key: &PublicKey) -> Result<(), PresetError> {
    let Value::Object(root) = doc else {
        return Err(PresetError::Malformed("顶层不是对象".to_owned()));
    };
    let signature = root
        .get("signature")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .ok_or(PresetError::NoSignature)?;
    let signature = STANDARD
        .decode(signature.trim())
        .map_err(|_| PresetError::BadSignature)?;
    let mut rest = root.clone();
    rest.remove("signature");
    let message = canonical_json(&Value::Object(rest))?;
    UnparsedPublicKey::new(&ED25519, key.0)
        .verify(message.as_bytes(), &signature)
        .map_err(|_| PresetError::BadSignature)
}

/// 验一份远端 / 缓存的清单文本：大小、JSON、签名、地址白名单、时间。
/// `floor_unix`：已经在用的清单的签发时间，比它旧的一律拒绝（防回放）。
pub fn verify_manifest(
    text: &str,
    key: &PublicKey,
    policy: &UrlPolicy,
    now: i64,
    floor_unix: Option<i64>,
) -> Result<PresetManifest, PresetError> {
    if text.len() > MAX_MANIFEST_BYTES {
        return Err(PresetError::TooLarge);
    }
    let doc: Value =
        serde_json::from_str(text).map_err(|err| PresetError::NotJson(err.to_string()))?;
    verify_signature(&doc, key)?;
    let manifest = parse_manifest(&doc, policy)?;
    if manifest.issued_at_unix > now + MAX_FUTURE_SKEW_SECS {
        return Err(PresetError::FromTheFuture(manifest.issued_at));
    }
    if now - manifest.issued_at_unix > MAX_AGE_SECS {
        return Err(PresetError::Expired(manifest.issued_at));
    }
    if let Some(expires) = doc
        .get("expires_at")
        .and_then(Value::as_str)
        .and_then(parse_utc_timestamp)
    {
        if expires < now {
            return Err(PresetError::Expired(manifest.issued_at));
        }
    }
    if floor_unix.is_some_and(|floor| manifest.issued_at_unix < floor) {
        return Err(PresetError::Rollback(manifest.issued_at));
    }
    Ok(manifest)
}

// ---------------------------------------------------------------------------
// 缓存与定时更新
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetSource {
    Builtin,
    Cached,
}

/// 现在生效的清单。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivePresets {
    pub manifest: PresetManifest,
    pub source: PresetSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// 公钥还是占位：没发任何请求。
    NoPublicKey,
    /// 距上次成功不到 24 小时（或刚失败过）。
    NotDue,
    /// 拉到了新的一份并存下了。
    Updated { issued_at: String },
    /// 拉到的和正在用的签发时间一样。
    Unchanged,
    /// 拉到了但没通过验证，已丢弃。
    Rejected(String),
    /// 网络 / 服务器问题。
    Failed(String),
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    /// 上次成功拉取并验过的时间。
    checked_at: i64,
    /// 上次尝试的时间（成败都记）。
    attempted_at: i64,
}

/// 清单缓存目录（`<数据目录>/ai-presets`）。
pub struct PresetStore {
    dir: PathBuf,
}

impl PresetStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 现在该用哪份：通过验证的缓存 > 内置。缓存每次都重新验。
    pub fn load(&self, key: Option<&PublicKey>, policy: &UrlPolicy, now: i64) -> ActivePresets {
        let builtin = builtin_manifest();
        if let Some(key) = key {
            if let Ok(text) = std::fs::read_to_string(self.dir.join(CACHE_FILE)) {
                match verify_manifest(&text, key, policy, now, Some(builtin.issued_at_unix)) {
                    Ok(manifest) => {
                        return ActivePresets {
                            manifest,
                            source: PresetSource::Cached,
                        }
                    }
                    Err(err) => tracing::info!(%err, "缓存的预设清单没通过验证，改用内置的"),
                }
            }
        }
        ActivePresets {
            manifest: builtin,
            source: PresetSource::Builtin,
        }
    }

    fn read_meta(&self) -> Meta {
        std::fs::read_to_string(self.dir.join(META_FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write_meta(&self, meta: &Meta) {
        if let Ok(text) = serde_json::to_string(meta) {
            let _ = write_atomic(&self.dir.join(META_FILE), text.as_bytes());
        }
    }

    /// 到点就拉一次。`force` = 不看间隔（设置页以后若加"立即更新"用；后台任务传 `false`）。
    /// 所有失败都只是"这次没更新"。
    pub async fn refresh(
        &self,
        transport: &dyn Transport,
        url: &str,
        key: Option<&PublicKey>,
        policy: &UrlPolicy,
        now: i64,
        force: bool,
    ) -> RefreshOutcome {
        let Some(key) = key else {
            return RefreshOutcome::NoPublicKey;
        };
        let mut meta = self.read_meta();
        // 上次成功不到 24 小时，或者刚试过（失败后不到 1 小时）：这次不拉。
        if !force
            && (now - meta.checked_at < REFRESH_INTERVAL_SECS
                || now - meta.attempted_at < RETRY_AFTER_FAILURE_SECS)
        {
            return RefreshOutcome::NotDue;
        }
        if let Err(why) = policy.check(url) {
            return RefreshOutcome::Failed(format!("清单地址不合格：{why}"));
        }
        meta.attempted_at = now;
        let _ = std::fs::create_dir_all(&self.dir);
        self.write_meta(&meta);

        let current = self.load(Some(key), policy, now);
        let text = match fetch_text(transport, url).await {
            Ok(text) => text,
            Err(why) => return RefreshOutcome::Failed(why),
        };
        let floor = current.manifest.issued_at_unix;
        let manifest = match verify_manifest(&text, key, policy, now, Some(floor)) {
            Ok(manifest) => manifest,
            Err(err) => return RefreshOutcome::Rejected(err.to_string()),
        };
        meta.checked_at = now;
        self.write_meta(&meta);
        if manifest.issued_at_unix == floor && current.source == PresetSource::Cached {
            return RefreshOutcome::Unchanged;
        }
        match write_atomic(&self.dir.join(CACHE_FILE), text.as_bytes()) {
            Ok(()) => RefreshOutcome::Updated {
                issued_at: manifest.issued_at,
            },
            Err(why) => RefreshOutcome::Failed(format!("写缓存失败：{why}")),
        }
    }
}

async fn fetch_text(transport: &dyn Transport, url: &str) -> Result<String, String> {
    let request = HttpRequest {
        url: url.to_owned(),
        headers: vec![("accept".to_owned(), "application/json".to_owned())],
        body: Vec::new(),
    };
    let mut response = transport
        .get(request)
        .await
        .map_err(|err| err.to_string())?;
    if response.status != 200 {
        return Err(format!("服务器返回 {}", response.status));
    }
    let mut body = Vec::new();
    loop {
        match response.body.next_chunk().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(&chunk);
                if body.len() > MAX_MANIFEST_BYTES {
                    return Err("清单太大".to_owned());
                }
            }
            Ok(None) => break,
            Err(err) => return Err(err.to_string()),
        }
    }
    String::from_utf8(body).map_err(|_| "清单不是 UTF-8 文本".to_owned())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests;
