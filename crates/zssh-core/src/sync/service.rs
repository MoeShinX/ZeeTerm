//! 同步设置页背后的逻辑（不依赖 Tauri，方便测试）：连接信息的保存、主密码的保存、
//! 自动同步设置、上次结果，以及“按保存的设置跑一次同步”。
//!
//! - 存储的密码 / Secret 用本机 DPAPI 封装后存在本机（**不进同步包**，每台电脑自己
//!   连自己的）。整个连接信息作为一个 JSON 一起封装，设置表里看不到任何明文；
//! - 主密码默认不落盘；“在这台电脑上记住”打开时才用 DPAPI 封装保存；
//! - 请求走 Z-SSH 的默认代理设置（直连 / HTTP / SOCKS5）。

use crate::error::{reason_keys, Reason};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use zeroize::Zeroizing;

use super::engine::{SyncEngine, SyncReport};
use super::error::SyncError;
use super::github::{self, GithubConfig};
use super::guard::DeletionDecision;
use super::net::{ProxyScheme, SyncProxy};
use super::onedrive::browser::BrowserOpener;
use super::onedrive::config::{is_valid_client_id, OneDriveConfig};
use super::onedrive::graph as onedrive_graph;
use super::onedrive::login::{self, LoginError, LoginParams};
use super::onedrive::oauth::Endpoints;
use super::onedrive::tokens::{DbTokenStore, TokenStore, TOKENS_KEY};
use super::remote::{ConcurrencyMode, ConnectionProbe};
use super::s3::{self, S3Config};
use super::sealer::SecretSealer;
use super::storage::SyncStorage;
use super::webdav::{self, WebDavConfig};
use crate::model::ProxyKind;
use crate::store::Database;

/// 以下键都只留在本机（`SETTINGS_SYNC_CLASS` 里归为 LocalOnly）。
pub const STORAGE_KEY: &str = "sync_storage";
pub const CONCURRENCY_KEY: &str = "sync_concurrency";
pub const MASTER_PASSWORD_KEY: &str = "sync_master_password";
pub const AUTO_KEY: &str = "sync_auto";
pub const LAST_RESULT_KEY: &str = "sync_last_result";
pub const READY_KEY: &str = "sync_ready";
/// 主同步的总开关。没有这一条 = 开着，所以升级前就配好同步的用户不用动。
pub const ENABLED_KEY: &str = "sync_enabled";

/// 主密码至少几个字符。
pub const MIN_MASTER_PASSWORD_CHARS: usize = 8;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum StorageConfig {
    Webdav(WebDavConfig),
    S3(S3Config),
    /// GitHub Gist（个人访问令牌）。
    Github(GithubConfig),
    /// OneDrive（浏览器授权登录，令牌另存在本机，见 `onedrive::tokens`）。
    Onedrive(OneDriveConfig),
}

impl std::fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Webdav(config) => config.fmt(f),
            Self::S3(config) => config.fmt(f),
            Self::Github(config) => config.fmt(f),
            Self::Onedrive(config) => config.fmt(f),
        }
    }
}

