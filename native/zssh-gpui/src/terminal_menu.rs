//! 终端的右键菜单（V2-U25）：右键该做什么、菜单里有哪些项、什么时候置灰、打开链接怎么放行。
//!
//! 全是不碰界面的纯逻辑 + 一个把条目画成弹出菜单的函数；谁来弹、点了之后怎么办由
//! [`crate::terminal_workspace::TerminalWorkspace`] 决定。
//!
//! 右键的去向（优先级从高到低）：
//! 1. 远端开了鼠标上报（vim、htop、tmux 的鼠标模式）且没按 Shift：整个按键交给远端，本地什么也不做；
//! 2. 鼠标在链接上：弹菜单（打开链接 / 复制链接 + 终端菜单），不论「右键粘贴」开不开；
//! 3. 「右键粘贴」开着：直接粘贴；
//! 4. 否则弹终端菜单。
//!
//! Shift 绕过上报和 Shift+左键选择是同一个惯例（xterm），所以按着 Shift 右键时第 1 条不成立。

use std::rc::Rc;

use ::gpui::{App, ClickEvent, Window};
use gpui_kit::component::menu::PopupMenu;
use zssh_terminal::MouseMode;

use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::menu_kit;

/// 一次右键按下该怎么处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightClick {
    /// 远端接管，本地不弹菜单也不粘贴。
    Remote,
    /// 直接粘贴。
    Paste,
    /// 弹终端菜单。
    Menu,
}

/// 远端实际接管鼠标的模式：按住 Shift 强制走本地（xterm 惯例，Shift+左键选择也是这条）。
pub fn effective_mouse_mode(remote: MouseMode, shift: bool) -> MouseMode {
    if shift { MouseMode::default() } else { remote }
}

/// 右键按下的去向。`mode` 是已经算过 Shift 的 [`effective_mouse_mode`] 结果。
pub fn right_click_route(mode: MouseMode, paste_on_right_click: bool, on_link: bool) -> RightClick {
    if mode.reports_buttons() {
        RightClick::Remote
    } else if on_link || !paste_on_right_click {
        RightClick::Menu
    } else {
        RightClick::Paste
    }
}

/// 菜单里的一项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuItem {
    OpenLink,
    CopyLink,
    Copy,
    Paste,
    SelectAll,
    /// 打开当前窗格的搜索条（只在右键菜单里，不配快捷键）。
    Search,
    /// 清屏（只在右键菜单里，不配快捷键）。
    Clear,
    SplitRight,
    SplitDown,
    /// 关闭右键点中的那个窗格（只在当前标签有分屏时出现）。
    ClosePane,
    Separator,
}

/// 弹菜单那一刻的状态。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MenuState {
    /// 鼠标下的链接；不在链接上是 `None`。
    pub link: Option<String>,
    /// 有非空的选区文字（空白格选区复制不出东西，按没有算）。
    pub has_selection: bool,
    /// 剪贴板里有文字。
    pub clipboard_has_text: bool,
    /// 当前标签有分屏（只有一个窗格时不显示「关闭窗格」）。
    pub split: bool,
}

/// 菜单上的一行。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub item: MenuItem,
    pub label: &'static str,
    pub enabled: bool,
    /// 右边的快捷键文字；项目里没有绑定的不写。
    pub shortcut: Option<&'static str>,
}

impl Entry {
    fn new(
        item: MenuItem,
        label: &'static str,
        enabled: bool,
        shortcut: Option<&'static str>,
    ) -> Self {
        Self {
            item,
            label,
            enabled,
            shortcut,
        }
    }

    fn separator() -> Self {
        Self::new(MenuItem::Separator, "", false, None)
    }
}

/// 这个链接能不能交给系统默认浏览器：只放行 http / https（OSC 8 链接可以带任何协议，终端输出也不可信）。
pub fn link_is_openable(link: &str) -> bool {
    zssh_core::external_url::validate_external_url(link).is_ok()
}

/// 打开链接：先过校验再交给 `opener`（生产里是系统默认浏览器，测试里是桩）。校验不过不会调用 `opener`。
pub fn open_link(
    link: &str,
    opener: impl FnOnce(&str) -> Result<(), String>,
) -> Result<(), String> {
    let url = zssh_core::external_url::validate_external_url(link)?;
    opener(url.as_str())
}

