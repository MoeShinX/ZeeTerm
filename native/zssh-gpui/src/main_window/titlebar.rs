//! 自绘标题栏：最左是图标块（侧栏开关、「主机」、AI、文件，侧栏折叠时末尾再补一个设置齿轮；纯图标，
//! 没有 logo 和带字按钮）；后面是会话标签（状态点、标题、关闭、中键关闭、双击复制、右键菜单）和「+」；
//! 右边只剩窗口按钮。快捷命令栏、撰写栏的开关在终端底部状态栏（`status_bar`），主题只在「设置 → 外观」里改，
//! 凭据和设置的入口在侧栏左下角和「管理 + 设置」页里。
//!
//! 结构照 0.2.14 的 `components/titlebar/TopTabs.tsx`。窗口拖动和最大化靠
//! `window_frame::drag_region`（Windows 上是 `WindowControlArea::Drag`，Linux 上是鼠标事件）：条本身是拖动区，
//! 所有可点的东西都 `occlude()` 一下，把拖动区在它们身上挖掉，这样点击不会被当成拖标题栏；
//! 双击空白处最大化也由它处理。

use ::gpui::{
    AnyElement, App, Hsla, MouseButton, SharedString, Stateful, Window, WindowControlArea, px,
};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::nav::MainView;
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::terminal_workspace::{TabInfo, TabStatus};
use crate::typography::{Role, Typo as _};
use crate::window_frame;
use zssh_core::model::{Host, Protocol, parse_quick_target};

/// 标题栏高度（0.2.14 的 `h-9`）。
pub const BAR_HEIGHT: f32 = 36.0;
/// 标签高度。
const TAB_HEIGHT: f32 = 32.0;
/// 窗口按钮宽度。
const CONTROL_WIDTH: f32 = 42.0;
/// 标题栏图标按钮的边长、图标间距（Tailwind `gap_1` = 4）、左侧块的左右内边距（`pl_3` / `pr_2`）。
const ICON_SIZE: f32 = 28.0;
const ICON_GAP: f32 = 4.0;
const LEFT_PAD_LEFT: f32 = 12.0;
const LEFT_PAD_RIGHT: f32 = 8.0;

/// 不同类型的标签使用同一条主题色标记，避免只靠背景色区分当前标签。
pub(crate) fn active_tab_indicator(tokens: &Tokens) -> Div {
    div()
        .absolute()
        .top_0()
        .left(px(10.0))
        .right(px(10.0))
        .h(px(2.0))
        .rounded_full()
        .bg(tokens.accent)
}

fn status_color(status: TabStatus, tokens: &Tokens) -> Hsla {
    match status {
        TabStatus::Connecting => tokens.dot_connecting,
        TabStatus::Connected => tokens.dot_connected,
        TabStatus::Disconnected => tokens.dot_disconnected,
        TabStatus::Empty => tokens.dot_idle,
    }
}

/// 标签「是哪一处」：序号按它编，同一处的标签同一个号。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum TabTarget {
    /// 有主机记录的按主机 id。
    Host(i64),
    /// 没有主机记录的快速连接按 `用户@地址:端口`。
    Quick(String),
    /// 空白标签（还没连任何东西）：各自单独一个号，用标签 id 区分。
    Blank(u64),
}

fn tab_target(info: &TabInfo) -> TabTarget {
    match (info.host_id, &info.quick_key) {
        (Some(id), _) => TabTarget::Host(id),
        (None, Some(key)) => TabTarget::Quick(key.clone()),
        (None, None) => TabTarget::Blank(info.id),
    }
}

/// 每个标签的序号（从 1 开始）：从左到右扫，第一次出现的「处」拿下一个号，之后同一处的标签沿用。
/// 例：A、A、B、A、C → 1、1、2、1、3。关标签、拖动排序、复制会话之后重新算，同一处的号可能因此变化。
fn badge_numbers(targets: &[TabTarget]) -> Vec<usize> {
    let mut seen: std::collections::HashMap<&TabTarget, usize> = std::collections::HashMap::new();
    targets
        .iter()
        .map(|target| {
            let next = seen.len() + 1;
            *seen.entry(target).or_insert(next)
        })
        .collect()
}

/// 一排终端标签的序号（标签条和调试断言共用）。
pub(super) fn tab_numbers(infos: &[TabInfo]) -> Vec<usize> {
    badge_numbers(&infos.iter().map(tab_target).collect::<Vec<_>>())
}

fn badge_label(number: usize) -> SharedString {
    number.to_string().into()
}

/// 序号的字色：琥珀色（连接中）底色偏亮，用深色字；其余状态底色偏深，用白字。
fn badge_ink(status: TabStatus) -> Hsla {
    match status {
        TabStatus::Connecting => ::gpui::rgb(0x1F2937).into(),
        _ => ::gpui::white(),
    }
}

/// 序号小方块的悬停提示：「<主机名> · <状态>」。
fn badge_tooltip(name: &str, status: TabStatus) -> SharedString {
    format!("{name} · {}", status_title(status)).into()
}

/// 标签整体的悬停提示里的地址（V2-U32）：SSH / Telnet 是 `用户@主机:端口`（端口默认值也写出来，方便
/// 核对）；串口是 `设备 · 波特率`；有跳板机只写最终目标，后面加「（经跳板）」。取主机配置里的值，用户填
/// 域名就是域名，不是解析后的 IP。快速连接（没有主机记录）取它的 `user@host[:port]`。
/// 空标签、本地标签等没有地址的返回 `None`，由调用方退回显示标题。
pub(super) fn tab_address_tip(host: Option<&Host>, quick_target: Option<&str>) -> Option<String> {
    if let Some(host) = host {
        let serial = host.protocol.protocol == Protocol::Serial;
        if serial {
            let options = host.protocol.serial.as_ref()?;
            let device = options.device.trim();
            return (!device.is_empty()).then(|| format!("{device} · {}", options.baud_rate));
        }
        let address = host.host.trim();
        if address.is_empty() {
            return None;
        }
        let mut text = endpoint_text(host.username.trim(), address, host.port);
        if host.jump_host_id.is_some() {
            text.push_str(t(TitlebarMsg::ViaJump));
        }
        return Some(text);
    }
    let target = parse_quick_target(quick_target?).ok()?;
    Some(endpoint_text(&target.username, &target.host, target.port))
}