impl StorageConfig {
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Webdav(_) => "webdav",
            Self::S3(_) => "s3",
            Self::Github(_) => "github",
            Self::Onedrive(_) => "onedrive",
        }
    }

    /// 存储身份：provider + 存储位置里**非机密**的字段，规范化之后取 SHA-256 的十六进制前 16 位。
    /// 同步基线绑定它（见 `SyncEngine::load_baseline`）：换了存储位置，旧基线就不再适用。
    ///
    /// - WebDAV：地址 + 子目录 + 用户名；
    /// - S3：端点 + 区域 + 桶 + 前缀 + Access Key ID；
    /// - GitHub：接口地址 + Gist ID（还没有 Gist ID 时认不出，`None`）；
    /// - OneDrive：账号显示名 + client ID（同步包放在固定的应用文件夹里，没有可配的文件夹）。
    ///
    /// **绝不用密码 / Secret / 令牌**：它们会轮换，换令牌不等于换了存储；也不该混进这个值。
    /// 规范化：去首尾空白、去结尾的 `/`、地址里的协议和主机名小写。
    pub fn identity(&self) -> Option<String> {
        let parts: Vec<String> = match self {
            Self::Webdav(config) => vec![
                "webdav".to_owned(),
                normalize_url(&config.url),
                normalize_text(&config.directory),
                config.username.trim().to_owned(),
            ],
            Self::S3(config) => vec![
                "s3".to_owned(),
                normalize_url(&config.endpoint),
                config.region.trim().to_owned(),
                config.bucket.trim().to_owned(),
                normalize_text(&config.prefix),
                config.access_key.trim().to_owned(),
            ],
            Self::Github(config) => {
                let gist = config.gist_id.trim();
                if gist.is_empty() {
                    return None;
                }
                vec![
                    "github".to_owned(),
                    normalize_url(config.effective_api_base()),
                    gist.to_ascii_lowercase(),
                ]
            }
            Self::Onedrive(config) => vec![
                "onedrive".to_owned(),
                config.account.trim().to_owned(),
                config.client_id.trim().to_ascii_lowercase(),
            ],
        };
        // 字段之间用 NUL 隔开：`a` + `bc` 和 `ab` + `c` 不会撞成同一个值。
        let mut hasher = Sha256::new();
        for part in &parts {
            hasher.update(part.as_bytes());
            hasher.update([0u8]);
        }
        let digest = hasher.finalize();
        Some(super::sigv4::hex(&digest)[..16].to_owned())
    }

    /// 这种存储本身就不支持并发保护（不用检测）：GitHub 的 Gist 接口不认 `If-Match`。
    pub fn never_protected(&self) -> bool {
        matches!(self, Self::Github(_))
    }

    /// 明文 `http://` 地址（WebDAV 会以明文发送密码；S3 签名不发送 Secret，
    /// 但对象路径和同步包的大小对网络上的人可见）。
    pub fn is_plain_http(&self) -> bool {
        match self {
            Self::Webdav(config) => config.is_plain_http(),
            Self::S3(config) => config
                .endpoint
                .trim()
                .to_ascii_lowercase()
                .starts_with("http://"),
            // 令牌会随每个请求明文发送。
            Self::Github(config) => config.is_plain_http(),
            // 地址固定是 https://。
            Self::Onedrive(_) => false,
        }
    }
}

/// 文字字段的规范化：去首尾空白、去结尾的 `/`。
fn normalize_text(text: &str) -> String {
    text.trim().trim_end_matches('/').to_owned()
}

/// 地址的规范化：同 [`normalize_text`]，另外协议和主机名（`://` 之后到下一个 `/` 为止）转小写，
/// 路径部分保持原样（路径大小写有区别）。
fn normalize_url(url: &str) -> String {
    let text = normalize_text(url);
    match text.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = match rest.find('/') {
                Some(index) => rest.split_at(index),
                None => (rest, ""),
            };
            format!(
                "{}://{}{path}",
                scheme.to_ascii_lowercase(),
                host.to_ascii_lowercase()
            )
        }
        None => text,
    }
}

/// 给界面的连接信息：**没有密码和 Secret**，只告诉界面有没有。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ConfigView {
    pub provider: String,
    // WebDAV
    pub url: String,
    pub directory: String,
    pub username: String,
    pub allow_insecure_http: bool,
    // S3
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub prefix: String,
    pub path_style: bool,
    // GitHub
    pub api_base: String,
    pub gist_id: String,
    // OneDrive：登录的账号显示名、用户在“高级”里填的 client ID（不是秘密）
    pub account: String,
    pub client_id: String,
    /// WebDAV 的密码 / S3 的 Secret / GitHub 的令牌已经保存过（界面永远拿不到它们的内容）。
    pub has_secret: bool,
    pub plain_http: bool,
}

impl StorageConfig {
    pub fn view(&self) -> ConfigView {
        let mut view = ConfigView {
            provider: self.provider().to_owned(),
            url: String::new(),
            directory: String::new(),
            username: String::new(),
            allow_insecure_http: false,
            endpoint: String::new(),
            region: String::new(),
            bucket: String::new(),
            access_key: String::new(),
            prefix: String::new(),
            path_style: true,
            api_base: String::new(),
            gist_id: String::new(),
            account: String::new(),
            client_id: String::new(),
            has_secret: false,
            plain_http: self.is_plain_http(),
        };
        match self {
            Self::Webdav(config) => {
                view.url = config.url.clone();
                view.directory = config.directory.clone();
                view.username = config.username.clone();
                view.allow_insecure_http = config.allow_insecure_http;
                view.has_secret = !config.password.is_empty();
            }
            Self::S3(config) => {
                view.endpoint = config.endpoint.clone();
                view.region = config.region.clone();
                view.bucket = config.bucket.clone();
                view.access_key = config.access_key.clone();
                view.prefix = config.prefix.clone();
                view.path_style = config.path_style;
                view.has_secret = !config.secret_key.is_empty();
            }
            Self::Github(config) => {
                view.api_base = config.effective_api_base().to_owned();
                view.gist_id = config.gist_id.clone();
                view.allow_insecure_http = config.allow_insecure_http;
                view.has_secret = !config.token.trim().is_empty();
            }
            Self::Onedrive(config) => {
                view.account = config.account.clone();
                view.client_id = config.client_id.clone();
                // 令牌不在这里（另存、DPAPI 加密）；有这个连接就说明登录成功过。
                view.has_secret = true;
            }
        }
        view
    }

