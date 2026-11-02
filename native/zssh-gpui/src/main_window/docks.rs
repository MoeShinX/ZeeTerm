// Adapted from Netcatty components/SftpSidePanel.tsx (via ui/src/components/sftp/SftpDockPanel.tsx)
//! 终端右侧的 AI 面板、文件（SFTP）停靠面板，以及 SFTP 主视图标签（0.2.14 `App.tsx` 的 `DockRow`、
//! `SftpDockPanel.tsx`、`SftpTabView.tsx`）。
//!
//! - AI 面板：一个终端标签一个对话（`Entity<AgentChatDemo>`，嵌在面板里的紧凑形态），目标主机
//!   跟着各自标签的主机走；切标签不丢对话，标签关了对话跟着结束。
//! - 文件面板：跟随当前终端标签的主机，一台主机一个文件视图（`Entity<SftpDemo>`，停靠布局：远端在上、
//!   本地在下）；开的时候复用终端那条已认证连接。「在新标签中打开」把同一个视图挪到主视图标签里
//!   （连接和两栏目录原样带过去）。
//! - 三块同屏的宽度按 `dock_layout` 分配；宽度可拖、双击恢复，记在设置表里。
//! - 退出时进行中的传输 / AI 回合走同一个确认（`close_guard`，数据来自 `install_state` 登记）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use ::gpui::{Entity, MouseButton, SharedString, Window, px};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::MainWindow;
use super::bar_settings;
use super::close_guard;
use super::dock_layout::{self, DockLayout};
use super::modal::{Confirm, ConfirmAction, Modal};
use super::nav::MainView;
use super::panel_width::{self, DragStart, PanelKind};
use crate::agent_chat_demo::{AgentChatDemo, EmbedContext};
use crate::agent_host::HostTarget;
use crate::app_theme::Tokens;
use crate::icons::Ico;
use crate::sftp_demo::SftpDemo;
use crate::ssh_backend::SshSessionFactory;
use crate::typography::{Role, Typo as _};

/// 主视图里的一个 SFTP 标签。
pub struct SftpTab {
    pub id: u64,
    pub host_id: i64,
    pub title: String,
    pub view: Entity<SftpDemo>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockKind {
    Ai,
    Sftp,
}

pub struct DockState {
    pub ai_open: bool,
    pub sftp_open: bool,
    /// 每个终端标签一个对话（标签 id → 面板里的对话）。
    pub ai: HashMap<u64, Entity<AgentChatDemo>>,
    /// 每台主机一个停靠用的文件视图（主机 id → 视图）。
    pub sftp: HashMap<i64, Entity<SftpDemo>>,
    pub sftp_tabs: Vec<SftpTab>,
    /// 主视图现在显示的 SFTP 标签；`None` = 显示终端。
    pub active_sftp: Option<u64>,
    /// 主视图里的编辑器标签（远端文件编辑），和 SFTP 标签并列。
    pub editor_tabs: Vec<super::editor_tabs::EditorTab>,
    /// 主视图现在显示的编辑器标签；`None` = 不是编辑器。
    pub active_editor: Option<u64>,
    next_tab: u64,
    pub drag: Option<(DockKind, DragStart)>,
    pub ai_live: Option<f32>,
    pub sftp_live: Option<f32>,
    pub overview_open: bool,
    /// 概览面板内容区的滚动位置（驱动脚本截图时把下面的几节滚进视野）。
    pub overview_scroll: ::gpui::ScrollHandle,
    pub net: VecDeque<(f64, f64)>,
    net_host: Option<i64>,
    net_revision: u64,
    /// 文件面板里本地栏收起了没有（所有主机的面板共用一个值，存在设置表里）；`None` = 还没从设置读。
    local_hidden: Option<bool>,
    /// 跟随终端目录：后台那次探测的结果（主机 id，终端目录）送回来的通道、有没有在探测、上次探测的时间。
    follow_rx: Option<std::sync::mpsc::Receiver<(i64, Option<String>)>>,
    follow_in_flight: bool,
    follow_last: Option<std::time::Instant>,
}

impl Default for DockState {
    fn default() -> Self {
        Self {
            ai_open: false,
            sftp_open: false,
            ai: HashMap::new(),
            sftp: HashMap::new(),
            sftp_tabs: Vec::new(),
            active_sftp: None,
            editor_tabs: Vec::new(),
            active_editor: None,
            next_tab: 1,
            drag: None,
            ai_live: None,
            sftp_live: None,
            overview_open: false,
            overview_scroll: ::gpui::ScrollHandle::new(),
            net: VecDeque::new(),
            net_host: None,
            net_revision: 0,
            local_hidden: None,
            follow_rx: None,
            follow_in_flight: false,
            follow_last: None,
        }
    }
}

impl DockState {
    pub fn sftp_tab_index(&self, id: u64) -> Option<usize> {
        self.sftp_tabs.iter().position(|tab| tab.id == id)
    }

