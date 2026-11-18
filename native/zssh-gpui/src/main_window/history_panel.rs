//! 命令历史面板（V2-U48）：主窗口右侧侧栏里的一个面板（和快捷命令管理共用 `side_panel` 的外壳和宽度），
//! 列出**活动窗格正在往里记的那个历史桶**，从新到旧；点一行把命令填回终端。
//!
//! - 数据只读自 `zssh_core::terminal_assist::history` 的进程内存历史（不写盘、不同步、不提供删除单条；
//!   清空照旧在「设置 → 终端 → 命令建议」里）。桶的选法和采集是同一个：[`HistoryScope`] + 窗格的目标身份。
//! - 刷新：面板自己缓存一份（`HistoryUi`），渲染时只比较「桶换没换」（切标签 / 窗格，不加锁），
//!   新记录靠主窗口 500 ms 的节拍里比较历史的内容修订号（`HistoryStore::revision`，一次短暂的加锁、一个整数），
//!   修订号变了才把整桶重读一遍。所以没有每帧加锁全量读取。
//! - 操作：单击 = 填入（不回车）；悬停出现的「运行」= 填入并回车；右键 = 填入 / 运行 / 复制。填入和运行
//!   走快捷命令发送的同一条路（[`MainWindow::send_text`] → `do_send`）：多行确认、窗格没连着的提示、
//!   发完键盘回到终端都和快捷命令一样；复制走可替换的写入口 [`crate::chat_copy`]。
//!   一律发给**活动窗格**，不理会输入广播（历史是这台主机自己的命令，不该因为开了广播就在别的窗格执行）。

use ::gpui::{ClickEvent, Entity, MouseButton, SharedString, Window, px};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zssh_core::terminal_assist::history::{HistoryHit, HistoryScope};

use super::MainWindow;
use super::side_panel::{COMMAND_HISTORY, PanelContent};
use super::widgets::{ButtonLook, act, empty_state, text_button};
use crate::app_theme::Tokens;
use crate::i18n::{Msg, t};
use crate::icons::Ico;
use crate::settings_nav::SettingsSection;

/// 面板里没有列表、改显示一段说明时的原因（优先级从上到下）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmptyReason {
    /// 「命令建议」总开关关着。
    AssistOff,
    /// 「记录命令历史」关着。
    HistoryOff,
    /// 活动窗格没有历史目标（离线 / 空白窗格；不是连到主机的会话）。
    NotHostSession,
    /// 这个桶里还一条都没有。
    NoRecords,
    /// 有记录，但搜索没命中。
    NoMatches,
}

impl EmptyReason {
    /// 窗口测试里认说明的名字（只用在 `debug_selector` 上）。
    pub fn slug(self) -> &'static str {
        match self {
            Self::AssistOff => "assist-off",
            Self::HistoryOff => "history-off",
            Self::NotHostSession => "not-host",
            Self::NoRecords => "no-records",
            Self::NoMatches => "no-matches",
        }
    }
}

/// 决定面板显示什么：`None` = 显示列表。
pub fn empty_reason(
    assist_enabled: bool,
    history_enabled: bool,
    has_bucket: bool,
    total: usize,
    shown: usize,
) -> Option<EmptyReason> {
    if !assist_enabled {
        Some(EmptyReason::AssistOff)
    } else if !history_enabled {
        Some(EmptyReason::HistoryOff)
    } else if !has_bucket {
        Some(EmptyReason::NotHostSession)
    } else if total == 0 {
        Some(EmptyReason::NoRecords)
    } else if shown == 0 {
        Some(EmptyReason::NoMatches)
    } else {
        None
    }
}

/// 搜索：命令文字里含有搜索词（子串、不分大小写）；搜索词是空的或全是空白就是全部。
/// 搜索词不去掉首尾空白：命令里的空格有意义（`git ` 和 `git` 不是一回事）。
pub fn filter<'a>(entries: &'a [HistoryHit], query: &str) -> Vec<&'a HistoryHit> {
    if query.trim().is_empty() {
        return entries.iter().collect();
    }
    let needle = query.to_lowercase();
    entries
        .iter()
        .filter(|hit| hit.text.to_lowercase().contains(&needle))
        .collect()
}

/// 面板自己的状态：搜索框 + 缓存的一桶内容。
pub struct HistoryUi {
    pub search: Entity<InputState>,
    /// 缓存的是哪个桶；`None` = 没有桶。
    bucket: Option<String>,
    /// 缓存读到时的历史内容修订号。
    revision: u64,
    entries: Vec<HistoryHit>,
}