    /// 界面提交的表单里密码 / Secret 留空 = 沿用已保存的。
    pub fn merge_saved_secret(&mut self, saved: Option<&StorageConfig>) {
        match (self, saved) {
            (Self::Webdav(config), Some(Self::Webdav(old))) if config.password.is_empty() => {
                config.password = old.password.clone();
            }
            (Self::S3(config), Some(Self::S3(old))) if config.secret_key.is_empty() => {
                config.secret_key = old.secret_key.clone();
            }
            (Self::Github(config), Some(Self::Github(old))) if config.token.trim().is_empty() => {
                config.token = old.token.clone();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 保存 / 读取

fn set_json<T: Serialize>(db: &Database, key: &str, value: &T) -> Result<(), SyncError> {
    let text = serde_json::to_string(value)
        .map_err(|err| SyncError::Internal(Reason::text(format!("设置序列化失败：{err}"))))?;
    db.setting_set(key, &text).map_err(SyncError::from)
}

fn get_json<T: for<'de> Deserialize<'de>>(
    db: &Database,
    key: &str,
) -> Result<Option<T>, SyncError> {
    match db.setting_get(key).map_err(SyncError::from)? {
        // 读不了 = 不确定：当成没有，不崩。
        Some(raw) => Ok(serde_json::from_str(&raw).ok()),
        None => Ok(None),
    }
}

pub(super) fn seal_to_setting(
    db: &Database,
    sealer: &dyn SecretSealer,
    key: &str,
    plaintext: &[u8],
) -> Result<(), SyncError> {
    let value = sealed_setting_value(sealer, plaintext)?;
    db.setting_set(key, &value).map_err(SyncError::from)
}

/// 先准备密文，供需要一次事务写回多项设置的调用方使用。
pub(super) fn sealed_setting_value(
    sealer: &dyn SecretSealer,
    plaintext: &[u8],
) -> Result<String, SyncError> {
    let sealed = sealer.seal(plaintext)?;
    serde_json::to_string(&base64::engine::general_purpose::STANDARD.encode(sealed))
        .map_err(|err| SyncError::Internal(Reason::text(format!("设置序列化失败：{err}"))))
}

pub(super) fn open_from_setting(
    db: &Database,
    sealer: &dyn SecretSealer,
    key: &str,
) -> Result<Option<Zeroizing<Vec<u8>>>, SyncError> {
    let Some(encoded) = get_json::<String>(db, key)? else {
        return Ok(None);
    };
    let sealed = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| {
            SyncError::InvalidData(Reason::new(
                reason_keys::SV_SETTINGS_DAMAGED,
                "本机保存的同步设置损坏",
            ))
        })?;
    Ok(Some(Zeroizing::new(sealer.open(&sealed)?)))
}

pub fn load_config(
    db: &Database,
    sealer: &dyn SecretSealer,
) -> Result<Option<StorageConfig>, SyncError> {
    let Some(plain) = open_from_setting(db, sealer, STORAGE_KEY)? else {
        return Ok(None);
    };
    serde_json::from_slice(&plain).map(Some).map_err(|err| {
        SyncError::InvalidData(Reason::text(format!("本机保存的存储设置损坏：{err}")))
    })
}

pub fn save_config(
    db: &Database,
    sealer: &dyn SecretSealer,
    config: &StorageConfig,
) -> Result<(), SyncError> {
    let plain =
        Zeroizing::new(serde_json::to_vec(config).map_err(|err| {
            SyncError::Internal(Reason::text(format!("存储设置序列化失败：{err}")))
        })?);
    seal_to_setting(db, sealer, STORAGE_KEY, &plain)?;
    // 换了存储，上次的并发保护检测结果、“已设置主密码”都不再适用。
    clear(db, CONCURRENCY_KEY)?;
    clear(db, READY_KEY)?;
    // 换成别的存储 = 断开 OneDrive：本机保存的 OneDrive 令牌不留着。
    if !matches!(config, StorageConfig::Onedrive(_)) {
        clear(db, TOKENS_KEY)?;
    }
    Ok(())
}

/// 删掉本机保存的存储连接（和它的检测结果、“已设置”标记）。
pub fn clear_config(db: &Database) -> Result<(), SyncError> {
    clear(db, STORAGE_KEY)?;
    clear(db, CONCURRENCY_KEY)?;
    clear(db, READY_KEY)?;
    // 清除 = 回到没配过的状态：下次重新配好同步时从"启用"开始，不带着上一份配置的停用。
    clear(db, ENABLED_KEY)
}

/// 主同步现在是不是启用的。没写过 = 启用；读不出来的值也按启用处理（停用是用户明确点的，
/// 读坏了宁可继续同步，也别悄悄让备份停摆）。
pub fn is_enabled(db: &Database) -> Result<bool, SyncError> {
    Ok(get_json::<bool>(db, ENABLED_KEY)?.unwrap_or(true))
}

/// 启用 / 停用主同步。只改这一个开关，存储连接、登录、主密码、云端备份目标都不动。
pub fn set_enabled(db: &Database, enabled: bool) -> Result<(), SyncError> {
    if enabled {
        // 启用是默认值：直接删掉这一条，库里就和没停用过一样。
        clear(db, ENABLED_KEY)
    } else {
        set_json(db, ENABLED_KEY, &false)
    }
}

/// 浏览器授权登录成功之后的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub config: OneDriveConfig,
    /// 连接设置和之前保存的不一样（换了账号 / client ID，或第一次登录）：检测结果、“已设置”标记已经清掉。
    pub config_changed: bool,
}

/// OneDrive 登录：校验 client ID → 浏览器授权（走软件网络代理换令牌）→ 令牌 DPAPI 加密存本机 →
/// 保存连接设置。`custom_client_id` 是“高级”里填的（空 = 用编进程序的）。
///
/// 令牌先存、设置后存：中途失败不会留下“有连接、没令牌”的半成品。再次登录同一个账号时设置不变，
/// 不会清掉“已设置主密码”的标记。
pub async fn onedrive_sign_in(
    db: &Database,
    sealer: &dyn SecretSealer,
    endpoints: &Endpoints,
    custom_client_id: &str,
    timeout: Duration,
    opener: &dyn BrowserOpener,
    cancel: &Notify,
) -> Result<SignedIn, LoginError> {
    let custom = custom_client_id.trim();
    if !custom.is_empty() && !is_valid_client_id(custom) {
        return Err(LoginError::InvalidClientId);
    }
    let draft = OneDriveConfig {
        client_id: custom.to_owned(),
        account: String::new(),
    };
    let client_id = draft.effective_client_id();
    let proxy =
        super::app_proxy::app_proxy(db).map_err(|err| LoginError::Internal(err.to_string()))?;
    let success = login::login(
        LoginParams {
            client_id: &client_id,
            endpoints,
            proxy: proxy.as_ref(),
            timeout,
        },
        opener,
        cancel,
    )
    .await?;
    DbTokenStore::new(db, sealer)
        .save(&success.tokens)
        .map_err(|err| LoginError::Internal(err.to_string()))?;
    let config = OneDriveConfig {
        client_id: custom.to_owned(),
        account: success.account,
    };
    let new = StorageConfig::Onedrive(config.clone());
    let saved = load_config(db, sealer).ok().flatten();
    let config_changed = saved.as_ref() != Some(&new);
    if config_changed {
        save_config(db, sealer, &new).map_err(|err| LoginError::Internal(err.to_string()))?;
    }
    Ok(SignedIn {
        config,
        config_changed,
    })
}

/// 断开 OneDrive：删掉本机的令牌和连接设置（云端的同步包不动）。
pub fn onedrive_disconnect(db: &Database) -> Result<(), SyncError> {
    clear(db, TOKENS_KEY)?;
    clear_config(db)
}

pub(super) fn clear(db: &Database, key: &str) -> Result<(), SyncError> {
    db.with_conn_as(|conn| -> Result<(), SyncError> {
        conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConcurrencyRecord {
    pub mode: ConcurrencyMode,
    pub notes: Vec<String>,
    pub checked_at: i64,
}

pub fn load_concurrency(db: &Database) -> Result<Option<ConcurrencyRecord>, SyncError> {
    get_json(db, CONCURRENCY_KEY)
}

pub fn save_concurrency(db: &Database, record: &ConcurrencyRecord) -> Result<(), SyncError> {
    set_json(db, CONCURRENCY_KEY, record)
}

/// 同步用的模式：没检测过一律按“不支持”走，不悄悄当成支持。
pub fn effective_mode(db: &Database) -> Result<ConcurrencyMode, SyncError> {
    Ok(load_concurrency(db)?
        .map(|record| record.mode)
        .unwrap_or(ConcurrencyMode::ReadBeforeWrite))
}

pub fn load_master_password(
    db: &Database,
    sealer: &dyn SecretSealer,
) -> Result<Option<Zeroizing<String>>, SyncError> {
    let Some(plain) = open_from_setting(db, sealer, MASTER_PASSWORD_KEY)? else {
        return Ok(None);
    };
    let text = String::from_utf8(plain.to_vec()).map_err(|_| {
        SyncError::InvalidData(Reason::new(
            reason_keys::SV_PASSWORD_DAMAGED,
            "本机保存的主密码损坏",
        ))
    })?;
    Ok(Some(Zeroizing::new(text)))
}

pub fn save_master_password(
    db: &Database,
    sealer: &dyn SecretSealer,
    password: &str,
) -> Result<(), SyncError> {
    seal_to_setting(db, sealer, MASTER_PASSWORD_KEY, password.as_bytes())
}

/// 删除本机保存的主密码（“记住”开关关掉时）。
pub fn clear_master_password(db: &Database) -> Result<(), SyncError> {
    clear(db, MASTER_PASSWORD_KEY)
}

pub fn is_ready(db: &Database) -> Result<bool, SyncError> {
    Ok(get_json::<bool>(db, READY_KEY)?.unwrap_or(false))
}

/// 这台电脑已经用主密码和云端对上过（首次上传成功或解开过云端数据）。
pub fn set_ready(db: &Database, ready: bool) -> Result<(), SyncError> {
    if ready {
        set_json(db, READY_KEY, &true)
    } else {
        clear(db, READY_KEY)
    }
}

pub const DEFAULT_AUTO_INTERVAL_MINUTES: u32 = 15;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoSettings {
    pub enabled: bool,
    pub interval_minutes: u32,
}

impl Default for AutoSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_minutes: DEFAULT_AUTO_INTERVAL_MINUTES,
        }
    }
}

pub fn load_auto(db: &Database) -> Result<AutoSettings, SyncError> {
    let mut settings: AutoSettings = get_json(db, AUTO_KEY)?.unwrap_or_default();
    settings.interval_minutes = settings.interval_minutes.clamp(1, 24 * 60);
    Ok(settings)
}

pub fn save_auto(db: &Database, settings: &AutoSettings) -> Result<(), SyncError> {
    let mut settings = settings.clone();
    settings.interval_minutes = settings.interval_minutes.clamp(1, 24 * 60);
    set_json(db, AUTO_KEY, &settings)
}

/// 上次同步的结果（状态栏和同步页显示）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LastResult {
    pub at: i64,
    /// `completed` / `needs_confirmation` / `cancelled` / `local_changed` / `error`
    pub status: String,
    pub summary: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub conflicts: usize,
    /// 这次是自动触发的。
    #[serde(default)]
    pub auto: bool,
}

pub fn load_last_result(db: &Database) -> Result<Option<LastResult>, SyncError> {
    get_json(db, LAST_RESULT_KEY)
}

pub fn save_last_result(db: &Database, result: &LastResult) -> Result<(), SyncError> {
    set_json(db, LAST_RESULT_KEY, result)
}

// ---------------------------------------------------------------------------
// 代理 / 存储 / 同步

/// 默认代理设置对应的代理（没设置 = 直连）。
pub fn default_proxy(db: &Database) -> Result<Option<SyncProxy>, SyncError> {
    let Some(id) = db.default_proxy_id().map_err(SyncError::from)? else {
        return Ok(None);
    };
    let login = db.proxy_login(id).map_err(SyncError::from)?;
    Ok(Some(SyncProxy {
        kind: match login.kind {
            ProxyKind::Socks5 => ProxyScheme::Socks5,
            ProxyKind::Http => ProxyScheme::Http,
        },
        host: login.host,
        port: login.port,
        username: login.username,
        password: login.password,
    }))
}

pub fn open_storage<'a>(
    config: &StorageConfig,
    proxy: Option<&SyncProxy>,
    mode: ConcurrencyMode,
    tokens: Arc<dyn TokenStore + 'a>,
    onedrive_endpoints: &Endpoints,
) -> Result<Box<dyn SyncStorage + 'a>, SyncError> {
    // 同步包在四种存储里都叫 `zssh-sync.json`，各家的常量值相同。
    open_storage_named(
        config,
        proxy,
        mode,
        tokens,
        onedrive_endpoints,
        webdav::OBJECT_NAME,
    )
}