    pub(super) fn next_tab_id(&mut self) -> u64 {
        let id = self.next_tab;
        self.next_tab += 1;
        id
    }
}

fn panel_kind(kind: DockKind) -> PanelKind {
    match kind {
        DockKind::Ai => PanelKind::Ai,
        DockKind::Sftp => PanelKind::Sftp,
    }
}

fn dock_icon_button(
    id: &'static str,
    icon: Icon,
    tip: &'static str,
    tokens: &Tokens,
) -> ::gpui::Stateful<Div> {
    let hover = tokens.hover;
    let text = tokens.text;
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(26.0))
        .flex_none()
        .rounded_md()
        .cursor_pointer()
        .text_color(tokens.text_muted)
        .hover(move |style| style.bg(hover).text_color(text))
        .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
        .child(icon.size_4())
}

impl MainWindow {
    // ---- 开关 ----

    /// 终端页（不是 SFTP 标签）才有 AI / 文件面板。
    fn docks_available(&self, cx: &App) -> bool {
        self.nav.view == MainView::Terminal
            && self.docks.active_sftp.is_none()
            && self.workspace.read(cx).tab_count() > 0
    }

    pub(crate) fn toggle_ai_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.docks_available(cx) {
            self.notice("AI 面板只在终端标签下可用", window, cx);
            return;
        }
        self.docks.ai_open = !self.docks.ai_open;
        if self.docks.ai_open {
            self.sync_docks(window, cx);
            self.enforce_dock_layout(window, cx);
        }
        cx.notify();
    }

    pub(crate) fn toggle_sftp_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.docks_available(cx) {
            self.notice("文件面板只在终端标签下可用", window, cx);
            return;
        }
        // Telnet / 串口主机没有 SFTP：打开面板之前拦下来说明（已经开着的面板随时可以关）。
        if !self.docks.sftp_open && !self.active_caps(cx).sftp {
            let host = self.workspace.read(cx).active_host_id();
            self.ssh_only_notice(host, "文件面板", window, cx);
            return;
        }
        self.docks.sftp_open = !self.docks.sftp_open;
        if self.docks.sftp_open {
            self.sync_docks(window, cx);
            self.enforce_dock_layout(window, cx);
        }
        cx.notify();
    }

    // ---- 宽度 ----

    fn ai_width_stored(&self) -> f32 {
        self.docks.ai_live.unwrap_or(self.store.bars().ai_width)
    }

    fn sftp_width_stored(&self) -> f32 {
        self.docks.sftp_live.unwrap_or(self.store.bars().sftp_width)
    }

    /// 主机树展开时占的宽度：算终端最少保留宽度时要先扣掉。
    fn tree_reserved(&self) -> f32 {
        if self.tree.collapsed || !self.nav.view.has_sidebar() {
            0.0
        } else {
            crate::window_state::clamp_tree_width(self.tree.width, self.window_width)
        }
    }

    pub(super) fn dock_layout_now(&self, cx: &App) -> DockLayout {
        let visible = self.docks_available(cx);
        dock_layout::compute(dock_layout::Input {
            window_width: self.window_width,
            ai_open: visible && self.docks.ai_open,
            sftp_open: visible && self.docks.sftp_open,
            ai_desired: self.ai_width_stored(),
            sftp_desired: self.sftp_width_stored(),
            reserved: self.tree_reserved()
                + if self.panels_on_page() && self.panels.visible().is_some() {
                    self.panel_width_now(self.window_width)
                } else {
                    0.0
                },
        })
    }

    /// 两块都开、窗口又太窄：收起 AI 面板并提示一次（窗口变宽后不自动再展开）。
    fn enforce_dock_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let layout = self.dock_layout_now(cx);
        if layout.collapse_ai && self.docks.ai_open {
            self.docks.ai_open = false;
            self.notice("窗口太窄，放不下两个面板：已收起 AI 面板", window, cx);
        }
    }

    pub(super) fn docks_mouse_move(
        &mut self,
        event: &::gpui::MouseMoveEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((kind, start)) = self.docks.drag else {
            return false;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.commit_dock_width(kind, cx);
            return true;
        }
        let layout = self.dock_layout_now(cx);
        let other = match kind {
            DockKind::Ai => layout.sftp_width,
            DockKind::Sftp => layout.ai_width,
        };
        let width = panel_width::dragged_width(
            panel_kind(kind),
            start,
            f32::from(event.position.x),
            self.window_width,
            self.tree_reserved() + other,
        );
        match kind {
            DockKind::Ai => self.docks.ai_live = Some(width),
            DockKind::Sftp => self.docks.sftp_live = Some(width),
        }
        cx.notify();
        true
    }

    pub(super) fn docks_mouse_up(&mut self, cx: &mut Context<Self>) {
        if let Some((kind, _)) = self.docks.drag {
            self.commit_dock_width(kind, cx);
        }
    }

    fn commit_dock_width(&mut self, kind: DockKind, cx: &mut Context<Self>) {
        self.docks.drag = None;
        let (live, key) = match kind {
            DockKind::Ai => (self.docks.ai_live.take(), PanelKind::Ai.setting_key()),
            DockKind::Sftp => (self.docks.sftp_live.take(), PanelKind::Sftp.setting_key()),
        };
        if let Some(width) = live
            && let Err(err) = self
                .store
                .write_bar_setting(|db| bar_settings::write_number(db, key, width))
        {
            tracing::warn!(%err, "保存面板宽度失败");
        }
        cx.notify();
    }

    fn reset_dock_width(&mut self, kind: DockKind, cx: &mut Context<Self>) {
        let (key, width) = match kind {
            DockKind::Ai => (PanelKind::Ai.setting_key(), PanelKind::Ai.default_width()),
            DockKind::Sftp => (
                PanelKind::Sftp.setting_key(),
                PanelKind::Sftp.default_width(),
            ),
        };
        if let Err(err) = self
            .store
            .write_bar_setting(|db| bar_settings::write_number(db, key, width))
        {
            tracing::warn!(%err, "保存面板宽度失败");
        }
        cx.notify();
    }

    // ---- 节拍：创建 / 回收面板里的实体，把上下文送进去 ----

    fn host_targets(&self) -> Vec<HostTarget> {
        self.store
            .hosts()
            .iter()
            .map(|host| {
                let mut target =
                    HostTarget::plain(host.id, host.name.clone(), host.username.clone());
                // Telnet / 串口主机：列在对话里但不能当目标，原因和工具桥给的是同一句。
                target.refusal = host.protocol.ai_exec_refusal();
                target
            })
            .collect()
    }

    pub(super) fn sync_docks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // AI 终端登记表：聚焦窗格每个节拍标一次（幂等），AI 把「这台主机的焦点终端」认成它。
        if let Some(id) = self.workspace.read(cx).active_terminal_id() {
            crate::ai_terminals::terminals().mark_focused(&id);
        }
        // AI 卡片上点了「查看终端」：切到那个终端所在的标签（目标还没连上的，切到这台主机的标签）。
        if let Some((target, session)) = crate::agent_setup::take_view_terminal_request() {
            self.show_view(MainView::Terminal, window, cx);
            self.workspace.update(cx, |ws, cx| {
                let found = session
                    .as_deref()
                    .is_some_and(|session| ws.select_terminal(session, cx));
                if !found
                    && let Some(host_id) = zssh_core::ai::protocol::parse_host_target_id(&target)
                {
                    ws.select_host_tab(host_id, cx);
                }
            });
        }
        // 标签上的 AI 活动点：AI 在哪个终端里执行命令，那个标签亮一个点。
        let dots: Vec<(u64, bool)> = self
            .workspace
            .read(cx)
            .tab_infos()
            .iter()
            .map(|info| (info.id, info.ai_active))
            .collect();
        if dots != self.ai_dots {
            self.ai_dots = dots;
            cx.notify();
        }
        // AI 在对话里改了某台主机的记忆：主机详情里开着的记忆框跟着刷新。
        let (seen, changed) = crate::host_activity::memory_changes_since(self.memory_seen);
        self.memory_seen = seen;
        if !changed.is_empty() {
            self.refresh_memory_from_ai(&changed, window, cx);
        }
        let (tab_hosts, active_tab, active_origin) = {
            let ws = self.workspace.read(cx);
            (ws.tab_hosts(), ws.active_tab_id(), ws.active_origin())
        };
        let open_hosts: HashSet<i64> = tab_hosts.iter().filter_map(|(_, host)| *host).collect();
        let hosts = self.host_targets();

        // AI 面板里点了「去设置」/「下载」：打开设置页（那里有各智能体的路径、检测、下载）。
        if crate::agent_setup::take_settings_request() {
            self.show_view(MainView::Settings, window, cx);
        }

        // AI：标签关了对话跟着结束（实体 Drop 会收尾会话）；开着面板时当前标签补一个。
        let live: HashSet<u64> = tab_hosts.iter().map(|(id, _)| *id).collect();
        self.docks.ai.retain(|id, _| live.contains(id));
        if self.docks.ai_open
            && let Some(id) = active_tab
            && !self.docks.ai.contains_key(&id)
        {
            let height = self.store.bars().ai_input_height;
            let chat = cx.new(|cx| AgentChatDemo::new_embedded(window, cx, height));
            self.docks.ai.insert(id, chat);
            // 新标签的面板实体是节拍里补的：不通知主窗口重画，它就一直是空白，直到别的什么触发重绘。
            cx.notify();
        }
        for (tab_id, host_id) in &tab_hosts {
            let Some(chat) = self.docks.ai.get(tab_id).cloned() else {
                continue;
            };
            let current =
                host_id.and_then(|id| self.store.host(id).map(|host| (host.id, host.name.clone())));
            let ctx = EmbedContext {
                current,
                open_hosts: open_hosts.clone(),
                hosts: hosts.clone(),
            };
            chat.update(cx, |chat, cx| chat.set_context(ctx, cx));
        }

        // 文件面板：当前标签的主机补一个视图（复用终端那条已认证连接）；没有标签开着的主机、
        // 又没有未结束传输的视图回收掉（连接回到池里）。
        if self.docks.sftp_open
            && let Some((host_id, factory)) = active_origin
            && self.caps_of(host_id).sftp
            && !self.docks.sftp.contains_key(&host_id)
        {
            let hidden = self.store.bars().sftp_local_hidden;
            let view =
                cx.new(|cx| SftpDemo::new_embedded(window, cx, host_id, factory, true, hidden));
            self.docks.sftp.insert(host_id, view);
        }
        let hosts_in_tabs: HashSet<i64> =
            self.docks.sftp_tabs.iter().map(|tab| tab.host_id).collect();
        let stale: Vec<i64> = self
            .docks
            .sftp
            .iter()
            .filter(|(host, view)| {
                !open_hosts.contains(host)
                    && !hosts_in_tabs.contains(host)
                    && view.read(cx).unfinished() == 0
            })
            .map(|(host, _)| *host)
            .collect();
        for host in stale {
            if let Some(view) = self.docks.sftp.remove(&host) {
                view.update(cx, |view, _| view.shutdown_embedded());
            }
        }
        self.sync_local_hidden(window, cx);
        self.poll_follow(cx);
    }

    /// 跟随终端目录（0.2.14 `useTerminalCwd`）：停靠面板开着、这台主机的文件视图开着跟随时，每 1.5 秒
    /// 问一次焦点终端在哪个目录；上一次没回来就不再问。走终端连接上的 exec 通道，不往终端里敲东西。
    fn poll_follow(&mut self, cx: &mut Context<Self>) {
        // 先收上一次的结果。
        if let Some(rx) = &self.docks.follow_rx {
            while let Ok((host, cwd)) = rx.try_recv() {
                self.docks.follow_in_flight = false;
                if let Some(view) = self.docks.sftp.get(&host).cloned() {
                    view.update(cx, |view, cx| view.follow_to(cwd.as_deref(), cx));
                }
            }
        }
        if self.docks.follow_in_flight || !self.docks_available(cx) || !self.docks.sftp_open {
            return;
        }
        let Some(host) = self.workspace.read(cx).active_host_id() else {
            return;
        };
        let Some(view) = self.docks.sftp.get(&host) else {
            return;
        };
        let wanted = {
            let view = view.read(cx);
            view.follow() && view.is_ready()
        };
        if !wanted {
            return;
        }
        let due = self.docks.follow_last.is_none_or(|last| {
            last.elapsed() >= std::time::Duration::from_millis(crate::sftp_demo::FOLLOW_POLL_MS)
        });
        if !due {
            return;
        }
        let Some((connection, ordinal, live)) = self.workspace.read(cx).active_shell_position()
        else {
            return;
        };
        let Ok(runtime) = crate::ssh_backend::runtime() else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.docks.follow_rx = Some(rx);
        self.docks.follow_in_flight = true;
        self.docks.follow_last = Some(std::time::Instant::now());
        runtime.spawn(async move {
            let shells = zssh_core::ssh::probe_shell_cwds(&connection).await;
            let cwd =
                zssh_core::ssh::pick_shell(&shells, ordinal, live).map(|shell| shell.cwd.clone());
            let _ = tx.send((host, cwd));
        });
    }

    /// 状态栏的网速历史（概览面板的曲线）：换了主机就重新记。
    pub(super) fn record_net_history(&mut self) {
        let Some(host) = self.status_host else {
            self.docks.net.clear();
            self.docks.net_host = None;
            return;
        };
        if self.docks.net_host != Some(host) {
            self.docks.net.clear();
            self.docks.net_host = Some(host);
            self.docks.net_revision = 0;
        }
        let Some(status) = &self.status else {
            return;
        };
        if status.revision == self.docks.net_revision {
            return;
        }
        self.docks.net_revision = status.revision;
        if let Some((rx, tx)) = status.summary.as_ref().and_then(|s| s.net_rate) {
            super::overview::push_net_point(&mut self.docks.net, (rx, tx));
        }
    }

    // ---- SFTP 主视图标签 ----

    /// 在主视图里打开这台主机的文件视图（同一台主机已经有标签就切过去）。
    pub(crate) fn open_sftp_tab(
        &mut self,
        host_id: i64,
        factory: Option<Arc<SshSessionFactory>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.store.host(host_id).map(|host| host.name.clone()) else {
            return;
        };
        if let Some(tab) = self
            .docks
            .sftp_tabs
            .iter()
            .find(|tab| tab.host_id == host_id)
        {
            let id = tab.id;
            self.select_sftp_tab(id, window, cx);
            return;
        }
        // 这台主机停靠面板里已经有视图：挪过来（连接和目录原样带过去）。
        let view = match self.docks.sftp.remove(&host_id) {
            Some(view) => {
                view.update(cx, |view, cx| view.set_dock_layout(false, cx));
                view
            }
            None => cx.new(|cx| SftpDemo::new_embedded(window, cx, host_id, factory, false, false)),
        };
        // 文件窗口开着：新开的 SFTP 放进文件窗口（同 0.2.14）。
        if crate::file_window::is_open(cx) {
            crate::file_window::adopt(cx, host_id, format!("{name} (SFTP)"), view);
            return;
        }
        self.push_sftp_tab(host_id, name, view, window, cx);
    }

    fn push_sftp_tab(
        &mut self,
        host_id: i64,
        name: String,
        view: Entity<SftpDemo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = self.docks.next_tab_id();
        self.docks.sftp_tabs.push(SftpTab {
            id,
            host_id,
            title: format!("{name} (SFTP)"),
            view,
        });
        self.select_sftp_tab(id, window, cx);
    }

    /// 停靠面板的「在新标签中打开」：同一个视图挪到主视图标签里。
    pub(crate) fn undock_sftp(
        &mut self,
        host_id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.docks.sftp.remove(&host_id) else {
            return;
        };
        let Some(name) = self.store.host(host_id).map(|host| host.name.clone()) else {
            return;
        };
        view.update(cx, |view, cx| view.set_dock_layout(false, cx));
        self.docks.sftp_open = false;
        // 文件窗口开着：拆出来的标签放进文件窗口（同 0.2.14 `undockToTab`）。
        if crate::file_window::is_open(cx) {
            crate::file_window::adopt(cx, host_id, format!("{name} (SFTP)"), view);
            return;
        }
        self.push_sftp_tab(host_id, name, view, window, cx);
    }

    pub(crate) fn select_sftp_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.docks.sftp_tab_index(id).is_none() {
            return;
        }
        self.nav = self.nav.show(MainView::Terminal);
        self.docks.active_sftp = Some(id);
        self.docks.active_editor = None;
        self.save_layout();
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(crate) fn close_sftp_tab(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(index) = self.docks.sftp_tab_index(id) else {
            return;
        };
        let tab = &self.docks.sftp_tabs[index];
        let unfinished = tab.view.read(cx).unfinished();
        if let Some((title, message)) = close_guard::close_sftp_prompt(&tab.title, unfinished) {
            self.modal = Some(Modal::Confirm(Confirm {
                title,
                message,
                confirm_label: Some("取消并关闭".to_owned()),
                destructive: true,
                action: ConfirmAction::CloseSftpTab(id),
            }));
            cx.notify();
            return;
        }
        self.do_close_sftp_tab(id, window, cx);
    }

    pub(crate) fn do_close_sftp_tab(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.docks.sftp_tab_index(id) else {
            return;
        };
        let tab = self.docks.sftp_tabs.remove(index);
        tab.view.update(cx, |view, _| view.shutdown_embedded());
        self.after_sftp_tab_removed(id, index, window, cx);
    }

    /// 摘掉一个 SFTP 标签之后：它是当前显示的就回到旁边的标签；没有就回终端 / 主机页。
    fn after_sftp_tab_removed(
        &mut self,
        id: u64,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.docks.active_sftp == Some(id) {
            self.docks.active_sftp = None;
            let next = self
                .docks
                .sftp_tabs
                .get(index)
                .or_else(|| self.docks.sftp_tabs.last())
                .map(|tab| tab.id);
            match next {
                Some(next) => self.select_sftp_tab(next, window, cx),
                None => self.fall_back_after_extra_tab(window, cx),
            }
        }
        cx.notify();
    }

    // ---- 独立文件窗口 ----

    /// 标签右键「移到文件窗口」：同一个视图交给文件窗口（连接、传输队列、目录都原样带过去，不重连）。
    /// 还没连上的不移（没有会话可交，同 0.2.14）。
    pub(crate) fn move_sftp_tab_to_file_window(
        &mut self,
        id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.docks.sftp_tab_index(id) else {
            return;
        };
        if !self.docks.sftp_tabs[index].view.read(cx).is_ready() {
            self.notice("这个 SFTP 还没连上，连上之后再移", window, cx);
            return;
        }
        let tab = self.docks.sftp_tabs.remove(index);
        self.after_sftp_tab_removed(id, index, window, cx);
        crate::file_window::adopt(cx, tab.host_id, tab.title, tab.view);
    }

    /// 文件窗口交回来的视图：作为 SFTP 标签放进主视图（任务不中断）。
    pub(crate) fn adopt_sftp_view(
        &mut self,
        host_id: i64,
        title: String,
        view: Entity<SftpDemo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        view.update(cx, |view, cx| {
            view.set_dock_layout(false, cx);
            view.rebind_window(window, cx);
        });
        let id = self.docks.next_tab_id();
        self.docks.sftp_tabs.push(SftpTab {
            id,
            host_id,
            title,
            view,
        });
        self.select_sftp_tab(id, window, cx);
    }

    // ---- 退出 ----

    /// 真正退出前先看一眼进行中的工作；有就弹确认（返回 `false` 先别退），没有返回 `true`。
    pub(crate) fn request_quit(&mut self, cx: &mut Context<Self>) -> bool {
        let work = crate::install_state::snapshot();
        let others = close_guard::quit_prompt(work);
        // 有编辑器没保存：先问保存还是丢掉；传输、AI 回合这些别的后果写进同一个框（0.2.14 的合并确认）。
        if self.ask_unsaved_before_quit(others.as_ref().map(|(_, message)| message.clone()), cx) {
            return false;
        }
        let Some((title, message)) = others else {
            return true;
        };
        self.modal = Some(Modal::Confirm(Confirm {
            title,
            message,
            confirm_label: Some("退出".to_owned()),
            destructive: true,
            action: ConfirmAction::QuitApp,
        }));
        cx.notify();
        false
    }

    pub(crate) fn do_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.on_close(window, cx);
        // opencode / codex 的共用进程不属于任何一个对话：退出时一起收掉。
        crate::agent_host::shutdown_global();
        cx.quit();
    }

    /// 窗口真正关闭：AI 会话、文件视图的连接都收尾。
    pub(super) fn shutdown_docks(&mut self, cx: &mut Context<Self>) {
        self.docks.ai.clear();
        for (_, view) in self.docks.sftp.drain() {
            view.update(cx, |view, _| view.shutdown_embedded());
        }
        for tab in self.docks.sftp_tabs.drain(..) {
            tab.view.update(cx, |view, _| view.shutdown_embedded());
        }
    }

    // ---- 画 ----

    /// 终端右边的 AI / 文件面板（按 终端｜AI｜文件 的顺序）。
    pub(super) fn render_docks(
        &mut self,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::new();
        if !self.docks_available(cx) {
            return out;
        }
        let layout = self.dock_layout_now(cx);
        if self.docks.ai_open && layout.ai_width > 0.0 {
            let close = cx.entity();
            // 刚切到一个还没有对话的标签：下一个节拍会补上，这半秒里先把壳子（宽度）留着，布局不跳。
            let chat = self
                .workspace
                .read(cx)
                .active_tab_id()
                .and_then(|id| self.docks.ai.get(&id).cloned());
            let body = match chat {
                Some(chat) => chat.into_any_element(),
                None => div().into_any_element(),
            };
            out.push(self.dock_shell(
                DockKind::Ai,
                layout.ai_width,
                "AI 助手".into(),
                None,
                Vec::new(),
                body,
                move |window, cx| {
                    close.update(cx, |this, cx| this.toggle_ai_panel(window, cx));
                },
                tokens,
                cx,
            ));
        }
        if self.docks.sftp_open && layout.sftp_width > 0.0 {
            let host_id = self.workspace.read(cx).active_host_id();
            out.push(self.render_sftp_dock(host_id, layout.sftp_width, tokens, cx));
        }
        out
    }

    fn render_sftp_dock(
        &mut self,
        host_id: Option<i64>,
        width: f32,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let label: SharedString = host_id
            .and_then(|id| self.store.host(id).map(|host| host.name.clone()))
            .unwrap_or_default()
            .into();
        let view = host_id.and_then(|id| self.docks.sftp.get(&id).cloned());
        let local_hidden = view
            .as_ref()
            .map(|view| view.read(cx).local_hidden())
            .unwrap_or(false);
        let mut actions: Vec<AnyElement> = Vec::new();
        if let (Some(view), true) = (view.as_ref(), local_hidden) {
            let view = view.clone();
            actions.push(
                div()
                    .id("sftp-show-local")
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1p5()
                    .py_1()
                    .rounded_md()
                    .text_xs()
                    .cursor_pointer()
                    .text_color(tokens.text_muted)
                    .hover({
                        let hover = tokens.hover;
                        move |style| style.bg(hover)
                    })
                    .tooltip(|window, cx| Tooltip::new("显示本地栏").build(window, cx))
                    .child(Icon::new(Ico::Monitor).size_3p5())
                    .child("本地")
                    .on_click(move |_, _window, cx| {
                        view.update(cx, |view, cx| view.set_local_hidden(false, cx));
                    })
                    .into_any_element(),
            );
        }
        if let Some(host_id) = host_id.filter(|id| self.docks.sftp.contains_key(id)) {
            let entity = entity.clone();
            actions.push(
                dock_icon_button(
                    "sftp-undock",
                    Icon::new(IconName::ExternalLink),
                    "在新标签中打开",
                    tokens,
                )
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.undock_sftp(host_id, window, cx));
                })
                .into_any_element(),
            );
        }
        let non_ssh = host_id
            .and_then(|id| self.store.host(id))
            .filter(|host| !host.protocol.supports_sftp())
            .map(|host| host.protocol.protocol.label());
        let body: AnyElement = match (host_id, view) {
            _ if non_ssh.is_some() => div()
                .p_4()
                .text_sm()
                .text_color(tokens.text_muted)
                .child(SharedString::from(format!(
                    "这是一台 {} 主机，文件面板仅 SSH 主机可用。",
                    non_ssh.unwrap_or_default()
                )))
                .into_any_element(),
            (None, _) => div()
                .p_4()
                .text_sm()
                .text_color(tokens.text_muted)
                .child("这个终端没有主机记录（快速连接）或还没连主机，不能用文件面板。")
                .into_any_element(),
            (Some(_), Some(view)) => view.into_any_element(),
            (Some(_), None) => div()
                .p_4()
                .text_sm()
                .text_color(tokens.text_muted)
                .child("正在打开 SFTP…")
                .into_any_element(),
        };
        let close = entity.clone();
        self.dock_shell(
            DockKind::Sftp,
            width,
            "文件".into(),
            Some(label),
            actions,
            body,
            move |window, cx| {
                close.update(cx, |this, cx| this.toggle_sftp_dock(window, cx));
            },
            tokens,
            cx,
        )
    }

    /// 文件面板里「本地栏收起」：用户在当前主机的视图里点了收起 / 显示就存进设置表（键同 0.2.14），
    /// 再把同一个值推给别的主机的视图；设置在别处变了（同步、另一个窗口）也跟过来。
    fn sync_local_hidden(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let stored = self.store.bars().sftp_local_hidden;
        let current = *self.docks.local_hidden.get_or_insert(stored);
        let active = self
            .workspace
            .read(cx)
            .active_host_id()
            .and_then(|id| self.docks.sftp.get(&id).cloned());
        let mut truth = current;
        if let Some(view) = &active {
            let seen = view.read(cx).local_hidden();
            if seen != current {
                truth = seen;
                self.save_local_hidden(seen, window, cx);
            }
        }
        if truth == current && stored != current {
            // 设置在别处变了。
            truth = stored;
        }
        self.docks.local_hidden = Some(truth);
        for view in self.docks.sftp.values() {
            if view.read(cx).local_hidden() != truth {
                view.update(cx, |view, cx| view.set_local_hidden(truth, cx));
            }
        }
    }

    pub(super) fn save_local_hidden(
        &mut self,
        hidden: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = self.store.write_bar_setting(|db| {
            bar_settings::write_bool(db, bar_settings::SFTP_LOCAL_HIDDEN, hidden)
        });
        if let Err(err) = result {
            self.notice(&format!("保存设置失败：{err}"), window, cx);
        }
    }

    /// 面板外壳：左边缘拖宽（双击恢复）、标题栏、内容区。
    #[allow(clippy::too_many_arguments)]
    fn dock_shell(
        &mut self,
        kind: DockKind,
        width: f32,
        title: SharedString,
        subtitle: Option<SharedString>,
        actions: Vec<AnyElement>,
        body: AnyElement,
        on_close: impl Fn(&mut Window, &mut App) + 'static,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let handle = {
            let down = entity.clone();
            div()
                .id(match kind {
                    DockKind::Ai => "ai-dock-resize",
                    DockKind::Sftp => "sftp-dock-resize",
                })
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(-3.0))
                .w(px(6.0))
                .cursor_col_resize()
                .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                    cx.stop_propagation();
                    let double = event.click_count >= 2;
                    let start = DragStart {
                        pointer_x: f32::from(event.position.x),
                        width,
                    };
                    down.update(cx, |this, cx| {
                        if double {
                            this.reset_dock_width(kind, cx);
                        } else {
                            this.docks.drag = Some((kind, start));
                            cx.notify();
                        }
                    });
                })
        };
        let close = dock_icon_button(
            match kind {
                DockKind::Ai => "ai-dock-close",
                DockKind::Sftp => "sftp-dock-close",
            },
            Icon::new(IconName::Close),
            match kind {
                DockKind::Ai => "关闭 AI 面板",
                DockKind::Sftp => "关闭文件面板",
            },
            tokens,
        )
        .on_click(move |_, window, cx| on_close(window, cx));
        let icon = match kind {
            DockKind::Ai => Icon::new(IconName::Bot),
            DockKind::Sftp => Icon::new(Ico::FolderTree),
        };
        v_flex()
            .id(match kind {
                DockKind::Ai => "ai-dock",
                DockKind::Sftp => "sftp-dock",
            })
            .relative()
            .flex_none()
            .h_full()
            .w(px(width))
            .min_h_0()
            .bg(tokens.background)
            .border_l_1()
            .border_color(tokens.border)
            .child(handle)
            .child(
                h_flex()
                    .flex_none()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(tokens.border)
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_2()
                            .items_center()
                            .child(icon.size_4().text_color(tokens.accent))
                            .child(
                                div()
                                    .flex_none()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .children(subtitle.map(|text| {
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .typo(Role::Caption, tokens)
                                    .child(text)
                            })),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_0p5()
                            .items_center()
                            .children(actions)
                            .child(close),
                    ),
            )
            .child(div().flex_1().min_h_0().w_full().child(body))
            .into_any_element()
    }

    // ---- 标题栏上的 SFTP 标签 ----

    pub(super) fn render_sftp_tab(
        &self,
        tab: &SftpTab,
        index: usize,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let active = self.docks.active_sftp == Some(tab.id) && self.nav.view == MainView::Terminal;
        let id = tab.id;
        let hover = tokens.hover;
        let text = tokens.text;
        let muted = tokens.text_muted;
        let select = entity.clone();
        let close = entity.clone();
        let middle = entity.clone();
        let menu_move = entity.clone();
        let menu_close = entity.clone();
        let drag_entity = entity.clone();
        let drag_title = tab.title.clone();
        let dragging = self.dragging_tab() == Some(crate::tab_drag::DragKey::Sftp(id));
        let title: SharedString = tab.title.clone().into();
        div()
            .id(("sftp-tab", index))
            .occlude()
            .relative()
            .flex()
            .items_center()
            .gap_2()
            .h(px(32.0))
            .min_w(px(112.0))
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
                d.border_color(::gpui::Hsla::transparent_black())
                    .text_color(muted)
                    .hover(move |style| style.bg(hover))
            })
            .when(active, |d| {
                d.child(super::titlebar::active_tab_indicator(tokens))
            })
            .tooltip({
                let title = title.clone();
                move |window, cx| Tooltip::new(title.clone()).build(window, cx)
            })
            .child(
                Icon::new(Ico::FolderTree)
                    .size_3p5()
                    .text_color(tokens.accent),
            )
            .child(div().flex_1().min_w_0().truncate().child(title))
            .child(
                div()
                    .id(("sftp-tab-close", index))
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.0))
                    .flex_none()
                    .rounded_sm()
                    .text_color(muted)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover).text_color(text))
                    .tooltip(|window, cx| Tooltip::new("关闭标签").build(window, cx))
                    .child(Icon::new(IconName::Close).size_3())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        close.update(cx, |this, cx| this.close_sftp_tab(id, window, cx));
                    }),
            )
            .on_click(move |_, window, cx| {
                select.update(cx, |this, cx| this.select_sftp_tab(id, window, cx));
            })
            .on_mouse_down(MouseButton::Middle, move |_, window, cx| {
                middle.update(cx, |this, cx| this.close_sftp_tab(id, window, cx));
            })
            // 左键按下记下起点：拖出窗口 / 拖到文件窗口的标签栏上松手 = 放进文件窗口。
            .on_mouse_down(MouseButton::Left, move |event, _window, cx| {
                let pos = (f32::from(event.position.x), f32::from(event.position.y));
                let title = drag_title.clone();
                drag_entity.update(cx, |this, _cx| this.press_sftp_tab(id, title, pos));
            })
            .context_menu(move |menu, _window, _cx| {
                let to_files = menu_move.clone();
                let close = menu_close.clone();
                menu.item(
                    PopupMenuItem::new("移到文件窗口").on_click(move |_, window, cx| {
                        to_files.update(cx, |this, cx| {
                            this.move_sftp_tab_to_file_window(id, window, cx);
                        });
                    }),
                )
                .item(PopupMenuItem::new("关闭").on_click(move |_, window, cx| {
                    close.update(cx, |this, cx| this.close_sftp_tab(id, window, cx));
                }))
            })
            .into_any_element()
    }

    /// 主视图里显示的 SFTP 标签内容（没有就 `None`）。
    pub(super) fn active_sftp_view(&self) -> Option<Entity<SftpDemo>> {
        let id = self.docks.active_sftp?;
        self.docks
            .sftp_tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.view.clone())
    }
}
