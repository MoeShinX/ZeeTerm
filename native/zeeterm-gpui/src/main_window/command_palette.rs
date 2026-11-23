//! 全局命令面板（`Ctrl+Shift+P` / macOS `Cmd+Shift+P`，非终端页亦支持 `Ctrl+K`）：
//! 统一模糊搜索已打开的终端标签、主机库、快捷命令、设置与管理分类以及常用窗口操作。

use ::gpui::{AnyElement, Context, SharedString, Window, px};
use gpui_kit::component::input::Input;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeeterm_core::model::Host;

use super::MainWindow;
use super::modal::Modal;
use super::quick_commands::QuickCommand;
use super::widgets::{BadgeKind, badge};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::settings_nav::SettingsSection;
use crate::typography::{Role, Typo as _};

const MAX_RESULTS: usize = 40;

#[derive(Clone, Copy, Debug)]
enum PaletteMsg {
    Placeholder,
    Empty,
    Hint,
    GroupTab,
    GroupHost,
    GroupQuick,
    GroupSettings,
    GroupAction,
    ActionNewTab,
    ActionNewTabSub,
    ActionToggleAi,
    ActionToggleAiSub,
    ActionToggleFiles,
    ActionToggleFilesSub,
    ActionListeningPorts,
    ActionListeningPortsSub,
    TabActiveTag,
    BroadcastTag,
    KwKeychain,
    KwQuickCommands,
    KwProxies,
    KwForwards,
    KwGeneral,
    KwAppearance,
    KwTerminal,
    KwNetwork,
    KwSftp,
    KwAi,
    KwSecurity,
    KwSync,
    KwShortcuts,
    KwAbout,
}

impl Msg for PaletteMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::Placeholder => "搜索主机、已开标签、快捷命令或设置项…",
            Self::Empty => "没有匹配的命令、主机或设置项。",
            Self::Hint => "↑↓ 选择 · Enter 执行 · Esc 关闭",
            Self::GroupTab => "标签",
            Self::GroupHost => "主机",
            Self::GroupQuick => "快捷命令",
            Self::GroupSettings => "设置",
            Self::GroupAction => "操作",
            Self::ActionNewTab => "新建空白标签",
            Self::ActionNewTabSub => "打开新的终端标签页",
            Self::ActionToggleAi => "切换 AI 助手面板",
            Self::ActionToggleAiSub => "展开或收起右侧 AI 面板",
            Self::ActionToggleFiles => "切换 SFTP 文件面板",
            Self::ActionToggleFilesSub => "展开或收起当前主机的文件浏览器",
            Self::ActionListeningPorts => "查看远端监听端口",
            Self::ActionListeningPortsSub => "列出当前服务器 LISTEN 端口并一键转发到本地",
            Self::TabActiveTag => "当前",
            Self::BroadcastTag => "（广播）",
            Self::KwKeychain => "凭据 密码 私钥 密钥",
            Self::KwQuickCommands => "快捷命令 常用命令 片段",
            Self::KwProxies => "代理",
            Self::KwForwards => "端口转发 隧道 本地转发 远程转发",
            Self::KwGeneral => "常规 语言 启动 退出 关闭确认 数据目录",
            Self::KwAppearance => "外观 主题 皮肤 深色 浅色 配色 字体大小",
            Self::KwTerminal => "终端 字体 字号 行高 光标 闪烁 粘贴 括号粘贴 选中复制 高亮 日志",
            Self::KwNetwork => "连接 网络 自动重连 保活 连接池 延迟 状态栏",
            Self::KwSftp => "文件 传输 并发 本地栏",
            Self::KwAi => "助手 模型 端点 智能体 权限",
            Self::KwSecurity => "安全 应用锁 锁屏 密码",
            Self::KwSync => "同步 备份 云同步 恢复",
            Self::KwShortcuts => "快捷键 键盘",
            Self::KwAbout => "关于 版本 检查更新 开源许可",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::Placeholder => "Search hosts, open tabs, quick commands, or settings…",
            Self::Empty => "No matching hosts, commands, or settings.",
            Self::Hint => "↑↓ Select · Enter Run · Esc Close",
            Self::GroupTab => "Tab",
            Self::GroupHost => "Host",
            Self::GroupQuick => "Quick Cmd",
            Self::GroupSettings => "Settings",
            Self::GroupAction => "Action",
            Self::ActionNewTab => "New Blank Tab",
            Self::ActionNewTabSub => "Open a new terminal tab",
            Self::ActionToggleAi => "Toggle AI Assistant",
            Self::ActionToggleAiSub => "Show or hide the AI panel",
            Self::ActionToggleFiles => "Toggle SFTP File Panel",
            Self::ActionToggleFilesSub => "Show or hide the file browser for this host",
            Self::ActionListeningPorts => "Inspect Remote Listening Ports",
            Self::ActionListeningPortsSub => "List remote LISTEN ports and forward locally",
            Self::TabActiveTag => "Active",
            Self::BroadcastTag => " (broadcast)",
            Self::KwKeychain => "keychain identity password key ssh",
            Self::KwQuickCommands => "quick commands snippets",
            Self::KwProxies => "socks5 http proxy",
            Self::KwForwards => "port forward tunnel",
            Self::KwGeneral => "general language startup exit close confirm",
            Self::KwAppearance => "appearance theme skin dark light color",
            Self::KwTerminal => "terminal font cursor paste highlight log",
            Self::KwNetwork => "network reconnect keepalive pool latency x11",
            Self::KwSftp => "sftp files transfer concurrency",
            Self::KwAi => "ai assistant model agent builtin claude codex",
            Self::KwSecurity => "security lock timeout",
            Self::KwSync => "webdav s3 gist onedrive sync backup restore",
            Self::KwShortcuts => "shortcuts keybindings",
            Self::KwAbout => "about version update license",
        }
    }
}

