//! 终端断线自动重连在工作区一侧的节拍（V2-U44 B）。每个窗格自己的状态机在 [`crate::auto_reconnect`]，
//! 这里只做需要「全局视角」的事：设置开关、应用锁、同一台主机上别的窗格、发起重连。
//!
//! 没有后台任务也没有定时器：工作区本来就每 60 毫秒走一次节拍，等待中的倒计时只是窗格里记着的一个
//! 到期时刻，节拍里看一眼到点没有。所以标签 / 窗格被关掉、设置被关掉时，等着的自动重连随窗格一起没了
//! 或者在下一拍（最多 60 毫秒）取消，不会留下孤儿任务。重连本身走手动重连同一条路
//! （[`TerminalWorkspace::reconnect_pane_connection`]）：保留回滚、写重连标记、沿用连接池。

use super::*;
use crate::auto_reconnect::{Next, Tick};

impl TerminalWorkspace {
    /// 节拍：开始新的一轮、处理上一次尝试的失败、到点发起下一次、刷新遮罩上的倒计时。
    /// `locked`：应用锁着（锁着时暂停计时，解锁后接着来）。返回是否要重画。
    pub(super) fn auto_reconnect_tick(&mut self, now: Instant, locked: bool) -> bool {
        let enabled = self.settings.auto_reconnect;
        // 锁着的这段时间不计时：每拍把等待中的到期时刻往后推一拍的长度。
        let elapsed = self
            .auto_last_tick
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.auto_last_tick = Some(now);

        let mut changed = false;
        let mut fire: Vec<(usize, usize)> = Vec::new();
        // 这一拍里遇到「要人处理」的主机：同一台主机上别的窗格也不要再试，免得重复弹同样的提问。
        // 分两遍走：第一遍只开新一轮、收上一次尝试的失败；停掉这些主机上还在等的窗格之后，第二遍才
        // 计时、到点发起。要是一遍里边收失败边发起，同主机上恰好这一拍到点的窗格会先被 `tick` 切成
        // 「正在连」，停止时（只看「等待中」）就漏掉它，多连一次、可能再弹一次提问。
        let mut blocked_targets: Vec<String> = Vec::new();
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                if !enabled {
                    // 自动重连关了：等着的、正在连的都停下（进行中的那一次一并放弃）。
                    changed |= pane.auto_cancel();
                    pane.auto_trigger = false;
                    pane.auto_failure = None;
                    continue;
                }
                if std::mem::take(&mut pane.auto_trigger) && pane.state == PaneState::Disconnected {
                    pane.auto_begin(now);
                    changed = true;
                }
                if let Some(failure) = pane.auto_failure.take() {
                    changed = true;
                    let next = pane
                        .auto
                        .attempt_failed(now, failure.blocked, &failure.reason);
                    if failure.blocked {
                        blocked_targets.push(pane.target_identity());
                    }
                    // 不再试了（次数用尽或要人处理）：在回滚里留一行原因，之后手动重连。
                    if !matches!(next, Next::Retry { .. }) {
                        pane.pump_mut()
                            .feed_local(failure_notice(&failure.reason).as_bytes());
                        pane.refresh();
                    }
                }
            }
        }
        if !blocked_targets.is_empty() {
            for tab in self.tabs.items_mut() {
                for pane in &mut tab.panes {
                    if pane.auto.is_waiting() && blocked_targets.contains(&pane.target_identity()) {
                        pane.auto.stop_for_human();
                        changed = true;
                    }
                }
            }
        }
        if enabled {
            for (tab_index, tab) in self.tabs.items_mut().iter_mut().enumerate() {
                for (pane_index, pane) in tab.panes.iter_mut().enumerate() {
                    if locked {
                        pane.auto.pause_for(elapsed);
                    }
                    if let Tick::Fire { .. } = pane.auto.tick(now, locked) {
                        fire.push((tab_index, pane_index));
                    }
                }
            }
        }
        for (tab_index, pane_index) in fire {
            self.reconnect_pane_connection(tab_index, pane_index);
            changed = true;
        }
        // 遮罩上的倒计时每秒变一次、状态切换时重画；没变就不重画。
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                let view = pane.auto_view(now);
                if view != pane.auto_shown {
                    pane.auto_shown = view;
                    changed = true;
                }
            }
        }
        if changed {
            self.refresh();
        }
        changed
    }

    /// 遮罩上的「取消」：停止这一轮自动重连，遮罩回到手动状态（按 R / 点遮罩仍可重连）。
    pub(crate) fn cancel_auto_reconnect(&mut self, pane_id: PaneId, cx: &mut Context<Self>) {
        let mut changed = false;
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                if pane.id == pane_id {
                    changed |= pane.auto_cancel();
                }
            }
        }
        if changed {
            self.refresh();
            cx.notify();
        }
    }

    /// 调试 / 截图：把活动标签第 `pane_index` 个窗格摆成自动重连的某个阶段（见 `AutoReconnect::dev_force`），
    /// 不需要真的有一台会掉线的 SSH 服务器。窗格要是个会话窗格（主机库 / 快速连接）。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_force_auto(
        &mut self,
        pane_index: usize,
        kind: &str,
        attempt: u32,
        secs: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(pane) = self
            .tabs
            .active_mut()
            .and_then(|tab| tab.panes.get_mut(pane_index))
        else {
            return false;
        };
        if !pane.is_session() {
            return false;
        }
        pane.ssh_session = true;
        let reason = pane
            .disconnect_reason
            .clone()
            .unwrap_or_else(|| crate::i18n::t(ConnMsg::ReasonRemoteClosed).to_owned());
        if kind == "connecting" {
            pane.state = PaneState::Connecting;
        } else {
            pane.state = PaneState::Disconnected;
            pane.disconnect_reason = Some(reason.clone());
        }
        pane.auto
            .dev_force(kind, attempt, secs, Instant::now(), &reason);
        cx.notify();
        true
    }

    /// 设置里把自动重连关了：所有等着的、正在连的立刻取消（不等下一拍）。
    pub(super) fn cancel_all_auto_reconnects(&mut self) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.auto_cancel();
                pane.auto_trigger = false;
            }
        }
        self.refresh();
    }
}
