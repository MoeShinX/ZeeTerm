//! 左侧边栏的单栏文件列表（V2-U46）：「设置 → 文件传输 → 文件面板 → 停靠位置」选「左侧边栏（单栏）」时，
//! 文件面板不再并排在终端右边，而是放进左侧边栏的「文件」页：只有远端一栏，下面接传输列表，边看终端边用。
//!
//! 要点（卡里的约束）：
//! - 右侧模式（默认）下这里的一切都不出现：侧边栏没有「主机 / 文件」切换，入口行为和以前一模一样；
//! - 「文件」页画的就是右侧停靠用的那个按主机建的 `SftpDemo` 实体（`DockState::sftp`），不另开一套连接、
//!   传输和跟随状态；换停靠位置只是换了画它的地方；
//! - 强制只显示远端一栏是视图层的覆盖（`sync_docks` 每拍给停靠实体推 `local_hidden = true`），不写
//!   `sftp_show_local`；换回右侧后下一拍按设置恢复；
//! - 跟随终端目录、轮询、切换停靠位置的收尾规则是纯函数（[`entry_plan`]、[`side_switch`]、[`follow_polls`]），有测试。

use ::gpui::SharedString;
use gpui_kit::component::*;
use gpui_kit::*;

use super::MainWindow;
use super::docks::{DockMsg, dock_icon_button};
use super::nav::MainView;
use super::widgets::segmented_bar;
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::settings::SftpDockSide;
use crate::typography::{Role, Typo as _};

/// 侧边栏现在显示哪一页（只有左侧模式下才有「文件」页；不持久化，每次启动是「主机」）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SidePage {
    #[default]
    Hosts,
    Files,
}

/// 本文件的界面文案。主机名、协议名来自用户数据，不进这张表。
#[derive(Clone, Copy, Debug)]
pub(super) enum SidebarFilesMsg {
    TabHosts,
    TabFiles,
    NoSession,
}

impl Msg for SidebarFilesMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::TabHosts => "主机",
            Self::TabFiles => "文件",
            Self::NoSession => "没有可用的 SSH 终端。切到一个 SSH 终端标签后，这里会显示它的文件。",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::TabHosts => "Hosts",
            Self::TabFiles => "Files",
            Self::NoSession => {
                "No SSH terminal to show. Switch to an SSH terminal tab and its files will appear here."
            }
        }
    }
}

/// 左侧模式下，打开文件面板的入口（标题栏文件图标、Ctrl+Shift+F、`toggle_sftp_dock`）要做什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum EntryPlan {
    /// 切到「文件」页；侧边栏折叠着就先展开。
    ShowFiles { expand: bool },
    /// 切回「主机」页。
    ShowHosts,
}

/// `toggle` 为真（图标 / 快捷键）：已经停在可见的「文件」页再按一次切回「主机」页；其余都是显示「文件」页。
/// `toggle` 为假（`set_sftp_dock(true)` 这种「打开」语义）：永远是显示「文件」页，不会反过来关掉。
pub(super) fn entry_plan(page: SidePage, collapsed: bool, toggle: bool) -> EntryPlan {
    if toggle && page == SidePage::Files && !collapsed {
        EntryPlan::ShowHosts
    } else {
        EntryPlan::ShowFiles { expand: collapsed }
    }
}

/// 设置里换了停靠位置之后主窗口要做的事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SideSwitch {
    /// 关掉所有标签的右侧文件停靠（它们的视图不删，只是不再画在右侧）。
    pub close_right_docks: bool,
    /// 侧边栏之后停在哪一页。
    pub page: SidePage,
    /// 侧边栏折叠着就展开（切到「文件」页时用户要看到它）。
    pub expand: bool,
    /// 给当前终端标签打开右侧文件停靠。
    pub open_right_for_active: bool,
}

/// 右侧 → 左侧：右侧停靠（当前标签的）开着就关掉、侧边栏切到「文件」页；没开着就什么都不开。
/// 左侧 → 右侧：侧边栏回到「主机」页；原来停在「文件」页的话，打开右侧停靠。
pub(super) fn side_switch(
    from: SftpDockSide,
    to: SftpDockSide,
    right_open_for_active: bool,
    page: SidePage,
) -> SideSwitch {
    match (from, to) {
        (SftpDockSide::Right, SftpDockSide::Sidebar) => SideSwitch {
            close_right_docks: true,
            page: if right_open_for_active {
                SidePage::Files
            } else {
                page
            },
            expand: right_open_for_active,
            open_right_for_active: false,
        },
        (SftpDockSide::Sidebar, SftpDockSide::Right) => SideSwitch {
            close_right_docks: false,
            page: SidePage::Hosts,
            expand: false,
            open_right_for_active: page == SidePage::Files,
        },
        _ => SideSwitch {
            close_right_docks: false,
            page,
            expand: false,
            open_right_for_active: false,
        },
    }
}

