//! 同步账号设置：本地自动备份和导出的 `.zsshbak` 额外带上的一块“这台电脑怎么连同步存储”。
//!
//! 内容：主同步的存储连接（含密码 / Secret / 令牌）、OneDrive 令牌、记住的主密码、自动同步设置、
//! 本地备份保留份数、云端备份目标列表和它们各自的 OneDrive 令牌。恢复后不用重新填。
//!
//! 边界（比功能本身更重要）：
//! - 这块**只**放在本地备份的信封（[`super::backup::BackupEnvelope`]，整体 DPAPI 加密）和导出文件
//!   明文里的独立字段 [`EXPORT_FIELD`]（整个明文由导出密码加密）里，**不是**同步快照的一部分，
//!   所以主同步包和云端备份包里永远没有它（这些键在白名单里都是 LocalOnly，快照根本不读）；
//! - 云端同步包 / 云端备份包由 [`super::snapshot`] 生成，不经过这个模块；
//! - 老版本导出的包没有这个字段：照常导入；老版本读到带这个字段的包会把它当不认识的字段丢掉。

use crate::error::Reason;
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::{Zeroize, Zeroizing};

use super::backup_targets::{self, BackupTarget, MAX_TARGETS};
use super::error::SyncError;
use super::onedrive::oauth::TokenSet;
use super::onedrive::tokens::{DbTokenStore, TokenStore, TOKENS_KEY};
use super::sealer::SecretSealer;
use super::service::{self, AutoSettings, StorageConfig};
use crate::store::settings::SYNC_BACKUP_KEEP_KEY;
use crate::store::Database;

/// 导出文件明文里放这块的字段名。
pub const EXPORT_FIELD: &str = "zeeterm_sync_accounts";

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncAccounts {
    /// 主同步的存储连接。
    #[serde(default)]
    pub storage: Option<StorageConfig>,
    /// 主同步是 OneDrive 时的令牌。
    #[serde(default)]
    pub onedrive_tokens: Option<TokenSet>,
    /// 本机记住的主密码。
    #[serde(default)]
    pub master_password: Option<String>,
    #[serde(default)]
    pub auto: Option<AutoSettings>,
    /// 本地备份保留份数的原始设置值（原样带着，不替别的版本解释它的格式）。
    #[serde(default)]
    pub backup_keep: Option<Value>,
    #[serde(default)]
    pub backup_targets: Vec<BackupTarget>,
    /// 备份目标的 OneDrive 令牌（目标 id → 令牌）。
    #[serde(default)]
    pub target_tokens: BTreeMap<String, TokenSet>,
}

/// 日志里只记有没有、几个，不记内容。
impl std::fmt::Debug for SyncAccounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncAccounts")
            .field(
                "storage",
                &self.storage.as_ref().map(StorageConfig::provider),
            )
            .field("master_password", &self.master_password.is_some())
            .field("backup_targets", &self.backup_targets.len())
            .finish_non_exhaustive()
    }
}

impl Drop for SyncAccounts {
    fn drop(&mut self) {
        if let Some(password) = self.master_password.as_mut() {
            password.zeroize();
        }
    }
}

/// 恢复 / 导入时对这块的处理结果。
#[derive(Debug, Clone, Default)]
pub enum AccountsRestore {
    /// 备份里没有这块（老版本的备份，或者备份时没配置过同步）。
    #[default]
    NotInBackup,
    /// 本机还没配置同步，已经直接写回。
    Applied,
    /// 本机已经配置了同步：没动。要不要用备份里的替换本机现在的，由用户决定
    /// （确认后调 [`SyncAccounts::apply`]）。
    NeedsConfirmation(Box<SyncAccounts>),
    /// 写回时出错（数据库问题）。数据本身已经恢复好了，只是同步账号设置没带回来。
    Failed(String),
}

/// 读不出来（换了 Windows 账户、内容损坏）按“没有”处理并记一条日志：本地备份不能因为
/// 这块读不了就整个失败；数据库本身的错误照常往上报。
fn lenient<T>(result: Result<Option<T>, SyncError>, what: &str) -> Result<Option<T>, SyncError> {
    match result {
        Ok(value) => Ok(value),
        Err(err @ (SyncError::Database(_) | SyncError::Io(_))) => Err(err),
        Err(err) => {
            tracing::warn!(%err, what, "同步账号设置读不出来，备份里不带这一项");
            Ok(None)
        }
    }
}

