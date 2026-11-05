//! 终端右键菜单里的「搜索」和「清屏」（V2-TERM-UX1）。
//!
//! 搜索条属于某一个窗格：命中、当前命中、大小写开关都记在 [`SearchState`] 里，查找本身在终端模型里做
//! （`zssh_terminal::TerminalModel::search`，按字面量，含回滚缓冲和软换行）。这里只管：
//! 什么时候重搜、当前命中选哪个、怎么滚过去、哪些格子要画高亮。
//! 两项都只在右键菜单里，不配快捷键（`Ctrl+Shift+F` 是文件面板，`Ctrl+Shift+K` 在串口里是 Break）。

use super::*;
use crate::app_theme::Tokens;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use zssh_terminal::{MAX_SEARCH_MATCHES, SearchMatch};

/// 所有命中的底色 / 文字色。固定色而不跟终端配色走：高亮要在任何配色下都比正文醒目。
const MATCH_BG: u32 = 0xF2CC60;
const MATCH_FG: u32 = 0x1A1A1A;
/// 当前命中：更深的橙色。
const CURRENT_BG: u32 = 0xFF8C1A;
const CURRENT_FG: u32 = 0x000000;

/// 搜索条的状态（不含输入框本身，输入框是工作区的一个实体）。
#[derive(Debug)]
pub(super) struct SearchState {
    /// 搜的是哪个窗格；它不在当前标签里了（切标签、被关）搜索条就收起。
    pub(super) pane_id: PaneId,
    pub(super) case_sensitive: bool,
    pub(super) query: String,
    /// 命中，按位置从旧到新排。
    pub(super) matches: Vec<SearchMatch>,
    pub(super) current: Option<usize>,
    /// 上次搜索时终端的文字修订号：变了说明有新输出，该重搜（滚动不算）。
    content_rev: u64,
}

impl SearchState {
    pub(super) fn new(pane_id: PaneId) -> Self {
        Self {
            pane_id,
            case_sensitive: false,
            query: String::new(),
            matches: Vec::new(),
            current: None,
            content_rev: 0,
        }
    }

    /// 重新搜一遍。`jump` 为真（查询或大小写开关变了）要重新选当前命中并滚到可见；
    /// 为假（只是终端有了新输出）保持原来的序号不动，也不滚屏——正在看别处的人不该被拽走。
    pub(super) fn run(&mut self, pane: &mut Pane, jump: bool) {
        let previous = self
            .current
            .and_then(|at| self.matches.get(at))
            .map(|found| found.start);
        self.content_rev = pane.pump().content_revision();
        self.matches = pane.pump().search(&self.query, self.case_sensitive);
        if self.matches.is_empty() {
            self.current = None;
            return;
        }
        if !jump {
            self.current = Some(self.current.unwrap_or(0).min(self.matches.len() - 1));
            return;
        }
        let pick = match previous {
            // 接着输入时留在原处：取不早于上一个当前命中的第一个，这样越输越精确而不乱跳；
            // 后面已经没有命中了就取最靠近它的上一个（最后一个），不绕回开头。
            Some(start) => self
                .matches
                .iter()
                .position(|found| found.start >= start)
                .unwrap_or(self.matches.len() - 1),
            // 刚开始搜：取视口底部以上最近的一个（站在底部的人先看到最新的），没有就取最旧的。
            None => {
                let bottom =
                    pane.snapshot.size.rows as i32 - 1 - pane.pump().display_offset() as i32;
                self.matches
                    .iter()
                    .rposition(|found| found.start.0 <= bottom)
                    .unwrap_or(0)
            }
        };
        self.current = Some(pick);
        self.reveal(pane);
    }

    /// 终端的文字自上次搜索以来没变（没有新输出）。
    pub(super) fn content_rev_matches(&self, pane: &Pane) -> bool {
        self.content_rev == pane.pump().content_revision()
    }

    /// 上一个 / 下一个，首尾相接。
    pub(super) fn step(&mut self, pane: &mut Pane, forward: bool) {
        let count = self.matches.len();
        if count == 0 {
            return;
        }
        let at = self.current.unwrap_or(0);
        self.current = Some(if forward {
            (at + 1) % count
        } else {
            (at + count - 1) % count
        });
        self.reveal(pane);
    }

    /// 把当前命中滚到可见区。
    fn reveal(&self, pane: &mut Pane) {
        let Some(found) = self.current.and_then(|at| self.matches.get(at)) else {
            return;
        };
        pane.pump_mut().scroll_to_line(found.start.0);
        pane.refresh();
    }