/// 要不要轮询终端目录（「跟随终端目录」）：右侧模式沿用原来的条件（当前标签的右侧停靠开着），
/// 左侧模式是「文件」页正显示着当前终端标签。
pub(super) fn follow_polls(side: SftpDockSide, right_dock_shown: bool, sidebar_live: bool) -> bool {
    match side {
        SftpDockSide::Right => right_dock_shown,
        SftpDockSide::Sidebar => sidebar_live,
    }
}

impl MainWindow {
    /// 现在的停靠位置设置。
    pub(super) fn dock_side(&self, cx: &::gpui::App) -> SftpDockSide {
        self.workspace.read(cx).settings().sftp_dock_side
    }

    /// 左侧模式（侧边栏有「主机 / 文件」切换）。
    pub(super) fn sidebar_mode(&self, cx: &::gpui::App) -> bool {
        self.dock_side(cx) == SftpDockSide::Sidebar
    }

    /// 「文件」页现在摆在用户眼前：左侧模式、停在「文件」页、侧边栏展开着、这一页有侧边栏。
    pub(super) fn sidebar_files_visible(&self, cx: &::gpui::App) -> bool {
        self.sidebar_mode(cx)
            && self.docks.side_page == SidePage::Files
            && !self.tree.collapsed
            && self.nav.view.has_sidebar()
    }

    /// 在「文件」页上显示着当前终端标签的文件列表：可见，并且终端页上有当前标签（不是文件 / 编辑器标签）。
    /// 建视图和跟随终端目录的轮询都以它为准。
    pub(super) fn sidebar_files_live(&self, cx: &::gpui::App) -> bool {
        self.sidebar_files_visible(cx) && self.docks_available(cx)
    }

    /// 标题栏文件图标的高亮：右侧模式是当前终端标签的右侧停靠开着，左侧模式是「文件」页可见。
    pub(super) fn files_entry_active(&self, cx: &::gpui::App) -> bool {
        if self.sidebar_mode(cx) {
            self.sidebar_files_visible(cx)
        } else {
            self.docks
                .sftp_open_for(self.workspace.read(cx).active_tab_id())
                && self.nav.view == MainView::Terminal
        }
    }

    /// 要不要轮询终端目录（见 [`follow_polls`]）。
    pub(super) fn follow_polls_now(&self, cx: &::gpui::App) -> bool {
        follow_polls(
            self.dock_side(cx),
            self.docks.sftp_shown(
                self.docks_available(cx),
                self.workspace.read(cx).active_tab_id(),
            ),
            self.sidebar_files_live(cx),
        )
    }

    /// 切到「文件」页（侧边栏折叠着先展开）。视图当场补建，不等下一个节拍，免得先闪一下「正在打开」。
    pub(super) fn show_sidebar_files(
        &mut self,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.tree.collapsed {
            self.tree.collapsed = false;
            self.save_layout();
        }
        self.docks.side_page = SidePage::Files;
        self.sync_docks(window, cx);
        // 上一次连接失败后又切回来：再试一次（同右侧停靠重新打开时的做法）。
        let host = self.workspace.read(cx).active_host_id();
        if let Some(view) = host.and_then(|host| self.docks.sftp.get(&host).cloned()) {
            view.update(cx, |view, cx| view.retry_if_failed(cx));
        }
        self.enforce_dock_layout(window, cx);
        cx.notify();
    }

    /// 切回「主机」页。
    pub(super) fn show_sidebar_hosts(&mut self, cx: &mut Context<Self>) {
        if self.docks.side_page != SidePage::Hosts {
            self.docks.side_page = SidePage::Hosts;
            cx.notify();
        }
    }

    /// 左侧模式下的入口（见 [`entry_plan`]）。返回 `false` 表示这一页没有侧边栏（设置页），由调用方提示。
    pub(super) fn sidebar_entry(
        &mut self,
        toggle: bool,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.nav.view.has_sidebar() {
            return false;
        }
        match entry_plan(self.docks.side_page, self.tree.collapsed, toggle) {
            EntryPlan::ShowFiles { .. } => self.show_sidebar_files(window, cx),
            EntryPlan::ShowHosts => self.show_sidebar_hosts(cx),
        }
        true
    }

