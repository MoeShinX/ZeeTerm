//! 自绘标题栏：最左 logo、主机树开关、固定的「主机」标签；后面是会话标签（状态点、标题、
//! 关闭、中键关闭、双击复制、右键菜单）和「+」；右边 AI（先禁用）、凭据、主题、设置和窗口按钮。
//!
//! 结构照 0.2.14 的 `components/titlebar/TopTabs.tsx`。窗口拖动和最大化靠
//! `WindowControlArea::Drag`：条本身是拖动区，所有可点的东西都 `occlude()` 一下，把拖动区
//! 在它们身上挖掉，这样点击不会被系统当成拖标题栏；双击空白处最大化也由系统处理。

use ::gpui::{AnyElement, Hsla, MouseButton, SharedString, Stateful, WindowControlArea, px};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::nav::MainView;
use super::{CloseScope, MainWindow};
use crate::app_theme::Tokens;
use crate::terminal_workspace::{TabInfo, TabStatus};

/// 标题栏高度（0.2.14 的 `h-9`）。
pub const BAR_HEIGHT: f32 = 36.0;
/// 标签高度。
const TAB_HEIGHT: f32 = 32.0;
/// 窗口按钮宽度。
const CONTROL_WIDTH: f32 = 42.0;

/// 不同类型的标签使用同一条主题色标记，避免只靠背景色区分当前标签。
pub(super) fn active_tab_indicator(tokens: &Tokens) -> Div {
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

fn status_title(status: TabStatus) -> &'static str {
    match status {
        TabStatus::Connecting => "连接中",
        TabStatus::Connected => "已连接",
        TabStatus::Disconnected => "已断开",
        TabStatus::Empty => "空标签",
    }
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
        .size(px(28.0))
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
        let dark = tokens.dark;
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

        // ---- 左侧块：logo、主机树开关、「主机」 ----
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
                    "展开侧边栏"
                } else {
                    "收起侧边栏"
                },
                false,
                true,
                tokens,
            )
            .on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| this.toggle_tree(cx));
            })
        };
        // 在 36px 标题栏中留出呼吸空间，同时让小尺寸的终端标志更容易辨认。
        let logo = ::gpui::img(crate::brand::logo()).size(px(26.0)).flex_none();
        let vault_active = view == MainView::Vault;
        let vault_button = {
            let entity = entity.clone();
            let hover = tokens.hover;
            let text = tokens.text;
            // 半透明主题色与标题栏叠合，模拟轻柔的磨砂层。
            let mut glass = tokens.accent;
            glass.a = if tokens.dark { 0.16 } else { 0.10 };
            div()
                .id("host-page-button")
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .gap_1p5()
                .h(px(24.0))
                .px_2()
                .min_w_0()
                .rounded_md()
                .border_1()
                .text_sm()
                .cursor_pointer()
                .when(vault_active, |d| {
                    d.border_color(Hsla::transparent_black())
                        .bg(glass)
                        .text_color(text)
                })
                .when(!vault_active, |d| {
                    d.border_color(Hsla::transparent_black())
                        .text_color(tokens.text_muted)
                        .hover(move |style| style.bg(hover).text_color(text))
                })
                .tooltip(|window, cx| Tooltip::new("会话").build(window, cx))
                .child(Icon::new(IconName::LayoutDashboard).size_3p5())
                .child("会话")
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.show_view(MainView::Vault, window, cx));
                })
        };
        let left = h_flex()
            .h_full()
            .flex_none()
            .items_center()
            .gap_1()
            .pl_3()
            .pr_2()
            .when_some(aligned_tree_width, |d, width| {
                // 主机树展开时左侧块就是树的「抬头」：宽度一样，右边那条线和树的分隔线连起来。
                d.w(px(width)).border_r_1().border_color(tokens.border)
            })
            .child(logo)
            .child(toggle)
            .child(vault_button);

        // ---- 会话标签 ----
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
                    index,
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
                .tooltip(|window, cx| Tooltip::new("新标签 (Ctrl+Shift+T)").build(window, cx))
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

        // ---- 右侧：AI（先禁用）、主题、设置、窗口按钮 ----
        let ai = {
            let entity = entity.clone();
            icon_button(
                "ai-button",
                IconName::Bot,
                "AI 助手 (Ctrl+Shift+A)",
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
                    "文件 (Ctrl+Shift+F)"
                } else {
                    "文件（仅 SSH 主机可用）"
                },
                self.docks.sftp_open && self.nav.view == MainView::Terminal,
                sftp_ok,
                tokens,
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.toggle_sftp_dock(window, cx));
            })
        };
        // 凭据面板的开关（同 0.2.14 标题栏上的钥匙按钮）：开着就关，没开就开。
        let keychain = {
            let entity = entity.clone();
            icon_button(
                "keychain-button",
                crate::icons::Ico::KeyRound,
                "凭据",
                self.panels.visible() == Some(super::side_panel::KEYCHAIN),
                true,
                tokens,
            )
            .on_click(move |_, _, cx| {
                entity.update(cx, |this, cx| this.open_panel_keychain_toggle(cx));
            })
        };
        // 快捷命令栏的开关（0.2.14 的 ⌘ 按钮）；主机页上它开关的是右侧的管理面板。
        let quick = {
            let entity = entity.clone();
            let active = if view == MainView::Vault {
                self.panels.visible() == Some(super::side_panel::QUICK_COMMANDS)
            } else {
                self.quick_bar_open()
            };
            icon_button(
                "quick-button",
                crate::icons::Ico::Command,
                if view == MainView::Vault {
                    "快捷命令"
                } else {
                    "快捷命令栏"
                },
                active,
                true,
                tokens,
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.toggle_quick_bar(window, cx));
            })
        };
        // 撰写栏的开关；主机页没有终端，不显示。
        let compose = (view != MainView::Vault).then(|| {
            let entity = entity.clone();
            icon_button(
                "compose-button",
                crate::icons::Ico::SquarePen,
                "撰写栏",
                self.compose_bar_open(),
                true,
                tokens,
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.toggle_compose_bar(window, cx));
            })
        });
        // 主题入口：模式（系统 / 浅色 / 深色）+ 当前明暗下的皮肤，选完立刻生效（同 0.2.14 的 `ThemeSwitcher`）。
        let theme = {
            let ws = self.workspace.clone();
            let settings = self.workspace.read(cx).settings().clone();
            let follow = settings.follow_system;
            let chosen_skin = crate::app_theme::skin_for(
                dark,
                if dark {
                    &settings.skin_dark
                } else {
                    &settings.skin_light
                },
            )
            .id;
            let icon = if dark { IconName::Moon } else { IconName::Sun };
            div().occlude().child(
                Button::new("theme-button")
                    .ghost()
                    .compact()
                    .icon(icon)
                    .tooltip("主题")
                    .dropdown_menu(move |menu, _window, _cx| {
                        let (auto, light, darker) = (ws.clone(), ws.clone(), ws.clone());
                        let mut menu = menu
                            .item(PopupMenuItem::new("系统").checked(follow).on_click(
                                move |_, _, cx| {
                                    auto.update(cx, |this, cx| this.set_theme_auto(cx));
                                },
                            ))
                            .item(
                                PopupMenuItem::new("浅色")
                                    .checked(!follow && !dark)
                                    .on_click(move |_, _, cx| {
                                        light.update(cx, |this, cx| this.set_theme(false, cx));
                                    }),
                            )
                            .item(
                                PopupMenuItem::new("深色")
                                    .checked(!follow && dark)
                                    .on_click(move |_, _, cx| {
                                        darker.update(cx, |this, cx| this.set_theme(true, cx));
                                    }),
                            )
                            .item(PopupMenuItem::separator())
                            .item(PopupMenuItem::label(if dark {
                                "深色皮肤"
                            } else {
                                "浅色皮肤"
                            }));
                        for skin in crate::app_theme::skins_of(dark) {
                            let ws = ws.clone();
                            let id = skin.id;
                            menu = menu.item(
                                PopupMenuItem::new(skin.name)
                                    .checked(skin.id == chosen_skin)
                                    .on_click(move |_, _, cx| {
                                        ws.update(cx, |this, cx| this.set_skin(dark, id, cx));
                                    }),
                            );
                        }
                        menu
                    }),
            )
        };
        let settings = {
            let entity = entity.clone();
            icon_button(
                "settings-button",
                IconName::Settings,
                "设置 (Ctrl+,)",
                view == MainView::Settings,
                true,
                tokens,
            )
            .on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.toggle_settings(window, cx));
            })
        };
        let right = h_flex()
            .h_full()
            .flex_none()
            .items_center()
            .gap_0p5()
            .pr_1()
            .child(ai)
            .child(files)
            .child(keychain)
            .child(quick)
            .children(compose)
            .child(theme)
            .child(settings);

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
                tokens.surface
            })
            .child(
                // 底边那条细线先画：当前标签后画，盖住它下面那一段，和内容连成一块。
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .h(px(1.0))
                    .bg(tokens.border),
            )
            .child(
                h_flex()
                    .id("title-drag")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .items_end()
                    .window_control_area(WindowControlArea::Drag)
                    .child(left.items_end())
                    .child(tabs)
                    .children(drag_hint)
                    .child(right),
            )
            .child(window_controls(tokens, window.is_maximized()))
    }

    fn render_tab(
        &self,
        index: usize,
        info: &TabInfo,
        active: bool,
        tab_count: usize,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let dot = status_color(info.status, tokens);
        let dot_title = status_title(info.status);
        let title: SharedString = info.title.clone().into();
        let group: SharedString = format!("tab-group-{}", info.id).into();
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let can_reconnect = info.can_reconnect;
        let can_disconnect = info.can_disconnect;
        let has_host = info.host_id.is_some();
        let caps = info
            .host_id
            .map_or(super::protocol_form::Caps::ALL, |id| self.caps_of(id));
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
                d.border_color(tokens.border)
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
                let title = title.clone();
                move |window, cx| Tooltip::new(title.clone()).build(window, cx)
            })
            .child(
                div()
                    .id(("tab-dot", index))
                    .size(px(6.0))
                    .flex_none()
                    .rounded_full()
                    .bg(dot)
                    .tooltip(move |window, cx| Tooltip::new(dot_title).build(window, cx)),
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
                            Tooltip::new("AI 正在这个终端里执行命令").build(window, cx)
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
                    .tooltip(|window, cx| Tooltip::new("关闭标签").build(window, cx))
                    .child(Icon::new(IconName::Close).size_3())
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
                let (reconnect, disconnect, duplicate, copy, sftp, send_break) = (
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                );
                let (close, others, right, all) = (
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                );
                menu.item(
                    PopupMenuItem::new("重新连接")
                        .disabled(!can_reconnect)
                        .on_click(move |_, window, cx| {
                            reconnect.update(cx, |this, cx| this.reconnect_tab(index, window, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new("断开连接")
                        .disabled(!can_disconnect)
                        .on_click(move |_, window, cx| {
                            disconnect
                                .update(cx, |this, cx| this.disconnect_tab(index, window, cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(if caps.send_break {
                        "复制标签页（串口只能开一个）"
                    } else {
                        "复制标签页"
                    })
                    .disabled(caps.send_break)
                    .on_click(move |_, window, cx| {
                        duplicate.update(cx, |this, cx| this.duplicate_tab(index, window, cx));
                    }),
                )
                .item(PopupMenuItem::new("复制地址").disabled(!has_host).on_click(
                    move |_, window, cx| {
                        copy.update(cx, |this, cx| this.copy_tab_address(index, window, cx));
                    },
                ))
                .item(if has_host && !caps.sftp {
                    // Telnet / 串口标签：置灰并说明原因。
                    super::vault_menus::ssh_only_item("打开 SFTP", None, false, |_, _, _| {})
                } else {
                    PopupMenuItem::new("打开 SFTP")
                        .disabled(!has_host)
                        .on_click(move |_, window, cx| {
                            sftp.update(cx, |this, cx| this.open_sftp_from_tab(index, window, cx));
                        })
                })
                .when(caps.send_break, |menu| {
                    menu.item(
                        PopupMenuItem::new("发送 Break")
                            .disabled(!can_disconnect)
                            .on_click(move |_, window, cx| {
                                send_break
                                    .update(cx, |this, cx| this.send_break_tab(index, window, cx));
                            }),
                    )
                })
                .item(PopupMenuItem::separator())
                .item(PopupMenuItem::new("关闭").on_click(move |_, window, cx| {
                    close.update(cx, |this, cx| this.close_tab(index, window, cx));
                }))
                .item(
                    PopupMenuItem::new("关闭其他标签页")
                        .disabled(tab_count < 2)
                        .on_click(move |_, window, cx| {
                            others.update(cx, |this, cx| {
                                this.close_tabs(index, CloseScope::Others, window, cx);
                            });
                        }),
                )
                .item(
                    PopupMenuItem::new("关闭右侧标签页")
                        .disabled(index + 1 >= tab_count)
                        .on_click(move |_, window, cx| {
                            right.update(cx, |this, cx| {
                                this.close_tabs(index, CloseScope::Right, window, cx);
                            });
                        }),
                )
                .item(
                    PopupMenuItem::new("关闭所有标签页").on_click(move |_, window, cx| {
                        all.update(cx, |this, cx| {
                            this.close_tabs(index, CloseScope::All, window, cx);
                        });
                    }),
                )
            });
        tab.into_any_element()
    }
}

/// 最小化 / 最大化（还原）/ 关闭：点击由系统按 `WindowControlArea` 处理，关闭走窗口的
/// `on_window_should_close`（关到托盘或退出，见 `main.rs`）。
pub(crate) fn window_controls(tokens: &Tokens, maximized: bool) -> impl IntoElement {
    let hover = tokens.hover;
    let text = tokens.text;
    let danger = tokens.danger;
    let button = move |id: &'static str, area: WindowControlArea, icon: IconName, close: bool| {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .w(px(CONTROL_WIDTH))
            .h_full()
            .flex_none()
            .text_color(text)
            .window_control_area(area)
            .hover(move |style| {
                if close {
                    style.bg(danger).text_color(::gpui::white())
                } else {
                    style.bg(hover)
                }
            })
            .child(Icon::new(icon).size_3p5())
    };
    h_flex()
        .h_full()
        .flex_none()
        .child(button(
            "win-min",
            WindowControlArea::Min,
            IconName::WindowMinimize,
            false,
        ))
        .child(button(
            "win-max",
            WindowControlArea::Max,
            if maximized {
                IconName::WindowRestore
            } else {
                IconName::WindowMaximize
            },
            false,
        ))
        .child(button(
            "win-close",
            WindowControlArea::Close,
            IconName::WindowClose,
            true,
        ))
}
