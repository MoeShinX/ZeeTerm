//! 端口转发（0.2.14 `PortForwardPanel.tsx`）：按主机列出规则，开 / 停、看状态，
//! 新建和修改。规则只存本机；打开时可以只看某一台主机（从主机右键菜单进来），也可以看全部。
//! V2-U3 起它是「管理 + 设置」页的「端口转发」分类（不再是右侧面板）：同样交出 [`PanelContent`]，
//! 由 `settings_page` 画在整页里，页面上方的下拉选主机、搜索框过滤规则。
//!
//! 运行时是 [`crate::forward_runtime`]（和 Tauri 入口 V2-F6 修好的重启语义一致）：保存规则后，
//! 正开着的转发按库里最新的规则重开；删除规则 / 删除主机会先停掉对应的转发。

use ::gpui::{Entity, SharedString};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::store::forwards::{ForwardKind, PortForward, PortForwardDraft};

use super::MainWindow;
use super::modal::{Confirm, ConfirmAction, Modal};
use super::side_panel::PanelContent;
use super::widgets::{
    BadgeKind, ButtonLook, Choice, act, badge, empty_state, error_note, field, icon_button, note,
    pick, select, switch_row, text_button,
};
use crate::app_theme::Tokens;
use crate::forward_runtime::{self, ForwardStatus, Phase};
use crate::i18n::{Msg, t};
use crate::typography::{Role, Typo as _};

/// 端口转发页自己的界面文案。
///
/// 主机名、规则名、地址、端口和 `forward_runtime` 的说明文字原样显示。
#[derive(Clone, Copy, Debug)]
enum ForwardPanelMsg {
    BindPort,
    TargetPort,
    PortRangeSuffix,
    BindHostRequired,
    DestHostRequired,
    RuleLocal,
    RuleRemote,
    ListeningPrefix,
    ListeningMiddle,
    CounterPrefix,
    CounterConnections,
    CounterTotal,
    CounterSeparator,
    NamePlaceholder,
    DeleteTitlePrefix,
    DeleteTitleSuffix,
    DeleteMessage,
    Delete,
    Title,
    TitleHostMiddle,
    PhaseNotStarted,
    PhaseStarting,
    PhaseFailed,
    PhaseRunning,
    Stop,
    Start,
    EmptyTitle,
    EmptyDetail,
    NewForward,
    NoMatches,
    KindLocal,
    KindRemote,
    EditTitle,
    NewTitle,
    HostLabel,
    NameLabel,
    DirectionLabel,
    BindHostLocal,
    BindHostRemote,
    PortLabel,
    DestLocal,
    DestRemote,
    NonLoopbackWarning,
    AutoStartRow,
    AutoStartHint,
    Cancel,
    Save,
    CountSuffix,
}