/// 同 [`open_storage`]，对象名（Gist 是文件名）由调用方给。云端备份（`backup_targets`）用另一个
/// 固定名字，所以和主同步指向同一个目录 / 桶 / Gist / 应用文件夹时也不会互相覆盖。
pub fn open_storage_named<'a>(
    config: &StorageConfig,
    proxy: Option<&SyncProxy>,
    mode: ConcurrencyMode,
    tokens: Arc<dyn TokenStore + 'a>,
    onedrive_endpoints: &Endpoints,
    object_name: &str,
) -> Result<Box<dyn SyncStorage + 'a>, SyncError> {
    let storage_error = SyncError::from;
    Ok(match config {
        StorageConfig::Webdav(config) => Box::new(
            webdav::open_storage_named(config, proxy, mode, object_name).map_err(storage_error)?,
        ),
        StorageConfig::S3(config) => Box::new(
            s3::open_storage_named(config, proxy, mode, object_name).map_err(storage_error)?,
        ),
        // Gist 没有条件写入，并发控制是固定的“写前再读 + 写后确认”，不看检测结果。
        StorageConfig::Github(config) => {
            Box::new(github::open_storage_named(config, proxy, object_name).map_err(storage_error)?)
        }
        StorageConfig::Onedrive(config) => {
            let client_id = config.effective_client_id();
            Box::new(
                onedrive_graph::open_storage_named(
                    onedrive_endpoints.clone(),
                    &client_id,
                    proxy,
                    mode,
                    tokens,
                    object_name,
                )
                .map_err(storage_error)?,
            )
        }
    })
}

