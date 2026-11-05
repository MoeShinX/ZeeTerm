//! 顶部终端标签的右键菜单：有哪些项、什么次序、什么时候置灰（数据驱动，和主机右键菜单
//! `vault_menus::host_menu_items` 同一个做法），画的时候只按 [`tab_menu`] 返回的表走。

use ::gpui::ClickEvent;
use gpui_kit::component::menu::PopupMenu;
use gpui_kit::*;

use super::protocol_form::{Caps, SSH_ONLY_HINT};
use super::{CloseScope, MainWindow};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;

/// 本文件（终端标签右键菜单）的界面文案。SFTP 是协议名，两个语言下都写 `SFTP`。
#[derive(Clone, Copy, Debug)]
enum TabMsg {
    DuplicateSession,
    DuplicateSerial,
    CopyHostName,
    CopyAddress,
    Sftp,
    Reconnect,
    Disconnect,
    SendBreak,
    Close,
    CloseLeft,
    CloseRight,
    CloseOthers,
    CloseAll,
    SshOnlyHint,
}

impl Msg for TabMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::DuplicateSession => "复制会话",
            Self::DuplicateSerial => SERIAL_DUPLICATE_LABEL,
            Self::CopyHostName => "复制主机名",
            Self::CopyAddress => "复制地址",
            Self::Sftp => "SFTP",
            Self::Reconnect => "重新连接",
            Self::Disconnect => "断开连接",
            Self::SendBreak => "发送 Break",
            Self::Close => "关闭",
            Self::CloseLeft => "关闭左侧",
            Self::CloseRight => "关闭右侧",
            Self::CloseOthers => "关闭其他",
            Self::CloseAll => "关闭全部",
            Self::SshOnlyHint => SSH_ONLY_HINT,
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::DuplicateSession => "Duplicate Session",
            Self::DuplicateSerial => "Duplicate Session (only one serial session is allowed)",
            Self::CopyHostName => "Copy Host Name",
            Self::CopyAddress => "Copy Address",
            Self::Sftp => "SFTP",
            Self::Reconnect => "Reconnect",
            Self::Disconnect => "Disconnect",
            Self::SendBreak => "Send Break",
            Self::Close => "Close",
            Self::CloseLeft => "Close Left",
            Self::CloseRight => "Close Right",
            Self::CloseOthers => "Close Others",
            Self::CloseAll => "Close All",
            Self::SshOnlyHint => "Only available for SSH hosts",
        }
    }
}

/// 菜单里的一项（从上到下的次序就是 [`tab_menu`] 返回的次序）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabMenuItem {
    DuplicateSession,
    CopyHostName,
    CopyAddress,
    Sftp,
    Reconnect,
    Disconnect,
    SendBreak,
    Close,
    CloseLeft,
    CloseRight,
    CloseOthers,
    CloseAll,
    Separator,
}

/// 画一个标签的右键菜单需要的全部状态（菜单打开时才生成，所以先在这里定下来）。
#[derive(Clone, Copy, Debug)]
pub struct TabMenuSpec {
    pub index: usize,
    pub tab_count: usize,
    /// 这个标签连着一台有主机记录的主机（快速连接、空白标签没有）。
    pub host_record: bool,
    /// 标签有主机，或者是快速连接（有 `user@host:port` 可复制）。
    pub has_address: bool,
    /// 标签有主机（和 `host_record` 不同：主机记录被删了标签还在）。
    pub has_host: bool,
    pub can_reconnect: bool,
    pub can_disconnect: bool,
    pub caps: Caps,
}

/// 菜单上的一行：写什么字、能不能点、置灰时悬停说什么。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabMenuEntry {
    pub item: TabMenuItem,
    pub label: &'static str,
    pub enabled: bool,
    pub hint: Option<&'static str>,
}

impl TabMenuEntry {
    fn new(item: TabMenuItem, label: &'static str, enabled: bool) -> Self {
        Self {
            item,
            label,
            enabled,
            hint: None,
        }
    }
}

/// 串口同一时间只能被一个标签打开，所以不能复制。
pub const SERIAL_DUPLICATE_LABEL: &str = "复制会话（串口只能开一个）";

