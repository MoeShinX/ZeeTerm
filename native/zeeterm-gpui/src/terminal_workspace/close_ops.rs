//! 关闭前确认的工作区一侧（V2-U44 A）：按标签 / 窗格编号描述「要关什么」、算出范围里正在使用的连接、
//! 到点按编号真正关闭。要不要问、问什么文字在 `crate::close_confirm`；问的框由外壳画（主窗口的 `Modal`、
//! 独立终端窗口自己的确认层），工作区只在右键「关闭窗格」时把请求交给外壳（[`TerminalWorkspace::take_close_request`]）。
//!
//! 范围一律用编号：确认框开着时标签可能被关、挪位、会话可能自己断开，确认后只处理当初选中的那些标签，
//! 已经不在的跳过，绝不按下标去关别的。

use super::*;
use crate::close_confirm::{CloseRequest, CloseScope, CloseSummary, needs_confirm, scope_ids};

impl Pane {
    /// 这个窗格是不是「正在使用的连接」：主机库 / 快速连接的会话，已连上或正在连接。
    /// 已断开、空白、离线演示窗格关掉不丢任何东西，不算。
    fn in_use(&self) -> bool {
        self.can_disconnect()
    }

    /// AI 此刻正在这个窗格的终端里工作。
    fn ai_active(&self) -> bool {
        self.terminal_id
            .as_deref()
            .is_some_and(|id| crate::ai_terminals::terminals().is_ai_active(id))
    }
}

impl TerminalWorkspace {
    // ---- 描述「要关什么」 ----

    /// 关第 `index` 个标签。
    pub(crate) fn close_request_for_tab(&self, index: usize) -> Option<CloseRequest> {
        let tab = self.tabs.get(index)?;
        Some(CloseRequest::Tabs {
            ids: vec![tab.id],
            anchor: None,
        })
    }

    /// 以第 `index` 个标签为基准的「关闭其他 / 左侧 / 右侧 / 全部」；范围是空的返回 `None`。
    pub(crate) fn close_request_for_scope(
        &self,
        index: usize,
        scope: CloseScope,
    ) -> Option<CloseRequest> {
        let anchor = self.tabs.get(index)?.id;
        let ids = scope_ids(&self.tab_ids(), index, scope);
        (!ids.is_empty()).then_some(CloseRequest::Tabs {
            ids,
            anchor: Some(anchor),
        })
    }

    /// 关掉这个窗口里所有标签（独立终端窗口的关闭按钮）；没有标签返回 `None`。
    pub(crate) fn close_request_for_window(&self) -> Option<CloseRequest> {
        let ids = self.tab_ids();
        (!ids.is_empty()).then_some(CloseRequest::Tabs { ids, anchor: None })
    }

    /// Ctrl+Shift+W：分屏时关聚焦的窗格，否则关活动标签；没有标签返回 `None`。
    pub(crate) fn focused_close_request(&self) -> Option<CloseRequest> {
        let tab = self.tabs.active()?;
        if tab.is_split() {
            return Some(CloseRequest::Pane {
                tab_id: tab.id,
                pane_id: tab.active().id,
            });
        }
        Some(CloseRequest::Tabs {
            ids: vec![tab.id],
            anchor: None,
        })
    }

    /// 右键「关闭窗格」：活动标签里编号为 `pane_id` 的窗格；只剩一个窗格（没有「关闭窗格」可言）返回 `None`。
    fn pane_close_request(&self, pane_id: PaneId) -> Option<CloseRequest> {
        let tab = self.tabs.active()?;
        (tab.is_split() && tab.panes.iter().any(|pane| pane.id == pane_id)).then_some(
            CloseRequest::Pane {
                tab_id: tab.id,
                pane_id,
            },
        )
    }

    // ---- 范围里有什么 ----

    /// 这次关闭会影响的连接：按**现在**的实际状态数（范围里已经不在的标签不算）。
    pub(crate) fn close_summary(&self, request: &CloseRequest) -> CloseSummary {
        match request {
            CloseRequest::Tabs { ids, .. } => {
                let mut summary = CloseSummary::default();
                for id in ids {
                    let Some(tab) = self.tabs.items().iter().find(|tab| tab.id == *id) else {
                        continue;
                    };
                    summary.tab_total += 1;
                    let busy = tab.panes.iter().filter(|pane| pane.in_use()).count();
                    if busy > 0 {
                        summary.connections += busy;
                        summary.busy_tabs.push(tab.title.clone());
                    }
                    summary.ai |= tab
                        .panes
                        .iter()
                        .any(|pane| pane.in_use() && pane.ai_active());
                }
                summary
            }
            CloseRequest::Pane { tab_id, pane_id } => {
                let mut summary = CloseSummary {
                    pane: true,
                    ..CloseSummary::default()
                };
                let found = self
                    .tabs
                    .items()
                    .iter()
                    .find(|tab| tab.id == *tab_id)
                    .and_then(|tab| {
                        tab.panes
                            .iter()
                            .find(|pane| pane.id == *pane_id)
                            .map(|pane| (tab, pane))
                    });
                if let Some((tab, pane)) = found {
                    summary.tab_total = 1;
                    if pane.in_use() {
                        summary.connections = 1;
                        summary.busy_tabs.push(tab.title.clone());
                        summary.ai = pane.ai_active();
                    }
                }
                summary
            }
        }
    }

