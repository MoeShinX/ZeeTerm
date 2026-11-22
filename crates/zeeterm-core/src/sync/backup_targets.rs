//! 云端备份目标：一个主同步 + 若干个只上传的备份。
//!
//! - 主同步（`service::StorageConfig`）照旧做双向三方合并；备份目标只做**单向**加密上传，从来不
//!   往回合并。每个目标是一份 [`StorageConfig`]（四种存储都行，可以和主同步同一种、不同账号 / 桶）。
//! - 备份对象用固定的另一个名字（[`OBJECT_NAME`]），所以和主同步指向同一个 WebDAV 目录 / S3 桶 /
//!   Gist / OneDrive 应用文件夹时也不会互相覆盖。每次覆盖写，只留一份。
//! - 内容和主同步完全相同：同一个快照（只含白名单里的设置，任何“只在本机”的键都不在里面）、同一个
//!   主密码加密。备份包里没有存储账号的任何凭据，也没有这份备份目标列表本身。
//! - 目标列表整个用本机 DPAPI 封装后放在 [`TARGETS_KEY`]；OneDrive 目标的令牌一个目标一个键
//!   （[`token_key`]），和主同步的令牌（`TOKENS_KEY`）互不覆盖、互不清除。这些键都不在同步白名单里，
//!   不会进同步包。
//! - 一个目标失败只记在它自己的状态里，不影响主同步的结果，也不影响其他目标。

use crate::error::{reason_keys, Reason};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use zeroize::Zeroizing;

use super::engine::SyncEngine;
use super::error::SyncError;
use super::github;
use super::offline::{OfflineImportMode, OfflineImportReport};
use super::onedrive::browser::BrowserOpener;
use super::onedrive::config::{is_valid_client_id, OneDriveConfig};
use super::onedrive::login::{self, LoginError, LoginParams};
use super::onedrive::oauth::Endpoints;
use super::onedrive::tokens::{DbTokenStore, TokenStore};
use super::package::SyncSnapshot;
use super::remote::{content_hash, ConcurrencyMode};
use super::sealer::SecretSealer;
use super::service::{
    self, clear, open_from_setting, seal_to_setting, StorageConfig, SyncContext,
    MIN_MASTER_PASSWORD_CHARS,
};
use super::snapshot::snapshot_local_with;
use super::storage::{StorageError, SyncStorage};
use crate::ssh::BoxFuture;
use crate::store::Database;

/// 备份目标列表在设置表里的键（只在本机，`settings.rs` 里归为 LocalOnly）。
pub const TARGETS_KEY: &str = "sync_backup_targets";

/// 备份目标的 OneDrive 令牌的键前缀，后面接目标 id。不在白名单里 = 一律不同步。
pub(super) const TOKEN_KEY_PREFIX: &str = "sync_backup_token.";

/// 云端备份的对象名（Gist 是文件名）。和主同步的 `zssh-sync.json` 不同。
pub const OBJECT_NAME: &str = "zeeterm-backup.json";

/// 最多几个备份目标。
pub const MAX_TARGETS: usize = 5;

/// 上传时遇到版本冲突 / 超时最多试几次。
const UPLOAD_ATTEMPTS: usize = 3;

/// 单个目标从开始到结束最长多久；超了就算这个目标失败，接着做下一个。
const TARGET_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// 上一次备份的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetResult {
    /// 结束时间（Unix 秒）。
    pub at: i64,
    pub ok: bool,
    /// 失败原因；成功时是一句简短的说明。
    pub message: String,
    /// 成功时：这次上传的内容指纹。数据没变就不用再传（自动备份用它跳过）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupTarget {
    pub id: String,
    pub config: StorageConfig,
    pub enabled: bool,
    #[serde(default)]
    pub last: Option<TargetResult>,
}

impl BackupTarget {
    /// 给界面的“这个目标指向哪”：不含任何凭据。
    pub fn describe(&self) -> String {
        describe(&self.config)
    }
}

/// 存储类型的中文名。
pub fn provider_label(config: &StorageConfig) -> &'static str {
    match config {
        StorageConfig::Webdav(_) => "WebDAV",
        StorageConfig::S3(_) => "S3",
        StorageConfig::Github(_) => "GitHub Gist",
        StorageConfig::Onedrive(_) => "OneDrive",
    }
}