/// 这个标签的右键菜单：
/// 复制会话 | 复制主机名、复制地址、SFTP、重新连接、断开连接、（串口）发送 Break |
/// 关闭、关闭左侧、关闭右侧、关闭其他、关闭全部。
pub fn tab_menu(spec: &TabMenuSpec) -> Vec<TabMenuEntry> {
    use TabMenuItem::*;
    let serial = spec.caps.send_break;
    let mut entries = vec![
        TabMenuEntry::new(
            DuplicateSession,
            if serial {
                t(TabMsg::DuplicateSerial)
            } else {
                t(TabMsg::DuplicateSession)
            },
            !serial,
        ),
        TabMenuEntry::new(Separator, "", false),
        TabMenuEntry::new(CopyHostName, t(TabMsg::CopyHostName), spec.host_record),
        TabMenuEntry::new(CopyAddress, t(TabMsg::CopyAddress), spec.has_address),
    ];
    // Telnet / 串口标签：SFTP 置灰并说明原因；没有主机的标签只是置灰。
    let mut sftp = TabMenuEntry::new(Sftp, t(TabMsg::Sftp), spec.has_host && spec.caps.sftp);
    if spec.has_host && !spec.caps.sftp {
        sftp.hint = Some(t(TabMsg::SshOnlyHint));
    }
    entries.push(sftp);
    entries.push(TabMenuEntry::new(
        Reconnect,
        t(TabMsg::Reconnect),
        spec.can_reconnect,
    ));
    entries.push(TabMenuEntry::new(
        Disconnect,
        t(TabMsg::Disconnect),
        spec.can_disconnect,
    ));
    if serial {
        entries.push(TabMenuEntry::new(
            SendBreak,
            t(TabMsg::SendBreak),
            spec.can_disconnect,
        ));
    }
    entries.extend([
        TabMenuEntry::new(Separator, "", false),
        TabMenuEntry::new(Close, t(TabMsg::Close), true),
        TabMenuEntry::new(CloseLeft, t(TabMsg::CloseLeft), spec.index > 0),
        TabMenuEntry::new(
            CloseRight,
            t(TabMsg::CloseRight),
            spec.index + 1 < spec.tab_count,
        ),
        TabMenuEntry::new(CloseOthers, t(TabMsg::CloseOthers), spec.tab_count >= 2),
        TabMenuEntry::new(CloseAll, t(TabMsg::CloseAll), true),
    ]);
    entries
}

/// 每项的图标（同侧栏主机右键菜单，用现有图标集里语义最接近的）。
pub fn icon_of(item: TabMenuItem) -> Option<Ico> {
    Some(match item {
        TabMenuItem::DuplicateSession | TabMenuItem::CopyHostName | TabMenuItem::CopyAddress => {
            Ico::Copy
        }
        TabMenuItem::Sftp => Ico::FolderOpen,
        TabMenuItem::Reconnect => Ico::RotateCcw,
        TabMenuItem::Disconnect => Ico::Unplug,
        TabMenuItem::SendBreak => Ico::Zap,
        TabMenuItem::Close => Ico::X,
        TabMenuItem::CloseLeft => Ico::ArrowLeftToLine,
        TabMenuItem::CloseRight => Ico::ArrowRightToLine,
        TabMenuItem::CloseOthers | TabMenuItem::CloseAll => Ico::CircleX,
        TabMenuItem::Separator => return None,
    })
}

