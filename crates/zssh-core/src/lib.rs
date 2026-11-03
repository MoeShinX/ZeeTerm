//! ZeeTerm 的核心库：纯 Rust，不依赖任何界面框架，可以单独测试。
//!
//! 含 SSH、SFTP、同步与备份、AI 智能体、X11 转发、本地存储和更新检查等业务逻辑。
//! 界面是 `native/` 里的 GPUI 应用，它只负责界面和转接，不放业务逻辑。

pub mod ai;
pub mod app_lock;
pub mod error;
pub mod external_url;
pub mod model;
pub mod pathnorm;
pub mod process_util;
pub mod sftp;
pub mod ssh;
pub mod store;
pub mod sync;
#[cfg(test)]
pub(crate) mod test_util;
pub mod updater;
pub mod x11;

pub use error::ZsshError;
pub use model::{
    parse_quick_target, AuthMethod, Group, GroupDraft, Host, HostDraft, Identity, IdentityDraft,
    IdentityKind, Proxy, ProxyDraft, ProxyKind, ProxyMode, QuickTarget,
};
pub use store::{Database, HostLogin, ProxyLogin};