impl Msg for ForwardPanelMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::BindPort => "监听端口",
            Self::TargetPort => "目标端口",
            Self::PortRangeSuffix => "要在 1-65535 之间",
            Self::BindHostRequired => "监听地址不能为空",
            Self::DestHostRequired => "目标地址不能为空",
            Self::RuleLocal => "本地转发",
            Self::RuleRemote => "远程转发",
            Self::ListeningPrefix => "服务器实际监听 ",
            Self::ListeningMiddle => " · ",
            Self::CounterPrefix => "当前 ",
            Self::CounterConnections => " 条连接 · 共 ",
            Self::CounterTotal => " 条 · ↑",
            Self::CounterSeparator => " ↓",
            Self::NamePlaceholder => "比如：数据库",
            Self::DeleteTitlePrefix => "删除转发「",
            Self::DeleteTitleSuffix => "」？",
            Self::DeleteMessage => "规则会被删除；正开着的转发会先停掉。",
            Self::Delete => "删除",
            Self::Title => "端口转发",
            Self::TitleHostMiddle => " · ",
            Self::PhaseNotStarted => "未开启",
            Self::PhaseStarting => "连接中…",
            Self::PhaseFailed => "出错",
            Self::PhaseRunning => "运行中",
            Self::Stop => "停止",
            Self::Start => "开启",
            Self::EmptyTitle => "暂无端口转发规则",
            Self::EmptyDetail => {
                "本地转发把服务器那边的端口（比如数据库）映射到本机；远程转发反过来，让服务器能连到本机这边。"
            }
            Self::NewForward => "新建转发",
            Self::NoMatches => "没有匹配的转发规则。",
            Self::KindLocal => "本地转发（-L）：本机端口 → 服务器那边",
            Self::KindRemote => "远程转发（-R）：服务器端口 → 本机这边",
            Self::EditTitle => "修改转发",
            Self::NewTitle => "新建转发",
            Self::HostLabel => "主机",
            Self::NameLabel => "名称",
            Self::DirectionLabel => "方向",
            Self::BindHostLocal => "本机监听地址",
            Self::BindHostRemote => "服务器监听地址",
            Self::PortLabel => "端口",
            Self::DestLocal => "目标地址（从服务器看）",
            Self::DestRemote => "目标地址（从本机看）",
            Self::NonLoopbackWarning => {
                "监听的不是回环地址：同一网络里的其他机器也能连上这个端口。"
            }
            Self::AutoStartRow => "连上这台主机时自动开启",
            Self::AutoStartHint => "打开它的终端标签时一起开。",
            Self::Cancel => "取消",
            Self::Save => "保存",
            Self::CountSuffix => " 条",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::BindPort => "Listen port",
            Self::TargetPort => "Target port",
            Self::PortRangeSuffix => " must be between 1 and 65535",
            Self::BindHostRequired => "Listen address cannot be empty",
            Self::DestHostRequired => "Target address cannot be empty",
            Self::RuleLocal => "Local forwarding",
            Self::RuleRemote => "Remote forwarding",
            Self::ListeningPrefix => "Server actually listens on ",
            Self::ListeningMiddle => " · ",
            Self::CounterPrefix => "Active ",
            Self::CounterConnections => " connections · ",
            Self::CounterTotal => " total · ↑",
            Self::CounterSeparator => " ↓",
            Self::NamePlaceholder => "e.g. Database",
            Self::DeleteTitlePrefix => "Delete forward \"",
            Self::DeleteTitleSuffix => "\"?",
            Self::DeleteMessage => "The rule will be deleted; a running forward is stopped first.",
            Self::Delete => "Delete",
            Self::Title => "Port Forwarding",
            Self::TitleHostMiddle => " · ",
            Self::PhaseNotStarted => "Not started",
            Self::PhaseStarting => "Connecting…",
            Self::PhaseFailed => "Error",
            Self::PhaseRunning => "Running",
            Self::Stop => "Stop",
            Self::Start => "Start",
            Self::EmptyTitle => "No port forwarding rules yet",
            Self::EmptyDetail => {
                "Local forwarding maps a port on the server side (such as a database) to this computer; remote forwarding does the opposite, letting the server reach this computer."
            }
            Self::NewForward => "New Forward",
            Self::NoMatches => "No matching forwarding rules.",
            Self::KindLocal => "Local forwarding (-L): local port → server side",
            Self::KindRemote => "Remote forwarding (-R): server port → local side",
            Self::EditTitle => "Edit Forward",
            Self::NewTitle => "New Forward",
            Self::HostLabel => "Host",
            Self::NameLabel => "Name",
            Self::DirectionLabel => "Direction",
            Self::BindHostLocal => "Local listen address",
            Self::BindHostRemote => "Server listen address",
            Self::PortLabel => "Port",
            Self::DestLocal => "Target address (from the server)",
            Self::DestRemote => "Target address (from this computer)",
            Self::NonLoopbackWarning => {
                "The listen address is not loopback: other machines on the same network can also connect to this port."
            }
            Self::AutoStartRow => "Start automatically when connected to this host",
            Self::AutoStartHint => "Starts together with its terminal tab.",
            Self::Cancel => "Cancel",
            Self::Save => "Save",
            Self::CountSuffix => " rules",
        }
    }
}

/// 监听地址是不是回环（不是的话要提醒：同一网络里的其他机器也能连上）。
pub fn is_loopback(host: &str) -> bool {
    matches!(host.trim(), "127.0.0.1" | "localhost" | "::1" | "[::1]")
}

/// 新建表单的默认值（同 0.2.14 `emptyDraft`）。
pub const DEFAULT_BIND_HOST: &str = "127.0.0.1";
pub const DEFAULT_BIND_PORT: &str = "8080";
pub const DEFAULT_DEST_HOST: &str = "localhost";
pub const DEFAULT_DEST_PORT: &str = "80";

/// 表单里的值。
#[derive(Clone, Debug)]
pub struct ForwardInput<'a> {
    pub id: Option<i64>,
    pub host_id: i64,
    pub name: &'a str,
    pub kind: ForwardKind,
    pub bind_host: &'a str,
    pub bind_port: &'a str,
    pub dest_host: &'a str,
    pub dest_port: &'a str,
    pub auto_start: bool,
}