/// 测试连接：能连上、读、写、并发保护，分别报告。构造阶段的错误（地址不合法、
/// http 没确认）也放进 `read` 里报告，不当成异常。
pub async fn test_connection(
    config: &StorageConfig,
    proxy: Option<&SyncProxy>,
    tokens: Arc<dyn TokenStore + '_>,
    onedrive_endpoints: &Endpoints,
) -> ConnectionProbe {
    let result = match config {
        StorageConfig::Webdav(config) => webdav::probe(config, proxy).await,
        StorageConfig::S3(config) => s3::probe(config, proxy).await,
        StorageConfig::Github(config) => github::probe(config, proxy).await,
        StorageConfig::Onedrive(config) => {
            let client_id = config.effective_client_id();
            onedrive_graph::probe(onedrive_endpoints.clone(), &client_id, proxy, tokens).await
        }
    };
    match result {
        Ok(probe) => probe,
        Err(err) => ConnectionProbe {
            read: super::remote::Check {
                ok: false,
                message: err.to_string(),
            },
            write: super::remote::Check {
                ok: false,
                message: "没有继续检查写权限".to_owned(),
            },
            concurrency: None,
            has_existing_data: false,
        },
    }
}

/// 同步引擎需要的本机信息。
pub struct SyncContext<'a> {
    pub db: &'a Database,
    pub sealer: &'a dyn SecretSealer,
    pub device_id: String,
    pub data_dir: std::path::PathBuf,
    pub known_hosts_path: Option<std::path::PathBuf>,
    pub app_version: String,
    /// 微软服务的地址（生产是 `Endpoints::microsoft()`，测试指向进程内的模拟服务）。
    pub onedrive_endpoints: Endpoints,
}

