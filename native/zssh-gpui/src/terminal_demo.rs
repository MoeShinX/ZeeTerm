//! 调试用的终端演示窗口（只在 debug 钩子下打开，见 `main.rs`）。
//!
//! 终端本身（标签、窗格、分屏、输入、鼠标、高亮）已经抽到 [`TerminalWorkspace`]，这里只
//! 剩演示用的外壳：两行工具栏、旧式的标签条、高亮/设置面板，以及跨窗口搬标签/合并标签
//! 的验证逻辑。它和主窗口 `MainWindow` 用同一个 `TerminalWorkspace`，不是另一份拷贝。

use ::gpui::{
    App, Entity, EntityId, FocusHandle, Subscription, WeakEntity, Window, WindowBounds,
    WindowOptions,
};
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use zssh_terminal::Size;

use crate::terminal_workspace::{Tab, TerminalWorkspace};

/// 新终端窗口的编号；主窗口固定用 "main"。
static NEXT_TERMINAL: AtomicU64 = AtomicU64::new(1);

/// 自动化钩子只复制一次，避免新窗口继续触发自己。
static DUPLICATED_TERMINAL: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 自动化钩子只搬一次标签，避免新窗口继续搬自己。
static MOVED_TERMINAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 自动化钩子只合并一次标签，避免两个窗口互相合并。
static MERGED_TAB: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 演示窗口的初始终端尺寸（第一次排版会按真实区域重算）。
const DEMO_SIZE: Size = Size {
    rows: 30,
    cols: 120,
};

thread_local! {
    /// 跨窗口搬标签的交接槽：GPUI 的窗口和 `open_window` 构建回调都在 UI 线程上，
    /// 标签里的会话驱动不用 `Send` 也能直接交出去。
    static PENDING_TAB: std::cell::RefCell<Option<Tab>> =
        const { std::cell::RefCell::new(None) };

    /// 活着的终端窗口：「合并标签」靠它找目标窗口。`Weak` 不延长窗口寿命。
    static WINDOWS: std::cell::RefCell<Vec<WeakEntity<TerminalDemo>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 登记当前窗口；已关闭的窗口顺手清掉，同一实体只留一份。
fn register_window(entity: WeakEntity<TerminalDemo>) {
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        windows.retain(|window| window.upgrade().is_some());
        let id = entity.entity_id();
        if windows.iter().all(|window| window.entity_id() != id) {
            windows.push(entity);
        }
    });
}

/// 当前活着的终端窗口；顺带清理已经关掉的窗口。
fn live_windows() -> Vec<Entity<TerminalDemo>> {
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        windows.retain(|window| window.upgrade().is_some());
        windows
            .iter()
            .filter_map(|window| window.upgrade())
            .collect()
    })
}

/// 除 `me` 外第一个活着的终端窗口。
fn other_window(me: EntityId) -> Option<Entity<TerminalDemo>> {
    live_windows()
        .into_iter()
        .find(|window| window.entity_id() != me)
}

/// 复制窗口前记下已经存在的窗口，钩子只认之后新出现的那个。
fn window_ids_except(me: EntityId) -> Vec<EntityId> {
    live_windows()
        .into_iter()
        .map(|window| window.entity_id())
        .filter(|id| *id != me)
        .collect()
}

/// 除 `me` 和 `earlier` 外第一个活着的终端窗口。
fn new_window_since(me: EntityId, earlier: &[EntityId]) -> Option<Entity<TerminalDemo>> {
    live_windows().into_iter().find(|window| {
        let id = window.entity_id();
        id != me && !earlier.contains(&id)
    })
}

/// 把标签放进交接槽；窗口没建起来时还能原样取回。
fn stash_tab(tab: Tab) {
    PENDING_TAB.with(|slot| {
        *slot.borrow_mut() = Some(tab);
    });
}

/// 取回交接槽里的标签；新窗口构建回调里调用。
fn take_stashed_tab() -> Option<Tab> {
    PENDING_TAB.with(|slot| slot.borrow_mut().take())
}

/// 从主机库打开一个真实 SSH 终端窗口。
pub fn open_ssh_window(cx: &mut App, host_id: i64) {
    open_ssh_window_with_factory(cx, host_id, None);
}

