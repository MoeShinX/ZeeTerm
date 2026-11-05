//! 左侧主机树（侧栏）：抬头一行小图标（新建主机、新建分组）、搜索框、「全部主机 / 分组 / 主机 / 未分组」、
//! 属性面板、底部固定的入口区（设置与管理、凭据）、拖动调宽。
//!
//! 主机页和终端页共用同一棵树（0.2.14 也是）。单击主机选中并展开底部的只读属性，双击连接，右键菜单
//! 同主机页；分组行单击是选中、箭头才展开（双击行也展开 / 收起）；主机 / 分组可以拖动换分组、调顺序。

use std::ops::Range;

use ::gpui::{
    DragMoveEvent, HighlightStyle, MouseButton, SharedString, StyledText, WeakEntity, px,
};
use gpui_kit::base::SelectableText;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::model::Host;

use super::drag::{DragChip, DragGroup, DragHost, DragItem, DropHint, Scope};
use super::drag_scroll::ScrollArea;
use super::host_tree::{self, TreeRow};
use super::page_logic::{InfoRow, PASSWORD_MASK, host_info_rows, info_row_copy_text};
use super::protocol_form;
use super::tree_guides;
use super::vault_menus::{GroupMenuSpec, HostMenuSpec, group_menu, host_menu, ungrouped_menu};
use super::widgets::{act, icon_button, protocol_icon};
use super::{GroupSel, MainWindow};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::menu_kit;
use crate::os_icons::{self, OsIconSize};
use crate::typography::{Role, Typo as _};

/// 缩进：每层 14px，再加行内左边距。
pub(super) const INDENT: f32 = 16.0;
/// 树里一行的高度（V2-U14：13px 字配 23px 行太挤，改成 14px 字配 30px 行）。
pub(super) const ROW_HEIGHT: f32 = 30.0;
/// 侧栏底部按钮的热区高度。
const FOOTER_BUTTON: f32 = 28.0;
/// 属性面板固定高度（放得下 7 行：基本 5 行 + 密码 + 跳板机 / 代理之一），切换主机时树不会跟着跳；
/// 8 行（密码 + 跳板机 + 代理都有）的极少数情况面板里可以滚动。
const INFO_HEIGHT: f32 = 196.0;

/// 属性面板这一帧要显示哪台主机：设置里的开关关着（默认）就一律不显示，开着才跟着选中的主机走。
/// 面板上的 × 只收起这一次（清掉 `info_host`），不动开关；开关只在 设置 → 外观 → 主机树 里改。
pub(super) fn info_target(enabled: bool, selected: Option<i64>) -> Option<i64> {
    selected.filter(|_| enabled)
}

/// 侧栏自己的文案（主机名、分组名、标签名这些用户数据不走这里）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SidebarMsg {
    /// 主机库是空的：提示点抬头的 +。
    EmptyHint,
    /// 抬头 / 折叠列的「新建主机」。
    NewHost,
    /// 抬头 / 折叠列的「新建分组」。
    NewGroup,
    /// 主题按钮：浅色。
    ThemeLight,
    /// 主题按钮：深色。
    ThemeDark,
    /// 主题按钮：跟随系统。
    ThemeFollowSystem,
    /// 底部设置入口的悬停提示。
    SettingsTooltip,
    /// 底部设置入口图标旁边的字。
    SettingsLabel,
    /// 底部凭据入口的悬停提示。
    Credentials,
    /// 搜索没有结果。
    NoMatch,
    /// 折叠列 / 标题栏的「展开侧边栏」。
    ExpandSidebar,
    /// 属性面板的标题。
    Properties,
    /// 属性面板右上角收起它的提示。
    CollapsePanel,
    /// 树顶部的「全部主机」。
    AllHosts,
    /// 属性面板密码行：隐藏明文。
    HidePassword,
    /// 属性面板密码行：显示明文。
    ShowPassword,
    /// 属性面板值的右键「复制」。
    Copy,
    /// 密码隐藏时右键复制项的说明。
    RevealFirst,
}

impl Msg for SidebarMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::EmptyHint => "还没有主机，点上方 + 添加",
            Self::NewHost => "新建主机",
            Self::NewGroup => "新建分组",
            Self::ThemeLight => "浅色",
            Self::ThemeDark => "深色",
            Self::ThemeFollowSystem => "跟随系统",
            Self::SettingsTooltip => "设置与管理 (Ctrl+,)",
            Self::SettingsLabel => "设置",
            Self::Credentials => "凭据",
            Self::NoMatch => "没有匹配的主机",
            Self::ExpandSidebar => "展开侧边栏",
            Self::Properties => "属性",
            Self::CollapsePanel => "收起",
            Self::AllHosts => "全部主机",
            Self::HidePassword => "隐藏密码",
            Self::ShowPassword => "显示密码",
            Self::Copy => "复制",
            Self::RevealFirst => "先点眼睛显示",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::EmptyHint => "No hosts yet — click + above to add one",
            Self::NewHost => "New Host",
            Self::NewGroup => "New Group",
            Self::ThemeLight => "Light",
            Self::ThemeDark => "Dark",
            Self::ThemeFollowSystem => "Follow System",
            Self::SettingsTooltip => "Settings & Management (Ctrl+,)",
            Self::SettingsLabel => "Settings",
            Self::Credentials => "Credentials",
            Self::NoMatch => "No matching hosts",
            Self::ExpandSidebar => "Expand Sidebar",
            Self::Properties => "Properties",
            Self::CollapsePanel => "Collapse",
            Self::AllHosts => "All Hosts",
            Self::HidePassword => "Hide Password",
            Self::ShowPassword => "Show Password",
            Self::Copy => "Copy",
            Self::RevealFirst => "Click the eye to show",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeaderKind {
    NewHost,
    NewGroup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FooterKind {
    Settings,
    Credentials,
}

/// 抬头上的一个图标按钮：稳定 id、悬停提示、点了做什么。
struct HeaderAction {
    id: &'static str,
    tooltip: SidebarMsg,
    kind: HeaderKind,
}

impl HeaderAction {
    fn icon(&self) -> Icon {
        match self.kind {
            HeaderKind::NewHost => Icon::new(IconName::Plus),
            HeaderKind::NewGroup => Icon::new(crate::icons::Ico::FolderPlus),
        }
    }
}

/// 侧栏抬头：只有这两个图标（靠右），都有悬停提示。
const HEADER_ACTIONS: [HeaderAction; 2] = [
    HeaderAction {
        id: "tree-new-host",
        tooltip: SidebarMsg::NewHost,
        kind: HeaderKind::NewHost,
    },
    HeaderAction {
        id: "tree-new-group",
        tooltip: SidebarMsg::NewGroup,
        kind: HeaderKind::NewGroup,
    },
];

struct FooterAction {
    id: &'static str,
    tooltip: SidebarMsg,
    /// 图标旁边的字（设置按钮是「图标 + 设置」，钥匙只有图标）。
    label: Option<SidebarMsg>,
    kind: FooterKind,
}

impl FooterAction {
    fn icon(&self) -> Icon {
        match self.kind {
            FooterKind::Settings => crate::icons::gear_icon(),
            FooterKind::Credentials => Icon::new(crate::icons::Ico::KeyRound),
        }
    }
}

/// 侧栏底部右侧的主题三按钮：浅色 / 深色 / 跟随系统（和「设置 → 外观」是同一个设置项）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeChoice {
    Light,
    Dark,
    FollowSystem,
}