/// `用户@主机:端口`；没有用户名就不写 `@`；IPv6 地址加方括号，免得和端口的冒号混在一起。
fn endpoint_text(username: &str, address: &str, port: u16) -> String {
    let address = super::host_store::normalize_address(address);
    let address = if address.contains(':') {
        format!("[{address}]")
    } else {
        address
    };
    if username.is_empty() {
        format!("{address}:{port}")
    } else {
        format!("{username}@{address}:{port}")
    }
}

/// 标签状态、左侧图标块、「移回主窗口」按钮的文案（主机名、地址等用户数据不走这里）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TitlebarMsg {
    /// 标签状态：连接中。
    Connecting,
    /// 标签状态：已连接。
    Connected,
    /// 标签状态：已断开。
    Disconnected,
    /// 标签状态：空标签。
    EmptyTab,
    /// 侧栏开关的提示（侧栏收起时是「展开」）。
    ExpandSidebar,
    /// 侧栏开关的提示（侧栏展开时是「收起」）。
    CollapseSidebar,
    /// 主机页入口的提示。
    Hosts,
    /// AI 助手入口的提示。
    AiAssistant,
    /// 文件（SFTP）入口的提示。
    Files,
    /// 文件入口置灰时的提示。
    FilesSshOnly,
    /// 设置与管理入口的提示。
    Settings,
    /// 「+」新标签的提示。
    NewTab,
    /// 标签上的小圆点：AI 正在这个终端里跑命令。
    AiRunning,
    /// 标签上 × 的提示。
    CloseTab,
    /// 独立窗口「移回主窗口」按钮上的字。
    BackToMain,
    /// 「移回主窗口」的悬停提示。
    BackTip,
    /// 跳板机地址后面的说明。
    ViaJump,
}

impl Msg for TitlebarMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Connecting => "连接中",
            Self::Connected => "已连接",
            Self::Disconnected => "已断开",
            Self::EmptyTab => "空标签",
            Self::ExpandSidebar => "展开侧边栏",
            Self::CollapseSidebar => "收起侧边栏",
            Self::Hosts => "主机",
            Self::AiAssistant => "AI 助手 (Ctrl+Shift+A)",
            Self::Files => "文件 (Ctrl+Shift+F)",
            Self::FilesSshOnly => "文件（仅 SSH 主机可用）",
            Self::Settings => "设置与管理 (Ctrl+,)",
            Self::NewTab => "新标签 (Ctrl+Shift+T)",
            Self::AiRunning => "AI 正在这个终端里执行命令",
            Self::CloseTab => "关闭标签",
            Self::BackToMain => "移回主窗口",
            Self::BackTip => "把当前标签移回主窗口（连接和传输不中断）",
            Self::ViaJump => "（经跳板）",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Connected => "Connected",
            Self::Disconnected => "Disconnected",
            Self::EmptyTab => "Empty Tab",
            Self::ExpandSidebar => "Expand Sidebar",
            Self::CollapseSidebar => "Collapse Sidebar",
            Self::Hosts => "Hosts",
            Self::AiAssistant => "AI Assistant (Ctrl+Shift+A)",
            Self::Files => "Files (Ctrl+Shift+F)",
            Self::FilesSshOnly => "Files (SSH hosts only)",
            Self::Settings => "Settings & Management (Ctrl+,)",
            Self::NewTab => "New Tab (Ctrl+Shift+T)",
            Self::AiRunning => "AI is running commands in this terminal",
            Self::CloseTab => "Close Tab",
            Self::BackToMain => "Move Back to Main Window",
            Self::BackTip => {
                "Move the current tab back to the main window (connection and transfers keep running)"
            }
            Self::ViaJump => " (via jump host)",
        }
    }
}

fn status_title(status: TabStatus) -> &'static str {
    match status {
        TabStatus::Connecting => t(TitlebarMsg::Connecting),
        TabStatus::Connected => t(TitlebarMsg::Connected),
        TabStatus::Disconnected => t(TitlebarMsg::Disconnected),
        TabStatus::Empty => t(TitlebarMsg::EmptyTab),
    }
}

/// 标题栏左侧图标块里的一项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LeftIcon {
    Sidebar,
    HostPage,
    Ai,
    Files,
    Settings,
}

/// 左侧块从左到右的图标：侧栏开关、主机页、AI、文件；侧栏折叠时左下角的入口跟着看不见，
/// 末尾补一个设置与管理的齿轮，保证总有入口。
pub(super) fn left_icons(sidebar_collapsed: bool) -> Vec<LeftIcon> {
    let mut icons = vec![
        LeftIcon::Sidebar,
        LeftIcon::HostPage,
        LeftIcon::Ai,
        LeftIcon::Files,
    ];
    if sidebar_collapsed {
        icons.push(LeftIcon::Settings);
    }
    icons
}

/// 左侧块的内容宽度：左右内边距加图标和间距。侧栏展开时块和侧栏同宽（最窄 `TREE_MIN_WIDTH`），
/// 必须放得下，否则图标会溢出或被挤变形。
pub(super) fn left_block_width(count: usize) -> f32 {
    LEFT_PAD_LEFT
        + LEFT_PAD_RIGHT
        + ICON_SIZE * count as f32
        + ICON_GAP * count.saturating_sub(1) as f32
}