fn act(
    entity: &Entity<MainWindow>,
    f: impl Fn(&mut MainWindow, &mut Window, &mut Context<MainWindow>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let entity = entity.clone();
    move |_, window, cx| {
        entity.update(cx, |this, cx| f(this, window, cx));
    }
}

/// 把 [`tab_menu`] 的结果画成弹出菜单。
pub fn build_tab_menu(
    mut menu: PopupMenu,
    spec: &TabMenuSpec,
    entity: &Entity<MainWindow>,
    tokens: &Tokens,
) -> PopupMenu {
    let index = spec.index;
    for entry in tab_menu(spec) {
        let item = entry.item;
        if item == TabMenuItem::Separator {
            menu = menu.separator();
            continue;
        }
        let icon = icon_of(item);
        let enabled = entry.enabled;
        let label = entry.label;
        let click = match item {
            TabMenuItem::DuplicateSession => Box::new(act(entity, move |this, window, cx| {
                this.duplicate_tab(index, window, cx)
            }))
                as Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>,
            TabMenuItem::CopyHostName => Box::new(act(entity, move |this, window, cx| {
                this.copy_tab_host_name(index, window, cx)
            })),
            TabMenuItem::CopyAddress => Box::new(act(entity, move |this, window, cx| {
                this.copy_tab_address(index, window, cx)
            })),
            TabMenuItem::Sftp => Box::new(act(entity, move |this, window, cx| {
                this.open_sftp_from_tab(index, window, cx)
            })),
            TabMenuItem::Reconnect => Box::new(act(entity, move |this, window, cx| {
                this.reconnect_tab(index, window, cx)
            })),
            TabMenuItem::Disconnect => Box::new(act(entity, move |this, window, cx| {
                this.disconnect_tab(index, window, cx)
            })),
            TabMenuItem::SendBreak => Box::new(act(entity, move |this, window, cx| {
                this.send_break_tab(index, window, cx)
            })),
            TabMenuItem::Close => Box::new(act(entity, move |this, window, cx| {
                this.close_tab(index, window, cx)
            })),
            TabMenuItem::CloseLeft => Box::new(act(entity, move |this, window, cx| {
                this.close_tabs(index, CloseScope::Left, window, cx)
            })),
            TabMenuItem::CloseRight => Box::new(act(entity, move |this, window, cx| {
                this.close_tabs(index, CloseScope::Right, window, cx)
            })),
            TabMenuItem::CloseOthers => Box::new(act(entity, move |this, window, cx| {
                this.close_tabs(index, CloseScope::Others, window, cx)
            })),
            TabMenuItem::CloseAll => Box::new(act(entity, move |this, window, cx| {
                this.close_tabs(index, CloseScope::All, window, cx)
            })),
            TabMenuItem::Separator => continue,
        };
        let Some(icon) = icon else { continue };
        let line = crate::menu_kit::row(tokens, label, icon)
            .disabled(!enabled)
            .tooltip(entry.hint);
        // 置灰并说明原因：悬停提示和主机右键菜单里的 SFTP 同一个做法；断开连接可点时用危险色。
        let line = if item == TabMenuItem::Disconnect {
            line.danger()
        } else {
            line
        };
        menu = menu.item(if enabled {
            line.on_click(click)
        } else {
            line.inert()
        });
    }
    menu
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{TabMenuEntry, TabMenuItem, TabMenuSpec, tab_menu};
    use crate::main_window::protocol_form::{Caps, SSH_ONLY_HINT};
    use zssh_core::model::Protocol;

    fn ssh(index: usize, tab_count: usize) -> TabMenuSpec {
        TabMenuSpec {
            index,
            tab_count,
            host_record: true,
            has_address: true,
            has_host: true,
            can_reconnect: true,
            can_disconnect: true,
            caps: Caps::of(Protocol::Ssh),
        }
    }

    fn labels(spec: &TabMenuSpec) -> Vec<&'static str> {
        tab_menu(spec).iter().map(|entry| entry.label).collect()
    }

    fn entry(spec: &TabMenuSpec, item: TabMenuItem) -> TabMenuEntry {
        tab_menu(spec)
            .into_iter()
            .find(|entry| entry.item == item)
            .expect("菜单里有这一项")
    }

    #[test]
    fn an_ssh_tab_menu_has_the_documented_order_and_names() {
        assert_eq!(
            labels(&ssh(1, 3)),
            [
                "复制会话",
                "",
                "复制主机名",
                "复制地址",
                "SFTP",
                "重新连接",
                "断开连接",
                "",
                "关闭",
                "关闭左侧",
                "关闭右侧",
                "关闭其他",
                "关闭全部",
            ]
        );
    }

    #[test]
    fn a_serial_tab_adds_break_right_after_disconnect_and_cannot_duplicate() {
        let mut spec = ssh(0, 2);
        spec.caps = Caps::of(Protocol::Serial);
        let labels = labels(&spec);
        let at = labels
            .iter()
            .position(|l| *l == "断开连接")
            .expect("有断开");
        assert_eq!(labels[at + 1], "发送 Break");
        assert_eq!(labels[at + 2], "", "Break 后面才是分隔线");
        let duplicate = entry(&spec, TabMenuItem::DuplicateSession);
        assert!(!duplicate.enabled);
        assert_eq!(duplicate.label, "复制会话（串口只能开一个）");
        // 别的协议没有 Break。
        assert!(
            !super::tab_menu(&ssh(0, 2))
                .iter()
                .any(|e| e.item == TabMenuItem::SendBreak)
        );
    }

    #[test]
    fn break_is_greyed_out_when_the_serial_tab_is_not_connected() {
        let mut spec = ssh(0, 1);
        spec.caps = Caps::of(Protocol::Serial);
        spec.can_disconnect = false;
        assert!(!entry(&spec, TabMenuItem::SendBreak).enabled);
        assert!(!entry(&spec, TabMenuItem::Disconnect).enabled);
    }

    #[test]
    fn a_tab_without_a_host_record_cannot_copy_the_host_name() {
        let mut quick = ssh(0, 1);
        quick.host_record = false;
        quick.has_host = false;
        assert!(!entry(&quick, TabMenuItem::CopyHostName).enabled);
        // 快速连接有 user@host:port 可复制。
        assert!(entry(&quick, TabMenuItem::CopyAddress).enabled);
        quick.has_address = false;
        assert!(!entry(&quick, TabMenuItem::CopyAddress).enabled);
        assert!(!entry(&quick, TabMenuItem::Sftp).enabled);
        assert_eq!(entry(&quick, TabMenuItem::Sftp).hint, None);
    }

    #[test]
    fn sftp_is_ssh_only_and_says_why_on_telnet_and_serial() {
        for protocol in [Protocol::Telnet, Protocol::Serial] {
            let mut spec = ssh(0, 1);
            spec.caps = Caps::of(protocol);
            let sftp = entry(&spec, TabMenuItem::Sftp);
            assert!(!sftp.enabled, "{protocol:?}");
            assert_eq!(sftp.hint, Some(SSH_ONLY_HINT));
        }
        let sftp = entry(&ssh(0, 1), TabMenuItem::Sftp);
        assert!(sftp.enabled);
        assert_eq!(sftp.hint, None);
    }

    #[test]
    fn reconnect_and_disconnect_follow_the_tab_state() {
        let mut spec = ssh(0, 1);
        spec.can_reconnect = false;
        spec.can_disconnect = true;
        assert!(!entry(&spec, TabMenuItem::Reconnect).enabled);
        assert!(entry(&spec, TabMenuItem::Disconnect).enabled);
        spec.can_reconnect = true;
        spec.can_disconnect = false;
        assert!(entry(&spec, TabMenuItem::Reconnect).enabled);
        assert!(!entry(&spec, TabMenuItem::Disconnect).enabled);
    }

    #[test]
    fn close_items_grey_out_at_the_ends_and_for_a_single_tab() {
        let enabled = |spec: &TabMenuSpec| {
            [
                TabMenuItem::Close,
                TabMenuItem::CloseLeft,
                TabMenuItem::CloseRight,
                TabMenuItem::CloseOthers,
                TabMenuItem::CloseAll,
            ]
            .map(|item| entry(spec, item).enabled)
        };
        // 第一个：左边没有。
        assert_eq!(enabled(&ssh(0, 3)), [true, false, true, true, true]);
        // 中间：都能点。
        assert_eq!(enabled(&ssh(1, 3)), [true, true, true, true, true]);
        // 最后一个：右边没有。
        assert_eq!(enabled(&ssh(2, 3)), [true, true, false, true, true]);
        // 唯一一个：只剩关闭和关闭全部。
        assert_eq!(enabled(&ssh(0, 1)), [true, false, false, false, true]);
    }

    #[test]
    fn separators_split_the_menu_into_three_groups() {
        let items: Vec<_> = tab_menu(&ssh(1, 3)).iter().map(|e| e.item).collect();
        let seps: Vec<_> = items
            .iter()
            .enumerate()
            .filter(|(_, item)| **item == TabMenuItem::Separator)
            .map(|(at, _)| at)
            .collect();
        assert_eq!(seps, [1, 7]);
        assert_eq!(items[0], TabMenuItem::DuplicateSession);
        assert_eq!(items[items.len() - 1], TabMenuItem::CloseAll);
    }

    #[test]
    fn every_tab_menu_item_has_an_icon_except_the_separator() {
        use super::icon_of;
        let mut serial = ssh(1, 3);
        serial.caps = Caps::of(Protocol::Serial);
        for spec in [ssh(1, 3), serial] {
            for entry in tab_menu(&spec) {
                assert_eq!(
                    icon_of(entry.item).is_none(),
                    entry.item == TabMenuItem::Separator,
                    "{:?}",
                    entry.item
                );
            }
        }
    }
}
