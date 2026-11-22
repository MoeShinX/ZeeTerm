//! 打开文件视图的入口按「设置 → 文件传输 → 文件面板 → 打开位置」走：终端右侧并排（停靠面板）、
//! 主窗口新标签页、独立的文件窗口。快捷键 Ctrl+Shift+F、标题栏「文件」、主机右键 / 终端标签右键的
//! 「打开 SFTP」都经过这里。
//!
//! 走哪条路是纯函数（[`route`]，有测试）；真正开视图复用现成的东西：并排 = 停靠面板，
//! 标签 = `open_sftp_tab`，窗口 = `file_window::adopt`。

use std::sync::Arc;

use ::gpui::Window;
use gpui_kit::*;

use super::MainWindow;
use super::docks::DockMsg;
use crate::i18n::t;
use crate::settings::SftpOpenIn;
use crate::ssh_backend::SshSessionFactory;

/// 这台主机的文件视图该怎么开。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenAction {
    /// 并排：开关当前终端标签的停靠面板（再按一次就收起）。
    ToggleDock,
    /// 在主窗口里新开一个文件标签。
    NewTab,
    /// 在独立的文件窗口里新开。
    NewWindow,
    /// 这台主机在主窗口里已经有文件标签：切过去，不重复开。
    FocusTab(u64),
    /// 这台主机在文件窗口里已经有标签：把窗口提到前面，不重复开。
    FocusWindow,
}

/// 按设置和「这台主机的文件视图现在在哪」决定怎么开。
/// `host_tab` = 主窗口里这台主机的文件标签；`in_file_window` = 文件窗口里有这台主机的标签。
/// 停靠面板里已经开着的视图不算「已有」：并排模式本来就是开关，标签 / 窗口模式会把它挪过去。
pub(crate) fn route(
    open_in: SftpOpenIn,
    host_tab: Option<u64>,
    in_file_window: bool,
) -> OpenAction {
    match open_in {
        SftpOpenIn::Dock => OpenAction::ToggleDock,
        SftpOpenIn::Tab => match (host_tab, in_file_window) {
            (Some(id), _) => OpenAction::FocusTab(id),
            (None, true) => OpenAction::FocusWindow,
            (None, false) => OpenAction::NewTab,
        },
        SftpOpenIn::Window => match (in_file_window, host_tab) {
            (true, _) => OpenAction::FocusWindow,
            (false, Some(id)) => OpenAction::FocusTab(id),
            (false, None) => OpenAction::NewWindow,
        },
    }
}