    /// 搜索条上的计数，如 `3/17`；封顶时是 `3/10000+`；没有命中是 `0/0`。
    pub(super) fn count_label(&self) -> String {
        let total = self.matches.len();
        let at = self.current.map_or(0, |at| at + 1);
        let more = if total >= MAX_SEARCH_MATCHES { "+" } else { "" };
        format!("{at}/{total}{more}")
    }

    /// 可见区每一行要画的高亮：所有命中一种底色，当前命中另一种。命中不重叠、按位置排序，
    /// 所以二分找到起点再往前补上从上一行折过来的那一个就够了。
    pub(super) fn visible_spans(&self, snapshot: &Snapshot) -> Vec<Vec<Span>> {
        let rows = snapshot.size.rows;
        let last_col = snapshot.size.cols.saturating_sub(1);
        let offset = snapshot.display_offset as i32;
        (0..rows)
            .map(|row| {
                let line = row as i32 - offset;
                let after = self.matches.partition_point(|found| found.start.0 <= line);
                let mut spans = Vec::new();
                for at in (0..after).rev() {
                    let found = &self.matches[at];
                    if found.end.0 < line {
                        break;
                    }
                    let (fg, bg) = if self.current == Some(at) {
                        (CURRENT_FG, CURRENT_BG)
                    } else {
                        (MATCH_FG, MATCH_BG)
                    };
                    spans.push(Span {
                        row,
                        start: if found.start.0 == line {
                            found.start.1
                        } else {
                            0
                        },
                        end: if found.end.0 == line {
                            found.end.1
                        } else {
                            last_col
                        },
                        fg,
                        bg: Some(bg),
                    });
                }
                spans.reverse();
                spans
            })
            .collect()
    }
}

/// 搜索条和清屏用到的文案。
#[derive(Clone, Copy, Debug)]
enum SearchMsg {
    Placeholder,
    Previous,
    Next,
    CaseSensitive,
    Close,
}

impl Msg for SearchMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Placeholder => "搜索",
            Self::Previous => "上一个（Shift+Enter）",
            Self::Next => "下一个（Enter）",
            Self::CaseSensitive => "区分大小写",
            Self::Close => "关闭（Esc）",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Placeholder => "Search",
            Self::Previous => "Previous (Shift+Enter)",
            Self::Next => "Next (Enter)",
            Self::CaseSensitive => "Match case",
            Self::Close => "Close (Esc)",
        }
    }
}

impl TerminalWorkspace {
    /// 搜索条所在的窗格（在当前标签里才算）。
    fn search_pane_mut(&mut self) -> Option<(&mut SearchState, &mut Pane)> {
        let state = self.search.as_mut()?;
        let tab = self.tabs.active_mut()?;
        let pane = tab.panes.iter_mut().find(|pane| pane.id == state.pane_id)?;
        Some((state, pane))
    }

    /// 右键菜单「搜索」：给 `pane_id` 窗格开搜索条并把键盘交给输入框。已经开着就重新聚焦并选中文字，
    /// 开在别的窗格上就搬过来，沿用输入框里的文字。有单行选区时拿它当初始查询。
    pub(super) fn open_search(
        &mut self,
        pane_id: PaneId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let selected = self.selected_text();
        let prefill = (!selected.is_empty() && !selected.contains('\n')).then_some(selected);
        // 占位文字跟着语言走，所以每次打开时重设。
        self.search_input.update(cx, |input, cx| {
            input.set_placeholder(t(SearchMsg::Placeholder), window, cx);
        });
        if self.search.as_ref().is_none_or(|s| s.pane_id != pane_id) {
            self.search = Some(SearchState::new(pane_id));
        }
        if let Some(text) = prefill {
            self.search_input
                .update(cx, |input, cx| input.set_value(text, window, cx));
        }
        self.search_text_changed(cx);
        let input = self.search_input.clone();
        // 菜单关掉时会把焦点还给终端：等这一拍过去再把焦点给输入框。
        window.defer(cx, move |window, cx| {
            input.update(cx, |input, cx| {
                input.focus(window, cx);
                input.select_all(window, cx);
            });
        });
        cx.notify();
    }

    /// 输入框里的文字变了：读成新的查询，重搜并跳到命中。文字没变（比如打开时重复通知）什么也不做。
    pub(super) fn search_text_changed(&mut self, cx: &mut Context<Self>) {
        let text = self.search_input.read(cx).value().to_string();
        let Some((state, pane)) = self.search_pane_mut() else {
            return;
        };
        if text == state.query && state.content_rev_matches(pane) {
            return;
        }
        state.query = text;
        state.run(pane, true);
        cx.notify();
    }