pub const THEME_CHOICES: [ThemeChoice; 3] = [
    ThemeChoice::Light,
    ThemeChoice::Dark,
    ThemeChoice::FollowSystem,
];

impl ThemeChoice {
    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Light => t(SidebarMsg::ThemeLight),
            Self::Dark => t(SidebarMsg::ThemeDark),
            Self::FollowSystem => t(SidebarMsg::ThemeFollowSystem),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Light => "sidebar-theme-light",
            Self::Dark => "sidebar-theme-dark",
            Self::FollowSystem => "sidebar-theme-auto",
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Light => Icon::new(IconName::Sun),
            Self::Dark => Icon::new(IconName::Moon),
            Self::FollowSystem => Icon::new(crate::icons::Ico::Monitor),
        }
    }

    /// 当前设置对应哪一项高亮（跟随系统时只亮「跟随系统」，不管系统现在是浅还是深）。
    pub fn current(follow_system: bool, dark: bool) -> Self {
        if follow_system {
            Self::FollowSystem
        } else if dark {
            Self::Dark
        } else {
            Self::Light
        }
    }
}

/// 侧栏底部左下角：设置与管理、凭据直达。
const FOOTER_ACTIONS: [FooterAction; 2] = [
    FooterAction {
        id: "sidebar-settings",
        tooltip: SidebarMsg::SettingsTooltip,
        label: Some(SidebarMsg::SettingsLabel),
        kind: FooterKind::Settings,
    },
    FooterAction {
        id: "sidebar-credentials",
        tooltip: SidebarMsg::Credentials,
        label: None,
        kind: FooterKind::Credentials,
    },
];

/// 名字里搜索词（小写）出现的位置（字节范围）；转小写后长度变了的少数字符就不标，宁可不标也别标错。
pub fn match_ranges(text: &str, term: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if term.is_empty() {
        return out;
    }
    let lower = text.to_lowercase();
    if lower.len() != text.len() {
        return out;
    }
    let mut from = 0;
    while let Some(at) = lower[from..].find(term) {
        let start = from + at;
        let end = start + term.len();
        if !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            break;
        }
        out.push(start..end);
        from = end;
    }
    out
}

/// 名字没匹配、是靠地址 / 用户名 / 标签匹配到的：返回匹配的那一项，让用户看得出为什么在结果里。
pub fn match_hint(host: &Host, term: &str) -> Option<String> {
    if host.name.to_lowercase().contains(term) {
        return None;
    }
    let address = protocol_form::host_search_address(host);
    if address.to_lowercase().contains(term) {
        return Some(address);
    }
    if host.username.to_lowercase().contains(term) {
        return Some(host.username.clone());
    }
    host.tags
        .iter()
        .find(|tag| tag.to_lowercase().contains(term))
        .cloned()
}

fn highlighted(text: &str, term: &str, tokens: &Tokens) -> AnyElement {
    let ranges = match_ranges(text, term);
    let text: SharedString = text.to_owned().into();
    if ranges.is_empty() {
        return div().truncate().child(text).into_any_element();
    }
    let style = HighlightStyle {
        background_color: Some(tokens.accent_soft),
        ..Default::default()
    };
    div()
        .truncate()
        .child(StyledText::new(text).with_highlights(ranges.into_iter().map(|r| (r, style))))
        .into_any_element()
}