    /// 设置里换了停靠位置：每个节拍比一次。第一次只记下现在的值（启动时不触发切换）。
    /// 实体一个都不动，只关 / 开右侧停靠的标记、换侧边栏的页，所以进行中的传输不会中断。
    pub(super) fn on_dock_side_changed(&mut self, cx: &mut Context<Self>) {
        let now = self.dock_side(cx);
        let Some(before) = self.docks.dock_side_seen.replace(now) else {
            return;
        };
        if before == now {
            return;
        }
        let active = self.workspace.read(cx).active_tab_id();
        let plan = side_switch(
            before,
            now,
            self.docks.sftp_open_for(active),
            self.docks.side_page,
        );
        if plan.close_right_docks {
            self.docks.close_all_sftp_docks();
        }
        self.docks.side_page = plan.page;
        if plan.expand && self.tree.collapsed {
            self.tree.collapsed = false;
            self.save_layout();
        }
        if plan.open_right_for_active
            && let Some(tab) = active
            && self.active_caps(cx).sftp
        {
            self.docks.set_sftp_open_for(tab, true);
        }
        cx.notify();
    }

    /// 侧边栏顶部的「主机 / 文件」两段切换（只在左侧模式下画）。
    pub(super) fn render_sidebar_switch(
        &self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> ::gpui::AnyElement {
        let entity = cx.entity();
        let page = self.docks.side_page;
        segmented_bar(
            "sidebar-page",
            vec![
                (
                    t(SidebarFilesMsg::TabHosts).to_owned(),
                    page == SidePage::Hosts,
                    None,
                ),
                (
                    t(SidebarFilesMsg::TabFiles).to_owned(),
                    page == SidePage::Files,
                    None,
                ),
            ],
            tokens,
            move |index, window, cx| {
                entity.update(cx, |this, cx| {
                    if index == 0 {
                        this.show_sidebar_hosts(cx);
                    } else {
                        this.show_sidebar_files(window, cx);
                    }
                });
            },
        )
        .into_any_element()
    }

    /// 「文件」页的内容：当前终端标签那台主机的文件视图（右侧停靠用的同一个实体，强制只显示远端一栏），
    /// 不是 SSH 会话 / 没有标签时是一句说明。
    pub(super) fn render_sidebar_files(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> ::gpui::AnyElement {
        let live = self.sidebar_files_live(cx);
        let host_id = live
            .then(|| self.workspace.read(cx).active_host_id())
            .flatten();
        let notice = |text: SharedString| {
            div()
                .p_4()
                .text_sm()
                .text_color(tokens.text_muted)
                .child(text)
                .into_any_element()
        };
        let non_ssh = host_id
            .and_then(|id| self.store.host(id))
            .filter(|host| !host.protocol.supports_sftp())
            .map(|host| crate::ops_text::protocol_label(host.protocol.protocol));
        let view = host_id.and_then(|id| self.docks.sftp.get(&id).cloned());
        let body = match (live, host_id, view.clone()) {
            (false, _, _) => notice(t(SidebarFilesMsg::NoSession).into()),
            _ if non_ssh.is_some() => notice(
                format!(
                    "{}{}{}",
                    t(DockMsg::NonSshPrefix),
                    non_ssh.unwrap_or_default(),
                    t(DockMsg::NonSshSuffix)
                )
                .into(),
            ),
            (true, None, _) => notice(t(DockMsg::NoHostRecord).into()),
            (true, Some(_), Some(view)) => view.into_any_element(),
            (true, Some(_), None) => notice(t(DockMsg::OpeningSftp).into()),
        };

        // 标题行：主机名 + 重新连接（连不上 / 断了才有）+ 「在新标签中打开」。只在有视图时画。
        let header = host_id.zip(view).map(|(host_id, view)| {
            let entity = cx.entity();
            let label: SharedString = self
                .store
                .host(host_id)
                .map(|host| host.name.clone())
                .unwrap_or_default()
                .into();
            let mut actions: Vec<::gpui::AnyElement> = Vec::new();
            if view.read(cx).is_down() {
                let view = view.clone();
                actions.push(
                    dock_icon_button(
                        "sidebar-sftp-reconnect",
                        Icon::new(Ico::RotateCcw),
                        t(DockMsg::Reconnect),
                        tokens,
                    )
                    .on_click(move |_, _window, cx| {
                        view.update(cx, |view, cx| {
                            view.reconnect(cx);
                        });
                    })
                    .into_any_element(),
                );
            }
            actions.push(
                dock_icon_button(
                    "sidebar-sftp-undock",
                    Icon::new(IconName::ExternalLink),
                    t(DockMsg::OpenInNewTab),
                    tokens,
                )
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.undock_sftp(host_id, window, cx));
                })
                .into_any_element(),
            );
            h_flex()
                .flex_none()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(tokens.divider)
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(Ico::FolderTree)
                                .size_4()
                                .flex_none()
                                .text_color(tokens.accent),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .typo(Role::ItemName, tokens)
                                .child(label),
                        ),
                )
                .child(h_flex().flex_none().items_center().children(actions))
        });

        v_flex()
            .id("sidebar-files")
            .flex_1()
            .min_h_0()
            .w_full()
            .children(header)
            .child(div().flex_1().min_h_0().w_full().child(body))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{EntryPlan, SidePage, SideSwitch, entry_plan, follow_polls, side_switch};
    use crate::settings::SftpDockSide::{Right, Sidebar};

    #[test]
    fn the_entry_shows_the_files_page_and_pressing_again_goes_back_to_the_hosts_page() {
        // 主机页：显示文件页，侧边栏折叠着就展开。
        assert_eq!(
            entry_plan(SidePage::Hosts, false, true),
            EntryPlan::ShowFiles { expand: false }
        );
        assert_eq!(
            entry_plan(SidePage::Hosts, true, true),
            EntryPlan::ShowFiles { expand: true }
        );
        // 文件页开着再按一次：回主机页。
        assert_eq!(
            entry_plan(SidePage::Files, false, true),
            EntryPlan::ShowHosts
        );
        // 文件页但侧边栏折叠着（看不见）：先展开，不是切回去。
        assert_eq!(
            entry_plan(SidePage::Files, true, true),
            EntryPlan::ShowFiles { expand: true }
        );
    }

    #[test]
    fn the_open_semantics_never_close_the_page() {
        // `set_sftp_dock(true)`（主机右键「打开 SFTP」等）：已经在文件页也还是显示文件页。
        assert_eq!(
            entry_plan(SidePage::Files, false, false),
            EntryPlan::ShowFiles { expand: false }
        );
        assert_eq!(
            entry_plan(SidePage::Hosts, true, false),
            EntryPlan::ShowFiles { expand: true }
        );
    }

    #[test]
    fn switching_from_right_to_the_sidebar_moves_an_open_right_dock_into_the_files_page() {
        assert_eq!(
            side_switch(Right, Sidebar, true, SidePage::Hosts),
            SideSwitch {
                close_right_docks: true,
                page: SidePage::Files,
                expand: true,
                open_right_for_active: false,
            }
        );
        // 右侧停靠没开着：关标记（没有可关的）、页不变、不展开。
        assert_eq!(
            side_switch(Right, Sidebar, false, SidePage::Hosts),
            SideSwitch {
                close_right_docks: true,
                page: SidePage::Hosts,
                expand: false,
                open_right_for_active: false,
            }
        );
    }

    #[test]
    fn switching_back_to_the_right_reopens_the_dock_only_if_the_files_page_was_showing() {
        assert_eq!(
            side_switch(Sidebar, Right, false, SidePage::Files),
            SideSwitch {
                close_right_docks: false,
                page: SidePage::Hosts,
                expand: false,
                open_right_for_active: true,
            }
        );
        assert_eq!(
            side_switch(Sidebar, Right, false, SidePage::Hosts),
            SideSwitch {
                close_right_docks: false,
                page: SidePage::Hosts,
                expand: false,
                open_right_for_active: false,
            }
        );
    }

    #[test]
    fn an_unchanged_side_does_nothing() {
        for side in [Right, Sidebar] {
            for page in [SidePage::Hosts, SidePage::Files] {
                for open in [false, true] {
                    assert_eq!(
                        side_switch(side, side, open, page),
                        SideSwitch {
                            close_right_docks: false,
                            page,
                            expand: false,
                            open_right_for_active: false,
                        }
                    );
                }
            }
        }
    }

    #[test]
    fn follow_polls_in_the_right_mode_keep_the_old_condition_and_the_sidebar_only_counts_in_the_left_mode()
     {
        // 右侧模式：只看右侧停靠，侧边栏文件页的状态不相干（和改动前一样）。
        for sidebar_live in [false, true] {
            assert!(follow_polls(Right, true, sidebar_live));
            assert!(!follow_polls(Right, false, sidebar_live));
        }
        // 左侧模式：只看文件页是否正显示当前标签；右侧停靠标记不相干（左侧模式下右侧不画）。
        for right_shown in [false, true] {
            assert!(follow_polls(Sidebar, right_shown, true));
            assert!(!follow_polls(Sidebar, right_shown, false));
        }
    }
}