/// 标题栏上的小图标按钮：`occlude` 把拖动区挖掉；`enabled == false` 时只显示灰的和提示。
fn icon_button(
    id: &'static str,
    icon: impl Into<Icon>,
    tooltip: &'static str,
    active: bool,
    enabled: bool,
    tokens: &Tokens,
) -> Stateful<Div> {
    let hover = tokens.hover;
    let text = tokens.text;
    let mut muted = tokens.text_muted;
    if !enabled {
        muted.a = 0.45;
    }
    div()
        .id(id)
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .size(px(ICON_SIZE))
        .flex_none()
        .rounded_md()
        .text_color(if active { text } else { muted })
        .when(active, |d| d.bg(hover))
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(move |style| style.bg(hover).text_color(text))
        })
        .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
        .child(icon.into().size_4())
}

impl MainWindow {
    pub(super) fn render_title_bar(
        &mut self,
        tokens: &Tokens,
        aligned_tree_width: Option<f32>,
        window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        let view = self.nav.view;
        let infos: Vec<TabInfo> = self.workspace.read(cx).tab_infos();
        let active = self.workspace.read(cx).active_index();
        let tab_count = infos.len();
        // 标签条的溢出处理：当前标签变了滚到可见；数一下有几个没露全（上一帧量到的位置）。
        let entries = self.tab_entries(&infos, cx);
        let active_entry = self.active_entry(&entries, &infos, active);
        let hidden_before = self.tab_strip.hidden;
        let hidden_now = self.tab_strip.sync(active_entry, entries.len());
        let settling = self.tab_strip.settle_frames > 0;
        self.tab_strip.settle_frames = self.tab_strip.settle_frames.saturating_sub(1);
        if hidden_now != hidden_before || settling {
            // 露不全的个数变了（窗口变宽 / 开关标签）：溢出按钮要跟着出现 / 消失，再画一帧。
            cx.notify();
        }
        let strip_scroll = self.tab_strip.scroll.clone();
        // 别的窗口正在拖标签、指针落在这个窗口的标签栏上：标签栏亮起来，松手会合并进来。
        let me = crate::tab_drag::id_of(window);
        let drop_target = crate::tab_drag::current(cx).is_some_and(|drag| {
            drag.hover == crate::tab_drag::Hover::Over(me) && drag.source != me
        });
        let drag_hint = crate::tab_drag::hint_chip(window, cx, tokens);

        // ---- 左侧块：侧栏开关、「主机」、AI、文件（折叠时再加设置齿轮） ----
        let toggle = {
            let entity = entity.clone();
            let collapsed = self.tree.collapsed;
            icon_button(
                "toggle-tree",
                if collapsed {
                    IconName::PanelLeftOpen
                } else {
                    IconName::PanelLeftClose
                },
                if collapsed {
                    t(TitlebarMsg::ExpandSidebar)
                } else {
                    t(TitlebarMsg::CollapseSidebar)
                },
                false,
                true,
                tokens,
            )
            .on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| this.toggle_tree(cx));
            })
        };
        // 主机卡片页的入口（原来带字的「会话」按钮）：在主机页上再点一次回到进来之前的页面。
        let host_page_button = {
            let entity = entity.clone();
            icon_button(
                "host-page-button",
                IconName::LayoutDashboard,
                t(TitlebarMsg::Hosts),
                view == MainView::Vault,
                true,
                tokens,
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.toggle_vault_page(window, cx));
            })
        };
        // AI 助手和文件（SFTP）：原来在标题栏右侧，现在和侧栏开关、主机页并排在左侧块里。
        let ai = {
            let entity = entity.clone();
            icon_button(
                "ai-button",
                IconName::Bot,
                t(TitlebarMsg::AiAssistant),
                self.docks.ai_open && self.nav.view == MainView::Terminal,
                true,
                tokens,
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.toggle_ai_panel(window, cx));
            })
        };
        let files = {
            let entity = entity.clone();
            // Telnet / 串口标签没有 SFTP：按钮变灰、悬停说明，点了也只提示一句。
            let sftp_ok = self.active_caps(cx).sftp;
            icon_button(
                "files-button",
                crate::icons::Ico::FolderOpen,
                if sftp_ok {
                    t(TitlebarMsg::Files)
                } else {
                    t(TitlebarMsg::FilesSshOnly)
                },
                self.docks
                    .sftp_open_for(self.workspace.read(cx).active_tab_id())
                    && self.nav.view == MainView::Terminal,
                sftp_ok,
                tokens,
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.open_files(window, cx));
            })
        };
        // 侧栏折叠时左下角的入口跟着看不见：在这里补一个设置与管理的齿轮，保证总有入口。
        let gear = {
            let entity = entity.clone();
            icon_button(
                "settings-button",
                crate::icons::gear_icon().text_color(super::welcome::brand_icon_color(cx)),
                t(TitlebarMsg::Settings),
                view == MainView::Settings,
                true,
                tokens,
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.toggle_settings(window, cx));
            })
        };
        let icons: Vec<AnyElement> = {
            let mut toggle = Some(toggle);
            let mut host_page_button = Some(host_page_button);
            let mut ai = Some(ai);
            let mut files = Some(files);
            let mut gear = Some(gear);
            left_icons(self.tree.collapsed)
                .into_iter()
                .filter_map(|icon| match icon {
                    LeftIcon::Sidebar => toggle.take().map(IntoElement::into_any_element),
                    LeftIcon::HostPage => {
                        host_page_button.take().map(IntoElement::into_any_element)
                    }
                    LeftIcon::Ai => ai.take().map(IntoElement::into_any_element),
                    LeftIcon::Files => files.take().map(IntoElement::into_any_element),
                    LeftIcon::Settings => gear.take().map(IntoElement::into_any_element),
                })
                .collect()
        };
        let left = h_flex()
            .h_full()
            .flex_none()
            // 再窄的侧栏也不能把图标挤变形：块不比内容窄。
            .min_w(px(left_block_width(icons.len())))
            .items_center()
            .gap(px(ICON_GAP))
            .pl(px(LEFT_PAD_LEFT))
            .pr(px(LEFT_PAD_RIGHT))
            .when_some(aligned_tree_width, |d, width| {
                // 主机树展开时左侧块就是树的「抬头」：宽度一样，右边那条线和树的分隔线连起来。
                d.w(px(width)).border_r_1().border_color(tokens.divider)
            })
            .children(icons);

        // ---- 会话标签 ----
        let numbers = tab_numbers(&infos);
        // 标签条：标签放不下先等比缩窄到最小宽度，再放不下就横向滚动（右边出现溢出按钮）。
        let wheel_entity = entity.clone();
        let strip = h_flex()
            .id("tab-strip")
            .flex_shrink(1.0)
            .min_w_0()
            .h_full()
            .items_end()
            .gap_0p5()
            .overflow_x_scroll()
            .track_scroll(&strip_scroll)
            .on_scroll(move |event, window, cx| {
                // 竖滚轮转成横向滚动；横向的交给 GPUI 自己的滚动。
                let (dx, dy) = super::tab_strip::wheel_deltas(event, window.line_height());
                if dy.abs() > dx.abs() {
                    wheel_entity.update(cx, |this, cx| {
                        if this.tab_strip.wheel(dx, dy) {
                            cx.notify();
                        }
                    });
                }
            })
            .children(infos.iter().enumerate().map(|(index, info)| {
                self.render_tab(
                    (index, numbers[index]),
                    info,
                    active == Some(index)
                        && view == MainView::Terminal
                        && self.docks.active_sftp.is_none()
                        && self.docks.active_editor.is_none(),
                    tab_count,
                    tokens,
                    cx,
                )
            }))
            .children(
                self.docks
                    .sftp_tabs
                    .iter()
                    .enumerate()
                    .map(|(index, tab)| self.render_sftp_tab(tab, index, tokens, cx)),
            )
            .children(
                self.docks
                    .editor_tabs
                    .iter()
                    .enumerate()
                    .map(|(index, tab)| self.render_editor_tab(tab, index, tokens, cx)),
            );
        let new_tab_button = {
            let entity = entity.clone();
            let hover = tokens.hover;
            let text = tokens.text;
            div()
                .id("new-tab")
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .size(px(28.0))
                .mb_1()
                .ml_0p5()
                .flex_none()
                .rounded_md()
                .text_color(tokens.text_muted)
                .cursor_pointer()
                .hover(move |style| style.bg(hover).text_color(text))
                .tooltip(|window, cx| Tooltip::new(t(TitlebarMsg::NewTab)).build(window, cx))
                .child(Icon::new(IconName::Plus).size_4())
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.new_tab(window, cx));
                })
        };
        let overflow_button = self.render_tab_overflow_button(tokens, cx);
        let tabs = h_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .items_end()
            .child(strip)
            .children(overflow_button)
            .child(new_tab_button);

        div()
            .id("title-bar")
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .h(px(BAR_HEIGHT))
            .w_full()
            .flex_none()
            .bg(if drop_target {
                tokens.accent_soft
            } else {
                tokens.background
            })
            .child(
                // 底边那条细线先画：当前标签后画，盖住它下面那一段，和内容连成一块。
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(1.0))
                    .bg(tokens.divider),
            )
            .child(
                h_flex()
                    .id("title-drag")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_end()
                    // macOS 的系统红黄绿画在左上角，这里让出它们的宽度，别被返回按钮压住。
                    .when(cfg!(target_os = "macos"), |row| row.pl(px(72.0)))
                    .map(window_frame::drag_region)
                    .child(left.items_end())
                    .child(tabs)
                    .children(drag_hint),
            )
            .child(window_controls(tokens, window))
    }

    fn render_tab(
        &self,
        // （标签位置，序号）：位置用来当元素 id / 菜单下标，序号是按主机编的小方块数字。
        (index, number): (usize, usize),
        info: &TabInfo,
        active: bool,
        tab_count: usize,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let dot = status_color(info.status, tokens);
        // 悬停提示「<主机名> · <状态>」：序号不是位置，别让人以为是。
        let host_name = info
            .host_id
            .and_then(|id| self.store.host(id))
            .map_or_else(|| info.title.clone(), |host| host.name.clone());
        let dot_title = badge_tooltip(&host_name, info.status);
        let title: SharedString = info.title.clone().into();
        // 标签整体的悬停提示：有地址只显示地址（标题标签上就看得见），没有地址（空标签、本地标签）退回标题。
        let tab_tip: SharedString = tab_address_tip(
            info.host_id.and_then(|id| self.store.host(id)),
            info.quick_target.as_deref(),
        )
        .map_or_else(|| title.clone(), SharedString::from);
        let group: SharedString = format!("tab-group-{}", info.id).into();
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let caps = info
            .host_id
            .map_or(super::protocol_form::Caps::ALL, |id| self.caps_of(id));
        let menu_spec = super::tab_menu::TabMenuSpec {
            index,
            tab_count,
            host_record: info.host_id.is_some_and(|id| self.store.host(id).is_some()),
            has_address: info.host_id.is_some() || info.quick_target.is_some(),
            has_host: info.host_id.is_some(),
            can_reconnect: info.can_reconnect,
            can_disconnect: info.can_disconnect,
            caps,
        };
        let menu_tokens = *tokens;
        let select = entity.clone();
        let middle = entity.clone();
        let close = entity.clone();
        let drag_entity = entity.clone();
        let drag_title = info.title.clone();
        let tab_id = info.id;
        let dragging = self.dragging_tab() == Some(crate::tab_drag::DragKey::Terminal(tab_id));
        let menu_entity = entity.clone();

        let tab = div()
            .id(("tab", index))
            .group(group.clone())
            .occlude()
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .h(px(TAB_HEIGHT))
            .min_w(px(super::tab_strip::TAB_MIN_WIDTH))
            .max_w(px(200.0))
            .pl_3()
            .pr_1p5()
            .flex_shrink(1.0)
            .rounded_t_md()
            .border_x_1()
            .border_t_1()
            .text_sm()
            .cursor_pointer()
            .when(dragging, |d| d.opacity(0.4))
            .when(active, |d| {
                d.border_color(tokens.divider)
                    .bg(tokens.background)
                    .text_color(text)
            })
            .when(!active, |d| {
                d.border_color(Hsla::transparent_black())
                    .text_color(muted)
                    .hover(move |style| style.bg(hover))
            })
            .when(active, |d| d.child(active_tab_indicator(tokens)))
            .tooltip({
                let tip = tab_tip.clone();
                move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
            })
            .child(
                // 序号小方块：数字按主机编（同一台主机的标签同一个号），底色是这个标签自己的连接状态。
                div()
                    .id(("tab-badge", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .h(px(16.0))
                    .min_w(px(16.0))
                    .px_1()
                    .flex_none()
                    .rounded_sm()
                    .bg(dot)
                    .text_color(badge_ink(info.status))
                    .text_size(px(11.0))
                    .font_weight(::gpui::FontWeight::BOLD)
                    .line_height(px(16.0))
                    .child(badge_label(number))
                    .tooltip(move |window, cx| Tooltip::new(dot_title.clone()).build(window, cx)),
            )
            .child(div().flex_1().min_w_0().truncate().child(title))
            .when(info.ai_active, |d| {
                d.child(
                    div()
                        .id(("tab-ai", index))
                        .size(px(6.0))
                        .flex_none()
                        .rounded_full()
                        .bg(tokens.accent)
                        .tooltip(|window, cx| {
                            Tooltip::new(t(TitlebarMsg::AiRunning)).build(window, cx)
                        }),
                )
            })
            .child(
                div()
                    .id(("tab-close", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.0))
                    .flex_none()
                    .rounded_sm()
                    .text_color(muted)
                    .cursor_pointer()
                    .when(!active, |d| {
                        d.opacity(0.0)
                            .group_hover(group.clone(), |style| style.opacity(0.7))
                    })
                    .hover(move |style| style.bg(hover).text_color(text).opacity(1.0))
                    .tooltip(|window, cx| Tooltip::new(t(TitlebarMsg::CloseTab)).build(window, cx))
                    .child(Icon::new(IconName::Close).size_3p5())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        close.update(cx, |this, cx| this.close_tab(index, window, cx));
                    }),
            )
            // 单击切换、双击复制标签（0.2.14：双击和右键「复制标签页」是同一个函数）。
            .on_click(move |event, window, cx| {
                if event.click_count() >= 2 {
                    select.update(cx, |this, cx| this.duplicate_tab(index, window, cx));
                } else {
                    select.update(cx, |this, cx| this.select_tab(index, window, cx));
                }
            })
            // 中键关闭。
            .on_mouse_down(MouseButton::Middle, move |_, window, cx| {
                middle.update(cx, |this, cx| this.close_tab(index, window, cx));
            })
            // 左键按下记下起点：拖出窗口 / 拖到别的窗口的标签栏上松手 = 弹出 / 合并（`tab_move`）。
            .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                let pos = (f32::from(event.position.x), f32::from(event.position.y));
                let title = drag_title.clone();
                drag_entity.update(cx, |this, _cx| this.press_terminal_tab(tab_id, title, pos));
            })
            .context_menu(move |menu, _window, _cx| {
                super::tab_menu::build_tab_menu(menu, &menu_spec, &menu_entity, &menu_tokens)
            });
        tab.into_any_element()
    }
}