/// 终端右键菜单的文案。
#[derive(Clone, Copy, Debug)]
enum TerminalMenuMsg {
    OpenLink,
    CopyLink,
    Copy,
    Paste,
    SelectAll,
    Search,
    Clear,
    SplitRight,
    SplitDown,
    ClosePane,
}

impl crate::i18n::Msg for TerminalMenuMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::OpenLink => "打开链接",
            Self::CopyLink => "复制链接",
            Self::Copy => "复制",
            Self::Paste => "粘贴",
            Self::SelectAll => "全选",
            Self::Search => "搜索",
            Self::Clear => "清屏",
            Self::SplitRight => "左右分屏",
            Self::SplitDown => "上下分屏",
            Self::ClosePane => "关闭窗格",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::OpenLink => "Open Link",
            Self::CopyLink => "Copy Link",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::SelectAll => "Select All",
            Self::Search => "Search",
            Self::Clear => "Clear Screen",
            Self::SplitRight => "Split Right",
            Self::SplitDown => "Split Down",
            Self::ClosePane => "Close Pane",
        }
    }
}

/// 菜单从上到下的内容：
/// （在链接上：打开链接、复制链接、分隔线）复制、粘贴、全选 | 搜索、清屏 | 左右分屏、上下分屏
/// （有分屏时：关闭窗格）。
///
/// 搜索和清屏只在这里，不配快捷键（`Ctrl+Shift+F` 是文件面板，`Ctrl+Shift+K` 在串口里是 Break），
/// 所以这两项右边不写快捷键。
pub fn menu_entries(state: &MenuState) -> Vec<Entry> {
    let mut entries = Vec::new();
    if let Some(link) = &state.link {
        entries.push(Entry::new(
            MenuItem::OpenLink,
            crate::i18n::t(TerminalMenuMsg::OpenLink),
            link_is_openable(link),
            None,
        ));
        entries.push(Entry::new(
            MenuItem::CopyLink,
            crate::i18n::t(TerminalMenuMsg::CopyLink),
            true,
            None,
        ));
        entries.push(Entry::separator());
    }
    entries.extend([
        Entry::new(
            MenuItem::Copy,
            crate::i18n::t(TerminalMenuMsg::Copy),
            state.has_selection,
            Some("Ctrl+Shift+C"),
        ),
        Entry::new(
            MenuItem::Paste,
            crate::i18n::t(TerminalMenuMsg::Paste),
            state.clipboard_has_text,
            Some("Ctrl+Shift+V"),
        ),
        Entry::new(
            MenuItem::SelectAll,
            crate::i18n::t(TerminalMenuMsg::SelectAll),
            true,
            None,
        ),
        Entry::separator(),
        Entry::new(
            MenuItem::Search,
            crate::i18n::t(TerminalMenuMsg::Search),
            true,
            None,
        ),
        Entry::new(
            MenuItem::Clear,
            crate::i18n::t(TerminalMenuMsg::Clear),
            true,
            None,
        ),
        Entry::separator(),
        Entry::new(
            MenuItem::SplitRight,
            crate::i18n::t(TerminalMenuMsg::SplitRight),
            true,
            Some("Ctrl+Shift+D"),
        ),
        Entry::new(
            MenuItem::SplitDown,
            crate::i18n::t(TerminalMenuMsg::SplitDown),
            true,
            Some("Ctrl+Shift+E"),
        ),
    ]);
    if state.split {
        entries.push(Entry::new(
            MenuItem::ClosePane,
            crate::i18n::t(TerminalMenuMsg::ClosePane),
            true,
            Some("Ctrl+Shift+W"),
        ));
    }
    entries
}

/// 每项的图标。
pub fn icon_of(item: MenuItem) -> Option<Ico> {
    Some(match item {
        MenuItem::OpenLink => Ico::ExternalLink,
        MenuItem::CopyLink => Ico::Link2,
        MenuItem::Copy => Ico::Copy,
        MenuItem::Paste => Ico::ClipboardPaste,
        MenuItem::SelectAll => Ico::ScanText,
        MenuItem::Search => Ico::Search,
        MenuItem::Clear => Ico::Eraser,
        MenuItem::SplitRight => Ico::Columns2,
        MenuItem::SplitDown => Ico::Rows2,
        MenuItem::ClosePane => Ico::X,
        MenuItem::Separator => return None,
    })
}