impl MainWindow {
    /// Ctrl+Shift+F、标题栏「文件」：用当前终端标签的主机，按「打开位置」打开（并排时是开关）。
    pub(crate) fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open_in = self.workspace.read(cx).settings().sftp_open_in;
        if open_in == SftpOpenIn::Dock {
            self.toggle_sftp_dock(window, cx);
            return;
        }
        let (origin, index) = {
            let ws = self.workspace.read(cx);
            (ws.active_origin(), ws.active_index())
        };
        match origin {
            Some((host_id, factory)) if self.caps_of(host_id).sftp => {
                self.open_files_for(host_id, factory, index, window, cx);
            }
            Some((host_id, _)) => {
                self.ssh_only_notice(Some(host_id), t(DockMsg::FilePanelFeature), window, cx);
            }
            None => self.notice(t(DockMsg::FileOnlyInTerminal), window, cx),
        }
    }

    /// 给一台主机打开文件视图（主机右键、终端标签右键的「打开 SFTP」）。`tab_index` 是触发它的终端标签，
    /// 并排模式要在那个标签上开面板；没给就找这台主机已经开着的终端标签。并排但这台主机没有终端标签
    /// （面板必须挂在终端标签上）时退成开一个文件标签。菜单入口是「打开」不是开关：面板已经开着就保持。
    pub(crate) fn open_files_for(
        &mut self,
        host_id: i64,
        factory: Option<Arc<SshSessionFactory>>,
        tab_index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open_in = self.workspace.read(cx).settings().sftp_open_in;
        let host_tab = self
            .docks
            .sftp_tabs
            .iter()
            .find(|tab| tab.host_id == host_id)
            .map(|tab| tab.id);
        let in_window = crate::file_window::has_host(cx, host_id);
        match route(open_in, host_tab, in_window) {
            OpenAction::ToggleDock => {
                let index = tab_index.or_else(|| self.terminal_tab_of(host_id, cx));
                match index {
                    Some(index) => {
                        self.select_tab(index, window, cx);
                        self.set_sftp_dock(true, window, cx);
                    }
                    None => self.open_sftp_tab(host_id, factory, window, cx),
                }
            }
            OpenAction::NewTab => self.open_sftp_tab(host_id, factory, window, cx),
            OpenAction::NewWindow => self.open_sftp_window(host_id, factory, window, cx),
            OpenAction::FocusTab(id) => self.select_sftp_tab(id, window, cx),
            OpenAction::FocusWindow => {
                crate::file_window::focus_host(cx, host_id);
            }
        }
    }

    /// 这台主机的终端标签（优先当前标签），没有就是 `None`。
    fn terminal_tab_of(&self, host_id: i64, cx: &App) -> Option<usize> {
        let ws = self.workspace.read(cx);
        let hosts = ws.tab_hosts();
        ws.active_index()
            .filter(|&index| {
                hosts
                    .get(index)
                    .is_some_and(|(_, host)| *host == Some(host_id))
            })
            .or_else(|| hosts.iter().position(|(_, host)| *host == Some(host_id)))
    }

    /// 直接开独立的文件窗口（已有的窗口里加一个标签）。这台主机的停靠面板里已经有视图就把它挪过去
    /// （连接和目录原样带走），当前标签的面板随之收起。
    fn open_sftp_window(
        &mut self,
        host_id: i64,
        factory: Option<Arc<SshSessionFactory>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.store.host(host_id).map(|host| host.name.clone()) else {
            return;
        };
        let view = match self.docks.sftp.remove(&host_id) {
            Some(view) => {
                view.update(cx, |view, cx| view.set_dock_layout(false, cx));
                if let Some(tab) = self.workspace.read(cx).active_tab_id() {
                    self.docks.set_sftp_open_for(tab, false);
                }
                view
            }
            None => cx
                .new(|cx| crate::sftp::SftpDemo::new_embedded(window, cx, host_id, factory, false)),
        };
        crate::file_window::adopt(cx, host_id, format!("{name} (SFTP)"), view);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{OpenAction, route};
    use crate::settings::SftpOpenIn;

    #[test]
    fn the_dock_setting_always_toggles_the_dock_whatever_is_already_open() {
        assert_eq!(route(SftpOpenIn::Dock, None, false), OpenAction::ToggleDock);
        assert_eq!(
            route(SftpOpenIn::Dock, Some(3), true),
            OpenAction::ToggleDock,
            "并排模式：有标签 / 窗口也只是开关面板（保持现在的切换行为）"
        );
    }

    #[test]
    fn the_tab_setting_opens_a_tab_once_and_then_focuses_it() {
        assert_eq!(route(SftpOpenIn::Tab, None, false), OpenAction::NewTab);
        assert_eq!(
            route(SftpOpenIn::Tab, Some(7), false),
            OpenAction::FocusTab(7)
        );
        assert_eq!(
            route(SftpOpenIn::Tab, None, true),
            OpenAction::FocusWindow,
            "这台主机已经在文件窗口里：不再另开标签"
        );
        assert_eq!(
            route(SftpOpenIn::Tab, Some(7), true),
            OpenAction::FocusTab(7)
        );
    }

    #[test]
    fn the_window_setting_opens_a_window_once_and_then_focuses_it() {
        assert_eq!(
            route(SftpOpenIn::Window, None, false),
            OpenAction::NewWindow
        );
        assert_eq!(
            route(SftpOpenIn::Window, None, true),
            OpenAction::FocusWindow
        );
        assert_eq!(
            route(SftpOpenIn::Window, Some(7), false),
            OpenAction::FocusTab(7),
            "主窗口里已经有这台主机的文件标签：聚焦它，不重复开"
        );
        assert_eq!(
            route(SftpOpenIn::Window, Some(7), true),
            OpenAction::FocusWindow
        );
    }
}