pub fn placeholder() -> &'static str {
    t(PaletteMsg::Placeholder)
}

/// 全局命令面板的打开状态。
#[derive(Clone, Debug, Default)]
pub struct PaletteDialog {
    pub cursor: usize,
}

/// 命令面板里一条候选项对应的动作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteAction {
    SwitchTab(usize),
    ConnectHost(i64),
    RunQuickCommand(String),
    OpenSettings(SettingsSection),
    NewTab,
    ToggleAiPanel,
    ToggleFilePanel,
    OpenListeningPorts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteKind {
    Tab,
    Host,
    Quick,
    Settings,
    Action,
}

impl PaletteKind {
    fn tag(self) -> &'static str {
        match self {
            Self::Tab => t(PaletteMsg::GroupTab),
            Self::Host => t(PaletteMsg::GroupHost),
            Self::Quick => t(PaletteMsg::GroupQuick),
            Self::Settings => t(PaletteMsg::GroupSettings),
            Self::Action => t(PaletteMsg::GroupAction),
        }
    }

    fn icon(self, action: &PaletteAction) -> Ico {
        match action {
            PaletteAction::ToggleAiPanel => Ico::Brain,
            PaletteAction::ToggleFilePanel => Ico::FolderOpen,
            PaletteAction::OpenListeningPorts => Ico::ArrowLeftRight,
            _ => match self {
                Self::Tab => Ico::Terminal,
                Self::Host => Ico::Server,
                Self::Quick => Ico::Zap,
                Self::Settings => Ico::MonitorCog,
                Self::Action => Ico::Command,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaletteItem {
    pub kind: PaletteKind,
    pub title: String,
    pub subtitle: String,
    pub search_blob: String,
    pub action: PaletteAction,
}

/// 已打开标签的轻量快照，供纯函数构造命令面板候选项。
#[derive(Clone, Debug)]
pub struct PaletteTab {
    pub index: usize,
    pub title: String,
    pub active: bool,
}

/// 轻量模糊匹配评分：按空白拆词，所有词都需命中（不区分大小写，支持中文与 ASCII 子序列）。
/// 前缀命中 > 词边界命中 > 连续子串 > 有序子序列；空查询返回 `Some(0)`。
pub fn fuzzy_score(haystack: &str, query: &str) -> Option<i32> {
    let tokens: Vec<&str> = query.split_whitespace().collect();
    if tokens.is_empty() {
        return Some(0);
    }
    let hay_lower = haystack.to_lowercase();
    let mut total = 0i32;
    for token in tokens {
        let needle = token.to_lowercase();
        if let Some(pos) = hay_lower.find(&needle) {
            if pos == 0 {
                total += 120;
            } else if hay_lower[..pos]
                .chars()
                .next_back()
                .is_some_and(|ch| !ch.is_alphanumeric())
            {
                total += 80;
            } else {
                total += 45;
            }
            total -= (pos.min(200)) as i32 / 8;
            continue;
        }
        // 有序子序列匹配（例如 "zf" 匹配 "转发" 之外的缩写，或 "ptf" 匹配 "Port Forwards"）。
        let mut hay_chars = hay_lower.chars().enumerate();
        let mut first_ix = None;
        let mut last_ix = 0usize;
        for nc in needle.chars() {
            let found = hay_chars.find(|(_, hc)| *hc == nc)?;
            if first_ix.is_none() {
                first_ix = Some(found.0);
            }
            last_ix = found.0;
        }
        let span = last_ix.saturating_sub(first_ix.unwrap_or(0));
        total += (20 - (span.min(18) as i32)).max(1);
    }
    Some(total)
}

fn both_langs(msg: PaletteMsg) -> String {
    format!("{} {}", msg.zh(), msg.en())
}

fn section_kw_msg(section: SettingsSection) -> PaletteMsg {
    match section {
        SettingsSection::Credentials => PaletteMsg::KwKeychain,
        SettingsSection::QuickCommands => PaletteMsg::KwQuickCommands,
        SettingsSection::Proxies => PaletteMsg::KwProxies,
        SettingsSection::Forwards => PaletteMsg::KwForwards,
        SettingsSection::General => PaletteMsg::KwGeneral,
        SettingsSection::Appearance => PaletteMsg::KwAppearance,
        SettingsSection::Terminal => PaletteMsg::KwTerminal,
        SettingsSection::Connection => PaletteMsg::KwNetwork,
        SettingsSection::Sftp => PaletteMsg::KwSftp,
        SettingsSection::Ai => PaletteMsg::KwAi,
        SettingsSection::Lock => PaletteMsg::KwSecurity,
        SettingsSection::Sync => PaletteMsg::KwSync,
        SettingsSection::Shortcuts => PaletteMsg::KwShortcuts,
        SettingsSection::About => PaletteMsg::KwAbout,
    }
}

/// 每个设置 / 管理分类对应的中英文同义关键词，方便用户直接搜具体设置名（如「字体」「代理」「重连」）。
fn section_keywords(section: SettingsSection) -> String {
    both_langs(section_kw_msg(section))
}

/// 生成并按模糊得分排序命令面板的候选项（纯函数，单测可直接校验）。
///
/// `active_live_target` 为当前活动且已连接（非空白）终端标签的 `(标题, 是否广播)`：
/// 只有它为 `Some` 时才列出快捷命令，并在副标题里写明目标标签名，避免在主机页 / 设置页误发到不可见终端。
pub fn build_palette_items(
    query: &str,
    tabs: &[PaletteTab],
    hosts: &[Host],
    quick_commands: &[QuickCommand],
    active_live_target: Option<(&str, bool)>,
    has_active_ssh: bool,
) -> Vec<PaletteItem> {
    let has_terminal = !tabs.is_empty();
    let mut candidates: Vec<(i32, usize, PaletteItem)> = Vec::new();
    let mut order = 0usize;
    let mut push = |base_priority: i32, item: PaletteItem| {
        if let Some(score) = fuzzy_score(&item.search_blob, query) {
            let final_score = if query.trim().is_empty() {
                base_priority
            } else {
                score * 10 + base_priority
            };
            candidates.push((final_score, order, item));
            order += 1;
        }
    };

    // 1. 已打开的终端标签
    for tab in tabs {
        let subtitle = if tab.active {
            t(PaletteMsg::TabActiveTag).to_owned()
        } else {
            format!("#{}", tab.index + 1)
        };
        let search_blob = format!("{} {}", tab.title, both_langs(PaletteMsg::GroupTab));
        push(
            50,
            PaletteItem {
                kind: PaletteKind::Tab,
                title: tab.title.clone(),
                subtitle,
                search_blob,
                action: PaletteAction::SwitchTab(tab.index),
            },
        );
    }

    // 2. 主机库
    for host in hosts {
        let addr = if host.username.is_empty() {
            format!("{}:{}", host.host, host.port)
        } else {
            format!("{}@{}:{}", host.username, host.host, host.port)
        };
        let tags = host.tags.join(" ");
        let search_blob = format!(
            "{} {} {} {}",
            host.name,
            addr,
            tags,
            both_langs(PaletteMsg::GroupHost)
        );
        push(
            40,
            PaletteItem {
                kind: PaletteKind::Host,
                title: host.name.clone(),
                subtitle: addr,
                search_blob,
                action: PaletteAction::ConnectHost(host.id),
            },
        );
    }

    // 3. 快捷命令（仅当前有已连接的非空白活动终端标签时提供，并在副标题标明目标标签与广播态）
    if let Some((target_title, broadcast)) = active_live_target {
        let target_note = if broadcast {
            format!("{target_title}{}", t(PaletteMsg::BroadcastTag))
        } else {
            target_title.to_owned()
        };
        for cmd in quick_commands {
            let preview = cmd.command.lines().next().unwrap_or("").trim();
            let subtitle = if preview.is_empty() {
                target_note.clone()
            } else {
                format!("{target_note} · {preview}")
            };
            let search_blob = format!(
                "{} {} {} {}",
                cmd.name,
                cmd.group.as_deref().unwrap_or(""),
                cmd.command,
                both_langs(PaletteMsg::GroupQuick)
            );
            push(
                35,
                PaletteItem {
                    kind: PaletteKind::Quick,
                    title: cmd.name.clone(),
                    subtitle,
                    search_blob,
                    action: PaletteAction::RunQuickCommand(cmd.id.clone()),
                },
            );
        }
    }

    // 4. 常用窗口操作
    push(
        30,
        PaletteItem {
            kind: PaletteKind::Action,
            title: t(PaletteMsg::ActionNewTab).to_owned(),
            subtitle: t(PaletteMsg::ActionNewTabSub).to_owned(),
            search_blob: format!(
                "{} {}",
                both_langs(PaletteMsg::ActionNewTab),
                both_langs(PaletteMsg::ActionNewTabSub)
            ),
            action: PaletteAction::NewTab,
        },
    );
    if has_terminal {
        push(
            30,
            PaletteItem {
                kind: PaletteKind::Action,
                title: t(PaletteMsg::ActionToggleAi).to_owned(),
                subtitle: t(PaletteMsg::ActionToggleAiSub).to_owned(),
                search_blob: format!(
                    "{} {} {}",
                    both_langs(PaletteMsg::ActionToggleAi),
                    both_langs(PaletteMsg::ActionToggleAiSub),
                    both_langs(PaletteMsg::KwAi)
                ),
                action: PaletteAction::ToggleAiPanel,
            },
        );
        push(
            29,
            PaletteItem {
                kind: PaletteKind::Action,
                title: t(PaletteMsg::ActionToggleFiles).to_owned(),
                subtitle: t(PaletteMsg::ActionToggleFilesSub).to_owned(),
                search_blob: format!(
                    "{} {} {}",
                    both_langs(PaletteMsg::ActionToggleFiles),
                    both_langs(PaletteMsg::ActionToggleFilesSub),
                    both_langs(PaletteMsg::KwSftp)
                ),
                action: PaletteAction::ToggleFilePanel,
            },
        );
    }
    if has_active_ssh {
        push(
            32,
            PaletteItem {
                kind: PaletteKind::Action,
                title: t(PaletteMsg::ActionListeningPorts).to_owned(),
                subtitle: t(PaletteMsg::ActionListeningPortsSub).to_owned(),
                search_blob: format!(
                    "{} {} {}",
                    both_langs(PaletteMsg::ActionListeningPorts),
                    both_langs(PaletteMsg::ActionListeningPortsSub),
                    both_langs(PaletteMsg::KwForwards)
                ),
                action: PaletteAction::OpenListeningPorts,
            },
        );
    }

    // 5. 设置与管理页所有分类（含具体设置关键词，一步直达）
    for section in SettingsSection::ALL {
        let title = section.label().to_owned();
        let subtitle = t(section_kw_msg(section)).to_owned();
        let search_blob = format!("{} {}", title, section_keywords(section));
        push(
            20,
            PaletteItem {
                kind: PaletteKind::Settings,
                title,
                subtitle,
                search_blob,
                action: PaletteAction::OpenSettings(section),
            },
        );
    }

    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    candidates
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, _, item)| item)
        .collect()
}

impl MainWindow {
    pub(crate) fn open_command_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.modal, Some(Modal::CommandPalette(_))) {
            self.close_modal(window, cx);
            return;
        }
        let query = self.palette_query.clone();
        query.update(cx, |input, cx| input.set_value("", window, cx));
        let focus = query.read(cx).focus_handle(cx);
        self.modal = Some(Modal::CommandPalette(PaletteDialog { cursor: 0 }));
        window.focus(&focus, cx);
        cx.notify();
    }

    fn palette_items(&self, cx: &App) -> Vec<PaletteItem> {
        let query = self.palette_query.read(cx).value().to_string();
        let ws = self.workspace.read(cx);
        let active_ix = ws.active_index();
        let tabs: Vec<PaletteTab> = ws
            .tab_infos()
            .into_iter()
            .enumerate()
            .map(|(index, info)| PaletteTab {
                index,
                title: info.title,
                active: active_ix == Some(index),
            })
            .collect();
        let active_live_title = if ws.active_pane_is_live() && !ws.active_tab_is_blank() {
            active_ix
                .and_then(|ix| tabs.get(ix))
                .map(|tab| (tab.title.as_str(), ws.broadcast_on()))
        } else {
            None
        };
        let has_active_ssh = self.status_host.is_some();
        let quick = self.bar_settings().commands.clone();
        build_palette_items(
            &query,
            &tabs,
            self.store.hosts(),
            &quick,
            active_live_title,
            has_active_ssh,
        )
    }

    pub(super) fn move_palette_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.palette_items(cx).len();
        if count == 0 {
            return;
        }
        if let Some(Modal::CommandPalette(dialog)) = &mut self.modal {
            dialog.cursor = (dialog.cursor as isize + delta).clamp(0, count as isize - 1) as usize;
            cx.notify();
        }
    }

    pub(super) fn confirm_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cursor = match &self.modal {
            Some(Modal::CommandPalette(dialog)) => dialog.cursor,
            _ => return,
        };
        let items = self.palette_items(cx);
        let Some(item) = items.get(cursor.min(items.len().saturating_sub(1))) else {
            return;
        };
        let action = item.action.clone();
        self.execute_palette_action(action, window, cx);
    }

    fn execute_palette_action(
        &mut self,
        action: PaletteAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.modal = None;
        match action {
            PaletteAction::SwitchTab(index) => self.select_tab(index, window, cx),
            PaletteAction::ConnectHost(id) => self.open_host(id, window, cx),
            PaletteAction::RunQuickCommand(id) => {
                // 先切回可见的活动终端标签再发送，避免在主机页 / 设置页把命令悄悄发给看不见的终端。
                let target_tab = {
                    let ws = self.workspace.read(cx);
                    (ws.active_pane_is_live() && !ws.active_tab_is_blank())
                        .then(|| ws.active_index())
                        .flatten()
                };
                if let Some(index) = target_tab {
                    self.select_tab(index, window, cx);
                    self.run_quick_command(&id, false, window, cx);
                }
            }
            PaletteAction::OpenSettings(section) => {
                self.open_settings_section(section, window, cx);
            }
            PaletteAction::NewTab => self.new_tab(window, cx),
            PaletteAction::ToggleAiPanel => self.toggle_ai_panel(window, cx),
            PaletteAction::ToggleFilePanel => self.toggle_sftp_dock(window, cx),
            PaletteAction::OpenListeningPorts => {
                self.open_connections_dialog_listening(window, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn palette_body(
        &self,
        dialog: &PaletteDialog,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let items = self.palette_items(cx);
        let cursor = dialog.cursor.min(items.len().saturating_sub(1));
        let hover = tokens.hover;
        let selected_bg = tokens.accent_soft;

        let rows: Vec<AnyElement> = items
            .into_iter()
            .enumerate()
            .map(|(at, item)| {
                let is_sel = at == cursor;
                let icon = item.kind.icon(&item.action);
                let tag = item.kind.tag();
                let action = item.action.clone();
                let click_entity = entity.clone();
                h_flex()
                    .id(("palette-row", at))
                    .w_full()
                    .px_3()
                    .py_2()
                    .gap_2p5()
                    .items_center()
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .when(is_sel, |d| d.bg(selected_bg))
                    .when(!is_sel, |d| d.hover(move |s| s.bg(hover)))
                    .on_click(move |_, window, cx| {
                        let action = action.clone();
                        click_entity.update(cx, |this, cx| {
                            this.execute_palette_action(action, window, cx);
                        });
                    })
                    .child(
                        Icon::new(icon)
                            .size(px(15.0))
                            .text_color(if is_sel {
                                tokens.accent
                            } else {
                                tokens.text_muted
                            })
                            .flex_none(),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .typo(Role::ItemName, tokens)
                                    .truncate()
                                    .child(SharedString::from(item.title)),
                            )
                            .when(!item.subtitle.is_empty(), |d| {
                                d.child(
                                    div()
                                        .typo(Role::Caption, tokens)
                                        .text_color(tokens.text_muted)
                                        .truncate()
                                        .child(SharedString::from(item.subtitle)),
                                )
                            }),
                    )
                    .child(badge(tag, BadgeKind::Neutral, tokens))
                    .into_any_element()
            })
            .collect();

        v_flex()
            .w_full()
            .gap_3()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().child(Input::new(&self.palette_query)))
                    .child(badge(
                        if cfg!(target_os = "macos") {
                            "Cmd+Shift+P"
                        } else {
                            "Ctrl+Shift+P"
                        },
                        BadgeKind::Neutral,
                        tokens,
                    )),
            )
            .child(
                div()
                    .id("palette-list")
                    .w_full()
                    .max_h(px(360.0))
                    .overflow_y_scroll()
                    .child(
                        v_flex()
                            .w_full()
                            .gap_0p5()
                            .when(rows.is_empty(), |d| {
                                d.child(
                                    div()
                                        .py_6()
                                        .text_center()
                                        .typo(Role::Caption, tokens)
                                        .text_color(tokens.text_muted)
                                        .child(t(PaletteMsg::Empty)),
                                )
                            })
                            .children(rows),
                    ),
            )
            .child(
                div()
                    .pt_2()
                    .border_t_1()
                    .border_color(tokens.divider)
                    .typo(Role::Caption, tokens)
                    .text_color(tokens.text_faint)
                    .child(t(PaletteMsg::Hint)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{PaletteAction, PaletteTab, build_palette_items, fuzzy_score};
    use crate::main_window::quick_commands::QuickCommand;
    use crate::settings_nav::SettingsSection;
    use zeeterm_core::model::{AuthMethod, Host, ProtocolSettings, ProxyMode};

    fn sample_host(id: i64, name: &str, addr: &str) -> Host {
        Host {
            protocol: ProtocolSettings::default(),
            id,
            group_id: None,
            name: name.to_owned(),
            host: addr.to_owned(),
            port: 22,
            username: "ops".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: vec!["prod".to_owned()],
            sort: id,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn fuzzy_score_ranks_prefix_above_substring_and_subsequence() {
        let prefix = fuzzy_score("web-prod-1", "web").expect("prefix");
        let mid = fuzzy_score("api-web-1", "web").expect("mid");
        let subseq = fuzzy_score("w_e_b_server", "web").expect("subseq");
        assert!(prefix > mid);
        assert!(mid > subseq);
        assert!(fuzzy_score("web-prod-1", "ops 99").is_none());
        assert!(fuzzy_score("终端 字体 字号", "字体").is_some());
    }

    #[test]
    fn palette_searches_tabs_hosts_quick_commands_and_settings_keywords() {
        let tabs = vec![PaletteTab {
            index: 0,
            title: "web-1".to_owned(),
            active: true,
        }];
        let hosts = vec![sample_host(7, "db-primary", "192.0.2.10")];
        let quick = vec![QuickCommand {
            id: "q1".to_owned(),
            name: "检查磁盘".to_owned(),
            command: "df -h".to_owned(),
            group: Some("巡检".to_owned()),
            color: None,
            enter: true,
            no_suggest: false,
        }];

        let by_ip = build_palette_items(
            "192.0.2",
            &tabs,
            &hosts,
            &quick,
            Some(("web-1", false)),
            true,
        );
        assert!(
            by_ip
                .iter()
                .any(|item| item.action == PaletteAction::ConnectHost(7))
        );

        let by_setting =
            build_palette_items("字体", &tabs, &hosts, &quick, Some(("web-1", false)), true);
        assert!(
            by_setting
                .iter()
                .any(|item| item.action == PaletteAction::OpenSettings(SettingsSection::Terminal))
        );

        let by_quick =
            build_palette_items("df -h", &tabs, &hosts, &quick, Some(("web-1", true)), true);
        let quick_item = by_quick
            .iter()
            .find(|item| item.action == PaletteAction::RunQuickCommand("q1".to_owned()))
            .expect("connected tab lists quick command");
        assert!(
            quick_item.subtitle.contains("web-1") && quick_item.subtitle.contains("df -h"),
            "副标题应写明目标标签名和命令摘要：{:?}",
            quick_item.subtitle
        );

        // 空白或未连接的标签不列出快捷命令。
        let no_live = build_palette_items("df -h", &tabs, &hosts, &quick, None, false);
        assert!(
            !no_live
                .iter()
                .any(|item| matches!(item.action, PaletteAction::RunQuickCommand(_)))
        );
    }
}