/// 从一个已认证的连接开新终端窗口：新窗口直接开 shell，不再握手。
pub fn open_ssh_window_with_factory(
    cx: &mut App,
    host_id: i64,
    factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
) {
    let n = NEXT_TERMINAL.fetch_add(1, Ordering::Relaxed);
    let key = format!("terminal-{n}");
    let close_key = key.clone();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::centered(
            size(::gpui::px(1100.), ::gpui::px(760.)),
            cx,
        )),
        ..crate::window_frame::app_options()
    };
    let _ = cx.open_window(options, move |window, cx| {
        let view =
            cx.new(|cx| TerminalDemo::new_ssh_with_factory(window, cx, key, host_id, factory));
        window.set_window_title(&format!("{} 终端 #{host_id}", crate::product::NAME));
        let close_view = view.clone();
        window.on_window_should_close(cx, move |_window, cx| {
            close_view.update(cx, |this, cx| this.close_all(cx));
            crate::ime::window_destroyed(&close_key);
            true
        });
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    });
}

pub struct TerminalDemo {
    workspace: Entity<TerminalWorkspace>,
    /// 工作区的焦点句柄：窗口一开就要用，不能等读 entity。
    focus: FocusHandle,
    /// 输入法恢复验证用的普通输入框。
    input: Entity<InputState>,
    /// 高亮面板：规则输入框、区分大小写、是否展开。
    pattern: Entity<InputState>,
    case_sensitive: bool,
    clear_pattern: bool,
    highlight_open: bool,
    settings_open: bool,
    /// 工作区变化时跟着重绘（标签条、状态文字都读它）。
    _observe: Subscription,
}

