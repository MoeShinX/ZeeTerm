//! 命令历史面板（主窗口右侧侧栏，V2-U48）要问工作区的几件事：活动窗格属于哪个历史目标、
//! 那个桶里现在有什么、有没有新内容。
//!
//! 全部是只读：不改记录、不算一次访问，也不碰命令建议自己的检索。桶的键和采集用的是同一个
//! [`history::bucket_key`]，所以面板看到的永远是这个窗格正在往里记的那一桶。

use zssh_core::terminal_assist::history::{self, HistoryHit, HistoryScope};

use super::*;

impl TerminalWorkspace {
    /// 活动窗格的目标身份（`host:<编号>` / `ssh://用户@地址:端口`）；离线 / 空白窗格是空串。
    pub(crate) fn active_history_target(&self) -> String {
        self.pane().map(Pane::target_identity).unwrap_or_default()
    }

    /// 活动窗格在给定范围下用的历史桶；没有目标（不是主机会话）时没有桶。
    pub(crate) fn active_history_bucket(&self, scope: HistoryScope) -> Option<String> {
        history::bucket_key(&self.active_history_target(), scope).map(str::to_owned)
    }

    /// 这个桶里的全部命令（从新到旧）和此刻的内容修订，一次拿锁读完。
    pub(crate) fn history_snapshot(&self, bucket: &str) -> (u64, Vec<HistoryHit>) {
        let store = super::assist_ops::shared_history();
        let guard = history::lock(&store);
        (guard.revision(), guard.recent(bucket))
    }

    /// 只读修订号（面板的节拍用它判断要不要重读整桶）。
    pub(crate) fn history_revision(&self) -> u64 {
        history::lock(&super::assist_ops::shared_history()).revision()
    }

    /// 测试 / 调试驱动里：给活动窗格挂一个主机编号，让它有一个历史目标。
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn dev_set_host_id(&mut self, host_id: i64) {
        if let Some(pane) = self.pane_mut() {
            pane.host_id = Some(host_id);
        }
    }

    /// 测试里：把活动窗格摆成「没连上」（`push_test_*` 出来的窗格默认是本地回显的已连上状态）。
    #[cfg(test)]
    pub(crate) fn set_test_pane_disconnected(&mut self) {
        if let Some(pane) = self.pane_mut() {
            pane.state = PaneState::Disconnected;
        }
    }

    /// 测试 / 调试驱动里：往这个进程的共用历史里记一条，和窗格自己采集走同一个入口。
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn dev_record_history(bucket: &str, command: &str) {
        history::lock(&super::assist_ops::shared_history()).record(
            bucket,
            command,
            zssh_core::terminal_assist::Provenance::Typed,
        );
    }
}