impl HistoryUi {
    pub fn new(window: &mut Window, cx: &mut Context<MainWindow>) -> Self {
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(t(HistoryMsg::SearchPlaceholder)));
        Self {
            search,
            bucket: None,
            revision: 0,
            entries: Vec::new(),
        }
    }

    #[cfg(any(test, debug_assertions))]
    pub fn entries(&self) -> &[HistoryHit] {
        &self.entries
    }
}

/// 命令历史面板的界面文案。命令本身是用户数据，原样显示。
#[derive(Clone, Copy, Debug)]
enum HistoryMsg {
    Title,
    ScopeHost,
    ScopeGlobal,
    SearchPlaceholder,
    SwitchTip,
    Fill,
    Run,
    Copy,
    Copied,
    RowHint,
    RunTip,
    UsedPrefix,
    UsedSuffix,
    AssistOffTitle,
    AssistOffHint,
    HistoryOffTitle,
    HistoryOffHint,
    OpenSettings,
    NotHostTitle,
    NotHostHint,
    NoRecordsTitle,
    NoRecordsHint,
    NoMatches,
}

impl Msg for HistoryMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Title => "命令历史",
            Self::ScopeHost => "当前主机",
            Self::ScopeGlobal => "所有主机共用",
            Self::SearchPlaceholder => "搜索历史命令",
            Self::SwitchTip => "命令历史",
            Self::Fill => "填入",
            Self::Run => "运行",
            Self::Copy => "复制",
            Self::Copied => "已复制命令",
            Self::RowHint => "\n单击填入（不回车）",
            Self::RunTip => "填入并回车执行",
            Self::UsedPrefix => "\n已使用 ",
            Self::UsedSuffix => " 次",
            Self::AssistOffTitle => "命令建议已关闭",
            Self::AssistOffHint => {
                "命令历史属于「命令建议」。到「设置 → 终端 → 命令建议」里打开它，之后执行的命令才会记录。"
            }
            Self::HistoryOffTitle => "没有在记录命令历史",
            Self::HistoryOffHint => {
                "到「设置 → 终端 → 命令建议」里打开「记录命令历史」，之后执行的命令才会记录。"
            }
            Self::OpenSettings => "前往终端设置",
            Self::NotHostTitle => "当前窗格没有命令历史",
            Self::NotHostHint => {
                "命令历史只记录连到主机的终端会话（SSH 等）；离线标签和空白标签没有。切到一个已连接的会话再看。"
            }
            Self::NoRecordsTitle => "还没有记录",
            Self::NoRecordsHint => {
                "要远端 shell 开着 Shell 集成（命令建议里可以为本次会话一键启用）才会记录执行过的命令；历史只保留在本次运行期间，退出 ZeeTerm 就清空。"
            }
            Self::NoMatches => "没有匹配的命令",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Title => "Command History",
            Self::ScopeHost => "This host",
            Self::ScopeGlobal => "Shared by all hosts",
            Self::SearchPlaceholder => "Search history",
            Self::SwitchTip => "Command History",
            Self::Fill => "Fill in",
            Self::Run => "Run",
            Self::Copy => "Copy",
            Self::Copied => "Command copied",
            Self::RowHint => "\nClick to fill in (without Enter)",
            Self::RunTip => "Fill in and press Enter",
            Self::UsedPrefix => "\nUsed ",
            Self::UsedSuffix => " time(s)",
            Self::AssistOffTitle => "Command Suggestions are off",
            Self::AssistOffHint => {
                "Command history is part of Command Suggestions. Turn it on under Settings → Terminal → Command Suggestions; commands run after that are recorded."
            }
            Self::HistoryOffTitle => "Command history is not being recorded",
            Self::HistoryOffHint => {
                "Turn on \"Record command history\" under Settings → Terminal → Command Suggestions; commands run after that are recorded."
            }
            Self::OpenSettings => "Open terminal settings",
            Self::NotHostTitle => "This pane has no command history",
            Self::NotHostHint => {
                "Command history is only recorded for terminal sessions connected to a host (SSH and so on), not for offline or blank tabs. Switch to a connected session."
            }
            Self::NoRecordsTitle => "Nothing recorded yet",
            Self::NoRecordsHint => {
                "Commands are only recorded when the remote shell has shell integration on (Command Suggestions can enable it for this session in one click). History is kept for this run only and is cleared when ZeeTerm exits."
            }
            Self::NoMatches => "No matching commands",
        }
    }
}

impl MainWindow {
    // ---- 开关 ----