impl SyncAccounts {
    /// 读本机现在的同步账号设置。什么都没配置（没有存储、没有备份目标、没有记住主密码）返回 `None`。
    pub fn read(db: &Database, sealer: &dyn SecretSealer) -> Result<Option<Self>, SyncError> {
        let storage = lenient(service::load_config(db, sealer), "存储连接")?;
        let master_password = lenient(
            service::load_master_password(db, sealer)
                .map(|password| password.map(|password| password.as_str().to_owned())),
            "主密码",
        )?;
        let backup_targets = lenient(
            backup_targets::load_targets(db, sealer).map(Some),
            "云端备份目标",
        )?
        .unwrap_or_default();
        if storage.is_none() && master_password.is_none() && backup_targets.is_empty() {
            return Ok(None);
        }
        let onedrive_tokens = match &storage {
            Some(StorageConfig::Onedrive(_)) => DbTokenStore::new(db, sealer).load().ok().flatten(),
            _ => None,
        };
        let mut target_tokens = BTreeMap::new();
        for target in &backup_targets {
            if matches!(target.config, StorageConfig::Onedrive(_)) {
                if let Ok(Some(tokens)) = backup_targets::token_store(db, sealer, &target.id).load()
                {
                    target_tokens.insert(target.id.clone(), tokens);
                }
            }
        }
        // 只有用户真的设置过才带：没设置过就让恢复到的电脑用它自己的默认。
        let auto = match db.setting_get(service::AUTO_KEY).map_err(SyncError::from)? {
            Some(_) => Some(service::load_auto(db)?),
            None => None,
        };
        let backup_keep = db
            .setting_get(SYNC_BACKUP_KEEP_KEY)
            .map_err(SyncError::from)?
            .and_then(|raw| serde_json::from_str(&raw).ok());
        Ok(Some(Self {
            storage,
            onedrive_tokens,
            master_password,
            auto,
            backup_keep,
            backup_targets,
            target_tokens,
        }))
    }

    /// 本机现在有没有配置同步（有主同步存储或有云端备份目标）。读不出来的算“有”：
    /// 宁可多问一句，也不能悄悄盖掉。
    pub fn local_is_configured(
        db: &Database,
        sealer: &dyn SecretSealer,
    ) -> Result<bool, SyncError> {
        let has_storage = match service::load_config(db, sealer) {
            Ok(storage) => storage.is_some(),
            Err(SyncError::Database(_) | SyncError::Io(_)) => true,
            Err(_) => db
                .setting_get(service::STORAGE_KEY)
                .map_err(SyncError::from)?
                .is_some(),
        };
        let has_targets = match backup_targets::load_targets(db, sealer) {
            Ok(targets) => !targets.is_empty(),
            Err(_) => true,
        };
        Ok(has_storage || has_targets)
    }

    fn check(&self) -> Result<(), SyncError> {
        if self.backup_targets.len() > MAX_TARGETS {
            return Err(SyncError::InvalidData(Reason::text(format!(
                "备份里的云端备份目标超过 {MAX_TARGETS} 个"
            ))));
        }
        let mut seen = std::collections::BTreeSet::new();
        for target in &self.backup_targets {
            let ok = !target.id.is_empty()
                && target.id.len() <= 64
                && target
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
            if !ok || !seen.insert(target.id.as_str()) {
                return Err(SyncError::InvalidData(Reason::text(
                    "备份里的云端备份目标 ID 不合法".to_owned(),
                )));
            }
        }
        Ok(())
    }