/// 独立窗口（文件窗口、终端窗口）窗口宽度低于这个值时，「移回主窗口」只剩图标：文件窗口最窄 640，
/// 带字的按钮要多占约 70 像素，会把标签挤没；720 以上标签、窗口名、按钮、窗口按钮能同时放下。
pub(crate) const BACK_TEXT_MIN_WIDTH: f32 = 720.0;
/// 按钮边长（只剩图标时）/ 高度。
const BACK_BUTTON_SIZE: f32 = 28.0;

/// 独立窗口里至少有一个标签时才显示「移回主窗口」（没有标签就没有可移的东西）。
pub(crate) fn back_button_visible(tab_count: usize) -> bool {
    tab_count > 0
}

/// 窗口够宽才显示按钮上的字；窄了只剩图标（悬停提示照旧）。
pub(crate) fn back_button_shows_text(window_width: f32) -> bool {
    window_width >= BACK_TEXT_MIN_WIDTH
}

/// 独立窗口标题栏右侧、最小化按钮左边的「移回主窗口」按钮：幽灵样式，悬停底色和窗口按钮一样。
/// 点击对当前选中的标签做和标签右键「移回主窗口」同一件事（由调用方传进来同一个移回函数）。
/// 整块 `occlude`：不属于标题栏的拖动区，双击它不会触发最大化。
pub(crate) fn back_button(
    tokens: &Tokens,
    show_text: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover = tokens.hover;
    let text = tokens.text;
    h_flex().h_full().flex_none().items_center().px_1().child(
        div()
            .id("back-to-main")
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .gap_1p5()
            .h(px(BACK_BUTTON_SIZE))
            .when(show_text, |d| d.px_2p5())
            .when(!show_text, |d| d.w(px(BACK_BUTTON_SIZE)))
            .flex_none()
            .rounded_md()
            .typo(Role::FieldLabel, tokens)
            .text_color(text)
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .tooltip(|window, cx| Tooltip::new(t(TitlebarMsg::BackTip)).build(window, cx))
            .child(Icon::new(IconName::PanelLeft).size_3p5())
            .when(show_text, |d| d.child(t(TitlebarMsg::BackToMain)))
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                on_click(window, cx);
            }),
    )
}

