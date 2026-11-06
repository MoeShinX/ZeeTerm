//! 远端栏「还没连上 / 连接失败」之后的重新连接（LINUX-POLISH 第 1 条）。
//!
//! 现象：主机认证第一次失败（例如 ssh-agent 里没有密钥）→ 终端显示已断开，文件面板远端栏停在「还没连上」；
//! 修好认证后在终端里重连成功，远端栏仍然是「还没连上」，刷新、关掉再打开面板都没用，只有关掉标签重来。
//! 原因：首屏连接只在创建视图时跑一次，失败后没有任何入口再试；刷新又因为路径是空的直接什么也不做。
//!
//! 现在三个入口会重新尝试，且都只试一次、不在后台反复重试：
//! - 终端窗格换了一条新连接（重连成功）：停靠面板每个节拍把窗格眼下持有的连接递过来
//!   （[`SftpDemo::offer_factory`]），视图处于失败状态、而且这条连接还没试过，就用它重试；
//! - 点远端栏的刷新：处于失败状态就重新连接（[`SftpDemo::retry_connect`]）；
//! - 关掉文件面板再打开：主窗口调 [`SftpDemo::retry_if_failed`]。

use std::sync::{Arc, Weak};

use gpui_kit::*;

use super::{Phase, SftpDemo, SftpMsg, load_first};
use crate::conn_pool::PoolItem as _;
use crate::i18n::t;
use crate::ssh_backend::{SshSessionFactory, runtime};

/// 视图失败了，终端又递来一条活着的、**没用来试过**的连接：该自动重试一次。
///
/// 同一条连接试过一次失败了就不再自动试（例如服务器没开 SFTP：连接是好的、SFTP 通道开不了，
/// 每个节拍重试只会刷屏），要重试靠刷新或重新打开面板。
pub(super) fn auto_retry_due<T>(
    failed: bool,
    offered: Option<&Arc<T>>,
    offered_closed: bool,
    attempted: Option<&Weak<T>>,
) -> bool {
    if !failed || offered_closed {
        return false;
    }
    let Some(offered) = offered else {
        return false;
    };
    !attempted.is_some_and(|tried| Weak::ptr_eq(tried, &Arc::downgrade(offered)))
}

impl SftpDemo {
    /// 终端窗格眼下持有的连接（`None` = 窗格没连着）。停靠面板里的视图由主窗口每个节拍递一次：
    /// 记下来供手动重试用；视图失败着、而且这是一条没试过的活连接时，自动用它重连一次。
    pub fn offer_factory(
        &mut self,
        factory: Option<Arc<SshSessionFactory>>,
        cx: &mut Context<Self>,
    ) {
        self.offered = factory.as_ref().map(Arc::downgrade);
        if self.fixture {
            return;
        }
        let closed = factory.as_ref().is_some_and(|f| f.is_closed());
        if auto_retry_due(
            self.phase == Phase::Failed,
            factory.as_ref(),
            closed,
            self.attempted.as_ref(),
        ) {
            self.begin_connect(factory, cx);
        }
    }

    /// 视图处于连接失败状态时重新连接一次（点刷新、重新打开面板）。返回是否真的发起了重试。
    pub(super) fn retry_connect(&mut self, cx: &mut Context<Self>) -> bool {
        if self.fixture || self.phase != Phase::Failed {
            return false;
        }
        // 优先用终端窗格眼下那条活着的连接；没有就让连接池按主机重新拨号。
        let factory = self
            .offered
            .as_ref()
            .and_then(Weak::upgrade)
            .filter(|f| !f.is_closed());
        self.begin_connect(factory, cx);
        true
    }

    /// 关掉再打开面板：失败着就重新尝试一次，其余状态什么也不做。
    pub fn retry_if_failed(&mut self, cx: &mut Context<Self>) {
        self.retry_connect(cx);
    }

    /// 和创建视图时的首屏同一条路径：连接（或复用池里的连接）→ 开 SFTP 通道 → 列目录。
    /// 之前列过目录（比如连上之后连接断了）就回到原来的目录，读不出来再退回家目录。
    fn begin_connect(&mut self, factory: Option<Arc<SshSessionFactory>>, cx: &mut Context<Self>) {
        let runtime = match runtime() {
            Ok(runtime) => runtime,
            Err(err) => {
                self.warn(err);
                cx.notify();
                return;
            }
        };
        self.attempted = factory.as_ref().map(Arc::downgrade);
        self.phase = Phase::Connecting;
        self.say(format!(
            "{}{}",
            t(SftpMsg::ConnectingPrefix),
            self.host_label
        ));
        let start_remote = (!self.remote.path.is_empty()).then(|| self.remote.path.clone());
        let (sender, host_id) = (self.sender.clone(), self.host_id);
        runtime.spawn(async move {
            let _ = sender.send(load_first(host_id, factory, start_remote).await);
        });
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::auto_retry_due;

    #[test]
    fn a_new_live_connection_retries_a_failed_view_once() {
        let first = Arc::new(1u8);
        let second = Arc::new(2u8);
        // 第一次是自己拨号（没用终端的连接）失败了；终端重连成功递来一条新的：重试。
        assert!(auto_retry_due(true, Some(&first), false, None));
        // 用这条连接试过又失败了：同一条不再自动试。
        let tried = Arc::downgrade(&first);
        assert!(!auto_retry_due(true, Some(&first), false, Some(&tried)));
        // 终端再重连出另一条：又可以试一次。
        assert!(auto_retry_due(true, Some(&second), false, Some(&tried)));
    }

    #[test]
    fn nothing_is_retried_without_a_live_connection_or_while_not_failed() {
        let live = Arc::new(1u8);
        // 窗格没连着（下线、断开）：没有可用的连接。
        assert!(!auto_retry_due::<u8>(true, None, false, None));
        // 递来的连接已经断了。
        assert!(!auto_retry_due(true, Some(&live), true, None));
        // 视图没失败（连着 / 正在连）：不去动它。
        assert!(!auto_retry_due(false, Some(&live), false, None));
    }

    #[test]
    fn a_dropped_attempt_never_matches_a_new_connection() {
        let gone = {
            let old = Arc::new(1u8);
            Arc::downgrade(&old)
        };
        let fresh = Arc::new(1u8);
        assert!(auto_retry_due(true, Some(&fresh), false, Some(&gone)));
    }
}