    /// 原子替换本机同步账号设置。校验和密封全部成功后才开始事务；任何写入失败都保留原状态。
    /// “已和云端对上”标记、并发保护检测结果一并清掉，第一次同步会重新确认。
    /// 老备份没带自动同步或保留份数时，仍保留本机的这些设置。
    pub fn apply(&self, db: &Database, sealer: &dyn SecretSealer) -> Result<(), SyncError> {
        self.check()?;
        let mut writes = Vec::<(String, String)>::new();
        if let Some(config) = &self.storage {
            writes.push((service::STORAGE_KEY.to_owned(), seal_json(sealer, config)?));
        }
        if let (Some(StorageConfig::Onedrive(_)), Some(tokens)) =
            (&self.storage, &self.onedrive_tokens)
        {
            writes.push((TOKENS_KEY.to_owned(), seal_json(sealer, tokens)?));
        }
        if let Some(password) = &self.master_password {
            writes.push((
                service::MASTER_PASSWORD_KEY.to_owned(),
                service::sealed_setting_value(sealer, password.as_bytes())?,
            ));
        }
        if let Some(auto) = &self.auto {
            let mut auto = auto.clone();
            auto.interval_minutes = auto.interval_minutes.clamp(1, 24 * 60);
            let value = serde_json::to_string(&auto).map_err(|err| {
                SyncError::Internal(Reason::text(format!("设置序列化失败：{err}")))
            })?;
            writes.push((service::AUTO_KEY.to_owned(), value));
        }
        if let Some(keep) = &self.backup_keep {
            writes.push((SYNC_BACKUP_KEEP_KEY.to_owned(), keep.to_string()));
        }
        if !self.backup_targets.is_empty() {
            writes.push((
                backup_targets::TARGETS_KEY.to_owned(),
                seal_json(sealer, &self.backup_targets)?,
            ));
        }
        for target in &self.backup_targets {
            if matches!(target.config, StorageConfig::Onedrive(_)) {
                if let Some(tokens) = self.target_tokens.get(&target.id) {
                    writes.push((
                        backup_targets::token_key(&target.id),
                        seal_json(sealer, tokens)?,
                    ));
                }
            }
        }
        // 与 setting_set 使用同样的校验，但不在持有数据库锁时调用它。
        for (key, value) in &writes {
            super::settings::validate_local_value(key, value).map_err(|reason| {
                SyncError::InvalidData(Reason::text(format!("设置 {key} 的值不合格：{reason}")))
            })?;
        }
        backup_targets::with_targets_lock(|| {
            db.with_conn_as(|conn| -> Result<(), SyncError> {
                let tx = conn.transaction()?;
                for key in [
                    service::STORAGE_KEY,
                    service::CONCURRENCY_KEY,
                    service::READY_KEY,
                    TOKENS_KEY,
                    service::MASTER_PASSWORD_KEY,
                    backup_targets::TARGETS_KEY,
                ] {
                    tx.execute("DELETE FROM settings WHERE key = ?1", [key])?;
                }
                // 按字面前缀清理，连旧列表损坏或已遗失的目标令牌也不会残留。
                // 不能用 LIKE：前缀中的下划线会被当作通配符。
                tx.execute(
                    "DELETE FROM settings WHERE substr(key, 1, length(?1)) = ?1",
                    [backup_targets::TOKEN_KEY_PREFIX],
                )?;
                for (key, value) in &writes {
                    tx.execute(
                        "INSERT INTO settings (key, value) VALUES (?1, ?2) \
                         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                        rusqlite::params![key, value],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
        })
    }

    /// 备份 / 导入里带着这块时的统一处理：本机还没配置同步就直接写回；已经配置了就原样交给
    /// 调用方去问用户（默认不替换）。
    pub fn restore_or_offer(
        accounts: Option<Self>,
        db: &Database,
        sealer: &dyn SecretSealer,
    ) -> Result<AccountsRestore, SyncError> {
        let Some(accounts) = accounts else {
            return Ok(AccountsRestore::NotInBackup);
        };
        // 数据已经恢复好了：这里出错不能让整次恢复看起来失败，单独报告。
        let configured = match Self::local_is_configured(db, sealer) {
            Ok(configured) => configured,
            Err(err) => return Ok(AccountsRestore::Failed(err.to_string())),
        };
        if configured {
            return Ok(AccountsRestore::NeedsConfirmation(Box::new(accounts)));
        }
        Ok(match accounts.apply(db, sealer) {
            Ok(()) => AccountsRestore::Applied,
            Err(err) => AccountsRestore::Failed(err.to_string()),
        })
    }
}

fn seal_json<T: Serialize>(sealer: &dyn SecretSealer, value: &T) -> Result<String, SyncError> {
    let plain = Zeroizing::new(serde_json::to_vec(value).map_err(|err| {
        SyncError::Internal(Reason::text(format!("同步账号设置序列化失败：{err}")))
    })?);
    service::sealed_setting_value(sealer, &plain)
}

#[cfg(test)]
mod tests;