/// 文件窗口标题栏上「移回主窗口」旁边的「并排到终端」按钮：同样的幽灵样式。`reason` 是 `Some` 时按钮置灰，
/// 悬停提示改成这句原因（这台主机没有终端标签等）；没有原因时提示是功能说明。
pub(crate) fn dock_button(
    tokens: &Tokens,
    show_text: bool,
    reason: Option<&'static str>,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover = tokens.hover;
    let text = tokens.text;
    let enabled = reason.is_none();
    let tip = reason.unwrap_or_else(super::docks::dock_tip);
    h_flex().h_full().flex_none().items_center().px_1().child(
        div()
            .id("dock-beside-terminal")
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .gap_1p5()
            .h(px(BACK_BUTTON_SIZE))
            .when(show_text, |d| d.px_2p5())
            .when(!show_text, |d| d.w(px(BACK_BUTTON_SIZE)))
            .flex_none()
            .rounded_md()
            .typo(Role::FieldLabel, tokens)
            .text_color(if enabled { text } else { tokens.text_muted })
            .when(enabled, |d| {
                d.cursor_pointer().hover(move |style| style.bg(hover))
            })
            .when(!enabled, |d| d.opacity(0.5))
            .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
            .child(Icon::new(crate::icons::Ico::Columns2).size_3p5())
            .when(show_text, |d| d.child(super::docks::dock_label()))
            .when(enabled, |d| {
                d.on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    on_click(window, cx);
                })
            }),
    )
}