impl MainWindow {
    pub(super) fn render_tree(
        &mut self,
        tokens: &Tokens,
        width: f32,
        _window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        let query = self.tree_query.read(cx).value().to_string();
        let filter = host_tree::tree_filter(
            self.store.groups(),
            self.store.hosts(),
            &query,
            self.store.flat(),
        );
        let rows = host_tree::visible_rows(
            self.store.groups(),
            self.store.hosts(),
            filter.as_ref(),
            &self.tree.expanded,
            self.store.flat(),
        );
        let searching = filter.is_some();
        let term = filter.as_ref().map(|f| f.text.clone());
        let empty_result = filter
            .as_ref()
            .is_some_and(|f| f.total == 0 && f.group_ids.is_empty() && !f.ungrouped);
        let flat = self.store.flat();
        let cursor = self.tree.cursor_host.or_else(|| {
            // 搜索时回车会连第一台：把它也标出来，用户看得见回车会连哪台。
            filter.as_ref().and_then(|f| {
                host_tree::filtered_host_order(
                    self.store.groups(),
                    self.store.hosts(),
                    f,
                    self.store.flat(),
                )
                .first()
                .copied()
            })
        });

        // 抬头：只有靠右的两个小图标（新建主机、新建分组），没有标题文字和带字按钮。
        let top = h_flex()
            .gap_0p5()
            .items_center()
            .justify_end()
            .px_2()
            .pt_2()
            .children(HEADER_ACTIONS.iter().map(|action| {
                let entity = entity.clone();
                let kind = action.kind;
                icon_button(
                    action.id,
                    action.icon(),
                    t(action.tooltip),
                    move |_, window, cx| {
                        entity.update(cx, |this, cx| match kind {
                            HeaderKind::NewHost => this.new_host(window, cx),
                            HeaderKind::NewGroup => this.new_group(None, window, cx),
                        });
                    },
                )
            }));

        // 虚线连接线：设置里可以关；关了就不算。
        let guides = if self.store.tree_guides() {
            tree_guides::row_guides(&rows)
        } else {
            Vec::new()
        };
        let list = v_flex()
            .w_full()
            .px_2()
            .pb_2()
            .gap_px()
            .children(rows.iter().enumerate().map(|(at, row)| {
                let guide = guides.get(at).and_then(Option::as_ref);
                self.render_tree_row(
                    at,
                    row,
                    guide,
                    cursor,
                    term.as_deref(),
                    searching,
                    tokens,
                    cx,
                )
            }));

        let resize = {
            let entity_down = entity.clone();
            let entity_reset = entity.clone();
            // 6px 宽的拖动手柄，中间一根 1px 的线：平时透明（分隔线是侧栏自己的细边），悬停 / 拖动时加深作提示。
            let (idle, active) = (tokens.border, tokens.accent);
            let resizing = self.tree.resizing;
            div()
                .id("tree-resize")
                .group("tree-resize")
                .absolute()
                .top_0()
                .bottom_0()
                .right(px(-3.0))
                .w(px(6.0))
                .flex()
                .justify_center()
                .cursor_col_resize()
                .child(
                    div()
                        .w(px(1.0))
                        .h_full()
                        .when(resizing, move |d| d.bg(active))
                        .group_hover("tree-resize", move |style| style.bg(idle)),
                )
                .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                    cx.stop_propagation();
                    // 双击恢复默认宽度，单击开始拖动。
                    if event.click_count >= 2 {
                        entity_reset.update(cx, |this, cx| this.reset_tree_width(cx));
                    } else {
                        entity_down.update(cx, |this, cx| {
                            this.tree.resizing = true;
                            cx.notify();
                        });
                    }
                })
        };

        let scroll_body = if empty_result {
            div()
                .px_2()
                .py_6()
                .text_center()
                .typo(Role::Body, tokens)
                .text_color(tokens.text_muted)
                .child(t(SidebarMsg::NoMatch))
                .into_any_element()
        } else if self.store.hosts().is_empty() && self.store.groups().is_empty() {
            div()
                .px_2()
                .py_6()
                .text_center()
                .typo(Role::Body, tokens)
                .text_color(tokens.text_muted)
                .child(
                    self.store
                        .error()
                        .unwrap_or(t(SidebarMsg::EmptyHint))
                        .to_owned(),
                )
                .into_any_element()
        } else {
            list.into_any_element()
        };
        let blank_hot = matches!(self.drop_hint, Some(DropHint::IntoRoot));
        let info = info_target(self.prefs.host_info_panel, self.info_host)
            .and_then(|id| self.store.host(id))
            .map(|host| self.render_info_panel(host, tokens, cx));

        v_flex()
            .id("host-tree")
            .relative()
            .w(px(width))
            .h_full()
            .flex_none()
            .bg(tokens.background)
            .border_r_1()
            .border_color(tokens.divider)
            .child(top)
            .child(
                div().px_2().pt_2().pb_3().child(
                    Input::new(&self.tree_query)
                        .prefix(
                            Icon::new(IconName::Search)
                                .size_4()
                                .text_color(tokens.text_muted),
                        )
                        .cleanable(true),
                ),
            )
            .child(
                div()
                    .id("tree-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.drag_scroll.tree)
                    .when(flat && blank_hot, |d| d.bg(tokens.accent_soft))
                    .on_drag_move::<DragHost>(cx.listener(
                        |this, event: &DragMoveEvent<DragHost>, window, cx| {
                            this.drag_over_tree_blank(event, cx);
                            this.drag_scroll_probe(
                                ScrollArea::Tree,
                                event.event.position,
                                window,
                                cx,
                            );
                        },
                    ))
                    .on_drag_move::<DragGroup>(cx.listener(
                        |this, event: &DragMoveEvent<DragGroup>, window, cx| {
                            this.drag_scroll_probe(
                                ScrollArea::Tree,
                                event.event.position,
                                window,
                                cx,
                            );
                        },
                    ))
                    .on_drop::<DragHost>(cx.listener(|this, dragged: &DragHost, window, cx| {
                        this.drop_host_on_tree_blank(dragged, window, cx);
                    }))
                    .child(scroll_body),
            )
            .children(info)
            .child(self.render_sidebar_footer(tokens, cx))
            .child(resize)
    }

    /// 侧栏底部固定的入口区：设置与管理（齿轮）、凭据（钥匙）。侧栏折叠时这里看不见，
    /// 标题栏会补一个齿轮（见 `titlebar`）。
    fn render_sidebar_footer(&self, tokens: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        h_flex()
            .id("sidebar-footer")
            .flex_none()
            .gap_0p5()
            .items_center()
            .px_2()
            .py_1p5()
            .border_t_1()
            .border_color(tokens.divider)
            .children(FOOTER_ACTIONS.iter().map(|action| {
                let entity = entity.clone();
                let kind = action.kind;
                let brand = super::welcome::brand_icon_color(cx);
                footer_button(action, tokens, brand, move |_, window, cx| {
                    entity.update(cx, |this, cx| match kind {
                        FooterKind::Settings => this.toggle_settings(window, cx),
                        FooterKind::Credentials => this.open_settings_section(
                            crate::settings_nav::SettingsSection::Credentials,
                            window,
                            cx,
                        ),
                    });
                })
            }))
            .child(self.render_theme_buttons(tokens, cx))
    }

    /// 底部右侧的主题三按钮：当前项浅底高亮，悬停提示，点了立即切换并保存。
    fn render_theme_buttons(&self, tokens: &Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let settings = self.workspace.read(cx).settings().clone();
        let current = ThemeChoice::current(settings.follow_system, settings.dark);
        let (hover, text, muted) = (tokens.hover, tokens.text, tokens.text_muted);
        h_flex()
            .id("sidebar-theme")
            .ml_auto()
            .flex_none()
            .gap_0p5()
            .items_center()
            .children(THEME_CHOICES.iter().map(|choice| {
                let choice = *choice;
                let ws = self.workspace.clone();
                let active = choice == current;
                div()
                    .id(choice.id())
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(28.0))
                    .flex_none()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(if active { text } else { muted })
                    .when(active, |d| d.bg(hover))
                    .hover(move |style| style.bg(hover).text_color(text))
                    .tooltip(move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(choice.tooltip())
                            .build(window, cx)
                    })
                    .child(choice.icon().size_4())
                    .on_click(move |_, _window, cx| {
                        ws.update(cx, |this, cx| match choice {
                            ThemeChoice::Light => this.set_theme(false, cx),
                            ThemeChoice::Dark => this.set_theme(true, cx),
                            ThemeChoice::FollowSystem => this.set_theme_auto(cx),
                        });
                    })
            }))
    }

    /// 树收起后在主机页上留下的一列图标（终端页直接不占地方）。
    pub(super) fn render_tree_collapsed(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entity = cx.entity();
        v_flex()
            .id("host-tree-collapsed")
            .w(px(44.0))
            .h_full()
            .flex_none()
            .items_center()
            .gap_1()
            .py_3()
            .bg(tokens.background)
            .border_r_1()
            .border_color(tokens.divider)
            .child(icon_button(
                "tree-expand",
                IconName::PanelLeftOpen,
                t(SidebarMsg::ExpandSidebar),
                act(&entity, |this, _window, cx| this.toggle_tree(cx)),
            ))
            .child(icon_button(
                "tree-collapsed-new-host",
                IconName::Plus,
                t(SidebarMsg::NewHost),
                act(&entity, |this, window, cx| this.new_host(window, cx)),
            ))
            .child(icon_button(
                "tree-collapsed-new-group",
                crate::icons::Ico::FolderPlus,
                t(SidebarMsg::NewGroup),
                act(&entity, |this, window, cx| this.new_group(None, window, cx)),
            ))
    }

    /// 树底部的只读属性表（名称、主机、用户名、协议、端口，有跳板机 / 指定代理再加一行）。
    fn render_info_panel(
        &self,
        host: &Host,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let rows = host_info_rows(
            host,
            self.store.hosts(),
            self.store.proxies(),
            self.store.identities(),
        );
        let id = host.id;
        let revealed = self.info_revealed(id);
        let cells: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(at, row)| info_row(at, id, row, revealed, &entity, tokens))
            .collect();
        v_flex()
            .id("host-info")
            .flex_none()
            .h(px(INFO_HEIGHT))
            .border_t_1()
            .border_color(tokens.divider)
            .text_size(px(12.0))
            .child(
                h_flex()
                    .h(px(24.0))
                    .flex_none()
                    .pl_3()
                    .pr_1()
                    .items_center()
                    .justify_between()
                    .text_color(tokens.text_muted)
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(t(SidebarMsg::Properties)),
                    )
                    .child(icon_button(
                        "host-info-close",
                        IconName::Close,
                        t(SidebarMsg::CollapsePanel),
                        act(&entity, |this, _window, cx| this.close_info_panel(cx)),
                    )),
            )
            .child(
                div()
                    .id("host-info-rows")
                    .mx_2()
                    .mb_2()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .rounded_sm()
                            .border_1()
                            .border_color(tokens.border)
                            .overflow_hidden()
                            .children(cells),
                    ),
            )
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tree_row(
        &self,
        at: usize,
        row: &TreeRow,
        guide: Option<&tree_guides::RowGuide>,
        cursor: Option<i64>,
        term: Option<&str>,
        searching: bool,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let weak: WeakEntity<MainWindow> = entity.downgrade();
        match row {
            TreeRow::All { count } => {
                let active = self.tree.selected_group == GroupSel::All;
                tree_line(("tree-row", at), 0.0, active, tokens)
                    .child(div().w(px(16.0)).flex_none())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(t(SidebarMsg::AllHosts)),
                    )
                    .child(count_label(*count, tokens))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.click_group(GroupSel::All, cx));
                    })
                    .into_any_element()
            }
            TreeRow::Group {
                id,
                name,
                depth,
                expanded,
                count,
            } => {
                let group_id = *id;
                let sel = GroupSel::Group(group_id);
                let active = self.tree.selected_group == sel;
                let parent = self
                    .store
                    .groups()
                    .iter()
                    .find(|g| g.id == group_id)
                    .and_then(|g| g.parent_id);
                let hot =
                    matches!(self.drop_hint, Some(DropHint::IntoGroup(Some(g))) if g == group_id);
                let spec = GroupMenuSpec::new(self, group_id, searching);
                let menu_entity = entity.clone();
                let drag = DragGroup {
                    id: group_id,
                    parent,
                    name: name.clone(),
                };
                let chip_drag = drag.clone();
                let chip_weak = weak.clone();
                let toggle_entity = entity.clone();
                let dbl_entity = entity.clone();
                let el = tree_line(("tree-row", at), *depth as f32 * INDENT, active, tokens)
                    .relative()
                    .children(guide.map(|g| tree_guides::paint(g, at, guide_color(tokens))))
                    .when(hot, |d| {
                        d.bg(tokens.accent_soft)
                            .border_1()
                            .border_color(tokens.accent)
                    })
                    .children(self.group_insert_line(group_id, tokens))
                    .child(
                        div()
                            .id(("tree-toggle", at))
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex_none()
                            .rounded_sm()
                            .text_color(tokens.text_muted)
                            .when(!searching, |d| {
                                d.hover(|style| style.bg(tokens.hover))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        toggle_entity.update(cx, |this, cx| {
                                            this.toggle_group(
                                                host_tree::group_key(Some(group_id)),
                                                cx,
                                            );
                                        });
                                    })
                            })
                            .when(!searching, |d| {
                                d.child(
                                    Icon::new(if *expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size_4(),
                                )
                            }),
                    )
                    .child(crate::icons::folder_icon(*expanded, 16.0))
                    .child(match term {
                        Some(term) => highlighted(name, term, tokens),
                        None => div()
                            .truncate()
                            .child(SharedString::from(name.clone()))
                            .into_any_element(),
                    })
                    .child(div().flex_1())
                    .child(count_label(*count, tokens))
                    .on_click(move |event, _, cx| {
                        if event.click_count() >= 2 {
                            dbl_entity.update(cx, |this, cx| {
                                this.toggle_group(host_tree::group_key(Some(group_id)), cx);
                            });
                        } else {
                            entity.update(cx, |this, cx| {
                                this.tree.selected_group = sel;
                                cx.notify();
                            });
                        }
                    })
                    .on_drag(drag, move |_, _offset, _window, cx| {
                        let item = DragItem::Group(chip_drag.clone());
                        let weak = chip_weak.clone();
                        cx.new(|_| DragChip::new(weak, item))
                    })
                    .on_drag_move::<DragHost>(cx.listener(
                        move |this, event: &DragMoveEvent<DragHost>, _w, cx| {
                            this.drag_over_group_row_host(Some(group_id), event, cx);
                        },
                    ))
                    .on_drag_move::<DragGroup>(cx.listener(
                        move |this, event: &DragMoveEvent<DragGroup>, _w, cx| {
                            this.drag_over_group_row_group(group_id, parent, event, cx);
                        },
                    ))
                    .on_drop::<DragHost>(cx.listener(
                        move |this, dragged: &DragHost, window, cx| {
                            cx.stop_propagation();
                            this.drop_host_on_group(dragged, Some(group_id), window, cx);
                        },
                    ))
                    .on_drop::<DragGroup>(cx.listener(
                        move |this, dragged: &DragGroup, window, cx| {
                            cx.stop_propagation();
                            this.drop_group_on_group(dragged, group_id, window, cx);
                        },
                    ));
                el.context_menu(move |menu, _window, cx| {
                    group_menu(menu, &spec, &menu_entity, &Tokens::current(cx))
                })
                .into_any_element()
            }
            TreeRow::Ungrouped { expanded, count } => {
                let sel = GroupSel::Ungrouped;
                let active = self.tree.selected_group == sel;
                let hot = matches!(self.drop_hint, Some(DropHint::IntoGroup(None)));
                let menu_entity = entity.clone();
                let toggle_entity = entity.clone();
                let el = tree_line(("tree-row", at), 0.0, active, tokens)
                    .when(hot, |d| {
                        d.bg(tokens.accent_soft)
                            .border_1()
                            .border_color(tokens.accent)
                    })
                    .child(
                        div()
                            .id(("tree-toggle", at))
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(16.0))
                            .h(px(16.0))
                            .flex_none()
                            .text_color(tokens.text_muted)
                            .when(!searching, |d| {
                                d.hover(|style| style.bg(tokens.hover))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        toggle_entity.update(cx, |this, cx| {
                                            this.toggle_group(
                                                host_tree::UNGROUPED_KEY.to_owned(),
                                                cx,
                                            );
                                        });
                                    })
                            })
                            .when(!searching, |d| {
                                d.child(
                                    Icon::new(if *expanded {
                                        IconName::ChevronDown
                                    } else {
                                        IconName::ChevronRight
                                    })
                                    .size_4(),
                                )
                            }),
                    )
                    .child(crate::icons::folder_icon(*expanded, 16.0))
                    .child(match term {
                        Some(term) => highlighted(host_tree::ungrouped_label(), term, tokens),
                        None => div()
                            .truncate()
                            .child(host_tree::ungrouped_label())
                            .into_any_element(),
                    })
                    .child(div().flex_1())
                    .child(count_label(*count, tokens))
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.click_group(sel, cx));
                    })
                    .on_drag_move::<DragHost>(cx.listener(
                        |this, event: &DragMoveEvent<DragHost>, _w, cx| {
                            this.drag_over_group_row_host(None, event, cx);
                        },
                    ))
                    .on_drop::<DragHost>(cx.listener(|this, dragged: &DragHost, window, cx| {
                        cx.stop_propagation();
                        this.drop_host_on_group(dragged, None, window, cx);
                    }));
                el.context_menu(move |menu, _window, cx| {
                    ungrouped_menu(menu, &menu_entity, &Tokens::current(cx))
                })
                .into_any_element()
            }
            TreeRow::Divider => div()
                .mx_1p5()
                .my_1()
                .h(px(1.0))
                .bg(tokens.border)
                .into_any_element(),
            TreeRow::Host { id, depth } => {
                let Some(host) = self.store.host(*id) else {
                    return div().into_any_element();
                };
                let host_id = *id;
                let selected = self.tree.selected_host == Some(host_id);
                let cursor_here = searching && cursor == Some(host_id);
                let hint = term.and_then(|term| match_hint(host, term));
                let menu = HostMenuSpec::new(
                    self,
                    host_id,
                    if searching {
                        super::page_logic::OrderState::Search
                    } else {
                        super::page_logic::OrderState::Manual
                    },
                );
                let menu_entity = entity.clone();
                let connect = entity.clone();
                let drag = DragHost {
                    id: host_id,
                    name: host.name.clone(),
                };
                let chip_drag = drag.clone();
                let el = tree_line(
                    ("tree-row", at),
                    *depth as f32 * INDENT,
                    selected || cursor_here,
                    tokens,
                )
                .relative()
                .children(guide.map(|g| tree_guides::paint(g, at, guide_color(tokens))))
                .children(self.tree_insert_line(host_id, tokens))
                .child(div().w(px(16.0)).flex_none())
                .child(host_icon(self.store.os_icon_of(host), host, tokens))
                .child(
                    div().flex_1().min_w_0().child(match term {
                        Some(term) => highlighted(&host.name, term, tokens),
                        None => div()
                            .truncate()
                            .child(SharedString::from(host.name.clone()))
                            .into_any_element(),
                    }),
                )
                .children(hint.map(|hint| {
                    div()
                        .max_w(px(96.0))
                        .flex_none()
                        .truncate()
                        .typo(Role::Caption, tokens)
                        .child(SharedString::from(hint))
                }))
                .when(
                    host.protocol.protocol != zssh_core::model::Protocol::Ssh,
                    |d| {
                        d.child(
                            div()
                                .flex_none()
                                .typo(Role::Caption, tokens)
                                .child(protocol_form::protocol_tag(host.protocol.protocol)),
                        )
                    },
                )
                .on_click(move |event, window, cx| {
                    if event.click_count() >= 2 {
                        // 双击会先后来两次单击：先取消待执行的「再点收起」，免得属性面板来回闪。
                        connect.update(cx, |this, cx| {
                            this.cancel_info_collapse();
                            this.open_host(host_id, window, cx);
                        });
                    } else {
                        connect.update(cx, |this, cx| this.tree_click_host(host_id, 1, cx));
                    }
                })
                .on_drag(drag, move |_, _offset, _window, cx| {
                    let item = DragItem::Host(chip_drag.clone());
                    let weak = weak.clone();
                    cx.new(|_| DragChip::new(weak, item))
                })
                .on_drag_move::<DragHost>(cx.listener(
                    move |this, event: &DragMoveEvent<DragHost>, _w, cx| {
                        this.drag_over_host_row(host_id, Scope::Tree, false, event, cx);
                    },
                ))
                .on_drop::<DragHost>(cx.listener(
                    move |this, dragged: &DragHost, window, cx| {
                        cx.stop_propagation();
                        this.drop_on_host_row(dragged, host_id, Scope::Tree, window, cx);
                    },
                ));
                el.context_menu(move |m, window, cx| host_menu(m, &menu, &menu_entity, window, cx))
                    .into_any_element()
            }
        }
    }

    fn tree_insert_line(&self, host_id: i64, tokens: &Tokens) -> Option<AnyElement> {
        match self.drop_hint {
            Some(DropHint::InsertHost {
                id,
                after,
                scope: Scope::Tree,
            }) if id == host_id => Some(line(after, tokens)),
            _ => None,
        }
    }

    fn group_insert_line(&self, group_id: i64, tokens: &Tokens) -> Option<AnyElement> {
        match self.drop_hint {
            Some(DropHint::InsertGroup { id, after }) if id == group_id => {
                Some(line(after, tokens))
            }
            _ => None,
        }
    }
}