/// 目标指向哪：主机 + 目录、桶 + 前缀、Gist ID、OneDrive 账号。不含密码 / Secret / 令牌。
pub fn describe(config: &StorageConfig) -> String {
    fn host_of(raw: &str) -> String {
        reqwest::Url::parse(raw.trim())
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_else(|| raw.trim().to_owned())
    }
    fn join(head: &str, tail: &str) -> String {
        let tail = tail.trim().trim_matches('/');
        if tail.is_empty() {
            head.to_owned()
        } else {
            format!("{head}/{tail}")
        }
    }
    match config {
        StorageConfig::Webdav(config) => join(&host_of(&config.url), &config.directory),
        StorageConfig::S3(config) => {
            let place = join(config.bucket.trim(), &config.prefix);
            format!("{place}（{}）", host_of(&config.endpoint))
        }
        StorageConfig::Github(config) => {
            let id = config.gist_id.trim();
            if id.is_empty() {
                "Gist（第一次备份时新建）".to_owned()
            } else {
                let short: String = id.chars().take(8).collect();
                format!("Gist {short}…")
            }
        }
        StorageConfig::Onedrive(config) => {
            let account = config.account.trim();
            if account.is_empty() {
                "OneDrive 账号".to_owned()
            } else {
                account.to_owned()
            }
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// 新目标的 id。
pub fn new_target_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 这个目标的 OneDrive 令牌放在哪个键。
pub fn token_key(id: &str) -> String {
    format!("{TOKEN_KEY_PREFIX}{id}")
}

/// 这个目标的令牌库（DPAPI 加密进本机数据库）。
pub fn token_store<'a>(
    db: &'a Database,
    sealer: &'a dyn SecretSealer,
    id: &str,
) -> DbTokenStore<'a> {
    DbTokenStore::with_key(db, sealer, token_key(id))
}

// ---------------------------------------------------------------------------
// 保存 / 读取

/// 读—改—写的互斥：后台备份写结果和界面改设置不会互相盖掉。
static TARGETS_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    TARGETS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn load_unlocked(db: &Database, sealer: &dyn SecretSealer) -> Result<Vec<BackupTarget>, SyncError> {
    let Some(plain) = open_from_setting(db, sealer, TARGETS_KEY)? else {
        return Ok(Vec::new());
    };
    serde_json::from_slice(&plain).map_err(|err| {
        SyncError::InvalidData(Reason::text(format!("本机保存的云端备份设置损坏：{err}")))
    })
}

fn save_unlocked(
    db: &Database,
    sealer: &dyn SecretSealer,
    targets: &[BackupTarget],
) -> Result<(), SyncError> {
    if targets.is_empty() {
        return clear(db, TARGETS_KEY);
    }
    let plain = Zeroizing::new(serde_json::to_vec(targets).map_err(|err| {
        SyncError::Internal(Reason::text(format!("云端备份设置序列化失败：{err}")))
    })?);
    seal_to_setting(db, sealer, TARGETS_KEY, &plain)
}

/// 读出本机保存的备份目标。读不出来（换了 Windows 账户、内容损坏）要报错，不能当成“没有”：
/// 否则下一次保存会把还在的目标盖掉。
pub fn load_targets(
    db: &Database,
    sealer: &dyn SecretSealer,
) -> Result<Vec<BackupTarget>, SyncError> {
    let _guard = lock();
    load_unlocked(db, sealer)
}

/// 账号恢复与目标列表的读—改—写共用锁；调用方必须先拿此锁再拿数据库锁，
/// 且闭包内不能再调用会拿目标锁的接口。
pub(super) fn with_targets_lock<T>(
    run: impl FnOnce() -> Result<T, SyncError>,
) -> Result<T, SyncError> {
    let _guard = lock();
    run()
}

/// 目标列表读不出来时的出路：删掉整个列表和所有目标的令牌（云端的备份文件不动）。
pub fn reset_targets(db: &Database, sealer: &dyn SecretSealer) -> Result<(), SyncError> {
    let _guard = lock();
    if let Ok(old) = load_unlocked(db, sealer) {
        for target in &old {
            let _ = token_store(db, sealer, &target.id).clear();
        }
    }
    clear(db, TARGETS_KEY)
}

/// 新增一个目标。`id` 给了就用它（OneDrive 要先登录、令牌已经存在这个 id 下）；不给就新生成。
/// OneDrive 目标必须已经登录过（有令牌）。
pub fn add_target(
    db: &Database,
    sealer: &dyn SecretSealer,
    id: Option<String>,
    config: StorageConfig,
    enabled: bool,
) -> Result<BackupTarget, SyncError> {
    let _guard = lock();
    let mut targets = load_unlocked(db, sealer)?;
    if targets.len() >= MAX_TARGETS {
        return Err(SyncError::InvalidData(
            Reason::new(
                reason_keys::BT_MAX_TARGETS,
                format!("云端备份目标最多 {MAX_TARGETS} 个"),
            )
            .with("max", MAX_TARGETS),
        ));
    }
    let id = id.unwrap_or_else(new_target_id);
    if !valid_id(&id) {
        return Err(SyncError::InvalidData(Reason::new(
            reason_keys::BT_ID_BAD,
            "备份目标 ID 不合法",
        )));
    }
    if targets.iter().any(|target| target.id == id) {
        return Err(SyncError::InvalidData(Reason::new(
            reason_keys::BT_EXISTS,
            "这个备份目标已经存在",
        )));
    }
    check_ready(db, sealer, &id, &config)?;
    let target = BackupTarget {
        id,
        config,
        enabled,
        last: None,
    };
    targets.push(target.clone());
    save_unlocked(db, sealer, &targets)?;
    Ok(target)
}

/// OneDrive 目标要有登录过的令牌，别的存储没有额外条件。
fn check_ready(
    db: &Database,
    sealer: &dyn SecretSealer,
    id: &str,
    config: &StorageConfig,
) -> Result<(), SyncError> {
    if matches!(config, StorageConfig::Onedrive(_))
        && token_store(db, sealer, id)
            .load()
            .map_err(SyncError::from)?
            .is_none()
    {
        return Err(SyncError::InvalidData(Reason::new(
            reason_keys::BT_LOGIN_FIRST,
            "请先登录 OneDrive，再保存这个备份目标",
        )));
    }
    Ok(())
}

/// 改一个目标的连接设置。上一次的结果清掉（指向的地方可能变了，旧指纹不再适用）。
/// 改成不是 OneDrive 时，这个目标的令牌一起删掉。
pub fn update_target(
    db: &Database,
    sealer: &dyn SecretSealer,
    id: &str,
    config: StorageConfig,
) -> Result<BackupTarget, SyncError> {
    let _guard = lock();
    let mut targets = load_unlocked(db, sealer)?;
    check_ready(db, sealer, id, &config)?;
    let target = targets
        .iter_mut()
        .find(|target| target.id == id)
        .ok_or_else(|| {
            SyncError::InvalidData(Reason::new(reason_keys::BT_NONE, "没有这个备份目标"))
        })?;
    let leaving_onedrive = matches!(target.config, StorageConfig::Onedrive(_))
        && !matches!(config, StorageConfig::Onedrive(_));
    target.config = config;
    target.last = None;
    let updated = target.clone();
    save_unlocked(db, sealer, &targets)?;
    if leaving_onedrive {
        let _ = token_store(db, sealer, id).clear();
    }
    Ok(updated)
}

/// 目标自己补全了连接设置（Gist 第一次备份时新建的 ID）：只换设置，不动上次结果。
fn complete_config(
    db: &Database,
    sealer: &dyn SecretSealer,
    id: &str,
    config: StorageConfig,
) -> Result<(), SyncError> {
    let _guard = lock();
    let mut targets = load_unlocked(db, sealer)?;
    if let Some(target) = targets.iter_mut().find(|target| target.id == id) {
        target.config = config;
        save_unlocked(db, sealer, &targets)?;
    }
    Ok(())
}

pub fn set_enabled(
    db: &Database,
    sealer: &dyn SecretSealer,
    id: &str,
    enabled: bool,
) -> Result<(), SyncError> {
    let _guard = lock();
    let mut targets = load_unlocked(db, sealer)?;
    let target = targets
        .iter_mut()
        .find(|target| target.id == id)
        .ok_or_else(|| {
            SyncError::InvalidData(Reason::new(reason_keys::BT_NONE, "没有这个备份目标"))
        })?;
    target.enabled = enabled;
    save_unlocked(db, sealer, &targets)
}

/// 删掉一个目标和它的令牌（云端的备份文件不动）。
pub fn delete_target(db: &Database, sealer: &dyn SecretSealer, id: &str) -> Result<(), SyncError> {
    let _guard = lock();
    let mut targets = load_unlocked(db, sealer)?;
    let before = targets.len();
    targets.retain(|target| target.id != id);
    if targets.len() != before {
        save_unlocked(db, sealer, &targets)?;
    }
    if valid_id(id) {
        let _ = token_store(db, sealer, id).clear();
    }
    Ok(())
}

/// 一次备份的结果记进这个目标。目标在这期间被删了就什么也不做。
pub fn record_result(
    db: &Database,
    sealer: &dyn SecretSealer,
    id: &str,
    result: TargetResult,
) -> Result<(), SyncError> {
    let _guard = lock();
    let mut targets = load_unlocked(db, sealer)?;
    if let Some(target) = targets.iter_mut().find(|target| target.id == id) {
        target.last = Some(result);
        save_unlocked(db, sealer, &targets)?;
    }
    Ok(())
}

/// 备份目标的 OneDrive 登录：和主同步的登录同一个流程，但令牌存到这个目标自己的键下，
/// 不碰主同步的连接设置和令牌。返回账号信息，调用方据此 [`add_target`] / [`update_target`]。
#[allow(clippy::too_many_arguments)]
pub async fn onedrive_sign_in_for_target(
    db: &Database,
    sealer: &dyn SecretSealer,
    endpoints: &Endpoints,
    target_id: &str,
    custom_client_id: &str,
    timeout: Duration,
    opener: &dyn BrowserOpener,
    cancel: &Notify,
) -> Result<OneDriveConfig, LoginError> {
    if !valid_id(target_id) {
        return Err(LoginError::Internal("备份目标 ID 不合法".to_owned()));
    }
    let custom = custom_client_id.trim();
    if !custom.is_empty() && !is_valid_client_id(custom) {
        return Err(LoginError::InvalidClientId);
    }
    let draft = OneDriveConfig {
        client_id: custom.to_owned(),
        account: String::new(),
        account_id: String::new(),
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
    token_store(db, sealer, target_id)
        .save(&success.tokens)
        .map_err(|err| LoginError::Internal(err.to_string()))?;
    Ok(OneDriveConfig {
        client_id: custom.to_owned(),
        account: success.account,
        account_id: success.account_id,
    })
}

// ---------------------------------------------------------------------------
// 备份包

/// 一份要上传的备份：密文 + 内容指纹。
pub struct BackupPackage {
    pub bytes: Vec<u8>,
    /// 快照内容（不含导出时间）加主密码的 SHA-256。两次之间本机数据和主密码都没变，指纹就一样；
    /// 加密每次都用新的盐和 nonce，所以不能拿密文比。带上主密码是因为改了主密码之后云端那份
    /// 还是旧密码加密的，必须重新传（这个指纹和“记住的主密码”一样只存在本机 DPAPI 保护的设置里）。
    pub digest: String,
}

impl SyncEngine<'_> {
    /// 云端备份包：和主同步同一个快照（只含白名单里的设置）、同一个主密码加密。
    pub fn backup_package(&self, master_password: &str) -> Result<BackupPackage, SyncError> {
        let now = Self::now();
        let mut snapshot = snapshot_local_with(
            self.db,
            self.sealer,
            &self.device_id,
            self.known_hosts_path.as_deref(),
            now,
            &self.policy,
        )?;
        snapshot.created_at = 0;
        let mut material = Zeroizing::new(snapshot.to_json()?);
        material.push(0);
        material.extend_from_slice(master_password.as_bytes());
        let digest = content_hash(&material);
        snapshot.created_at = now;
        Ok(BackupPackage {
            bytes: snapshot.seal(master_password)?,
            digest,
        })
    }
}

/// 覆盖写一份：先读到当前版本再带着它写；版本冲突 / 超时 / 暂时不可用就重来几次。
pub async fn upload_overwrite(
    storage: &dyn SyncStorage,
    bytes: &[u8],
) -> Result<String, StorageError> {
    let mut attempt = 0usize;
    loop {
        attempt += 1;
        let step = async {
            let current = storage.read().await?;
            let expected = current.as_ref().map(|object| object.version.as_str());
            storage.write(bytes, expected).await
        };
        match step.await {
            Ok(version) => return Ok(version),
            Err(StorageError::Conflict | StorageError::Timeout | StorageError::Unavailable(_))
                if attempt < UPLOAD_ATTEMPTS =>
            {
                continue
            }
            Err(err) => return Err(err),
        }
    }
}

// ---------------------------------------------------------------------------
// 跑备份 / 恢复

/// 怎么打开一个目标的存储。生产的实现在 [`SyncContext`] 上；测试换成内存存储。
pub trait TargetOpener: Sync {
    fn open<'s>(
        &'s self,
        target: &'s BackupTarget,
    ) -> BoxFuture<'s, Result<Box<dyn SyncStorage + 's>, SyncError>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutcomeStatus {
    Uploaded,
    /// 自动备份：上次成功的内容和现在一样，没有重新上传。
    Unchanged,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOutcome {
    pub id: String,
    pub status: OutcomeStatus,
}

struct ContextOpener<'c, 'a> {
    ctx: &'c SyncContext<'a>,
}

impl TargetOpener for ContextOpener<'_, '_> {
    fn open<'s>(
        &'s self,
        target: &'s BackupTarget,
    ) -> BoxFuture<'s, Result<Box<dyn SyncStorage + 's>, SyncError>> {
        Box::pin(async move {
            let ctx = self.ctx;
            let proxy = super::app_proxy::app_proxy(ctx.db)?;
            let mut config = target.config.clone();
            // Gist ID 允许留空：第一次备份时新建一个，之后都用它。
            if let StorageConfig::Github(gist) = &mut config {
                if gist.gist_id.trim().is_empty() {
                    let id = github::create_gist_named(gist, proxy.as_ref(), OBJECT_NAME)
                        .await
                        .map_err(SyncError::from)?;
                    gist.gist_id = id;
                    // 先存下来：后面上传失败，下一次也不会再新建一个。
                    complete_config(ctx.db, ctx.sealer, &target.id, config.clone())?;
                }
            }
            let tokens: Arc<dyn TokenStore + 's> =
                Arc::new(token_store(ctx.db, ctx.sealer, &target.id));
            service::open_storage_named(
                &config,
                proxy.as_ref(),
                // 备份只是覆盖写，用不到并发保护，也不依赖“测试连接”的结果。
                ConcurrencyMode::ReadBeforeWrite,
                tokens,
                &ctx.onedrive_endpoints,
                OBJECT_NAME,
            )
        })
    }
}

/// 把本机当前数据加密上传到目标。`only` 为 `None`：主同步成功后的自动备份——只做启用的目标，
/// 内容和上次成功的一样就跳过；`Some(id)`：用户点了这个目标的「立即备份」——不管启用与否、
/// 内容有没有变都传。
///
/// 返回每个被处理的目标的结果；单个目标失败只记在它自己的状态里，其他目标照做。
/// 只有目标列表读不出来、主密码不合格这类“整体”问题才返回 `Err`。
pub async fn backup_with(
    engine: &SyncEngine<'_>,
    opener: &dyn TargetOpener,
    master_password: &str,
    only: Option<&str>,
) -> Result<Vec<TargetOutcome>, SyncError> {
    let (db, sealer) = (engine.db, engine.sealer);
    let targets = load_targets(db, sealer)?;
    let selected: Vec<BackupTarget> = match only {
        Some(id) => vec![targets
            .into_iter()
            .find(|target| target.id == id)
            .ok_or_else(|| {
                SyncError::InvalidData(Reason::new(reason_keys::BT_NONE, "没有这个备份目标"))
            })?],
        None => targets
            .into_iter()
            .filter(|target| target.enabled)
            .collect(),
    };
    if selected.is_empty() {
        return Ok(Vec::new());
    }
    if master_password.chars().count() < MIN_MASTER_PASSWORD_CHARS {
        return Err(SyncError::InvalidData(
            Reason::new(
                reason_keys::SV_MASTER_SHORT,
                format!("主密码至少 {MIN_MASTER_PASSWORD_CHARS} 个字符"),
            )
            .with("min", MIN_MASTER_PASSWORD_CHARS),
        ));
    }
    let package = match engine.backup_package(master_password) {
        Ok(package) => package,
        Err(err) => {
            // 读本机数据 / 加密出问题：每个目标都记一笔，界面上看得到原因。
            let message = err.to_string();
            let mut outcomes = Vec::new();
            for target in &selected {
                record_failure(db, sealer, &target.id, &message);
                outcomes.push(TargetOutcome {
                    id: target.id.clone(),
                    status: OutcomeStatus::Failed(message.clone()),
                });
            }
            return Ok(outcomes);
        }
    };

    let mut outcomes = Vec::new();
    for target in &selected {
        let unchanged = only.is_none()
            && target
                .last
                .as_ref()
                .is_some_and(|last| last.ok && last.digest.as_deref() == Some(&package.digest));
        if unchanged {
            outcomes.push(TargetOutcome {
                id: target.id.clone(),
                status: OutcomeStatus::Unchanged,
            });
            continue;
        }
        let run = async {
            let storage = opener.open(target).await?;
            upload_overwrite(storage.as_ref(), &package.bytes)
                .await
                .map_err(SyncError::from)
        };
        let result = match tokio::time::timeout(TARGET_TIMEOUT, run).await {
            Ok(result) => result,
            Err(_) => Err(target_timeout()),
        };
        let status = match result {
            Ok(_) => {
                let _ = record_result(
                    db,
                    sealer,
                    &target.id,
                    TargetResult {
                        at: SyncEngine::now(),
                        ok: true,
                        message: "已上传".to_owned(),
                        digest: Some(package.digest.clone()),
                    },
                );
                OutcomeStatus::Uploaded
            }
            Err(err) => {
                let message = err.to_string();
                record_failure(db, sealer, &target.id, &message);
                OutcomeStatus::Failed(message)
            }
        };
        outcomes.push(TargetOutcome {
            id: target.id.clone(),
            status,
        });
    }
    Ok(outcomes)
}

/// 失败也要留下指纹之外的记录；之前成功的指纹保留不了（这次没传成，远端内容不确定）。
fn record_failure(db: &Database, sealer: &dyn SecretSealer, id: &str, message: &str) {
    if let Err(err) = record_result(
        db,
        sealer,
        id,
        TargetResult {
            at: SyncEngine::now(),
            ok: false,
            message: message.to_owned(),
            digest: None,
        },
    ) {
        tracing::warn!(%err, "记录云端备份失败结果时出错");
    }
}

/// 从一个目标恢复：下载、用主密码解开，走“导入离线备份”同一套（先做本地备份、再在一个事务里
/// 让本机变成和备份一样），不改同步基线。
pub async fn restore_with(
    engine: &SyncEngine<'_>,
    opener: &dyn TargetOpener,
    id: &str,
    master_password: &str,
) -> Result<OfflineImportReport, SyncError> {
    let targets = load_targets(engine.db, engine.sealer)?;
    let target = targets
        .iter()
        .find(|target| target.id == id)
        .ok_or_else(|| {
            SyncError::InvalidData(Reason::new(reason_keys::BT_NONE, "没有这个备份目标"))
        })?;
    let object = {
        let run = async {
            let storage = opener.open(target).await?;
            storage.read().await.map_err(SyncError::from)
        };
        match tokio::time::timeout(TARGET_TIMEOUT, run).await {
            Ok(result) => result?,
            Err(_) => return Err(target_timeout()),
        }
    }
    .ok_or_else(|| {
        SyncError::InvalidData(Reason::new(
            reason_keys::BT_NO_BACKUP_YET,
            "这个备份目标上还没有备份",
        ))
    })?;
    let snapshot = SyncSnapshot::open(&object.bytes, master_password)?;
    engine.import_snapshot(snapshot, OfflineImportMode::Replace)
}

impl SyncContext<'_> {
    /// 见 [`backup_with`]。
    pub async fn backup_to_targets(
        &self,
        master_password: &str,
        only: Option<&str>,
    ) -> Result<Vec<TargetOutcome>, SyncError> {
        backup_with(
            &self.engine(),
            &ContextOpener { ctx: self },
            master_password,
            only,
        )
        .await
    }

    /// 见 [`restore_with`]。
    pub async fn restore_from_target(
        &self,
        id: &str,
        master_password: &str,
    ) -> Result<OfflineImportReport, SyncError> {
        restore_with(
            &self.engine(),
            &ContextOpener { ctx: self },
            id,
            master_password,
        )
        .await
    }
}

#[cfg(test)]
mod tests;

/// 一个备份目标超过 `TARGET_TIMEOUT` 还没完成。
fn target_timeout() -> SyncError {
    let minutes = TARGET_TIMEOUT.as_secs() / 60;
    SyncError::Storage(
        Reason::new(
            reason_keys::BT_TIMEOUT,
            format!("超过 {minutes} 分钟没有完成，已放弃这一次"),
        )
        .with("minutes", minutes),
    )
}