    /// Enter / Shift+Enter 和按钮：下一个 / 上一个。
    pub(super) fn search_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some((state, pane)) = self.search_pane_mut() {
            state.step(pane, forward);
            cx.notify();
        }
    }

    /// 「区分大小写」开关。
    pub(super) fn toggle_search_case(&mut self, cx: &mut Context<Self>) {
        if let Some((state, pane)) = self.search_pane_mut() {
            state.case_sensitive = !state.case_sensitive;
            state.run(pane, true);
            cx.notify();
        }
    }

    /// 收起搜索条，键盘还给终端。
    pub(super) fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            window.focus(&self.focus, cx);
            cx.notify();
        }
    }

    /// 每个节拍调一次：搜索的窗格不在当前标签里了就收起搜索条；终端有了新输出就重搜
    /// （高亮跟着内容走，不错位）。返回要不要重画。
    pub(super) fn tick_search(&mut self) -> bool {
        if self.search.is_none() {
            return false;
        }
        match self.search_pane_mut() {
            None => {
                self.search = None;
                true
            }
            Some((state, pane)) => {
                if state.content_rev_matches(pane) {
                    return false;
                }
                state.run(pane, false);
                true
            }
        }
    }

    /// 右键菜单「清屏」：清掉这个窗格的本地屏幕和回滚，只留光标所在行在第一行。不向远端发任何东西。
    pub(super) fn clear_pane_screen(&mut self, pane_id: PaneId, cx: &mut Context<Self>) {
        let Some(pane) = self
            .tabs
            .active_mut()
            .and_then(|tab| tab.panes.iter_mut().find(|pane| pane.id == pane_id))
        else {
            return;
        };
        pane.pump_mut().clear_screen();
        pane.refresh();
        // 选区、悬停链接的坐标都对不上了。
        self.selection = None;
        self.selecting = false;
        self.drag_anchor = None;
        self.hover_link = None;
        self.tick_search();
        cx.notify();
    }

    /// 搜索条：输入框、计数、区分大小写、上一个 / 下一个、关闭。画在被搜窗格的右上角。
    pub(super) fn search_bar(&self, tokens: &Tokens, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.search.as_ref()?;
        let entity = cx.entity();
        let case_on = state.case_sensitive;
        let no_match = !state.query.is_empty() && state.matches.is_empty();
        let count = state.count_label();
        let button = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id).ghost().compact().icon(icon).tooltip(tip)
        };
        Some(
            h_flex()
                .id("terminal-search")
                .absolute()
                .top(::gpui::px(6.0))
                .right(::gpui::px(14.0))
                .w(::gpui::px(380.0))
                .gap_1()
                .items_center()
                .px_1p5()
                .py_1()
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .bg(tokens.surface)
                .shadow_md()
                // 条上的点击、滚轮不能落到后面的终端里（会抢焦点、开始选区、滚动）。
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
                .on_scroll(|_, _, cx| cx.stop_propagation())
                // Esc 在输入框的动作之前拦下，收条并把键盘还给终端。
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    let keystroke = &event.keystroke;
                    if keystroke.key == "escape" && !keystroke.modifiers.modified() {
                        this.close_search(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(
                    Input::new(&self.search_input)
                        .small()
                        .flex_1()
                        .min_w_0()
                        .prefix(Icon::new(IconName::Search)),
                )
                .child(
                    div()
                        .flex_none()
                        .min_w(::gpui::px(52.0))
                        .text_xs()
                        .text_color(if no_match {
                            tokens.danger
                        } else {
                            tokens.text_muted
                        })
                        .child(count),
                )
                .child({
                    let entity = entity.clone();
                    button(
                        "terminal-search-case",
                        IconName::CaseSensitive,
                        t(SearchMsg::CaseSensitive),
                    )
                    .selected(case_on)
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.toggle_search_case(cx));
                    })
                })
                .child({
                    let entity = entity.clone();
                    button(
                        "terminal-search-prev",
                        IconName::ChevronUp,
                        t(SearchMsg::Previous),
                    )
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.search_step(false, cx));
                    })
                })
                .child({
                    let entity = entity.clone();
                    button(
                        "terminal-search-next",
                        IconName::ChevronDown,
                        t(SearchMsg::Next),
                    )
                    .on_click(move |_, _, cx| {
                        entity.update(cx, |this, cx| this.search_step(true, cx));
                    })
                })
                .child({
                    let entity = entity.clone();
                    button(
                        "terminal-search-close",
                        IconName::Close,
                        t(SearchMsg::Close),
                    )
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| this.close_search(window, cx));
                    })
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::SearchState;
    use crate::terminal_workspace::Pane;
    use zssh_terminal::Size;

    const SIZE: Size = Size { rows: 6, cols: 30 };

    /// 一个离线窗格，里面先喂好 `lines`（每行一个），代次 1。
    fn pane_with(lines: &[String]) -> Pane {
        let mut pane = Pane::new(SIZE, 1000, 1);
        for line in lines {
            pane.pump_mut().on_data(1, format!("{line}\r\n").as_bytes());
        }
        pane.refresh();
        pane
    }

    fn numbered(prefix: &str, count: usize) -> Vec<String> {
        (0..count).map(|i| format!("{prefix} {i}")).collect()
    }

    fn searcher(pane: &Pane, query: &str) -> SearchState {
        let mut state = SearchState::new(pane.id);
        state.query = query.to_owned();
        state
    }

    #[test]
    fn typing_starts_at_the_newest_match_above_the_bottom_and_scrolls_to_it() {
        let mut pane = pane_with(&numbered("needle", 40));
        let mut state = searcher(&pane, "needle");
        state.run(&mut pane, true);
        assert_eq!(state.matches.len(), 40);
        // 站在底部：先看到最新的一个（屏幕上可见的），不用滚。
        assert_eq!(state.current, Some(39));
        assert_eq!(pane.snapshot.display_offset, 0);
        assert_eq!(state.count_label(), "40/40");
    }

    #[test]
    fn stepping_cycles_through_the_matches_and_scrolls_the_history_into_view() {
        let mut pane = pane_with(&numbered("needle", 40));
        let mut state = searcher(&pane, "needle");
        state.run(&mut pane, true);

        // 下一个：从最后一个回到第一个（最旧的，在回滚缓冲里），视口跟着滚过去。
        state.step(&mut pane, true);
        assert_eq!(state.current, Some(0));
        assert_eq!(state.count_label(), "1/40");
        assert!(pane.snapshot.display_offset > 0, "最旧的在回滚里，要滚上去");
        let visible = pane.snapshot.rows.iter().any(|row| row.text == "needle 0");
        assert!(visible, "命中滚进了可见区");

        // 上一个：从第一个回到最后一个。
        state.step(&mut pane, false);
        assert_eq!(state.current, Some(39));
        assert!(pane.snapshot.rows.iter().any(|row| row.text == "needle 39"));
        // 再往前一步走到倒数第二个。
        state.step(&mut pane, false);
        assert_eq!(state.current, Some(38));
    }

    #[test]
    fn the_case_switch_changes_what_matches() {
        let mut pane = pane_with(&["Error".to_owned(), "error".to_owned(), "ERROR".to_owned()]);
        let mut state = searcher(&pane, "error");
        state.run(&mut pane, true);
        assert_eq!(state.matches.len(), 3);
        state.case_sensitive = true;
        state.run(&mut pane, true);
        assert_eq!(state.matches.len(), 1);
        assert_eq!(state.count_label(), "1/1");
        state.query = "Error".to_owned();
        state.run(&mut pane, true);
        assert_eq!(state.matches.len(), 1);
    }

    #[test]
    fn no_match_shows_zero_and_stepping_does_nothing() {
        let mut pane = pane_with(&numbered("abc", 3));
        let mut state = searcher(&pane, "zzz");
        state.run(&mut pane, true);
        assert_eq!(state.current, None);
        assert_eq!(state.count_label(), "0/0");
        state.step(&mut pane, true);
        state.step(&mut pane, false);
        assert_eq!(state.current, None);
        // 空查询也是 0/0，不报错。
        state.query.clear();
        state.run(&mut pane, true);
        assert_eq!(state.count_label(), "0/0");
    }

    #[test]
    fn refining_the_query_stays_near_the_current_spot() {
        let mut lines = numbered("alpha beta", 20);
        lines.extend(numbered("alpha gamma", 20));
        let mut pane = pane_with(&lines);
        let mut state = searcher(&pane, "alpha");
        state.run(&mut pane, true);
        // 往回走到前半部分的某一个命中。
        for _ in 0..25 {
            state.step(&mut pane, false);
        }
        let at = state.matches[state.current.unwrap()].start;
        // 接着输入 "alpha g"：命中变少，但当前命中不该跳回开头，而是取不早于原位置的第一个。
        state.query = "alpha g".to_owned();
        state.run(&mut pane, true);
        let now = state.matches[state.current.unwrap()].start;
        assert!(now >= at, "{now:?} 应该不早于 {at:?}");
        assert_eq!(state.matches.len(), 20);
    }

    #[test]
    fn refining_past_the_last_match_falls_back_to_the_nearest_earlier_one_not_the_start() {
        // 当前命中在最后（"alpha 29" 之后没有别的）；把查询收窄到只剩前面的命中，应落在最后一个，而不是绕回第一个。
        let mut lines = numbered("alpha", 30);
        lines.push("tail".to_owned());
        let mut pane = pane_with(&lines);
        let mut state = searcher(&pane, "a");
        state.run(&mut pane, true);
        let last = state.matches.len() - 1;
        assert_eq!(state.current, Some(last));
        state.query = "alpha 1".to_owned();
        state.run(&mut pane, true);
        assert_eq!(state.current, Some(state.matches.len() - 1));
        assert!(state.matches.len() > 1);
    }

    #[test]
    fn highlights_cover_every_visible_match_and_mark_the_current_one() {
        let mut pane = pane_with(&["xx hit yy hit".to_owned(), "none".to_owned()]);
        let mut state = searcher(&pane, "hit");
        state.run(&mut pane, true);
        assert_eq!(state.matches.len(), 2);
        let spans = state.visible_spans(&pane.snapshot);
        assert_eq!(spans.len(), SIZE.rows);
        let row = &spans[0];
        assert_eq!(row.len(), 2);
        assert_eq!((row[0].start, row[0].end), (3, 5));
        assert_eq!((row[1].start, row[1].end), (10, 12));
        // 当前命中（最后一个）的底色和其余的不同。
        assert_ne!(row[0].bg, row[1].bg);
        assert!(spans[1].is_empty() && spans[2].is_empty());
    }

    #[test]
    fn a_match_wrapped_over_two_rows_is_highlighted_on_both() {
        let mut pane = Pane::new(Size { rows: 6, cols: 10 }, 100, 1);
        pane.pump_mut().on_data(1, b"abcdefghijklmno\r\n");
        pane.refresh();
        let mut state = SearchState::new(pane.id);
        state.query = "fghijkl".to_owned();
        state.run(&mut pane, true);
        let spans = state.visible_spans(&pane.snapshot);
        assert_eq!((spans[0][0].start, spans[0][0].end), (5, 9));
        assert_eq!((spans[1][0].start, spans[1][0].end), (0, 1));
    }

    #[test]
    fn highlights_follow_the_view_when_scrolled_back() {
        let mut pane = pane_with(&numbered("needle", 40));
        let mut state = searcher(&pane, "needle");
        state.run(&mut pane, true);
        pane.pump_mut().scroll_to_top();
        pane.refresh();
        let spans = state.visible_spans(&pane.snapshot);
        // 滚到最顶上：屏幕上全是命中。
        assert!(spans.iter().all(|row| row.len() == 1));
    }

    #[test]
    fn new_output_updates_the_matches_without_moving_the_index() {
        let mut pane = pane_with(&numbered("needle", 40));
        let mut state = searcher(&pane, "needle");
        state.run(&mut pane, true);
        state.step(&mut pane, true); // 回到第一个，视口在回滚里
        assert!(state.content_rev_matches(&pane));

        pane.pump_mut().on_data(1, b"needle again\r\n");
        pane.refresh();
        assert!(!state.content_rev_matches(&pane), "有新输出，该重搜了");
        state.run(&mut pane, false);
        assert_eq!(state.matches.len(), 41);
        assert_eq!(state.current, Some(0), "序号不动");
        assert!(state.content_rev_matches(&pane));
    }

    #[test]
    fn the_index_is_clamped_when_the_matches_shrink() {
        let mut pane = pane_with(&numbered("needle", 10));
        let mut state = searcher(&pane, "needle");
        state.run(&mut pane, true);
        assert_eq!(state.current, Some(9));
        // 清屏之后旧命中全没了，只剩新输出里的一处：序号要夹回范围内，不能越界。
        pane.pump_mut().clear_screen();
        pane.pump_mut().on_data(1, b"needle\r\n");
        pane.refresh();
        state.run(&mut pane, false);
        assert_eq!(state.matches.len(), 1);
        assert_eq!(state.current, Some(0));
    }

    #[test]
    fn the_count_label_marks_a_capped_search() {
        let mut state = SearchState::new(1);
        state.matches = vec![
            zssh_terminal::SearchMatch {
                start: (0, 0),
                end: (0, 0)
            };
            zssh_terminal::MAX_SEARCH_MATCHES
        ];
        state.current = Some(2);
        assert_eq!(state.count_label(), "3/10000+");
    }
}