impl TerminalDemo {
    /// 离线演示窗口：一个回环标签，带演示输出。
    pub fn new(window: &mut Window, cx: &mut Context<Self>, ime_key: String) -> Self {
        let workspace = cx.new(|cx| TerminalWorkspace::new(window, cx, ime_key));
        let focus = workspace.read(cx).focus_handle();
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("AI 输入框：点这里测试输入法恢复"));
        let pattern =
            cx.new(|cx| InputState::new(window, cx).placeholder("高亮正则，如 error|fail"));
        let _observe = cx.observe(&workspace, |_this, _workspace, cx| cx.notify());
        workspace.update(cx, |ws, _cx| {
            ws.push_offline_tab("终端 1", DEMO_SIZE);
            ws.feed_batch(40);
            let styles = "\x1b[41m  RED BG  \x1b[0m  \x1b[7m INVERSE \x1b[0m  \x1b[4mUNDER\x1b[0m \x1b[9mSTRIKE\x1b[0m \x1b[8mHIDDEN\x1b[0m e\u{301} \x1b]8;;https://example.com\x1b\\OSC8-LINK\x1b]8;;\x1b\\\r\n";
            ws.write_active(styles.as_bytes());
            ws.refresh_all();
        });
        let this = Self {
            workspace,
            focus,
            input,
            pattern,
            case_sensitive: false,
            clear_pattern: false,
            highlight_open: false,
            settings_open: false,
            _observe,
        };
        // 登记到窗口列表；「合并标签」要能找到这个窗口。
        register_window(cx.entity().downgrade());
        this
    }

    /// 新窗口接管一个已经存在的标签：先建好界面状态，再换掉演示标签。
    fn new_with_tab(
        window: &mut Window,
        cx: &mut Context<Self>,
        ime_key: String,
        tab: Tab,
    ) -> Self {
        let this = Self::new(window, cx, ime_key);
        this.workspace
            .update(cx, |ws, _cx| ws.replace_with_tab(tab));
        // `new` 已经登记过；再登记一次让「新窗口接管标签」也走同一条路，重复会被去掉。
        register_window(cx.entity().downgrade());
        this
    }

    /// 打开一台已保存主机的真实 SSH 会话（后台连接）。
    /// 复用一条已认证连接开 SSH 窗口；`factory` 为空时按主机库重新连接。
    pub fn new_ssh_with_factory(
        window: &mut Window,
        cx: &mut Context<Self>,
        ime_key: String,
        host_id: i64,
        factory: Option<Arc<crate::ssh_backend::SshSessionFactory>>,
    ) -> Self {
        let mut this = Self::new(window, cx, ime_key);
        // 连接在后台做：窗格先显示"正在连接…"，界面不会卡住；结果由 60 ms 轮询接上，
        // 失败会把原因写在窗格里（不会退化成本地回显）。
        this.workspace.update(cx, |ws, cx| {
            ws.replace_with_host_tab(host_id, factory, DEMO_SIZE, cx);
        });
        // 截图/手动验证用：启动时直接开始记录本会话。
        if crate::dev_hooks::var_os("ZSSH_LOG").is_some() {
            this.workspace.update(cx, |ws, cx| ws.toggle_log(cx));
        }
        // 截图/手动验证用：SSH 窗口建好后立刻从它开 SFTP，验证连接交接。
        if crate::dev_hooks::var_os("ZSSH_SFTP_FROM_TERMINAL").is_some() {
            this.open_sftp_for_active(cx);
        }
        // 截图/手动验证用：从当前连接再开一个终端窗口，验证多窗口复用。
        let duplicated_here = crate::dev_hooks::var_os("ZSSH_DUPLICATE_TERMINAL").is_some()
            && !DUPLICATED_TERMINAL.swap(true, Ordering::Relaxed);
        // 合并钩子只认复制之后新出现的窗口；主窗口也是终端窗口，不能选错。
        let earlier_windows = if duplicated_here {
            window_ids_except(cx.entity().entity_id())
        } else {
            Vec::new()
        };
        if duplicated_here && let Some((host_id, factory)) = this.workspace.read(cx).active_origin()
        {
            open_ssh_window_with_factory(cx, host_id, factory);
        }
        // 截图/自动验证用：另一个窗口建好后，把本窗口的活动标签合并过去。
        // 只有发起复制的窗口算「源窗口」，否则两个窗口会互相合并。
        if duplicated_here
            && crate::dev_hooks::var_os("ZSSH_MERGE_TAB").is_some()
            && !MERGED_TAB.swap(true, Ordering::Relaxed)
        {
            // 「最后一个标签不能合并」，先补一个同连接的标签，再把原来的活动标签合并走。
            this.add_sibling_tab_keep_active(cx);
            // 另一个窗口可能还没建好：在 UI 线程上小步重试，不阻塞绘制。
            cx.spawn_in(window, async move |this, cx| {
                for _ in 0..40 {
                    cx.background_executor()
                        .timer(Duration::from_millis(50))
                        .await;
                    match this.update_in(cx, |this, _window, cx| {
                        this.merge_active_tab_into_new_window(&earlier_windows, cx)
                    }) {
                        // 合并完成，或本来就不能合并：都不用再等。
                        Ok(true) => break,
                        Ok(false) => {}
                        // 窗口已经销毁。
                        Err(_) => break,
                    }
                }
            })
            .detach();
        }
        // 截图/自动验证用：SSH 窗口建好后把活动标签搬到新窗口；只搬一次，
        // 新窗口不再触发自己。
        if crate::dev_hooks::var_os("ZSSH_MOVE_TAB").is_some()
            && !MOVED_TERMINAL.swap(true, Ordering::Relaxed)
        {
            // 「最后一个标签不能移动」，先补一个同连接的标签，再把原来的活动标签搬走。
            this.add_sibling_tab_keep_active(cx);
            this.move_active_tab_to_new_window(cx);
        }
        this
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// 关闭窗口前结束所有会话，避免 SSH 通道挂着。
    pub fn close_all(&mut self, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, _cx| ws.close_all());
    }

    /// 补一个同连接（离线则是演示）的标签，但活动标签不变。
    fn add_sibling_tab_keep_active(&mut self, cx: &mut Context<Self>) {
        let original = self.workspace.read(cx).active_index();
        self.new_tab(cx);
        if let Some(original) = original {
            self.workspace
                .update(cx, |ws, cx| ws.select_tab(original, cx));
        }
    }

    /// 「+ 标签」：当前是 SSH 会话就复用它的连接再开一个 shell，否则开一个离线演示标签。
    pub(crate) fn new_tab(&mut self, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| {
            match ws.active_origin() {
                Some((host_id, factory)) => {
                    let title = format!("终端 {}", ws.tab_count() + 1);
                    ws.open_host_tab_with_factory(host_id, title, factory, cx);
                }
                None => {
                    let title = format!("终端 {}", ws.tab_count() + 1);
                    ws.push_offline_tab(&title, DEMO_SIZE);
                    for i in 0..5 {
                        ws.write_active(format!("new tab line {i}\r\n").as_bytes());
                    }
                    cx.notify();
                }
            };
        });
    }

    /// 打开主机库窗口；同一个进程可以开多个，IME 按窗口 key 管理。
    pub(crate) fn open_hosts_window(&mut self, cx: &mut Context<Self>) {
        crate::hosts::open_hosts_window(cx);
    }

    /// 从当前 SSH 窗格打开 SFTP 窗口，直接复用这条已认证连接。
    fn open_sftp_for_active(&mut self, cx: &mut Context<Self>) {
        let Some((host_id, factory)) = self.workspace.read(cx).active_origin() else {
            self.workspace
                .update(cx, |ws, cx| ws.set_status_text("当前标签不是 SSH 会话", cx));
            return;
        };
        crate::sftp::open_sftp_window_with_factory(cx, host_id, factory);
    }

    /// 「新建终端」：当前是 SSH 会话就复用它的连接开新窗口，否则保持原来的离线演示窗口。
    fn new_terminal_window(&mut self, cx: &mut Context<Self>) {
        if let Some((host_id, factory)) = self.workspace.read(cx).active_origin() {
            open_ssh_window_with_factory(cx, host_id, factory);
            return;
        }
        let n = NEXT_TERMINAL.fetch_add(1, Ordering::Relaxed);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(
                size(::gpui::px(1100.), ::gpui::px(760.)),
                cx,
            )),
            ..crate::window_frame::app_options()
        };
        let key = format!("terminal-{n}");
        let close_key = key.clone();
        let _ = cx.open_window(options, move |window, cx| {
            let view = cx.new(|cx| TerminalDemo::new(window, cx, key));
            window.on_window_should_close(cx, move |_window, _cx| {
                crate::ime::window_destroyed(&close_key);
                true
            });
            window.focus(&view.read(cx).focus_handle(), cx);
            view
        });
    }

    /// 把活动标签搬到新窗口：直接交接同一个 `Tab`，会话不断、不重连。
    fn move_active_tab_to_new_window(&mut self, cx: &mut Context<Self>) {
        let host_id = self.workspace.read(cx).active_host_id();
        let Some((index, tab)) = self.workspace.update(cx, |ws, cx| ws.take_active_tab(cx)) else {
            self.workspace
                .update(cx, |ws, cx| ws.set_status_text("最后一个标签不能移动", cx));
            return;
        };

        stash_tab(tab);
        let n = NEXT_TERMINAL.fetch_add(1, Ordering::Relaxed);
        let key = format!("terminal-{n}");
        let close_key = key.clone();
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(
                size(::gpui::px(1100.), ::gpui::px(760.)),
                cx,
            )),
            ..crate::window_frame::app_options()
        };
        let opened = cx.open_window(options, move |window, cx| {
            let view = cx.new(|cx| match take_stashed_tab() {
                Some(tab) => TerminalDemo::new_with_tab(window, cx, key, tab),
                // 交接槽意外为空时退回普通演示窗口，不能让构建回调失败。
                None => TerminalDemo::new(window, cx, key),
            });
            let title = match host_id {
                Some(host_id) => format!("{} 终端 #{host_id}", crate::product::NAME),
                None => format!("{} 终端", crate::product::NAME),
            };
            window.set_window_title(&title);
            let close_view = view.clone();
            window.on_window_should_close(cx, move |_window, cx| {
                close_view.update(cx, |this, cx| this.close_all(cx));
                crate::ime::window_destroyed(&close_key);
                true
            });
            window.focus(&view.read(cx).focus_handle(), cx);
            window.activate_window();
            view
        });
        if opened.is_err() {
            // 窗口没建起来：把标签放回原位置，会话还在。
            if let Some(tab) = take_stashed_tab() {
                self.workspace
                    .update(cx, |ws, cx| ws.restore_tab(index, tab, cx));
            }
            self.workspace
                .update(cx, |ws, cx| ws.set_status_text("无法打开新窗口", cx));
        }
    }

    /// 把活动标签合并到另一个终端窗口：同一个 `Tab` 直接交接，会话不断、不重连。
    /// 返回是否已经结束（合并成功或本来就不能合并），自动钩子据此决定还要不要等目标窗口。
    fn merge_active_tab_into_other_window(&mut self, cx: &mut Context<Self>) -> bool {
        if self.workspace.read(cx).tab_count() <= 1 {
            self.workspace
                .update(cx, |ws, cx| ws.set_status_text("最后一个标签不能合并", cx));
            return true;
        }
        let Some(target) = other_window(cx.entity().entity_id()) else {
            self.workspace
                .update(cx, |ws, cx| ws.set_status_text("没有其它终端窗口", cx));
            return false;
        };
        self.hand_off_active_tab(target, cx);
        true
    }

    /// 自动钩子专用：只合并到复制之后新出现的窗口，避免落到主窗口上。
    fn merge_active_tab_into_new_window(
        &mut self,
        earlier: &[EntityId],
        cx: &mut Context<Self>,
    ) -> bool {
        if self.workspace.read(cx).tab_count() <= 1 {
            self.workspace
                .update(cx, |ws, cx| ws.set_status_text("最后一个标签不能合并", cx));
            return true;
        }
        let Some(target) = new_window_since(cx.entity().entity_id(), earlier) else {
            self.workspace
                .update(cx, |ws, cx| ws.set_status_text("没有其它终端窗口", cx));
            return false;
        };
        self.hand_off_active_tab(target, cx);
        true
    }

    /// 把活动标签交给 `target`：目标窗口接管同一个 `Tab`，驱动和连接原样过去。
    fn hand_off_active_tab(&mut self, target: Entity<TerminalDemo>, cx: &mut Context<Self>) {
        let Some((_, tab)) = self.workspace.update(cx, |ws, cx| ws.take_active_tab(cx)) else {
            return;
        };
        target.update(cx, |target, cx| {
            target.workspace.update(cx, |ws, cx| ws.adopt_tab(tab, cx));
        });
    }

    // ---- 高亮面板 ----

    fn add_highlight_rule_from_input(&mut self, cx: &mut Context<Self>) {
        let pattern = self.pattern.read(cx).value().trim().to_owned();
        if pattern.is_empty() {
            self.workspace.update(cx, |ws, cx| {
                ws.highlight_set_status(Some("正则不能为空".to_owned()));
                cx.notify();
            });
            return;
        }
        self.seed_highlight_rule(pattern, cx);
        self.workspace
            .update(cx, |ws, _cx| ws.highlight_set_status(None));
        self.clear_pattern = true;
        cx.notify();
    }

    /// 直接加一条规则；输入框和验证钩子共用。
    pub(crate) fn seed_highlight_rule(&mut self, pattern: String, cx: &mut Context<Self>) {
        let case_sensitive = self.case_sensitive;
        self.workspace.update(cx, |ws, cx| {
            ws.add_highlight_rule(pattern, case_sensitive, cx)
        });
    }

    pub(crate) fn toggle_highlight_panel(&mut self, cx: &mut Context<Self>) {
        self.highlight_open = !self.highlight_open;
        cx.notify();
    }

    pub(crate) fn toggle_settings_panel(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        cx.notify();
    }

    pub(crate) fn toggle_split(&mut self, cx: &mut Context<Self>) {
        self.workspace.update(cx, |ws, cx| ws.toggle_split(cx));
    }

    pub(crate) fn seed_mouse_reporting(&mut self, cx: &mut Context<Self>) {
        self.workspace
            .update(cx, |ws, cx| ws.seed_mouse_reporting(cx));
    }

    /// 高亮面板：一行编辑规则，一行规则标签。
    fn highlight_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let workspace = self.workspace.clone();
        let case_entity = entity.clone();
        let add_entity = entity.clone();
        let reset_workspace = workspace.clone();
        let rules: Vec<_> = workspace.read(cx).highlight_rules().to_vec();
        let status = workspace.read(cx).highlight_status().map(str::to_owned);
        v_flex()
            .w_full()
            .px_2()
            .py_1()
            .gap_1()
            .bg(cx.theme().tab_bar)
            .child(
                h_flex()
                    .h(::gpui::px(28.0))
                    .gap_2()
                    .items_center()
                    .child(Input::new(&self.pattern).w(::gpui::px(280.)))
                    .child(
                        Button::new("hl-case")
                            .label(if self.case_sensitive {
                                "\u{533a}\u{5206}\u{5927}\u{5c0f}\u{5199}"
                            } else {
                                "\u{5ffd}\u{7565}\u{5927}\u{5c0f}\u{5199}"
                            })
                            .on_click(move |_, _, cx| {
                                case_entity.update(cx, |this, cx| {
                                    this.case_sensitive = !this.case_sensitive;
                                    cx.notify();
                                });
                            }),
                    )
                    .child(Button::new("hl-add").label("\u{6dfb}\u{52a0}").on_click(
                        move |_, _, cx| {
                            add_entity
                                .update(cx, |this, cx| this.add_highlight_rule_from_input(cx));
                        },
                    ))
                    .child(Button::new("hl-reset").label("\u{91cd}\u{7f6e}").on_click(
                        move |_, _, cx| {
                            reset_workspace.update(cx, |ws, cx| ws.reset_highlight_rules(cx));
                        },
                    ))
                    .child(
                        div()
                            .text_sm()
                            .when(status.is_some(), |d| d.text_color(cx.theme().danger))
                            .child(status.unwrap_or_default()),
                    ),
            )
            .child(
                h_flex()
                    .h(::gpui::px(24.0))
                    .gap_1()
                    .items_center()
                    .children(rules.into_iter().map(|rule| {
                        let toggle = workspace.clone();
                        let remove = workspace.clone();
                        let id = rule.id;
                        h_flex()
                            .id(("hl-rule", id))
                            .h(::gpui::px(22.0))
                            .px_1()
                            .gap_1()
                            .items_center()
                            .bg(cx.theme().list_active)
                            .opacity(if rule.enabled { 1.0 } else { 0.45 })
                            .child(
                                div()
                                    .w(::gpui::px(8.0))
                                    .h(::gpui::px(8.0))
                                    .rounded_full()
                                    .bg(::gpui::rgb(rule.foreground)),
                            )
                            .child(
                                h_flex()
                                    .id(("hl-rule-label", id))
                                    .cursor_pointer()
                                    .child(div().text_xs().child(rule.label()))
                                    .on_click(move |_, _, cx| {
                                        toggle.update(cx, |ws, cx| {
                                            ws.toggle_highlight_rule(id, cx);
                                        });
                                    }),
                            )
                            .child(
                                div()
                                    .id(("hl-rule-remove", id))
                                    .px_1()
                                    .cursor_pointer()
                                    .child(div().text_xs().child("\u{00d7}"))
                                    .on_click(move |_, _, cx| {
                                        remove.update(cx, |ws, cx| {
                                            ws.remove_highlight_rule(id, cx);
                                        });
                                    }),
                            )
                    })),
            )
    }
}