/// 侧栏底部左边的一个入口：16px 图标，有 `label` 时图标右边跟一个字（设置按钮），热区高 28px，
/// 悬停有底色。
fn footer_button(
    action: &FooterAction,
    tokens: &Tokens,
    brand: ::gpui::Hsla,
    on_click: impl Fn(&::gpui::ClickEvent, &mut ::gpui::Window, &mut ::gpui::App) + 'static,
) -> impl IntoElement {
    let (hover, text, muted) = (tokens.hover, tokens.text, tokens.text_muted);
    let tip = t(action.tooltip);
    div()
        .id(action.id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .gap_2()
        .h(px(FOOTER_BUTTON))
        .min_w(px(FOOTER_BUTTON))
        .when(action.label.is_some(), |d| d.px_2p5())
        .flex_none()
        .rounded_md()
        .cursor_pointer()
        .text_color(muted)
        .hover(move |style| style.bg(hover).text_color(text))
        .tooltip(move |window, cx| {
            gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
        })
        // 设置齿轮用品牌紫（和首页标题同色系），钥匙照常跟文字色。
        .child(match action.kind {
            FooterKind::Settings => action.icon().text_color(brand).size_4(),
            FooterKind::Credentials => action.icon().size_4(),
        })
        .children(action.label.map(|label| {
            div()
                .typo(Role::Body, tokens)
                .text_color(muted)
                .child(t(label))
        }))
        .on_click(on_click)
}

/// 一条细的水平插入线，画在带 `relative` 的行里面。
fn line(after: bool, tokens: &Tokens) -> AnyElement {
    let line = div()
        .absolute()
        .left(px(4.0))
        .right(px(4.0))
        .h(px(2.0))
        .rounded_full()
        .bg(tokens.accent);
    if after { line.bottom_0() } else { line.top_0() }.into_any_element()
}

/// 连接线的颜色：次要文字色压低透明度（同 0.2.14 的 `muted-foreground / 0.4`）：1px 的小点，
/// 用区域分隔线的颜色在浅色皮肤上几乎看不见，用文字色的淡色深浅皮肤都看得清又不抢眼。
fn guide_color(tokens: &Tokens) -> ::gpui::Hsla {
    let mut color = tokens.text_muted;
    color.a = 0.55;
    color
}

/// 主机行的图标：探测出过系统的 SSH 主机是发行版图标，没探测出的 SSH 主机是 ZeeTerm Logo，
/// Telnet / 串口是协议图标。
fn host_icon(os: Option<&str>, host: &Host, tokens: &Tokens) -> AnyElement {
    os.and_then(|os| os_icons::badge(os, OsIconSize::Row))
        .or_else(|| os_icons::fallback_badge(host.protocol.protocol, OsIconSize::Row))
        .unwrap_or_else(|| {
            protocol_icon(host.protocol.protocol)
                .size_4()
                .text_color(tokens.accent)
                .into_any_element()
        })
}

/// 树里的一行：缩进、悬停和选中底色。
fn tree_line(
    id: (&'static str, usize),
    indent: f32,
    active: bool,
    tokens: &Tokens,
) -> Stateful<Div> {
    let hover = tokens.hover;
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_1p5()
        .h(px(ROW_HEIGHT))
        .w_full()
        .pl(px(8.0 + indent))
        .pr_2p5()
        .rounded_md()
        .typo(Role::Tree, tokens)
        .cursor_pointer()
        .when(active, |d| {
            d.bg(tokens.accent_soft).font_weight(FontWeight::MEDIUM)
        })
        .when(!active, |d| d.hover(move |style| style.bg(hover)))
}

/// 属性面板的一行：灰底标签 + 可以用鼠标选中复制的值（V2-U34）。
///
/// 值是 `SelectableText`：拖选、双击选词、Ctrl+C 都是窗口级的文字选区。值太长时**换行显示**（行高跟着撑开，
/// 面板自己能滚动）：带省略号的单行截断会让选区对不上可见文字，选中也复制不全。
/// 右键「复制」复制整行的值。密码行隐藏时只画圆点（不是 `SelectableText`，选不中，也没有明文可复制），
/// 点行尾小眼睛才显示明文并变成可选中。
fn info_row(
    at: usize,
    host_id: i64,
    row: InfoRow,
    revealed: Option<&str>,
    entity: &Entity<MainWindow>,
    tokens: &Tokens,
) -> AnyElement {
    let is_password = row.key == "password";
    let copy_text = info_row_copy_text(&row, revealed);
    let value: AnyElement = match (is_password, revealed) {
        (true, None) => div().child(PASSWORD_MASK).into_any_element(),
        (true, Some(secret)) => div()
            .child(SelectableText::new(
                SharedString::from(format!("host-info-secret-{host_id}")),
                SharedString::from(secret.to_owned()),
            ))
            .into_any_element(),
        (false, _) => div()
            .child(SelectableText::new(
                SharedString::from(format!("host-info-value-{host_id}-{at}")),
                SharedString::from(row.value.clone()),
            ))
            .into_any_element(),
    };
    let eye = is_password.then(|| {
        let shown = revealed.is_some();
        let entity = entity.clone();
        Button::new("host-info-password-eye")
            .ghost()
            .compact()
            .xsmall()
            .icon(if shown {
                IconName::EyeOff
            } else {
                IconName::Eye
            })
            .tooltip(if shown {
                t(SidebarMsg::HidePassword)
            } else {
                t(SidebarMsg::ShowPassword)
            })
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| {
                    this.toggle_info_password(host_id, window, cx)
                });
            })
    });
    let menu_entity = entity.clone();
    let menu_ready = copy_text.is_some();
    // 普通行把值带进菜单；密码行不带明文，点的时候再从面板状态里取（隐藏时菜单项是灰的）。
    let menu_text = if is_password { None } else { copy_text };
    h_flex()
        .min_h(px(22.0))
        .items_stretch()
        .when(at > 0, |d| d.border_t_1().border_color(tokens.border))
        .child(
            div()
                .w(px(64.0))
                .flex_none()
                .flex()
                .items_center()
                .px_2()
                .border_r_1()
                .border_color(tokens.border)
                .bg(tokens.hover)
                .text_color(tokens.text_muted)
                .child(row.label),
        )
        .child(
            h_flex()
                .id(("host-info-value", at))
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_1()
                .pl_2()
                .pr_1()
                .child(div().flex_1().min_w_0().child(value))
                .children(eye)
                .context_menu(move |menu, _window, cx| {
                    let tokens = Tokens::current(cx);
                    let line = menu_kit::row(&tokens, t(SidebarMsg::Copy), IconName::Copy);
                    let item = if !menu_ready {
                        line.disabled(true)
                            .right(Some(t(SidebarMsg::RevealFirst)))
                            .inert()
                    } else {
                        let entity = menu_entity.clone();
                        let text = menu_text.clone();
                        line.on_click(move |_, window, cx| {
                            entity.update(cx, |this, cx| match &text {
                                Some(text) => this.copy_info_value(text.clone(), window, cx),
                                None => this.copy_info_password(host_id, window, cx),
                            });
                        })
                    };
                    menu.item(item)
                }),
        )
        .into_any_element()
}

