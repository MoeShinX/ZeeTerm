//! 远端栏「还没连上 / 连接失败」之后的重新连接（LINUX-POLISH 第 1 条）。
//!
//! 现象：主机认证第一次失败（例如 ssh-agent 里没有密钥）→ 终端显示已断开，文件面板远端栏停在「还没连上」；
//! 修好认证后在终端里重连成功，远端栏仍然是「还没连上」，刷新、关掉再打开面板都没用，只有关掉标签重来。
//! 原因：首屏连接只在创建视图时跑一次，失败后没有任何入口再试；刷新又因为路径是空的直接什么也不做。
//!
//! **连上之后断线**（LINUX-FIX 第 24 条）同样走这里：视图转成 [`Phase::Disconnected`]（标题栏小点和文字
//! 显示「已断开」，旧目录还留在界面上），先停旧引擎、没完成的传输标为失败，再自动试一次（见 [`SftpDemo::mark_disconnected`]），
//! 之后和「连不上」一样靠下面的入口恢复。
//!
//! 现在这些入口会重新尝试，且都只试一次、不在后台反复重试：
//! - 终端窗格换了一条新连接（重连成功）：停靠面板每个节拍把窗格眼下持有的连接递过来
//!   （[`SftpDemo::offer_factory`]），视图处于失败状态、而且这条连接还没试过，就用它重试；
//! - 点远端栏的刷新：处于失败状态就重新连接（[`SftpDemo::retry_connect`]）；
//! - 关掉文件面板再打开：主窗口调 [`SftpDemo::retry_if_failed`]；
//! - 标签右键 / 面板标题栏的「重新连接」：[`SftpDemo::reconnect`]。

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use gpui_kit::*;

use zssh_core::sftp::transfer::{TransferId, TransferState};

use super::{Phase, SftpDemo, SftpMsg, TransferRow, load_first};
use crate::conn_pool::PoolItem as _;
use crate::i18n::t;
use crate::ssh_backend::{SshSessionFactory, runtime};

/// 检测到断线后自动重连的最短间隔：间隔内不再自动试（通道一连上就死的话不会一直循环），要重连靠手动入口。
const AUTO_RECONNECT_GAP: Duration = Duration::from_secs(60);

/// 连上之后连接被关了吗：`closed` 是视图手里那条连接现在是不是已经断了（`None` = 手里没有连接）。
pub(super) fn connection_lost(phase: Phase, fixture: bool, closed: Option<bool>) -> bool {
    phase == Phase::Ready && !fixture && closed == Some(true)
}

/// 这次断线要不要自动重连：上一次自动重连是 `AUTO_RECONNECT_GAP` 之前（或从没有过）才试。
pub(super) fn auto_reconnect_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|at| now.saturating_duration_since(at) >= AUTO_RECONNECT_GAP)
}

