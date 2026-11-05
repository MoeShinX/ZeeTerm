//! 同步相关的错误。
//!
//! 主密码错误、文件被改过、格式版本太新/太旧要能分开告诉用户：界面按这些
//! 变体给出不同提示（S-2）。错误文本里**不能带密码、密钥或同步包明文**。

use crate::error::{reason_keys, reason_params, ErrorWithCode, Reason, ZsshError};

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("同步包格式不对：{0}")]
    Format(Reason),

    /// 别的电脑上是更新的 Z-SSH。**不能尝试解析，更不能覆盖**，提示用户升级。
    #[error("同步包的格式版本太新（v{found}，本软件只支持到 v{supported}），请先升级 Z-SSH")]
    FormatTooNew { found: u32, supported: u32 },

    #[error("同步包的格式版本太旧（v{found}），这个版本不再支持")]
    FormatTooOld { found: u32 },

    #[error("同步包用了不支持的密钥派生算法：{0}（可能是更新的 Z-SSH 写的）")]
    UnsupportedKdf(String),

    #[error("主密码错误（或文件头被改过）")]
    WrongPassword,

    #[error("同步包被改过（认证失败），没有解密")]
    Tampered,

    #[error("同步数据不合法：{0}")]
    InvalidData(Reason),

    /// 装箱：`ZsshError` 里有好几个大字段，直接放进来会让所有 `Result` 都变大。
    #[error("数据库错误：{0}")]
    Database(Box<ZsshError>),

    #[error("读写失败：{0}")]
    Io(#[from] std::io::Error),

    #[error("存储出错：{0}")]
    Storage(Reason),

    /// 凭据被存储服务拒绝：重试没有用，要用户改配置（或手动同步）。
    #[error("存储出错：{0}")]
    AuthFailed(Reason),

    /// 存储服务限流（GitHub 的接口会这样）：要等一段时间，等到之前不要再试。
    #[error("{service} 限流，{}后重试", super::storage::wait_text(*retry_after_secs))]
    RateLimited {
        service: String,
        retry_after_secs: u64,
    },

    /// 合并和写入之间用户改了本地数据。本地没动、基线没存，下次同步会把这些
    /// 改动当成“相对旧基线的改动”正常合进去。
    #[error("同步期间本地有改动，稍后自动重新同步")]
    LocalChanged,

    #[error("还没有连接存储")]
    NotConfigured,

    #[error("内部错误：{0}")]
    Internal(Reason),
}

impl From<ZsshError> for SyncError {
    fn from(err: ZsshError) -> Self {
        SyncError::Database(Box::new(err))
    }
}

impl From<rusqlite::Error> for SyncError {
    fn from(err: rusqlite::Error) -> Self {
        SyncError::Database(Box::new(ZsshError::from(err)))
    }
}

impl From<super::storage::StorageError> for SyncError {
    fn from(err: super::storage::StorageError) -> Self {
        match err {
            super::storage::StorageError::RateLimited {
                service,
                retry_after_secs,
            } => SyncError::RateLimited {
                service,
                retry_after_secs,
            },
            super::storage::StorageError::AuthFailed(message) => SyncError::AuthFailed(message),
            other => SyncError::Storage(
                Reason::new(reason_keys::SYNC_STORAGE_CAUSE, other.to_string()).caused_by(&other),
            ),
        }
    }
}

/// 错误码与结构化参数：界面按它们出当前语言的整句（`Display` 的中文只留给日志和兜底）。
impl ErrorWithCode for SyncError {
    fn code(&self) -> &'static str {
        match self {
            SyncError::Format(_) => "sync.format",
            SyncError::FormatTooNew { .. } => "sync.format_too_new",
            SyncError::FormatTooOld { .. } => "sync.format_too_old",
            SyncError::UnsupportedKdf(_) => "sync.unsupported_kdf",
            SyncError::WrongPassword => "sync.wrong_password",
            SyncError::Tampered => "sync.tampered",
            SyncError::InvalidData(_) => "sync.invalid_data",
            SyncError::Database(_) => "sync.database",
            SyncError::Io(_) => "sync.io",
            SyncError::Storage(_) => "sync.storage",
            SyncError::AuthFailed(_) => "sync.auth_failed",
            SyncError::RateLimited { .. } => "sync.rate_limited",
            SyncError::LocalChanged => "sync.local_changed",
            SyncError::NotConfigured => "sync.not_configured",
            SyncError::Internal(_) => "sync.internal",
        }
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        match self {
            SyncError::Format(reason)
            | SyncError::InvalidData(reason)
            | SyncError::Storage(reason)
            | SyncError::AuthFailed(reason)
            | SyncError::Internal(reason) => reason_params(reason),
            SyncError::FormatTooNew { found, supported } => {
                vec![
                    ("found", found.to_string()),
                    ("supported", supported.to_string()),
                ]
            }
            SyncError::FormatTooOld { found } => vec![("found", found.to_string())],
            SyncError::UnsupportedKdf(kdf) => vec![("kdf", kdf.clone())],
            SyncError::Io(err) => vec![("detail", err.to_string())],
            SyncError::RateLimited {
                service,
                retry_after_secs,
            } => vec![
                ("service", service.clone()),
                ("secs", retry_after_secs.to_string()),
            ],
            SyncError::Database(_)
            | SyncError::WrongPassword
            | SyncError::Tampered
            | SyncError::LocalChanged
            | SyncError::NotConfigured => Vec::new(),
        }
    }

    fn causes(&self) -> Vec<(&'static str, &dyn ErrorWithCode)> {
        match self {
            SyncError::Format(reason)
            | SyncError::InvalidData(reason)
            | SyncError::Storage(reason)
            | SyncError::AuthFailed(reason)
            | SyncError::Internal(reason) => reason.causes(),
            SyncError::Database(inner) => vec![("detail", inner.as_ref() as &dyn ErrorWithCode)],
            _ => Vec::new(),
        }
    }
}