impl<'a> SyncContext<'a> {
    pub fn engine(&self) -> SyncEngine<'a> {
        SyncEngine::new(
            self.db,
            self.sealer,
            self.device_id.clone(),
            self.data_dir.clone(),
            self.known_hosts_path.clone(),
            Some(self.app_version.clone()),
        )
    }

    fn storage(&self) -> Result<Box<dyn SyncStorage + 'a>, SyncError> {
        let config = load_config(self.db, self.sealer)?.ok_or(SyncError::NotConfigured)?;
        let proxy = super::app_proxy::app_proxy(self.db)?;
        // OneDrive 的访问令牌会在同步中途刷新，新令牌要存回本机数据库，所以存储持有令牌库。
        let tokens: Arc<dyn TokenStore + 'a> = Arc::new(DbTokenStore::new(self.db, self.sealer));
        open_storage(
            &config,
            proxy.as_ref(),
            effective_mode(self.db)?,
            tokens,
            &self.onedrive_endpoints,
        )
    }

    /// 按保存的设置跑一次同步。成功（拿到云端数据并合并）之后标记“已设置”。
    pub async fn run_sync(
        &self,
        master_password: &str,
        decision: Option<DeletionDecision>,
    ) -> Result<SyncReport, SyncError> {
        let storage = self.storage()?;
        let report = self
            .engine()
            .sync(storage.as_ref(), master_password, decision)
            .await?;
        if report.status == super::engine::SyncStatus::Completed {
            set_ready(self.db, true)?;
        }
        Ok(report)
    }

    /// 修改主密码：读出云端包，用旧密码解开、新密码重新加密，带着读到的版本号写回。
    /// 写入时云端被别人改过会返回错误，让用户先同步再改。
    pub async fn change_master_password(&self, old: &str, new: &str) -> Result<(), SyncError> {
        if new.chars().count() < MIN_MASTER_PASSWORD_CHARS {
            return Err(SyncError::InvalidData(
                Reason::new(
                    reason_keys::SV_NEW_MASTER_SHORT,
                    format!("新主密码至少 {MIN_MASTER_PASSWORD_CHARS} 个字符"),
                )
                .with("min", MIN_MASTER_PASSWORD_CHARS),
            ));
        }
        let storage = self.storage()?;
        let object = storage
            .read()
            .await
            .map_err(SyncError::from)?
            .ok_or_else(|| {
                SyncError::InvalidData(Reason::new(
                    reason_keys::SV_NO_CLOUD_DATA,
                    "云端还没有同步数据，先同步一次",
                ))
            })?;
        let reencrypted = super::crypto::change_password(&object.bytes, old, new)?;
        storage
            .write(&reencrypted, Some(&object.version))
            .await
            .map_err(|err| match err {
                super::storage::StorageError::Conflict => SyncError::Storage(Reason::new(
                    reason_keys::SV_CLOUD_CHANGED,
                    "云端刚刚被另一台电脑改过，请先同步一次再改主密码",
                )),
                other => SyncError::from(other),
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod enabled_tests {
    use super::*;
    use crate::sync::sealer::TestSealer;
    use crate::sync::settings::{sync_mode, SyncMode};

    #[test]
    fn sync_is_enabled_by_default_so_existing_setups_keep_syncing() {
        let db = Database::open_in_memory().expect("内存库");
        assert!(is_enabled(&db).expect("读取"));
    }

    #[test]
    fn disabling_round_trips_and_enabling_removes_the_row() {
        let db = Database::open_in_memory().expect("内存库");
        set_enabled(&db, false).expect("停用");
        assert!(!is_enabled(&db).expect("读取"));
        assert!(db.setting_get(ENABLED_KEY).expect("读取").is_some());

        set_enabled(&db, true).expect("启用");
        assert!(is_enabled(&db).expect("读取"));
        assert!(db.setting_get(ENABLED_KEY).expect("读取").is_none());
    }

    #[test]
    fn an_unreadable_value_counts_as_enabled() {
        let db = Database::open_in_memory().expect("内存库");
        db.setting_set(ENABLED_KEY, "not json").expect("写入");
        assert!(is_enabled(&db).expect("读取"));
    }

    #[test]
    fn the_switch_is_a_local_setting_and_never_travels_in_the_sync_package() {
        assert_eq!(sync_mode(ENABLED_KEY), Some(SyncMode::LocalOnly));
    }

    #[test]
    fn disabling_keeps_every_other_sync_setting() {
        let db = Database::open_in_memory().expect("内存库");
        let sealer = TestSealer(7);
        let config = StorageConfig::Webdav(WebDavConfig {
            url: "https://dav.example.com/".to_owned(),
            directory: "zeeterm".to_owned(),
            username: "demo".to_owned(),
            password: "demo-password".to_owned(),
            allow_insecure_http: false,
        });
        save_config(&db, &sealer, &config).expect("保存");
        set_ready(&db, true).expect("标记");
        set_enabled(&db, false).expect("停用");
        assert!(load_config(&db, &sealer).expect("读取").is_some());
        assert!(is_ready(&db).expect("读取"));
    }

    #[test]
    fn clearing_the_storage_resets_the_switch_to_enabled() {
        let db = Database::open_in_memory().expect("内存库");
        set_enabled(&db, false).expect("停用");
        clear_config(&db).expect("清除");
        assert!(is_enabled(&db).expect("读取"));
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;

    fn dav(url: &str, directory: &str, username: &str, password: &str) -> StorageConfig {
        StorageConfig::Webdav(WebDavConfig {
            url: url.to_owned(),
            directory: directory.to_owned(),
            username: username.to_owned(),
            password: password.to_owned(),
            allow_insecure_http: false,
        })
    }

    fn s3(endpoint: &str, bucket: &str, prefix: &str, access: &str, secret: &str) -> StorageConfig {
        StorageConfig::S3(S3Config {
            endpoint: endpoint.to_owned(),
            region: "region-1".to_owned(),
            bucket: bucket.to_owned(),
            access_key: access.to_owned(),
            secret_key: secret.to_owned(),
            prefix: prefix.to_owned(),
            path_style: true,
        })
    }

    fn gist(id: &str, token: &str) -> StorageConfig {
        StorageConfig::Github(GithubConfig {
            api_base: String::new(),
            token: token.to_owned(),
            gist_id: id.to_owned(),
            allow_insecure_http: false,
        })
    }

    #[test]
    fn identity_is_sixteen_lowercase_hex_chars() {
        let id = dav("https://dav.example.com/", "", "demo", "pw")
            .identity()
            .unwrap();
        assert_eq!(id.len(), 16);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
    }

    /// 密码 / Secret / 令牌不参与身份：轮换它们不等于换了存储。
    #[test]
    fn secrets_never_change_the_identity() {
        assert_eq!(
            dav("https://dav.example.com/", "sync", "demo", "pw-1").identity(),
            dav("https://dav.example.com/", "sync", "demo", "pw-2").identity()
        );
        assert_eq!(
            s3("https://s3.example.com", "bucket", "p", "AK1", "secret-1").identity(),
            s3("https://s3.example.com", "bucket", "p", "AK1", "secret-2").identity()
        );
        assert_eq!(
            gist("abc123", "token-1").identity(),
            gist("abc123", "token-2").identity()
        );
    }

    #[test]
    fn a_different_location_is_a_different_storage() {
        let base = dav("https://dav.example.com/", "sync", "demo", "pw");
        for other in [
            dav("https://other.example.com/", "sync", "demo", "pw"),
            dav("https://dav.example.com/", "other", "demo", "pw"),
            dav("https://dav.example.com/", "sync", "someone", "pw"),
            dav("https://dav.example.com/dav2", "sync", "demo", "pw"),
        ] {
            assert_ne!(base.identity(), other.identity(), "{other:?}");
        }
        let s3_base = s3("https://s3.example.com", "bucket", "p", "AK1", "s");
        for other in [
            s3("https://s3.other.example.com", "bucket", "p", "AK1", "s"),
            s3("https://s3.example.com", "bucket-2", "p", "AK1", "s"),
            s3("https://s3.example.com", "bucket", "q", "AK1", "s"),
            s3("https://s3.example.com", "bucket", "p", "AK2", "s"),
        ] {
            assert_ne!(s3_base.identity(), other.identity(), "{other:?}");
        }
        assert_ne!(gist("abc", "t").identity(), gist("abd", "t").identity());
        // 不同 provider 即使字段文字相同也不会撞。
        assert_ne!(
            dav("https://x.example.com", "", "u", "p").identity(),
            s3("https://x.example.com", "", "", "u", "p").identity()
        );
    }

    /// 规范化：首尾空白、结尾的 `/`、协议和主机名大小写不改变身份；路径大小写要区分。
    #[test]
    fn location_is_normalized_before_hashing() {
        assert_eq!(
            dav("https://dav.example.com/dav", "sync", "demo", "pw").identity(),
            dav("  HTTPS://DAV.Example.COM/dav/ ", "sync/", " demo ", "pw").identity()
        );
        assert_ne!(
            dav("https://dav.example.com/Dav", "", "demo", "pw").identity(),
            dav("https://dav.example.com/dav", "", "demo", "pw").identity(),
            "路径大小写有区别"
        );
        assert_eq!(
            s3("https://S3.Example.com/", "bucket", "p/", "AK1", "s").identity(),
            s3("https://s3.example.com", "bucket", "p", "AK1", "s").identity()
        );
    }

    /// 还没有 Gist ID：认不出身份（`None`），不拿“不确定”当“换了存储”。
    #[test]
    fn a_gist_without_an_id_has_no_identity() {
        assert_eq!(gist("", "token").identity(), None);
        assert_eq!(gist("   ", "token").identity(), None);
        assert!(gist("abc", "token").identity().is_some());
    }

    #[test]
    fn onedrive_identity_follows_the_account_and_client_id() {
        let od = |account: &str, client: &str| {
            StorageConfig::Onedrive(OneDriveConfig {
                client_id: client.to_owned(),
                account: account.to_owned(),
            })
        };
        assert!(od("someone", "").identity().is_some());
        assert_eq!(od("someone", "").identity(), od(" someone ", "").identity());
        assert_ne!(od("someone", "").identity(), od("other", "").identity());
        assert_ne!(
            od("someone", "").identity(),
            od("someone", "00000000-0000-0000-0000-000000000000").identity()
        );
    }
}