/// 检查并拼出保存草稿：四个字段不能空、两个端口 1–65535（存储层还会再核一遍）。
pub fn build_forward_draft(input: &ForwardInput<'_>) -> Result<PortForwardDraft, String> {
    let port = |text: &str, what: &str| -> Result<u16, String> {
        text.trim()
            .parse::<u32>()
            .ok()
            .filter(|port| (1..=65535).contains(port))
            .map(|port| port as u16)
            .ok_or_else(|| format!("{what}{}", t(ForwardPanelMsg::PortRangeSuffix)))
    };
    if input.bind_host.trim().is_empty() {
        return Err(t(ForwardPanelMsg::BindHostRequired).to_owned());
    }
    if input.dest_host.trim().is_empty() {
        return Err(t(ForwardPanelMsg::DestHostRequired).to_owned());
    }
    Ok(PortForwardDraft {
        id: input.id,
        host_id: input.host_id,
        name: input.name.trim().to_owned(),
        kind: input.kind,
        bind_host: input.bind_host.trim().to_owned(),
        bind_port: port(input.bind_port, t(ForwardPanelMsg::BindPort))?,
        dest_host: input.dest_host.trim().to_owned(),
        dest_port: port(input.dest_port, t(ForwardPanelMsg::TargetPort))?,
        auto_start: input.auto_start,
    })
}

/// 规则在列表里的标题：没起名就按方向叫。
pub fn rule_title(rule: &PortForward) -> String {
    if rule.name.is_empty() {
        match rule.kind {
            ForwardKind::Local => t(ForwardPanelMsg::RuleLocal),
            ForwardKind::Remote => t(ForwardPanelMsg::RuleRemote),
        }
        .to_owned()
    } else {
        rule.name.clone()
    }
}

/// 状态行下面的计数文字（运行中才有）。
pub fn counters_line(status: &ForwardStatus, bind_port: u16) -> String {
    let c = &status.counters;
    let actual = status
        .listening_port
        .filter(|port| *port != u32::from(bind_port))
        .map(|port| {
            format!(
                "{}{port}{}",
                t(ForwardPanelMsg::ListeningPrefix),
                t(ForwardPanelMsg::ListeningMiddle)
            )
        })
        .unwrap_or_default();
    format!(
        "{actual}{}{}{}{}{}{}{}{}",
        t(ForwardPanelMsg::CounterPrefix),
        c.active,
        t(ForwardPanelMsg::CounterConnections),
        c.total,
        t(ForwardPanelMsg::CounterTotal),
        crate::format::size(c.bytes_sent),
        t(ForwardPanelMsg::CounterSeparator),
        crate::format::size(c.bytes_received)
    )
}

/// 页面上的搜索：规则名 / 主机名 / 描述（`本地 :端口 → 目标` 那一行）里含搜索词（不分大小写）。
pub fn forward_matches(rule: &PortForward, host_name: &str, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || rule_title(rule).to_lowercase().contains(&query)
        || host_name.to_lowercase().contains(&query)
        || forward_runtime::describe(rule)
            .to_lowercase()
            .contains(&query)
}

pub struct ForwardDraftUi {
    pub id: Option<i64>,
    pub host_id: i64,
    pub kind: ForwardKind,
    pub auto_start: bool,
    pub name: Entity<InputState>,
    pub bind_host: Entity<InputState>,
    pub bind_port: Entity<InputState>,
    pub dest_host: Entity<InputState>,
    pub dest_port: Entity<InputState>,
}

#[derive(Default)]
pub struct ForwardUi {
    /// 只看这台主机；`None` 看全部。
    pub host_filter: Option<i64>,
    pub draft: Option<ForwardDraftUi>,
}

impl MainWindow {
    /// 打开「管理 + 设置」页的端口转发；`host` 为空看全部。
    pub(crate) fn open_forwards(
        &mut self,
        host: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Telnet / 串口主机没有端口转发：不打开（规则只能建在 SSH 主机上）。
        if let Some(id) = host
            && !self.caps_of(id).port_forward
        {
            let (name, label) = self
                .store
                .host(id)
                .map(|h| (h.name.clone(), h.protocol.protocol.label()))
                .unwrap_or_default();
            // 入口（右键菜单）上这一项已经置灰，走到这里的是调试驱动之类的间接调用。
            tracing::info!(host = %name, protocol = label, "端口转发只有 SSH 主机能用，没有打开");
            return;
        }
        self.forward_ui.host_filter = host;
        self.forward_ui.draft = None;
        self.open_settings_section(crate::settings_nav::SettingsSection::Forwards, window, cx);
    }