fn count_label(count: usize, tokens: &Tokens) -> impl IntoElement {
    div()
        .flex_none()
        .typo(Role::Caption, tokens)
        .child(count.to_string())
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        FOOTER_ACTIONS, HEADER_ACTIONS, SidebarMsg, THEME_CHOICES, ThemeChoice, info_target,
        match_hint, match_ranges,
    };
    use crate::i18n::{Msg as _, t};
    use std::ops::Range;
    use zssh_core::model::{AuthMethod, Host, ProxyMode};

    #[test]
    fn the_info_panel_only_shows_when_the_setting_is_on() {
        assert_eq!(
            info_target(false, Some(7)),
            None,
            "默认关：选中主机也不出面板"
        );
        assert_eq!(info_target(false, None), None);
        assert_eq!(info_target(true, Some(7)), Some(7));
        assert_eq!(info_target(true, None), None, "开着但没选主机：没有面板");
    }

    #[test]
    fn the_footer_theme_buttons_are_light_dark_and_follow_system_with_tips() {
        let tips: Vec<&str> = THEME_CHOICES.iter().map(|c| c.tooltip()).collect();
        assert_eq!(tips, ["浅色", "深色", "跟随系统"]);
    }

    #[test]
    fn the_highlighted_theme_button_follows_the_setting() {
        assert_eq!(ThemeChoice::current(false, false), ThemeChoice::Light);
        assert_eq!(ThemeChoice::current(false, true), ThemeChoice::Dark);
        assert_eq!(
            ThemeChoice::current(true, true),
            ThemeChoice::FollowSystem,
            "跟随系统时只亮它，不管系统现在是浅是深"
        );
        assert_eq!(ThemeChoice::current(true, false), ThemeChoice::FollowSystem);
    }

    fn host(name: &str, address: &str, user: &str, tags: &[&str]) -> Host {
        Host {
            protocol: zssh_core::model::ProtocolSettings::default(),
            id: 1,
            group_id: None,
            name: name.to_owned(),
            host: address.to_owned(),
            port: 22,
            username: user.to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn the_sidebar_header_is_two_icons_and_each_has_a_chinese_tooltip() {
        let tips: Vec<_> = HEADER_ACTIONS.iter().map(|a| t(a.tooltip)).collect();
        assert_eq!(
            tips,
            ["新建主机", "新建分组"],
            "抬头只有 + 和新建分组两个图标"
        );
        let ids: std::collections::HashSet<_> = HEADER_ACTIONS.iter().map(|a| a.id).collect();
        assert_eq!(ids.len(), HEADER_ACTIONS.len(), "按钮 id 不重复");
        for action in HEADER_ACTIONS.iter() {
            assert!(!t(action.tooltip).is_empty());
        }
    }

    #[test]
    fn the_sidebar_footer_has_settings_and_credentials_with_tooltips() {
        let tips: Vec<_> = FOOTER_ACTIONS.iter().map(|a| t(a.tooltip)).collect();
        assert_eq!(tips, ["设置与管理 (Ctrl+,)", "凭据"]);
        let labels: Vec<_> = FOOTER_ACTIONS.iter().map(|a| a.label.map(t)).collect();
        assert_eq!(
            labels,
            [Some("设置"), None],
            "设置是图标加文字，钥匙只有图标"
        );
    }

    #[test]
    fn tree_rows_are_14px_text_on_30_to_32px_rows_with_a_28px_footer_hit_area() {
        use crate::typography::Role;
        assert_eq!(Role::Tree.spec().size, 14.0);
        let height = super::ROW_HEIGHT;
        assert!((30.0..=32.0).contains(&height), "行高 {height}");
        assert!(height >= Role::Tree.spec().size * Role::Tree.spec().leading);
        const { assert!(super::FOOTER_BUTTON >= 28.0) };
        const { assert!(super::INDENT >= 16.0) };
    }

    #[test]
    fn the_empty_hint_points_at_the_plus_icon() {
        assert!(t(SidebarMsg::EmptyHint).contains('+'));
        assert!(
            !t(SidebarMsg::EmptyHint).contains("新建」"),
            "旧的「新建」下拉已经没有了"
        );
    }

    #[test]
    fn every_sidebar_message_keeps_its_chinese_and_has_english() {
        // 中文是原来那句，一字不改（含全角括号、ASCII 空格）。
        assert_eq!(SidebarMsg::EmptyHint.zh(), "还没有主机，点上方 + 添加");
        assert_eq!(SidebarMsg::NewHost.zh(), "新建主机");
        assert_eq!(SidebarMsg::NewGroup.zh(), "新建分组");
        assert_eq!(SidebarMsg::ThemeLight.zh(), "浅色");
        assert_eq!(SidebarMsg::ThemeDark.zh(), "深色");
        assert_eq!(SidebarMsg::ThemeFollowSystem.zh(), "跟随系统");
        assert_eq!(SidebarMsg::SettingsTooltip.zh(), "设置与管理 (Ctrl+,)");
        assert_eq!(SidebarMsg::SettingsLabel.zh(), "设置");
        assert_eq!(SidebarMsg::Credentials.zh(), "凭据");
        assert_eq!(SidebarMsg::NoMatch.zh(), "没有匹配的主机");
        assert_eq!(SidebarMsg::ExpandSidebar.zh(), "展开侧边栏");
        assert_eq!(SidebarMsg::Properties.zh(), "属性");
        assert_eq!(SidebarMsg::CollapsePanel.zh(), "收起");
        assert_eq!(SidebarMsg::AllHosts.zh(), "全部主机");
        assert_eq!(SidebarMsg::HidePassword.zh(), "隐藏密码");
        assert_eq!(SidebarMsg::ShowPassword.zh(), "显示密码");
        assert_eq!(SidebarMsg::Copy.zh(), "复制");
        assert_eq!(SidebarMsg::RevealFirst.zh(), "先点眼睛显示");
        for key in [
            SidebarMsg::EmptyHint,
            SidebarMsg::NewHost,
            SidebarMsg::NewGroup,
            SidebarMsg::ThemeLight,
            SidebarMsg::ThemeDark,
            SidebarMsg::ThemeFollowSystem,
            SidebarMsg::SettingsTooltip,
            SidebarMsg::SettingsLabel,
            SidebarMsg::Credentials,
            SidebarMsg::NoMatch,
            SidebarMsg::ExpandSidebar,
            SidebarMsg::Properties,
            SidebarMsg::CollapsePanel,
            SidebarMsg::AllHosts,
            SidebarMsg::HidePassword,
            SidebarMsg::ShowPassword,
            SidebarMsg::Copy,
            SidebarMsg::RevealFirst,
        ] {
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
        }
    }

    #[test]
    fn every_occurrence_of_the_term_is_marked_case_insensitively() {
        assert_eq!(match_ranges("Web-web", "web"), vec![0..3, 4..7]);
        assert_eq!(match_ranges("alpha", "x"), Vec::<Range<usize>>::new());
        assert!(match_ranges("alpha", "").is_empty());
        assert_eq!(
            match_ranges("测试 web", "web"),
            vec![7..10],
            "按字节范围，中文在前也对得上"
        );
    }

    #[test]
    fn a_match_hint_explains_why_a_host_with_a_different_name_is_listed() {
        let target = host("db", "192.0.2.9", "postgres", &["blue"]);
        assert_eq!(match_hint(&target, "db"), None, "名字匹配不需要解释");
        assert_eq!(match_hint(&target, "192.0").as_deref(), Some("192.0.2.9"));
        assert_eq!(match_hint(&target, "postg").as_deref(), Some("postgres"));
        assert_eq!(match_hint(&target, "blu").as_deref(), Some("blue"));
        assert_eq!(match_hint(&target, "zzz"), None);
    }
}