    pub(super) fn history_panel_visible(&self) -> bool {
        self.panels_on_page() && self.panels.visible() == Some(COMMAND_HISTORY)
    }

    /// 状态栏按钮：开 / 关命令历史面板（侧栏同一时间只显示一个面板，开它会换掉快捷命令管理）。
    pub(crate) fn toggle_history_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panels.visible() == Some(COMMAND_HISTORY) {
            self.panels.close_side();
            self.focus_content(window, cx);
        } else {
            self.panels.open_side(COMMAND_HISTORY);
            self.history_reload(cx);
        }
        cx.notify();
    }

    // ---- 刷新 ----

    fn history_scope(&self) -> HistoryScope {
        self.prefs.terminal_assist_history_scope
    }

    /// 无条件重读活动窗格对应的桶。
    fn history_reload(&mut self, cx: &mut Context<Self>) {
        let bucket = self
            .workspace
            .read(cx)
            .active_history_bucket(self.history_scope());
        let (revision, entries) = match &bucket {
            Some(bucket) => self.workspace.read(cx).history_snapshot(bucket),
            None => (self.workspace.read(cx).history_revision(), Vec::new()),
        };
        self.history_ui.bucket = bucket;
        self.history_ui.revision = revision;
        self.history_ui.entries = entries;
    }

    /// 渲染时调：只看桶换没换（切标签 / 窗格 / 改了范围），不为了看新记录加锁。
    fn history_follow_bucket(&mut self, cx: &mut Context<Self>) {
        let bucket = self
            .workspace
            .read(cx)
            .active_history_bucket(self.history_scope());
        if bucket != self.history_ui.bucket {
            self.history_reload(cx);
        }
    }

    /// 500 ms 节拍里调：面板开着才做事；桶换了或历史内容修订号变了才重读。返回要不要重绘。
    pub(super) fn history_tick(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.history_panel_visible() {
            return false;
        }
        let bucket = self
            .workspace
            .read(cx)
            .active_history_bucket(self.history_scope());
        let revision = self.workspace.read(cx).history_revision();
        if bucket == self.history_ui.bucket && revision == self.history_ui.revision {
            return false;
        }
        self.history_reload(cx);
        true
    }

    // ---- 操作 ----

    /// 填入（`run = false`）或填入并回车（`run = true`）：和快捷命令同一条发送路径，只发活动窗格。
    pub(crate) fn history_send(
        &mut self,
        text: &str,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.send_text(text, text, run, false, None, window, cx);
    }

    pub(crate) fn history_copy(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        crate::chat_copy::write(text.to_owned(), cx);
        self.notice(t(HistoryMsg::Copied), window, cx);
    }

    // ---- 画 ----

    fn history_row(
        &self,
        index: usize,
        hit: &HistoryHit,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let entity = cx.entity();
        let (hover, text_color, muted, accent) =
            (tokens.hover, tokens.text, tokens.text_muted, tokens.accent);
        let group: SharedString = format!("history-row-group-{index}").into();
        let command = hit.text.clone();
        let tooltip: SharedString = format!(
            "{}{}{}{}{}",
            command,
            t(HistoryMsg::RowHint),
            t(HistoryMsg::UsedPrefix),
            hit.uses,
            t(HistoryMsg::UsedSuffix)
        )
        .into();
        let run_button = {
            let entity = entity.clone();
            let command = command.clone();
            h_flex()
                .id(("history-run", index))
                .debug_selector(move || format!("history-run-{index}"))
                // 不用 occlude：盖住行的话，鼠标移到按钮上时行就不再算「悬停」，按钮会闪着消失。
                .flex_none()
                .gap_1()
                .items_center()
                .px_2()
                .h(px(20.0))
                .rounded_full()
                .cursor_pointer()
                .bg(tokens.accent_soft)
                .text_color(accent)
                .text_xs()
                .invisible()
                .group_hover(group.clone(), |style| style.visible())
                .tooltip(|window, cx| Tooltip::new(t(HistoryMsg::RunTip)).build(window, cx))
                // 按下就吃掉：不让整行的「单击填入」也跟着触发。
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(Icon::new(Ico::SendHorizontal).size_3())
                .child(t(HistoryMsg::Run))
                .on_click(move |_: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    let command = command.clone();
                    entity.update(cx, |this, cx| this.history_send(&command, true, window, cx));
                })
        };
        let fill_entity = entity.clone();
        let fill_command = command.clone();
        let menu_entity = entity;
        let menu_command = command.clone();
        h_flex()
            .id(("history-row", index))
            .debug_selector(move || format!("history-row-{index}"))
            .group(group)
            .w_full()
            .min_h(px(28.0))
            .px_2()
            .gap_1p5()
            .items_center()
            .rounded_full()
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(text_color)
                    .child(SharedString::from(command)),
            )
            .when(hit.uses > 1, |row| {
                row.child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("×{}", hit.uses)),
                )
            })
            .child(run_button)
            .on_click(move |_: &ClickEvent, window, cx| {
                let command = fill_command.clone();
                fill_entity.update(cx, |this, cx| {
                    this.history_send(&command, false, window, cx)
                });
            })
            .context_menu(move |menu, _window, cx| {
                let tokens = Tokens::current(cx);
                let (fill, run, copy) = (
                    menu_entity.clone(),
                    menu_entity.clone(),
                    menu_entity.clone(),
                );
                let (a, b, c) = (
                    menu_command.clone(),
                    menu_command.clone(),
                    menu_command.clone(),
                );
                menu.item(
                    crate::menu_kit::row(&tokens, t(HistoryMsg::Fill), Ico::ClipboardPaste)
                        .on_click(move |_, window, cx| {
                            let command = a.clone();
                            fill.update(cx, |this, cx| {
                                this.history_send(&command, false, window, cx);
                            });
                        }),
                )
                .item(
                    crate::menu_kit::row(&tokens, t(HistoryMsg::Run), Ico::SendHorizontal)
                        .on_click(move |_, window, cx| {
                            let command = b.clone();
                            run.update(cx, |this, cx| {
                                this.history_send(&command, true, window, cx);
                            });
                        }),
                )
                .item(
                    crate::menu_kit::row(&tokens, t(HistoryMsg::Copy), IconName::Copy).on_click(
                        move |_, window, cx| {
                            let command = c.clone();
                            copy.update(cx, |this, cx| this.history_copy(&command, window, cx));
                        },
                    ),
                )
            })
            .into_any_element()
    }

    /// 没有列表时显示的说明（原因 + 怎么办）。
    fn history_empty(
        &self,
        reason: EmptyReason,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, hint) = match reason {
            EmptyReason::AssistOff => (HistoryMsg::AssistOffTitle, Some(HistoryMsg::AssistOffHint)),
            EmptyReason::HistoryOff => (
                HistoryMsg::HistoryOffTitle,
                Some(HistoryMsg::HistoryOffHint),
            ),
            EmptyReason::NotHostSession => {
                (HistoryMsg::NotHostTitle, Some(HistoryMsg::NotHostHint))
            }
            EmptyReason::NoRecords => (HistoryMsg::NoRecordsTitle, Some(HistoryMsg::NoRecordsHint)),
            EmptyReason::NoMatches => (HistoryMsg::NoMatches, None),
        };
        // 关着的两种原因给一个跳到终端设置页的按钮（命令建议卡片在那一页；设置页还不能直接落到某张卡片）。
        let button =
            matches!(reason, EmptyReason::AssistOff | EmptyReason::HistoryOff).then(|| {
                let entity = cx.entity();
                let button = text_button(
                    "history-open-settings",
                    t(HistoryMsg::OpenSettings),
                    ButtonLook::Outline,
                    act(&entity, |this, window, cx| {
                        this.open_settings_section(SettingsSection::Terminal, window, cx);
                    }),
                )
                .into_any_element();
                div()
                    .debug_selector(|| "history-open-settings".to_owned())
                    .child(button)
                    .into_any_element()
            });
        empty_state(t(title), hint.map(t), button, tokens)
            .id("history-empty")
            .debug_selector(move || format!("history-empty-{}", reason.slug()))
            .into_any_element()
    }

    pub(super) fn history_panel_content(
        &mut self,
        tokens: &Tokens,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> PanelContent {
        self.history_follow_bucket(cx);
        let query = self.history_ui.search.read(cx).value().to_string();
        let scope = self.history_scope();
        let entries = std::mem::take(&mut self.history_ui.entries);
        let shown: Vec<&HistoryHit> = filter(&entries, &query);
        let reason = empty_reason(
            self.prefs.terminal_assist_enabled,
            self.prefs.terminal_assist_history,
            self.history_ui.bucket.is_some(),
            entries.len(),
            shown.len(),
        );
        let body_list: Option<AnyElement> = reason.is_none().then(|| {
            let rows: Vec<AnyElement> = shown
                .iter()
                .enumerate()
                .map(|(index, hit)| self.history_row(index, hit, tokens, cx))
                .collect();
            v_flex()
                .id("history-list")
                .debug_selector(|| "history-list".to_owned())
                .w_full()
                .gap_0p5()
                .children(rows)
                .into_any_element()
        });
        // 有桶时搜索框常驻（搜索没命中时它得留着，不然没法改词）；关着 / 没有桶时不画它。
        let searchable = !matches!(
            reason,
            Some(EmptyReason::AssistOff | EmptyReason::HistoryOff | EmptyReason::NotHostSession)
        );
        let search = searchable.then(|| {
            div()
                .debug_selector(|| "history-search".to_owned())
                .child(
                    Input::new(&self.history_ui.search)
                        .prefix(Icon::new(IconName::Search))
                        .cleanable(true),
                )
                .into_any_element()
        });
        let empty = reason.map(|reason| self.history_empty(reason, tokens, cx));
        self.history_ui.entries = entries;
        // 范围说明只在真的有桶可看时给（命令建议 / 历史关着时它没有意义）。
        let subtitle: Option<SharedString> =
            (searchable && self.history_ui.bucket.is_some()).then(|| {
                match scope {
                    HistoryScope::Host => t(HistoryMsg::ScopeHost),
                    HistoryScope::Global => t(HistoryMsg::ScopeGlobal),
                }
                .into()
            });
        PanelContent {
            title: t(HistoryMsg::Title).into(),
            subtitle,
            back: None,
            actions: Vec::new(),
            body: v_flex()
                .w_full()
                .gap_3()
                .children(search)
                .children(body_list)
                .children(empty)
                .into_any_element(),
            footer: None,
        }
    }

    /// 状态栏按钮的悬停提示。
    pub(super) fn history_switch_tip() -> &'static str {
        t(HistoryMsg::SwitchTip)
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{EmptyReason, empty_reason, filter};
    use zssh_core::terminal_assist::history::HistoryHit;

    fn hit(text: &str, uses: u32, last_used: u64) -> HistoryHit {
        HistoryHit {
            text: text.into(),
            uses,
            last_used,
        }
    }

    #[test]
    fn the_empty_reasons_follow_one_precedence_order() {
        // 顺序：命令建议关 > 历史关 > 没有桶 > 桶是空的 > 搜索没命中。
        assert_eq!(
            empty_reason(false, false, false, 0, 0),
            Some(EmptyReason::AssistOff)
        );
        assert_eq!(
            empty_reason(false, true, true, 5, 5),
            Some(EmptyReason::AssistOff)
        );
        assert_eq!(
            empty_reason(true, false, true, 5, 5),
            Some(EmptyReason::HistoryOff)
        );
        assert_eq!(
            empty_reason(true, true, false, 5, 5),
            Some(EmptyReason::NotHostSession)
        );
        assert_eq!(
            empty_reason(true, true, true, 0, 0),
            Some(EmptyReason::NoRecords)
        );
        assert_eq!(
            empty_reason(true, true, true, 5, 0),
            Some(EmptyReason::NoMatches)
        );
        assert_eq!(empty_reason(true, true, true, 5, 2), None);
    }

    #[test]
    fn filtering_is_a_case_insensitive_substring_and_keeps_the_order() {
        let entries = vec![
            hit("systemctl restart Nginx", 1, 3),
            hit("ls -la /var/log", 2, 2),
            hit("tail -f /var/log/nginx/error.log", 1, 1),
        ];
        let texts = |found: Vec<&HistoryHit>| -> Vec<String> {
            found.into_iter().map(|hit| hit.text.clone()).collect()
        };
        assert_eq!(texts(filter(&entries, "")).len(), 3);
        assert_eq!(texts(filter(&entries, "   ")).len(), 3, "全是空白 = 没搜索");
        assert_eq!(
            texts(filter(&entries, "NGINX")),
            [
                "systemctl restart Nginx",
                "tail -f /var/log/nginx/error.log"
            ],
            "不分大小写、子串（不只是前缀）、保持从新到旧"
        );
        assert_eq!(texts(filter(&entries, "/var/log")).len(), 2);
        assert!(filter(&entries, "不存在").is_empty());
        assert_eq!(
            texts(filter(&entries, "ls ")),
            ["ls -la /var/log"],
            "搜索词里的空格有意义，不被去掉"
        );
    }

    #[test]
    fn a_non_ascii_search_matches_without_panicking() {
        let entries = vec![hit("echo 你好 World", 1, 1)];
        assert_eq!(filter(&entries, "你好").len(), 1);
        assert_eq!(filter(&entries, "WORLD").len(), 1);
    }
}