/// 全选：聚焦窗格当前屏幕上所有的格子（锚点在左上、终点在右下）。回滚缓冲不在内：选区坐标只有可见区的行号。
pub fn select_all_range(rows: usize, cols: usize) -> Option<((usize, usize), (usize, usize))> {
    (rows > 0 && cols > 0).then(|| ((0, 0), (rows - 1, cols - 1)))
}

/// 点了菜单某项时的回调。
pub type Dispatch = Rc<dyn Fn(MenuItem, &mut Window, &mut App)>;

/// 把 [`menu_entries`] 画成弹出菜单。点某项时调 `dispatch(项, 窗口, 应用)`；置灰的项不会收到点击。
pub fn build_menu(
    menu: PopupMenu,
    entries: &[Entry],
    tokens: &Tokens,
    dispatch: Dispatch,
) -> PopupMenu {
    let mut menu = menu.min_w(::gpui::px(220.0));
    for entry in entries {
        let Some(icon) = icon_of(entry.item) else {
            menu = menu.separator();
            continue;
        };
        let line = menu_kit::row(tokens, entry.label, icon)
            .disabled(!entry.enabled)
            .right(entry.shortcut);
        let item = entry.item;
        let dispatch = dispatch.clone();
        menu = menu.item(if entry.enabled {
            line.on_click(move |_: &ClickEvent, window, cx| dispatch(item, window, cx))
        } else {
            line.inert()
        });
    }
    menu
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        Entry, MenuItem, MenuState, RightClick, effective_mouse_mode, link_is_openable,
        menu_entries, open_link, right_click_route, select_all_range,
    };
    use zssh_terminal::MouseMode;

    fn reporting() -> MouseMode {
        MouseMode {
            click: true,
            drag: true,
            motion: false,
            sgr: true,
        }
    }

    fn state(link: Option<&str>, selection: bool, clipboard: bool) -> MenuState {
        MenuState {
            link: link.map(str::to_owned),
            has_selection: selection,
            clipboard_has_text: clipboard,
            split: false,
        }
    }

    fn split_state() -> MenuState {
        MenuState {
            split: true,
            ..state(None, true, true)
        }
    }

    fn items(entries: &[Entry]) -> Vec<MenuItem> {
        entries.iter().map(|entry| entry.item).collect()
    }

    fn entry(entries: &[Entry], item: MenuItem) -> &Entry {
        entries
            .iter()
            .find(|entry| entry.item == item)
            .expect("菜单里有这一项")
    }

    #[test]
    fn right_click_goes_to_the_menu_when_paste_is_off_and_the_mouse_is_not_on_a_link() {
        let plain = MouseMode::default();
        assert_eq!(right_click_route(plain, false, false), RightClick::Menu);
    }

    #[test]
    fn right_click_pastes_only_when_the_switch_is_on_and_the_mouse_is_not_on_a_link() {
        let plain = MouseMode::default();
        assert_eq!(right_click_route(plain, true, false), RightClick::Paste);
        // 在链接上右键：开关开着也弹菜单，不粘贴。
        assert_eq!(right_click_route(plain, true, true), RightClick::Menu);
        assert_eq!(right_click_route(plain, false, true), RightClick::Menu);
    }

    #[test]
    fn a_remote_that_reports_the_mouse_keeps_the_right_button_for_every_setting() {
        for paste in [false, true] {
            for on_link in [false, true] {
                assert_eq!(
                    right_click_route(reporting(), paste, on_link),
                    RightClick::Remote,
                    "paste={paste} on_link={on_link}"
                );
            }
        }
        // 只开 1000（只报点击）也算接管。
        let click_only = MouseMode {
            click: true,
            ..Default::default()
        };
        assert_eq!(
            right_click_route(click_only, false, false),
            RightClick::Remote
        );
    }

    #[test]
    fn shift_bypasses_the_mouse_report_and_brings_back_the_local_menu() {
        let mode = effective_mouse_mode(reporting(), true);
        assert!(!mode.reports_buttons());
        assert_eq!(right_click_route(mode, false, false), RightClick::Menu);
        assert_eq!(right_click_route(mode, true, false), RightClick::Paste);
        assert_eq!(right_click_route(mode, false, true), RightClick::Menu);
        // 不按 Shift 时原样交给远端。
        assert_eq!(effective_mouse_mode(reporting(), false), reporting());
    }

    #[test]
    fn the_plain_menu_has_copy_paste_select_all_search_clear_and_the_two_splits() {
        let entries = menu_entries(&state(None, true, true));
        assert_eq!(
            items(&entries),
            [
                MenuItem::Copy,
                MenuItem::Paste,
                MenuItem::SelectAll,
                MenuItem::Separator,
                MenuItem::Search,
                MenuItem::Clear,
                MenuItem::Separator,
                MenuItem::SplitRight,
                MenuItem::SplitDown,
            ]
        );
        assert!(
            entries
                .iter()
                .all(|e| e.item == MenuItem::Separator || e.enabled)
        );
        let labels: Vec<_> = entries.iter().map(|e| e.label).collect();
        assert_eq!(
            labels,
            [
                "复制",
                "粘贴",
                "全选",
                "",
                "搜索",
                "清屏",
                "",
                "左右分屏",
                "上下分屏"
            ]
        );
    }

    #[test]
    fn close_pane_shows_up_last_only_when_the_tab_is_split() {
        let single = menu_entries(&state(None, true, true));
        assert!(!items(&single).contains(&MenuItem::ClosePane));

        let split = menu_entries(&split_state());
        assert_eq!(items(&split).last(), Some(&MenuItem::ClosePane));
        let close = entry(&split, MenuItem::ClosePane);
        assert_eq!(close.label, "关闭窗格");
        assert!(close.enabled);
        // 和 Ctrl+Shift+W 同一个快捷键，菜单上照写。
        assert_eq!(close.shortcut, Some("Ctrl+Shift+W"));
        // 其余部分和没分屏时一样，只是末尾多一项。
        assert_eq!(
            &items(&split)[..items(&split).len() - 1],
            &items(&single)[..]
        );
    }

    #[test]
    fn the_new_items_have_both_languages() {
        use super::TerminalMenuMsg as M;
        use crate::i18n::Msg;
        assert_eq!(
            [M::Search.en(), M::Clear.en(), M::ClosePane.en()],
            ["Search", "Clear Screen", "Close Pane"]
        );
        assert_eq!(
            [M::Search.zh(), M::Clear.zh(), M::ClosePane.zh()],
            ["搜索", "清屏", "关闭窗格"]
        );
    }

    #[test]
    fn search_and_clear_have_no_shortcut_because_the_keys_belong_to_other_features() {
        // Ctrl+Shift+F 是文件面板，Ctrl+Shift+K 在串口里是 Break：这两项只放在右键菜单。
        for entries in [
            menu_entries(&state(None, false, false)),
            menu_entries(&split_state()),
        ] {
            for item in [MenuItem::Search, MenuItem::Clear] {
                let found = entry(&entries, item);
                assert!(found.enabled, "{item:?}");
                assert_eq!(found.shortcut, None, "{item:?}");
            }
        }
    }

    #[test]
    fn copy_is_greyed_out_without_a_selection_and_paste_without_clipboard_text() {
        let none = menu_entries(&state(None, false, false));
        assert!(!entry(&none, MenuItem::Copy).enabled);
        assert!(!entry(&none, MenuItem::Paste).enabled);
        assert!(entry(&none, MenuItem::SelectAll).enabled);
        let some = menu_entries(&state(None, true, false));
        assert!(entry(&some, MenuItem::Copy).enabled);
        assert!(!entry(&some, MenuItem::Paste).enabled);
        let clip = menu_entries(&state(None, false, true));
        assert!(!entry(&clip, MenuItem::Copy).enabled);
        assert!(entry(&clip, MenuItem::Paste).enabled);
    }

    #[test]
    fn on_a_link_the_link_items_come_first_then_a_separator_then_the_plain_menu() {
        let entries = menu_entries(&state(Some("https://example.com/docs"), false, true));
        assert_eq!(
            &items(&entries)[..4],
            [
                MenuItem::OpenLink,
                MenuItem::CopyLink,
                MenuItem::Separator,
                MenuItem::Copy
            ]
        );
        assert_eq!(items(&entries).len(), 12);
        assert!(entry(&entries, MenuItem::OpenLink).enabled);
        assert!(entry(&entries, MenuItem::CopyLink).enabled);
        // 没在链接上就没有这两项。
        let plain = menu_entries(&state(None, false, true));
        assert!(!items(&plain).contains(&MenuItem::OpenLink));
        assert!(!items(&plain).contains(&MenuItem::CopyLink));
    }

    #[test]
    fn a_link_that_is_not_http_or_https_can_be_copied_but_not_opened() {
        for link in [
            "javascript:alert(1)",
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:network",
            "ftp://example.com/x",
            "https://example.com/has space",
        ] {
            let entries = menu_entries(&state(Some(link), false, true));
            assert!(!entry(&entries, MenuItem::OpenLink).enabled, "{link}");
            assert!(entry(&entries, MenuItem::CopyLink).enabled, "{link}");
        }
        for link in ["http://example.com/a", "https://example.com/a?b=1#c"] {
            assert!(link_is_openable(link), "{link}");
        }
    }

    #[test]
    fn opening_a_link_goes_through_the_url_check_before_the_opener() {
        let opened = std::cell::RefCell::new(Vec::<String>::new());
        let stub = |url: &str| {
            opened.borrow_mut().push(url.to_owned());
            Ok(())
        };
        assert!(open_link("https://example.com/a", stub).is_ok());
        // 规范化之后的地址才交给系统（大写主机名被小写）。
        assert!(open_link("HTTPS://Example.COM", stub).is_ok());
        assert_eq!(
            *opened.borrow(),
            ["https://example.com/a", "https://example.com/"]
        );
        for bad in [
            "javascript:alert(1)",
            "file:///C:/x.exe",
            "ms-settings:network",
            r"C:\Windows\System32\calc.exe",
            "https://example.com/a b",
            "",
        ] {
            assert!(open_link(bad, stub).is_err(), "{bad}");
        }
        assert_eq!(opened.borrow().len(), 2, "被拒绝的链接不能走到系统调用");
    }

    #[test]
    fn an_opener_failure_is_passed_back() {
        let result = open_link("https://example.com/", |_| Err("没有浏览器".to_owned()));
        assert_eq!(result, Err("没有浏览器".to_owned()));
    }

    #[test]
    fn shortcuts_are_only_written_for_keys_that_are_really_bound() {
        let entries = menu_entries(&state(Some("https://example.com/"), true, true));
        let shortcut = |item| entry(&entries, item).shortcut;
        assert_eq!(shortcut(MenuItem::Copy), Some("Ctrl+Shift+C"));
        assert_eq!(shortcut(MenuItem::Paste), Some("Ctrl+Shift+V"));
        assert_eq!(shortcut(MenuItem::SplitRight), Some("Ctrl+Shift+D"));
        assert_eq!(shortcut(MenuItem::SplitDown), Some("Ctrl+Shift+E"));
        // 全选、链接项没有快捷键。
        assert_eq!(shortcut(MenuItem::SelectAll), None);
        assert_eq!(shortcut(MenuItem::OpenLink), None);
        assert_eq!(shortcut(MenuItem::CopyLink), None);
    }

    #[test]
    fn every_real_item_has_an_icon_and_only_the_separator_does_not() {
        use super::icon_of;
        let mut every = menu_entries(&state(Some("https://example.com/"), true, true));
        every.extend(menu_entries(&split_state()));
        for entry in every {
            assert_eq!(
                icon_of(entry.item).is_none(),
                entry.item == MenuItem::Separator,
                "{:?}",
                entry.item
            );
        }
    }

    #[test]
    fn select_all_covers_the_whole_visible_grid() {
        assert_eq!(select_all_range(30, 120), Some(((0, 0), (29, 119))));
        assert_eq!(select_all_range(1, 1), Some(((0, 0), (0, 0))));
        assert_eq!(select_all_range(0, 80), None);
        assert_eq!(select_all_range(24, 0), None);
    }
}