/// 断线时把没完成的传输行标成「失败（连接已断开）」，返回它们的任务号（交给旧引擎取消）。
/// 已经结束的行（完成 / 失败 / 取消 / 跳过）原样不动。
pub(super) fn interrupt_rows(
    rows: &mut [TransferRow],
    reason: &str,
    now: Instant,
) -> Vec<TransferId> {
    let mut interrupted = Vec::new();
    for row in rows {
        if !row.state.is_terminal() {
            row.state = TransferState::Failed(reason.to_owned());
            row.finished.get_or_insert(now);
            interrupted.push(row.id);
        }
    }
    interrupted
}

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
            self.phase.is_down(),
            factory.as_ref(),
            closed,
            self.attempted.as_ref(),
        ) {
            self.begin_connect(factory, cx);
        }
    }

    /// 视图处于连接失败状态时重新连接一次（点刷新、重新打开面板）。返回是否真的发起了重试。
    pub(super) fn retry_connect(&mut self, cx: &mut Context<Self>) -> bool {
        if self.fixture || !self.phase.is_down() {
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

    /// 连上之后发现断了（连接被关，或者远端操作报「通道已关闭」）：转成断开状态。
    ///
    /// 顺序：先停旧引擎（没完成的传输标为失败、可重试，不静默丢）→ 丢掉死通道 → 状态栏说明 →
    /// **只自动试一次**（间隔 [`AUTO_RECONNECT_GAP`]）：优先用终端递来的活连接，没有就让连接池按主机重拨
    /// （连接还活着只是通道死了，池里还是那条连接，等于在同一条连接上重开 SFTP 通道）。
    /// 自动试失败后停在断开 / 失败状态，靠刷新或「重新连接」。
    pub(super) fn mark_disconnected(&mut self, cx: &mut Context<Self>) {
        if self.fixture || self.phase != Phase::Ready {
            return;
        }
        self.phase = Phase::Disconnected;
        self.stop_engine_failing_transfers();
        self.channel = None;
        self.factory = None;
        self.remote.loading = false;
        self.warn(t(SftpMsg::DisconnectedStatus));
        let now = Instant::now();
        if auto_reconnect_due(self.last_auto_reconnect, now) {
            self.last_auto_reconnect = Some(now);
            self.retry_connect(cx);
        }
        cx.notify();
    }

    /// 停掉传输引擎：没完成的传输交给旧引擎取消，界面上的行标成「失败（连接已断开）」，之后可以重试
    /// （重连后按原来的来源 / 目标重新排，见 `resubmit_failed`）。同名文件提问作废，递归删除 / 压缩 /
    /// 改权限也一并叫停（它们拿着死通道）。
    fn stop_engine_failing_transfers(&mut self) {
        // 旧引擎的事件转发任务认代号：加一，它们的事件就不会落到以后新引擎的行上。
        self.engine_gen
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // 旧引擎的任务号以后不会再有行（新引擎另起任务号），暂存的早到报告一并作废。
        self.orphan_reports.clear();
        let interrupted = interrupt_rows(
            &mut self.transfers,
            t(SftpMsg::TransferInterrupted),
            Instant::now(),
        );
        if let Some(engine) = &self.engine {
            for id in interrupted {
                engine.cancel(id);
            }
        }
        if let Some(resolver) = self.resolver.take() {
            resolver.shutdown();
        }
        self.conflicts.clear();
        if let Some(job) = &self.deleting {
            job.cancel.cancel();
        }
        if let Some(busy) = &self.busy {
            busy.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(cancel) = &self.chmod_cancel {
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.engine = None;
        self.endpoints = None;
        self.provider = None;
        self.sync_install_state();
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
    use std::time::{Duration, Instant};

    use super::{Phase, auto_reconnect_due, auto_retry_due, connection_lost, interrupt_rows};
    use crate::sftp::{SftpEvent, TransferRow, load_first};
    use crate::ssh_backend::SshSessionFactory;
    use crate::test_sftp_server::{Options, TempRoot, spawn};
    use crate::test_ssh_server::connect_trusting;
    use zssh_core::sftp::SftpChannel;
    use zssh_core::sftp::transfer::{Direction, TransferState};

    fn row(id: u64, state: TransferState) -> TransferRow {
        let mut row = TransferRow::new(
            id,
            format!("file-{id}"),
            Direction::Download,
            None,
            format!("/remote/{id}"),
            format!("/local/{id}"),
        );
        row.state = state;
        row
    }

    #[test]
    fn unfinished_transfers_become_failed_and_finished_ones_stay() {
        let mut rows = vec![
            row(1, TransferState::Running),
            row(2, TransferState::Done),
            row(3, TransferState::Queued),
            row(4, TransferState::Paused),
            row(5, TransferState::WaitingReconnect),
            row(6, TransferState::Failed("old".to_owned())),
            row(7, TransferState::Cancelled),
        ];
        let at = Instant::now();
        let interrupted = interrupt_rows(&mut rows, "lost", at);
        assert_eq!(interrupted, vec![1, 3, 4, 5]);
        for index in [0, 2, 3, 4] {
            assert_eq!(rows[index].state, TransferState::Failed("lost".to_owned()));
            assert_eq!(rows[index].finished, Some(at));
        }
        // 已经结束的不改。
        assert_eq!(rows[1].state, TransferState::Done);
        assert_eq!(rows[5].state, TransferState::Failed("old".to_owned()));
        assert_eq!(rows[6].state, TransferState::Cancelled);
        // 再来一次没有可标的了。
        assert!(interrupt_rows(&mut rows, "lost", at).is_empty());
    }

    /// 真实的 SFTP 客户端连进程内 SFTP 服务器夹具：连接被关之后，远端操作返回的就是「断线」一类的错误
    /// （视图据此转成断开状态）；在新连接上重连能回到原来的目录，原目录没了就退回家目录。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_dropped_connection_is_detected_and_reconnecting_returns_to_the_same_directory() {
        let root = TempRoot::new("reconnect-root");
        root.mkdir("work");
        root.write("work/a.txt", b"hello");
        root.write("top.txt", b"top");
        let known_hosts = TempRoot::new("reconnect-kh");
        let addr = spawn(root.path().into(), 0, Options { posix_rename: true })
            .await
            .expect("起夹具服务器");

        // 连上，列出 /work。
        let connection = std::sync::Arc::new(connect_trusting(addr, known_hosts.path()).await);
        let channel = SftpChannel::open(&connection).await.expect("开通道");
        assert!(channel.read_dir("/work").await.is_ok());

        // 连接被关（对端断开 / keepalive 超时的效果）：操作失败，并且被认成「断线」。
        connection.disconnect().await.ok();
        let err = channel
            .read_dir("/work")
            .await
            .expect_err("连接关了，列目录必须失败");
        assert!(err.is_disconnect(), "要被认成断线才会转成断开状态：{err:?}");
        assert!(connection.is_closed());

        // 重连（新连接，起始目录是断线前的 /work）：回到 /work。
        let factory =
            SshSessionFactory::from_connection(connect_trusting(addr, known_hosts.path()).await);
        match load_first(0, Some(factory.clone()), Some("/work".to_owned())).await {
            SftpEvent::Ready { cwd, entries, .. } => {
                assert_eq!(cwd, "/work");
                assert!(entries.iter().any(|entry| entry.name == "a.txt"));
            }
            _ => panic!("重连应该回到原来的目录"),
        }
        // 原目录读不出来（比如断线期间被删了）：退回家目录，不失败。
        match load_first(0, Some(factory), Some("/gone".to_owned())).await {
            SftpEvent::Ready { cwd, entries, .. } => {
                assert_ne!(cwd, "/gone");
                assert!(entries.iter().any(|entry| entry.name == "top.txt"));
            }
            _ => panic!("原目录不在时应退回家目录"),
        }
    }

    #[test]
    fn only_a_connected_view_whose_connection_closed_counts_as_lost() {
        assert!(connection_lost(Phase::Ready, false, Some(true)));
        // 连接还活着、没有连接（已收尾）、夹具视图、还没连上的视图：都不算。
        assert!(!connection_lost(Phase::Ready, false, Some(false)));
        assert!(!connection_lost(Phase::Ready, false, None));
        assert!(!connection_lost(Phase::Ready, true, Some(true)));
        for phase in [Phase::Connecting, Phase::Failed, Phase::Disconnected] {
            assert!(!connection_lost(phase, false, Some(true)));
        }
    }

    #[test]
    fn a_lost_connection_is_retried_automatically_only_once_per_minute() {
        let start = Instant::now();
        assert!(auto_reconnect_due(None, start));
        assert!(!auto_reconnect_due(
            Some(start),
            start + Duration::from_secs(5)
        ));
        assert!(!auto_reconnect_due(
            Some(start),
            start + Duration::from_secs(59)
        ));
        assert!(auto_reconnect_due(
            Some(start),
            start + Duration::from_secs(60)
        ));
    }

    #[test]
    fn both_down_states_can_be_reconnected_but_a_live_view_cannot() {
        assert!(Phase::Failed.is_down());
        assert!(Phase::Disconnected.is_down());
        assert!(!Phase::Ready.is_down());
        assert!(!Phase::Connecting.is_down());
    }

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