impl Render for TerminalDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.clear_pattern {
            self.clear_pattern = false;
            self.pattern =
                cx.new(|cx| InputState::new(window, cx).placeholder("高亮正则，如 error|fail"));
        }
        let entity = cx.entity();
        let workspace = self.workspace.clone();
        let (infos, active, split, broadcast, stats, status_text) = {
            let ws = workspace.read(cx);
            (
                ws.tab_infos(),
                ws.active_index(),
                ws.is_split(),
                ws.broadcast_on(),
                ws.stats_line(),
                ws.status_text().to_owned(),
            )
        };
        let copy_ws = workspace.clone();
        let feed_ws = workspace.clone();
        let log_ws = workspace.clone();
        let theme_ws = workspace.clone();
        let reconnect_ws = workspace.clone();
        let broadcast_ws = workspace.clone();
        let tab_count = infos.len();
        let tab_entity = entity.clone();
        let new_tab_entity = entity.clone();
        let split_entity = entity.clone();
        let highlight_entity = entity.clone();
        let settings_entity = entity.clone();
        let hosts_entity = entity.clone();
        let sftp_entity = entity.clone();
        let new_terminal_entity = entity.clone();
        let move_tab_entity = entity.clone();
        let merge_tab_entity = entity.clone();

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .h(::gpui::px(40.0))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_lg()
                            .child("ZeeTerm \u{7ec8}\u{7aef}\u{539f}\u{578b}"),
                    )
                    .child(Input::new(&self.input).w(::gpui::px(220.)))
                    .child(div().text_sm().child(stats))
                    .child(div().text_sm().child(status_text)),
            )
            .child(
                h_flex()
                    .h(::gpui::px(40.0))
                    .px_2()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("feed")
                            .label("\u{7ee7}\u{7eed}\u{8f93}\u{51fa} 20 \u{884c}")
                            .on_click(move |_, _, cx| {
                                feed_ws.update(cx, |ws, cx| {
                                    ws.feed_more();
                                    cx.notify();
                                });
                            }),
                    )
                    .child(
                        Button::new("copy")
                            .label("\u{590d}\u{5236}\u{9009}\u{4e2d}")
                            .on_click(move |_, _, cx| {
                                copy_ws.update(cx, |ws, cx| ws.copy_selection(cx));
                            }),
                    )
                    .child(
                        Button::new("editor")
                            .label("\u{6253}\u{5f00}\u{7f16}\u{8f91}\u{5668}")
                            .on_click(|_, _window, cx| {
                                let options = WindowOptions {
                                    window_bounds: Some(WindowBounds::centered(
                                        size(::gpui::px(900.), ::gpui::px(620.)),
                                        cx,
                                    )),
                                    ..crate::window_frame::app_options()
                                };
                                let _ = cx.open_window(options, |window, cx| {
                                    let view = cx
                                        .new(|cx| crate::editor_demo::EditorDemo::new(window, cx));
                                    window.focus(&view.read(cx).focus_handle(), cx);
                                    view
                                });
                            }),
                    )
                    .child(Button::new("theme").label("\u{4e3b}\u{9898}").on_click(
                        move |_, _window, cx| {
                            theme_ws.update(cx, |ws, cx| ws.toggle_theme(cx));
                        },
                    ))
                    .child(
                        Button::new("new-terminal")
                            .label("\u{65b0}\u{5efa}\u{7ec8}\u{7aef}")
                            .on_click(move |_, _window, cx| {
                                new_terminal_entity
                                    .update(cx, |this, cx| this.new_terminal_window(cx));
                            }),
                    )
                    .child(
                        Button::new("move-tab")
                            .label("\u{79fb}\u{52a8}\u{6807}\u{7b7e}")
                            .on_click(move |_, _window, cx| {
                                move_tab_entity.update(cx, |this, cx| {
                                    this.move_active_tab_to_new_window(cx);
                                });
                            }),
                    )
                    .child(
                        Button::new("merge-tab")
                            .label("\u{5408}\u{5e76}\u{6807}\u{7b7e}")
                            .on_click(move |_, _window, cx| {
                                merge_tab_entity.update(cx, |this, cx| {
                                    this.merge_active_tab_into_other_window(cx);
                                });
                            }),
                    )
                    .child(Button::new("log").label("\u{8bb0}\u{5f55}").on_click(
                        move |_, _, cx| {
                            log_ws.update(cx, |ws, cx| ws.toggle_log(cx));
                        },
                    ))
                    .child(Button::new("reconnect").label("\u{91cd}\u{8fde}").on_click(
                        move |_, _, cx| {
                            reconnect_ws.update(cx, |ws, cx| ws.reconnect(cx));
                        },
                    ))
                    .child(Button::new("sftp").label("SFTP").on_click(move |_, _, cx| {
                        sftp_entity.update(cx, |this, cx| this.open_sftp_for_active(cx));
                    }))
                    .child(Button::new("hosts").label("\u{4e3b}\u{673a}").on_click(
                        move |_, _, cx| {
                            hosts_entity.update(cx, |this, cx| this.open_hosts_window(cx));
                        },
                    ))
                    .child(
                        Button::new("backup")
                            .label("\u{5907}\u{4efd}")
                            .on_click(|_, _, cx| {
                                crate::backup::open_backup_window(cx);
                            }),
                    )
                    .child(
                        Button::new("sync")
                            .label("\u{540c}\u{6b65}")
                            .on_click(|_, _, cx| {
                                crate::sync::open_sync_window(cx);
                            }),
                    )
                    .child(
                        Button::new("status")
                            .label("\u{72b6}\u{6001}")
                            .on_click(|_, _, cx| {
                                crate::status::open_status_window(cx);
                            }),
                    )
                    .child(
                        Button::new("update")
                            .label("\u{66f4}\u{65b0}")
                            .on_click(|_, _, cx| {
                                crate::update::open_update_window(cx);
                            }),
                    )
                    .child(
                        Button::new("agents")
                            .label("AI 智能体")
                            .on_click(|_, _, cx| {
                                crate::agents::open_agents_window(cx);
                            }),
                    )
                    .child(
                        Button::new("agent-chat")
                            .label("AI 对话")
                            .on_click(|_, _, cx| {
                                crate::agent_chat::open_agent_chat_window(cx);
                            }),
                    ),
            )
            .child(
                h_flex()
                    .h(::gpui::px(32.0))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .bg(cx.theme().tab_bar)
                    .children(infos.iter().enumerate().map(|(index, _info)| {
                        let select_entity = tab_entity.clone();
                        let close_entity = tab_entity.clone();
                        let is_active = Some(index) == active;
                        h_flex()
                            .id(("tab", index))
                            .h(::gpui::px(26.0))
                            .px_2()
                            .gap_1()
                            .items_center()
                            .bg(if is_active {
                                cx.theme().tab_active
                            } else {
                                cx.theme().tab
                            })
                            .text_color(if is_active {
                                cx.theme().tab_active_foreground
                            } else {
                                cx.theme().tab_foreground
                            })
                            .when(is_active, |d| d.font_weight(FontWeight::SEMIBOLD))
                            .child(
                                h_flex()
                                    .id(("tab-label", index))
                                    .cursor_pointer()
                                    .child(
                                        div()
                                            .text_sm()
                                            .child(format!("\u{7ec8}\u{7aef} {}", index + 1)),
                                    )
                                    .on_click(move |_, _, cx| {
                                        select_entity.update(cx, |this, cx| {
                                            this.workspace
                                                .update(cx, |ws, cx| ws.select_tab(index, cx));
                                        });
                                    }),
                            )
                            .child(
                                div()
                                    .id(("tab-close", index))
                                    .px_1()
                                    .cursor_pointer()
                                    .child(div().text_sm().child("\u{00d7}"))
                                    .on_click(move |_, _, cx| {
                                        // 演示窗口保留「最后一个标签不能关」，关掉就没窗格可看了。
                                        close_entity.update(cx, |this, cx| {
                                            this.workspace.update(cx, |ws, cx| {
                                                if ws.tab_count() > 1 {
                                                    ws.close_tab(index, cx);
                                                }
                                            });
                                        });
                                    }),
                            )
                    }))
                    .child(Button::new("new-tab").label("+ \u{6807}\u{7b7e}").on_click(
                        move |_, _, cx| {
                            new_tab_entity.update(cx, |this, cx| this.new_tab(cx));
                        },
                    ))
                    .child(
                        Button::new("broadcast")
                            .label(if broadcast {
                                "\u{5e7f}\u{64ad}\u{ff1a}\u{5f00}"
                            } else {
                                "\u{5e7f}\u{64ad}\u{ff1a}\u{5173}"
                            })
                            .toggled(broadcast)
                            .on_click(move |_, _, cx| {
                                broadcast_ws.update(cx, |ws, cx| ws.toggle_broadcast(cx));
                            }),
                    )
                    .child(
                        Button::new("split")
                            .label(if split {
                                "\u{5408}\u{5e76}\u{5206}\u{5c4f}"
                            } else {
                                "\u{5206}\u{5c4f}"
                            })
                            .on_click(move |_, _, cx| {
                                split_entity.update(cx, |this, cx| this.toggle_split(cx));
                            }),
                    )
                    .child(
                        Button::new("highlight")
                            .label("\u{9ad8}\u{4eae}")
                            .toggled(self.highlight_open)
                            .on_click(move |_, _, cx| {
                                highlight_entity
                                    .update(cx, |this, cx| this.toggle_highlight_panel(cx));
                            }),
                    )
                    .child(
                        Button::new("settings")
                            .label("\u{8bbe}\u{7f6e}")
                            .toggled(self.settings_open)
                            .on_click(move |_, _, cx| {
                                settings_entity.update(cx, |this, cx| {
                                    this.toggle_settings_panel(cx);
                                });
                            }),
                    ),
            )
            .when(self.highlight_open, |d| d.child(self.highlight_panel(cx)))
            .when(self.settings_open, |d| {
                d.child(
                    div()
                        .px_3()
                        .py_2()
                        .max_h(::gpui::px(360.0))
                        .child(crate::settings_view::settings_body(&workspace, cx)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(workspace.clone())
                    .when(tab_count == 0, |d| d),
            )
    }
}