    /// 用户手动关这个范围要不要先问：设置开着并且范围里有正在使用的连接时返回确认框要写的内容，否则 `None`
    /// （直接关）。
    pub(crate) fn close_confirmation(&self, request: &CloseRequest) -> Option<CloseSummary> {
        let summary = self.close_summary(request);
        needs_confirm(self.settings.confirm_close_tab, &summary).then_some(summary)
    }

    // ---- 真正关 ----

    /// 按编号关闭：不问用户（要问的由外壳先问）。返回关掉了几个标签（窗格请求关掉了返回 1）。
    /// 范围里已经不在的标签 / 窗格跳过；之后新开的标签不在范围里，绝不会被带上。
    pub(crate) fn perform_close(
        &mut self,
        request: &CloseRequest,
        cx: &mut Context<Self>,
    ) -> usize {
        match request {
            CloseRequest::Tabs { ids, anchor } => self.close_tabs_by_ids(ids, *anchor, cx),
            CloseRequest::Pane { tab_id, pane_id } => {
                usize::from(self.close_pane_in_tab(*tab_id, *pane_id, cx))
            }
        }
    }

    /// 关掉编号在 `ids` 里的标签。活动标签被关掉时：有 `anchor`（批量关闭）且它还在，焦点落到它上面；
    /// 否则按标签列表一贯的规则（先回到左边那个）。
    fn close_tabs_by_ids(
        &mut self,
        ids: &[u64],
        anchor: Option<u64>,
        cx: &mut Context<Self>,
    ) -> usize {
        let removed = self
            .tabs
            .remove_matching(|tab| ids.contains(&tab.id), |tab| Some(tab.id) == anchor);
        let closed = removed.len();
        if closed == 0 {
            return 0;
        }
        for tab in removed {
            Self::close_tab_panes(tab);
        }
        self.reset_interaction();
        self.refresh();
        cx.notify();
        closed
    }

    /// 关掉编号为 `tab_id` 的标签里编号为 `pane_id` 的窗格：兄弟顶上、焦点规则同 Ctrl+Shift+W。
    /// 标签或窗格已经不在、或者标签只剩这一个窗格时什么也不做（不顺手把整个标签关掉）。
    fn close_pane_in_tab(&mut self, tab_id: u64, pane_id: PaneId, cx: &mut Context<Self>) -> bool {
        let Some(tab) = self
            .tabs
            .items_mut()
            .iter_mut()
            .find(|tab| tab.id == tab_id)
        else {
            return false;
        };
        let Some(index) = tab.panes.iter().position(|pane| pane.id == pane_id) else {
            return false;
        };
        let Some(mut pane) = tab.close_pane_at(index) else {
            return false;
        };
        pane.close();
        self.reset_interaction();
        self.refresh();
        cx.notify();
        true
    }

    // ---- 工作区里发起的关闭（右键菜单） ----

    /// 右键「关闭窗格」：要问的话把请求留给外壳（它在重画通知里 [`Self::take_close_request`]），
    /// 不用问的直接关。
    pub(super) fn request_close_pane(&mut self, pane_id: PaneId, cx: &mut Context<Self>) {
        let Some(request) = self.pane_close_request(pane_id) else {
            return;
        };
        if self.close_confirmation(&request).is_some() {
            self.close_request = Some(request);
            cx.notify();
        } else {
            self.perform_close(&request, cx);
        }
    }

    /// 外壳取走工作区发起的、等着先问用户的关闭请求（取一次就没有了）。
    pub(crate) fn take_close_request(&mut self) -> Option<CloseRequest> {
        self.close_request.take()
    }

    // ---- 给别的模块的窗口测试用的小口子 ----

    /// 把第 `index` 个标签的聚焦窗格标成「已断开」（模拟会话自己断了）。
    #[cfg(test)]
    pub(crate) fn dev_mark_disconnected_for_test(&mut self, index: usize) {
        if let Some(tab) = self.tabs.get_mut(index) {
            tab.active_mut().disconnect("test".to_owned(), "");
        }
    }

    /// 活动标签聚焦窗格的编号。
    #[cfg(test)]
    pub(crate) fn dev_focused_pane_id_for_test(&self) -> PaneId {
        self.tabs.active().map_or(0, |tab| tab.active().id)
    }

    /// 活动标签的窗格个数。
    #[cfg(test)]
    pub(crate) fn dev_pane_count_for_test(&self) -> usize {
        self.tabs.active().map_or(0, |tab| tab.panes.len())
    }

    /// 走右键「关闭窗格」那条路径。
    #[cfg(test)]
    pub(crate) fn request_close_pane_for_test(&mut self, pane_id: PaneId, cx: &mut Context<Self>) {
        self.request_close_pane(pane_id, cx);
    }
}
