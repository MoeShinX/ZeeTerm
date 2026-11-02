//! SSH-only 功能的置灰判定在主窗口上的入口：哪台主机 / 哪个标签能用 SFTP、AI 执行、服务器状态、
//! 端口转发、X11。规则本身在 `protocol_form::Caps`（有单测），这里只是把"主机 id → 能力"接到
//! 主机库和标签上，并统一提示文案（"仅 SSH 主机可用"）。

use super::MainWindow;
use super::protocol_form::{Caps, SSH_ONLY_HINT};
use gpui_kit::*;

impl MainWindow {
    /// 这台主机的能力；主机库里找不到按全部可用（各入口自己会报"找不到主机"）。
    pub(crate) fn caps_of(&self, host_id: i64) -> Caps {
        self.store
            .host(host_id)
            .map_or(Caps::ALL, |host| Caps::of(host.protocol.protocol))
    }

    /// 活动标签聚焦窗格的主机的能力；空白标签、快速连接（都是 SSH）按全部可用。
    pub(crate) fn active_caps(&self, cx: &App) -> Caps {
        self.workspace
            .read(cx)
            .active_host_id()
            .map_or(Caps::ALL, |id| self.caps_of(id))
    }

    /// 某个标签聚焦窗格的主机的能力。
    pub(crate) fn tab_caps(&self, index: usize, cx: &App) -> Caps {
        self.workspace
            .read(cx)
            .tab_host_id(index)
            .map_or(Caps::ALL, |id| self.caps_of(id))
    }

    /// 主机是 Telnet / 串口时，说一句"某功能仅 SSH 主机可用"（带上主机名和它的协议）。
    pub(crate) fn ssh_only_notice(
        &mut self,
        host_id: Option<i64>,
        what: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let detail = host_id
            .and_then(|id| self.store.host(id))
            .map(|host| {
                format!(
                    "（「{}」是 {} 主机）",
                    host.name,
                    host.protocol.protocol.label()
                )
            })
            .unwrap_or_default();
        self.notice(&format!("{what}{SSH_ONLY_HINT}{detail}"), window, cx);
    }
}