    /// 页面上的主机下拉：只看这台（`None` = 全部）；换主机时编辑到一半的草稿作废。
    pub(crate) fn set_forward_host_filter(&mut self, host: Option<i64>, cx: &mut Context<Self>) {
        self.forward_ui.host_filter = host;
        self.forward_ui.draft = None;
        cx.notify();
    }

    pub(super) fn forward_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host_id) = self.forward_ui.host_filter.or_else(|| {
            self.store
                .hosts()
                .iter()
                .find(|h| h.protocol.supports_port_forward())
                .map(|h| h.id)
        }) else {
            return;
        };
        let draft = self.make_forward_draft(
            None,
            host_id,
            ForwardKind::Local,
            false,
            [
                "",
                DEFAULT_BIND_HOST,
                DEFAULT_BIND_PORT,
                DEFAULT_DEST_HOST,
                DEFAULT_DEST_PORT,
            ],
            window,
            cx,
        );
        self.forward_ui.draft = Some(draft);
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn make_forward_draft(
        &mut self,
        id: Option<i64>,
        host_id: i64,
        kind: ForwardKind,
        auto_start: bool,
        values: [&str; 5],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ForwardDraftUi {
        let make = |value: &str,
                    placeholder: &'static str,
                    window: &mut Window,
                    cx: &mut Context<Self>| {
            let value = value.to_owned();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .default_value(value)
                    .placeholder(placeholder)
            })
        };
        ForwardDraftUi {
            id,
            host_id,
            kind,
            auto_start,
            name: make(values[0], t(ForwardPanelMsg::NamePlaceholder), window, cx),
            bind_host: make(values[1], "127.0.0.1", window, cx),
            bind_port: make(values[2], "", window, cx),
            dest_host: make(values[3], "localhost", window, cx),
            dest_port: make(values[4], "", window, cx),
        }
    }

    pub(super) fn forward_edit(
        &mut self,
        rule: &PortForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = self.make_forward_draft(
            Some(rule.id),
            rule.host_id,
            rule.kind,
            rule.auto_start,
            [
                &rule.name,
                &rule.bind_host,
                &rule.bind_port.to_string(),
                &rule.dest_host,
                &rule.dest_port.to_string(),
            ],
            window,
            cx,
        );
        self.forward_ui.draft = Some(draft);
        cx.notify();
    }

    pub(super) fn forward_save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = &self.forward_ui.draft else {
            return;
        };
        let (name, bind_host, bind_port, dest_host, dest_port) = (
            draft.name.read(cx).value().to_string(),
            draft.bind_host.read(cx).value().to_string(),
            draft.bind_port.read(cx).value().to_string(),
            draft.dest_host.read(cx).value().to_string(),
            draft.dest_port.read(cx).value().to_string(),
        );
        let input = ForwardInput {
            id: draft.id,
            host_id: draft.host_id,
            name: &name,
            kind: draft.kind,
            bind_host: &bind_host,
            bind_port: &bind_port,
            dest_host: &dest_host,
            dest_port: &dest_port,
            auto_start: draft.auto_start,
        };
        let draft = match build_forward_draft(&input) {
            Ok(draft) => draft,
            Err(message) => {
                self.notice_error(&message, window, cx);
                return;
            }
        };
        let saved = self.store.db().and_then(|db| {
            db.forward_save(&draft)
                .map_err(|err| crate::error_text::error_text(&err))
        });
        self.store.reload();
        match saved {
            Ok(rule) => {
                self.forward_ui.draft = None;
                // 正开着的规则改了之后按新设置重开（停旧 → 等它放掉端口 → 开新）。
                forward_runtime::restart_if_running(rule.id);
                cx.notify();
            }
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    pub(crate) fn forward_ask_delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.forward_ui.draft.as_ref().and_then(|d| d.id) else {
            return;
        };
        let title = self
            .store
            .forwards()
            .iter()
            .find(|r| r.id == id)
            .map(rule_title)
            .unwrap_or_default();
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!(
                "{}{title}{}",
                t(ForwardPanelMsg::DeleteTitlePrefix),
                t(ForwardPanelMsg::DeleteTitleSuffix)
            ),
            message: t(ForwardPanelMsg::DeleteMessage).to_owned(),
            confirm_label: Some(t(ForwardPanelMsg::Delete).to_owned()),
            destructive: true,
            action: ConfirmAction::DeleteForward(id),
        }));
        cx.notify();
    }

    pub(crate) fn do_delete_forward(
        &mut self,
        id: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match forward_runtime::delete_rule(id) {
            Ok(rx) => self.await_result(rx, window, cx, |this, result, window, cx| {
                this.store.reload();
                match result {
                    Ok(()) => this.forward_ui.draft = None,
                    Err(err) => this.notice_error(&err, window, cx),
                }
                cx.notify();
            }),
            Err(err) => self.notice_error(&err, window, cx),
        }
    }

    pub(super) fn forward_toggle(&mut self, rule: &PortForward) {
        let active = forward_runtime::manager()
            .statuses()
            .iter()
            .any(|s| s.id == rule.id && s.active());
        if active {
            forward_runtime::stop_rule(rule.id);
        } else {
            forward_runtime::start_rule(rule.clone());
        }
    }

    /// 数据库变了之后（删主机、同步合并、恢复备份）调用：规则已经不在的转发停掉。
    pub(crate) fn reconcile_forwards(&mut self) {
        self.store.reload();
        let existing = self.store.forwards().iter().map(|rule| rule.id).collect();
        for id in forward_runtime::reconcile(&existing) {
            tracing::info!(id, "端口转发的规则已经不在了，停掉对应的转发");
        }
    }

    // ---- 画 ----

    pub(super) fn forward_panel_content(
        &mut self,
        tokens: &Tokens,
        _window: &mut ::gpui::Window,
        cx: &mut Context<Self>,
    ) -> PanelContent {
        let entity = cx.entity();
        let hosts = self.store.hosts();
        let host_name = |id: i64| {
            hosts
                .iter()
                .find(|h| h.id == id)
                .map(|h| h.name.clone())
                .unwrap_or_default()
        };
        let statuses = forward_runtime::manager().statuses();
        let query = self.manage_query(cx);
        let ui = &self.forward_ui;
        let in_scope: Vec<&PortForward> = self
            .store
            .forwards()
            .iter()
            .filter(|rule| ui.host_filter.is_none_or(|id| rule.host_id == id))
            .collect();
        let visible: Vec<&PortForward> = in_scope
            .iter()
            .copied()
            .filter(|rule| forward_matches(rule, &host_name(rule.host_id), &query))
            .collect();
        let title = match ui.host_filter {
            None => t(ForwardPanelMsg::Title).to_owned(),
            Some(id) => format!(
                "{}{}{}",
                t(ForwardPanelMsg::Title),
                t(ForwardPanelMsg::TitleHostMiddle),
                host_name(id)
            ),
        };
        // 端口转发只能建在 SSH 主机上：Telnet / 串口主机不出现在选择里。
        let ssh_hosts: Vec<&zssh_core::model::Host> = hosts
            .iter()
            .filter(|h| h.protocol.supports_port_forward())
            .collect();
        let can_create = ui.host_filter.is_some() || !ssh_hosts.is_empty();

        let rows: Vec<AnyElement> = visible
            .iter()
            .map(|rule| {
                let status = statuses.iter().find(|s| s.id == rule.id);
                let active = status.is_some_and(ForwardStatus::active);
                let editing = ui.draft.as_ref().and_then(|d| d.id) == Some(rule.id);
                let (edit_rule, toggle_rule) = ((*rule).clone(), (*rule).clone());
                let badge_el = match status.map(|s| s.phase) {
                    None => badge(
                        t(ForwardPanelMsg::PhaseNotStarted),
                        BadgeKind::Outline,
                        tokens,
                    ),
                    Some(Phase::Starting) => {
                        badge(t(ForwardPanelMsg::PhaseStarting), BadgeKind::Busy, tokens)
                    }
                    Some(Phase::Failed) => {
                        badge(t(ForwardPanelMsg::PhaseFailed), BadgeKind::Bad, tokens)
                    }
                    Some(Phase::Running) => {
                        badge(t(ForwardPanelMsg::PhaseRunning), BadgeKind::Good, tokens)
                    }
                };
                v_flex()
                    .w_full()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .border_1()
                    .border_color(if editing {
                        tokens.accent
                    } else {
                        tokens.border
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                v_flex()
                                    .id(("forward-edit", rule.id as usize))
                                    .flex_1()
                                    .min_w_0()
                                    .cursor_pointer()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_baseline()
                                            .child(
                                                div()
                                                    .truncate()
                                                    .typo(Role::ItemName, tokens)
                                                    .child(SharedString::from(rule_title(rule))),
                                            )
                                            .when(ui.host_filter.is_none(), |d| {
                                                d.child(
                                                    div()
                                                        .truncate()
                                                        .typo(Role::Caption, tokens)
                                                        .child(SharedString::from(host_name(
                                                            rule.host_id,
                                                        ))),
                                                )
                                            }),
                                    )
                                    .child(
                                        div()
                                            .truncate()
                                            .typo(Role::Caption, tokens)
                                            .font_family("Consolas")
                                            .child(SharedString::from(forward_runtime::describe(
                                                rule,
                                            ))),
                                    )
                                    .on_click({
                                        let entity = entity.clone();
                                        move |_, window, cx| {
                                            let rule = edit_rule.clone();
                                            entity.update(cx, |this, cx| {
                                                this.forward_edit(&rule, window, cx)
                                            });
                                        }
                                    }),
                            )
                            .child(badge_el)
                            .child(icon_button(
                                SharedString::from(format!("forward-toggle-{}", rule.id)),
                                if active {
                                    IconName::Square
                                } else {
                                    IconName::Play
                                },
                                if active {
                                    t(ForwardPanelMsg::Stop)
                                } else {
                                    t(ForwardPanelMsg::Start)
                                },
                                {
                                    let entity = entity.clone();
                                    move |_, _window, cx| {
                                        let rule = toggle_rule.clone();
                                        entity.update(cx, |this, cx| {
                                            this.forward_toggle(&rule);
                                            cx.notify();
                                        });
                                    }
                                },
                            )),
                    )
                    .children(
                        status
                            .filter(|s| s.phase == Phase::Failed)
                            .and_then(|s| s.error.clone())
                            .map(|error| error_note(error, tokens.danger)),
                    )
                    .children(
                        status
                            .filter(|s| s.phase == Phase::Running)
                            .map(|s| note(counters_line(s, rule.bind_port), tokens)),
                    )
                    .into_any_element()
            })
            .collect();

        let list = v_flex()
            .w_full()
            .gap_1()
            .when(in_scope.is_empty() && ui.draft.is_none(), |d| {
                d.child(empty_state(
                    t(ForwardPanelMsg::EmptyTitle),
                    Some(t(ForwardPanelMsg::EmptyDetail)),
                    can_create.then(|| {
                        text_button(
                            "forward-empty-new",
                            t(ForwardPanelMsg::NewForward),
                            ButtonLook::Primary,
                            act(&entity, |this, window, cx| this.forward_new(window, cx)),
                        )
                        .icon(IconName::Plus)
                        .into_any_element()
                    }),
                    tokens,
                ))
            })
            .when(!in_scope.is_empty() && rows.is_empty(), |d| {
                d.child(note(t(ForwardPanelMsg::NoMatches), tokens))
            })
            .children(rows);

        let form = ui.draft.as_ref().map(|draft| {
            let kinds = [ForwardKind::Local, ForwardKind::Remote];
            let kind_choices = vec![
                Choice::new(
                    t(ForwardPanelMsg::KindLocal),
                    draft.kind == ForwardKind::Local,
                ),
                Choice::new(
                    t(ForwardPanelMsg::KindRemote),
                    draft.kind == ForwardKind::Remote,
                ),
            ];
            let kind_label = match draft.kind {
                ForwardKind::Local => t(ForwardPanelMsg::KindLocal),
                ForwardKind::Remote => t(ForwardPanelMsg::KindRemote),
            };
            let bind_text = draft.bind_host.read(cx).value().to_string();
            let host_ids: Vec<i64> = ssh_hosts.iter().map(|h| h.id).collect();
            let host_choices: Vec<Choice> = ssh_hosts
                .iter()
                .map(|h| Choice::new(h.name.clone(), h.id == draft.host_id))
                .collect();
            v_flex()
                .w_full()
                .gap_3()
                .child(
                    div()
                        .typo(Role::GroupTitle, tokens)
                        .child(if draft.id.is_none() {
                            t(ForwardPanelMsg::NewTitle)
                        } else {
                            t(ForwardPanelMsg::EditTitle)
                        }),
                )
                .when(ui.host_filter.is_none(), |d| {
                    d.child(field(
                        t(ForwardPanelMsg::HostLabel),
                        None,
                        select(
                            "forward-host",
                            host_name(draft.host_id),
                            host_choices,
                            pick(&entity, move |this, index, _window, cx| {
                                if let (Some(id), Some(draft)) =
                                    (host_ids.get(index).copied(), this.forward_ui.draft.as_mut())
                                {
                                    draft.host_id = id;
                                }
                                cx.notify();
                            }),
                        ),
                        tokens,
                    ))
                })
                .child(field(
                    t(ForwardPanelMsg::NameLabel),
                    None,
                    Input::new(&draft.name),
                    tokens,
                ))
                .child(field(
                    t(ForwardPanelMsg::DirectionLabel),
                    None,
                    select(
                        "forward-kind",
                        kind_label,
                        kind_choices,
                        pick(&entity, move |this, index, _window, cx| {
                            if let (Some(kind), Some(draft)) =
                                (kinds.get(index).copied(), this.forward_ui.draft.as_mut())
                            {
                                draft.kind = kind;
                            }
                            cx.notify();
                        }),
                    ),
                    tokens,
                ))
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(field(
                            if draft.kind == ForwardKind::Local {
                                t(ForwardPanelMsg::BindHostLocal)
                            } else {
                                t(ForwardPanelMsg::BindHostRemote)
                            },
                            None,
                            Input::new(&draft.bind_host),
                            tokens,
                        )))
                        .child(div().w(::gpui::px(96.0)).flex_none().child(field(
                            t(ForwardPanelMsg::PortLabel),
                            None,
                            Input::new(&draft.bind_port),
                            tokens,
                        ))),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(field(
                            if draft.kind == ForwardKind::Local {
                                t(ForwardPanelMsg::DestLocal)
                            } else {
                                t(ForwardPanelMsg::DestRemote)
                            },
                            None,
                            Input::new(&draft.dest_host),
                            tokens,
                        )))
                        .child(div().w(::gpui::px(96.0)).flex_none().child(field(
                            t(ForwardPanelMsg::PortLabel),
                            None,
                            Input::new(&draft.dest_port),
                            tokens,
                        ))),
                )
                .when(
                    !bind_text.trim().is_empty() && !is_loopback(&bind_text),
                    |d| {
                        d.child(error_note(
                            t(ForwardPanelMsg::NonLoopbackWarning),
                            tokens.danger,
                        ))
                    },
                )
                .child(switch_row(
                    "forward-auto-start",
                    t(ForwardPanelMsg::AutoStartRow),
                    Some(t(ForwardPanelMsg::AutoStartHint).to_owned()),
                    draft.auto_start,
                    {
                        let entity = entity.clone();
                        move |_window, cx| {
                            entity.update(cx, |this, cx| {
                                if let Some(draft) = this.forward_ui.draft.as_mut() {
                                    draft.auto_start = !draft.auto_start;
                                }
                                cx.notify();
                            });
                        }
                    },
                    tokens,
                ))
                .into_any_element()
        });

        let footer = ui.draft.as_ref().map(|draft| {
            h_flex()
                .gap_2()
                .items_center()
                .when(draft.id.is_some(), |d| {
                    d.child(div().mr_auto().child(text_button(
                        "forward-delete",
                        t(ForwardPanelMsg::Delete),
                        ButtonLook::Ghost,
                        act(&entity, |this, _window, cx| this.forward_ask_delete(cx)),
                    )))
                })
                .when(draft.id.is_none(), |d| d.child(div().mr_auto()))
                .child(text_button(
                    "forward-cancel",
                    t(ForwardPanelMsg::Cancel),
                    ButtonLook::Outline,
                    act(&entity, |this, _window, cx| {
                        this.forward_ui.draft = None;
                        cx.notify();
                    }),
                ))
                .child(text_button(
                    "forward-save",
                    t(ForwardPanelMsg::Save),
                    ButtonLook::Primary,
                    act(&entity, |this, window, cx| this.forward_save(window, cx)),
                ))
                .into_any_element()
        });

        PanelContent {
            title: title.into(),
            subtitle: Some(format!("{}{}", visible.len(), t(ForwardPanelMsg::CountSuffix)).into()),
            back: None,
            actions: can_create
                .then(|| {
                    text_button(
                        "forward-new",
                        t(ForwardPanelMsg::NewForward),
                        ButtonLook::Outline,
                        act(&entity, |this, window, cx| this.forward_new(window, cx)),
                    )
                    .icon(IconName::Plus)
                    .into_any_element()
                })
                .into_iter()
                .collect(),
            body: v_flex()
                .w_full()
                .gap_4()
                .child(list)
                .children(form)
                .into_any_element(),
            footer,
        }
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        ForwardInput, build_forward_draft, counters_line, forward_matches, is_loopback, rule_title,
    };
    use crate::forward_runtime::{ForwardStatus, Phase};
    use zssh_core::ssh::forward::ForwardCounters;
    use zssh_core::store::forwards::{ForwardKind, PortForward};

    fn input<'a>() -> ForwardInput<'a> {
        ForwardInput {
            id: None,
            host_id: 1,
            name: "  db ",
            kind: ForwardKind::Local,
            bind_host: "127.0.0.1",
            bind_port: "15432",
            dest_host: "localhost",
            dest_port: "5432",
            auto_start: false,
        }
    }

    #[test]
    fn the_page_search_matches_title_host_and_description() {
        let named = rule("db", ForwardKind::Local);
        assert!(forward_matches(&named, "web-1", ""));
        assert!(forward_matches(&named, "web-1", "DB"));
        assert!(forward_matches(&named, "web-1", "web"), "主机名");
        assert!(forward_matches(&named, "web-1", "8080"), "端口在描述里");
        assert!(!forward_matches(&named, "web-1", "nothing"));
    }

    #[test]
    fn a_valid_form_becomes_a_trimmed_draft() {
        let draft = build_forward_draft(&input()).expect("合法");
        assert_eq!(draft.name, "db");
        assert_eq!(draft.bind_port, 15432);
        assert_eq!(draft.dest_port, 5432);
        assert_eq!(draft.kind, ForwardKind::Local);
    }

    #[test]
    fn empty_hosts_and_bad_ports_are_refused_with_the_field_named() {
        let mut form = input();
        form.bind_host = " ";
        assert_eq!(build_forward_draft(&form).unwrap_err(), "监听地址不能为空");
        let mut form = input();
        form.dest_host = "";
        assert_eq!(build_forward_draft(&form).unwrap_err(), "目标地址不能为空");
        for port in ["", "0", "65536", "abc"] {
            let mut form = input();
            form.bind_port = port;
            assert_eq!(
                build_forward_draft(&form).unwrap_err(),
                "监听端口要在 1-65535 之间",
                "{port}"
            );
            let mut form = input();
            form.dest_port = port;
            assert_eq!(
                build_forward_draft(&form).unwrap_err(),
                "目标端口要在 1-65535 之间",
                "{port}"
            );
        }
    }

    #[test]
    fn only_loopback_listen_addresses_skip_the_warning() {
        for yes in ["127.0.0.1", "localhost", "::1", "[::1]", " 127.0.0.1 "] {
            assert!(is_loopback(yes), "{yes}");
        }
        for no in ["0.0.0.0", "192.0.2.1", "::", "example.com"] {
            assert!(!is_loopback(no), "{no}");
        }
    }

    fn rule(name: &str, kind: ForwardKind) -> PortForward {
        PortForward {
            id: 1,
            host_id: 1,
            name: name.to_owned(),
            kind,
            bind_host: "127.0.0.1".to_owned(),
            bind_port: 8080,
            dest_host: "localhost".to_owned(),
            dest_port: 80,
            auto_start: false,
        }
    }

    #[test]
    fn unnamed_rules_are_called_after_their_direction() {
        assert_eq!(rule_title(&rule("", ForwardKind::Local)), "本地转发");
        assert_eq!(rule_title(&rule("", ForwardKind::Remote)), "远程转发");
        assert_eq!(rule_title(&rule("数据库", ForwardKind::Local)), "数据库");
    }

    #[test]
    fn the_counter_line_mentions_a_different_remote_port_only_when_it_differs() {
        let status = |listening_port| ForwardStatus {
            id: 1,
            phase: Phase::Running,
            error: None,
            listening_port,
            counters: ForwardCounters {
                active: 2,
                total: 9,
                bytes_sent: 2048,
                bytes_received: 10,
            },
        };
        assert_eq!(
            counters_line(&status(None), 8080),
            "当前 2 条连接 · 共 9 条 · ↑2.0 KB ↓10 B"
        );
        assert_eq!(
            counters_line(&status(Some(8080)), 8080),
            "当前 2 条连接 · 共 9 条 · ↑2.0 KB ↓10 B",
            "和规则里一样就不重复说"
        );
        assert!(counters_line(&status(Some(40001)), 0).starts_with("服务器实际监听 40001 · "));
    }
}