/// 最小化 / 最大化（还原）/ 关闭。Windows 上点击由系统按 `WindowControlArea` 处理；Linux 客户端装饰下
/// 用点击事件实现（见 `window_frame`），合成器不支持的按钮不画，服务端装饰时整组都不画。
/// 关闭都走窗口的 `on_window_should_close`（关到托盘或退出，见 `main.rs`）。
pub(crate) fn window_controls(tokens: &Tokens, window: &Window) -> impl IntoElement {
    window_controls_with(tokens, window, true)
}

/// 同 [`window_controls`]，`with_maximize` 为假时不放最大化按钮（固定用途的小窗口，如更新窗口：只有最小化 / 关闭）。
pub(crate) fn window_controls_with(
    tokens: &Tokens,
    window: &Window,
    with_maximize: bool,
) -> impl IntoElement {
    let chrome = window_frame::chrome(window);
    let maximized = window.is_maximized();
    let shown = window_frame::buttons_for(
        window.window_controls(),
        window.is_minimizable(),
        window.is_resizable(),
        with_maximize,
    );
    let hover = tokens.hover;
    let text = tokens.text;
    let danger = tokens.danger;
    let button = move |id: &'static str,
                       area: WindowControlArea,
                       icon: IconName,
                       close: bool,
                       on_click: fn(&mut Window, &mut App)| {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(CONTROL_WIDTH))
            .h_full()
            .flex_none()
            .text_color(text)
            .hover(move |style| {
                if close {
                    style.bg(danger).text_color(::gpui::white())
                } else {
                    style.bg(hover)
                }
            })
            .child(Icon::new(icon).size_3p5())
            .map(|button| match chrome {
                // 系统按区域做命中测试，不挂事件。
                window_frame::Chrome::Native => button.window_control_area(area),
                // `occlude`：窗口边缘的缩放带伸进标题栏最上面几像素，不挡掉的话按钮上沿点了会变成拉窗口。
                _ => button
                    .cursor_pointer()
                    .occlude()
                    .on_click(move |_, window, cx| on_click(window, cx)),
            })
    };
    h_flex()
        .h_full()
        .flex_none()
        // 服务端装饰：窗口管理器已经画了一组，这里一个都不放。macOS 同理：系统自己的红黄绿就在
        // 左上角，再画一组 Windows 风格的按钮就成了两套最小化 / 关闭。
        .when(
            chrome != window_frame::Chrome::Server && !cfg!(target_os = "macos"),
            |row| {
                row.when(
                    chrome == window_frame::Chrome::Native || shown.minimize,
                    |row| {
                        row.child(button(
                            "win-min",
                            WindowControlArea::Min,
                            IconName::WindowMinimize,
                            false,
                            |window, _cx| window.minimize_window(),
                        ))
                    },
                )
                .when(
                    with_maximize && (chrome == window_frame::Chrome::Native || shown.maximize),
                    |row| {
                        row.child(button(
                            "win-max",
                            WindowControlArea::Max,
                            if maximized {
                                IconName::WindowRestore
                            } else {
                                IconName::WindowMaximize
                            },
                            false,
                            |window, _cx| window.zoom_window(),
                        ))
                    },
                )
                .child(button(
                    "win-close",
                    WindowControlArea::Close,
                    IconName::WindowClose,
                    true,
                    window_frame::request_close,
                ))
            },
        )
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        BACK_TEXT_MIN_WIDTH, LeftIcon, TabTarget, TitlebarMsg, back_button_shows_text,
        back_button_visible, badge_ink, badge_label, badge_numbers, badge_tooltip,
        left_block_width, left_icons, status_color, tab_address_tip, tab_target,
    };
    use crate::app_theme::Tokens;
    use crate::i18n::Msg as _;
    use crate::terminal_workspace::{TabInfo, TabStatus};
    use crate::window_state::TREE_MIN_WIDTH;
    use zssh_core::model::{AuthMethod, Host, Protocol, ProtocolSettings, ProxyMode};

    #[test]
    fn the_left_block_holds_sidebar_hosts_ai_and_files_in_that_order() {
        assert_eq!(
            left_icons(false),
            [
                LeftIcon::Sidebar,
                LeftIcon::HostPage,
                LeftIcon::Ai,
                LeftIcon::Files
            ]
        );
    }

    #[test]
    fn the_gear_joins_the_end_of_the_left_block_only_when_the_sidebar_is_collapsed() {
        assert!(
            !left_icons(false).contains(&LeftIcon::Settings),
            "侧栏展开时入口在左下角"
        );
        let collapsed = left_icons(true);
        assert_eq!(collapsed.last(), Some(&LeftIcon::Settings));
        assert_eq!(collapsed.len(), left_icons(false).len() + 1);
        assert_eq!(
            collapsed[..4],
            left_icons(false)[..],
            "齿轮只是追加，前四个图标的位置不变"
        );
    }

    #[test]
    fn the_back_button_shows_only_while_the_window_has_a_tab() {
        assert!(!back_button_visible(0));
        assert!(back_button_visible(1));
        assert!(back_button_visible(5));
    }

    #[test]
    fn the_back_button_drops_its_text_below_the_threshold() {
        // 文件窗口最窄 640、终端窗口最窄 480：都只剩图标；阈值上下一像素各一边。
        assert!(!back_button_shows_text(480.0));
        assert!(!back_button_shows_text(640.0));
        assert!(!back_button_shows_text(BACK_TEXT_MIN_WIDTH - 1.0));
        assert!(back_button_shows_text(BACK_TEXT_MIN_WIDTH));
        assert!(back_button_shows_text(1100.0));
    }

    #[test]
    fn the_icons_fit_in_the_narrowest_sidebar_header() {
        // 侧栏展开时左侧块和侧栏同宽（最窄 TREE_MIN_WIDTH），图标不能溢出或被挤变形。
        assert!(
            left_block_width(left_icons(false).len()) <= TREE_MIN_WIDTH,
            "{} > {TREE_MIN_WIDTH}",
            left_block_width(left_icons(false).len())
        );
        // 折叠时块按内容定宽（不和侧栏对齐），多一个齿轮也不能比最窄的侧栏还宽。
        assert!(left_block_width(left_icons(true).len()) <= TREE_MIN_WIDTH);
    }

    fn host(id: i64) -> TabTarget {
        TabTarget::Host(id)
    }

    fn quick(key: &str) -> TabTarget {
        TabTarget::Quick(key.to_owned())
    }

    #[test]
    fn the_same_host_keeps_the_same_number_and_new_hosts_take_the_next_one() {
        // A、A、B、A、C → 1、1、2、1、3
        assert_eq!(
            badge_numbers(&[host(1), host(1), host(2), host(1), host(3)]),
            [1, 1, 2, 1, 3]
        );
        assert_eq!(badge_numbers(&[]), Vec::<usize>::new());
        assert_eq!(badge_numbers(&[host(7)]), [1]);
        // 号码只按第一次出现的先后给，不是主机 id 的大小。
        assert_eq!(badge_numbers(&[host(9), host(2), host(9)]), [1, 2, 1]);
    }

    #[test]
    fn quick_connects_merge_by_user_address_and_port_and_blanks_stand_alone() {
        let tabs = [
            quick("ops@192.0.2.5:22"),
            host(1),
            quick("ops@192.0.2.5:22"),
            quick("ops@192.0.2.5:2222"),
            quick("root@192.0.2.5:22"),
            TabTarget::Blank(10),
            TabTarget::Blank(11),
            host(1),
        ];
        assert_eq!(badge_numbers(&tabs), [1, 2, 1, 3, 4, 5, 6, 2]);
    }

    #[test]
    fn a_tab_is_identified_by_host_then_quick_key_then_itself() {
        let mut info = TabInfo {
            id: 5,
            title: "x".to_owned(),
            status: TabStatus::Empty,
            host_id: None,
            quick_target: None,
            quick_key: None,
            can_reconnect: false,
            can_disconnect: false,
            ai_active: false,
        };
        assert_eq!(tab_target(&info), TabTarget::Blank(5), "空白标签各自一个号");
        info.quick_key = Some("ops@192.0.2.5:22".to_owned());
        assert_eq!(tab_target(&info), quick("ops@192.0.2.5:22"));
        info.host_id = Some(3);
        assert_eq!(tab_target(&info), host(3), "有主机记录的按主机 id");
    }

    #[test]
    fn numbers_are_recomputed_after_a_close_or_a_reorder() {
        let before = [host(1), host(2), host(1), host(3)];
        assert_eq!(badge_numbers(&before), [1, 2, 1, 3]);
        // 关掉最左边的标签：host 2 成了第一个，它的号变成 1，其余顺延。
        assert_eq!(badge_numbers(&before[1..]), [1, 2, 3]);
        // 把最后一个拖到最前：host 3 先出现，拿 1。
        assert_eq!(
            badge_numbers(&[host(3), host(1), host(2), host(1)]),
            [1, 2, 3, 2]
        );
    }

    fn tip_host(protocol: Protocol) -> Host {
        Host {
            protocol: ProtocolSettings::new(protocol),
            id: 1,
            group_id: None,
            name: "web-1".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn the_tab_tip_is_user_at_host_and_port_with_the_ports_spelled_out() {
        let mut ssh = tip_host(Protocol::Ssh);
        assert_eq!(
            tab_address_tip(Some(&ssh), None).as_deref(),
            Some("ops@192.0.2.10:22")
        );
        // 用户填的是域名就显示域名；端口不是默认值照样写。
        ssh.host = " example.com ".to_owned();
        ssh.port = 2222;
        assert_eq!(
            tab_address_tip(Some(&ssh), None).as_deref(),
            Some("ops@example.com:2222")
        );
        // IPv6 加方括号（带方括号存的老数据也一样）。
        ssh.host = "2001:db8::1".to_owned();
        assert_eq!(
            tab_address_tip(Some(&ssh), None).as_deref(),
            Some("ops@[2001:db8::1]:2222")
        );
        ssh.host = "[2001:db8::1]".to_owned();
        assert_eq!(
            tab_address_tip(Some(&ssh), None).as_deref(),
            Some("ops@[2001:db8::1]:2222")
        );
        // Telnet 同样写出 23；没有用户名就不写 @。
        let mut telnet = tip_host(Protocol::Telnet);
        telnet.port = 23;
        assert_eq!(
            tab_address_tip(Some(&telnet), None).as_deref(),
            Some("ops@192.0.2.10:23")
        );
        telnet.username = String::new();
        assert_eq!(
            tab_address_tip(Some(&telnet), None).as_deref(),
            Some("192.0.2.10:23")
        );
    }

    #[test]
    fn a_jump_host_adds_a_note_but_only_the_final_target_is_shown() {
        let mut ssh = tip_host(Protocol::Ssh);
        ssh.jump_host_id = Some(7);
        assert_eq!(
            tab_address_tip(Some(&ssh), None).as_deref(),
            Some("ops@192.0.2.10:22（经跳板）")
        );
    }

    #[test]
    fn a_serial_tab_shows_the_device_and_the_baud_rate() {
        let mut serial = tip_host(Protocol::Serial);
        let options = serial.protocol.serial.as_mut().expect("串口选项");
        options.device = "COM3".to_owned();
        options.baud_rate = 115_200;
        assert_eq!(
            tab_address_tip(Some(&serial), None).as_deref(),
            Some("COM3 · 115200")
        );
        // 设备名还没填：没有地址。
        serial.protocol.serial.as_mut().expect("串口选项").device = "  ".to_owned();
        assert_eq!(tab_address_tip(Some(&serial), None), None);
    }

    #[test]
    fn quick_connects_use_their_target_and_blank_tabs_have_no_address() {
        assert_eq!(
            tab_address_tip(None, Some("ops@192.0.2.5")).as_deref(),
            Some("ops@192.0.2.5:22")
        );
        assert_eq!(
            tab_address_tip(None, Some("ops@[2001:db8::1]:2200")).as_deref(),
            Some("ops@[2001:db8::1]:2200")
        );
        // 空标签、本地标签：没有主机也没有快速连接串；读不懂的串也当没有。
        assert_eq!(tab_address_tip(None, None), None);
        assert_eq!(tab_address_tip(None, Some("not a target")), None);
        // 主机地址空着也没有。
        let mut blank = tip_host(Protocol::Ssh);
        blank.host = String::new();
        assert_eq!(tab_address_tip(Some(&blank), None), None);
    }

    #[test]
    fn the_badge_tooltip_names_the_host_and_the_status() {
        assert_eq!(
            badge_tooltip("web-1", TabStatus::Connected).to_string(),
            "web-1 · 已连接"
        );
        assert_eq!(
            badge_tooltip("ops@192.0.2.5", TabStatus::Disconnected).to_string(),
            "ops@192.0.2.5 · 已断开"
        );
    }

    #[test]
    fn the_badge_text_is_the_number_and_two_digits_still_show() {
        assert_eq!(badge_label(1).to_string(), "1");
        assert_eq!(badge_label(9).to_string(), "9");
        assert_eq!(badge_label(10).to_string(), "10");
    }

    #[test]
    fn every_titlebar_message_keeps_its_chinese_and_has_english() {
        // 中文是原来那句，一字不改（含全角括号 / 逗号、ASCII 空格）。
        assert_eq!(TitlebarMsg::Connecting.zh(), "连接中");
        assert_eq!(TitlebarMsg::Connected.zh(), "已连接");
        assert_eq!(TitlebarMsg::Disconnected.zh(), "已断开");
        assert_eq!(TitlebarMsg::EmptyTab.zh(), "空标签");
        assert_eq!(TitlebarMsg::ExpandSidebar.zh(), "展开侧边栏");
        assert_eq!(TitlebarMsg::CollapseSidebar.zh(), "收起侧边栏");
        assert_eq!(TitlebarMsg::Hosts.zh(), "主机");
        assert_eq!(TitlebarMsg::AiAssistant.zh(), "AI 助手 (Ctrl+Shift+A)");
        assert_eq!(TitlebarMsg::Files.zh(), "文件 (Ctrl+Shift+F)");
        assert_eq!(TitlebarMsg::FilesSshOnly.zh(), "文件（仅 SSH 主机可用）");
        assert_eq!(TitlebarMsg::Settings.zh(), "设置与管理 (Ctrl+,)");
        assert_eq!(TitlebarMsg::NewTab.zh(), "新标签 (Ctrl+Shift+T)");
        assert_eq!(TitlebarMsg::AiRunning.zh(), "AI 正在这个终端里执行命令");
        assert_eq!(TitlebarMsg::CloseTab.zh(), "关闭标签");
        assert_eq!(TitlebarMsg::BackToMain.zh(), "移回主窗口");
        assert_eq!(
            TitlebarMsg::BackTip.zh(),
            "把当前标签移回主窗口（连接和传输不中断）"
        );
        assert_eq!(TitlebarMsg::ViaJump.zh(), "（经跳板）");
        for key in [
            TitlebarMsg::Connecting,
            TitlebarMsg::Connected,
            TitlebarMsg::Disconnected,
            TitlebarMsg::EmptyTab,
            TitlebarMsg::ExpandSidebar,
            TitlebarMsg::CollapseSidebar,
            TitlebarMsg::Hosts,
            TitlebarMsg::AiAssistant,
            TitlebarMsg::Files,
            TitlebarMsg::FilesSshOnly,
            TitlebarMsg::Settings,
            TitlebarMsg::NewTab,
            TitlebarMsg::AiRunning,
            TitlebarMsg::CloseTab,
            TitlebarMsg::BackToMain,
            TitlebarMsg::BackTip,
            TitlebarMsg::ViaJump,
        ] {
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
        }
    }

    #[test]
    fn the_badge_colour_follows_the_status_and_stays_readable() {
        let tokens = Tokens::for_mode(false);
        let colours: Vec<_> = [
            TabStatus::Connected,
            TabStatus::Connecting,
            TabStatus::Disconnected,
            TabStatus::Empty,
        ]
        .map(|status| status_color(status, &tokens))
        .into();
        for (at, a) in colours.iter().enumerate() {
            for b in &colours[at + 1..] {
                assert_ne!(a, b, "四种状态的颜色各不相同");
            }
        }
        assert_eq!(
            status_color(TabStatus::Connected, &tokens),
            tokens.dot_connected
        );
        // 亮底（连接中）配深字，其余配白字。
        assert_ne!(
            badge_ink(TabStatus::Connecting),
            badge_ink(TabStatus::Connected)
        );
        assert_eq!(
            badge_ink(TabStatus::Connected),
            badge_ink(TabStatus::Disconnected)
        );
    }
}
