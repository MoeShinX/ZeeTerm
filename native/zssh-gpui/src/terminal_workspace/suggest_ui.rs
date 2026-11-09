//! 终端内命令建议的浮层和交互入口（按键、鼠标、输入法、"选择建议"）。
//!
//! 候选只画在浮层里，锚定活动窗格的光标，底部放不下就向上翻转；**不写进终端格子、复制内容和会话日志**。
//! 每行显示命令文字和来源，悬浮提示只给这一行自己的完整文字（候选已经过敏感过滤，敏感的根本不在列表里）。
//!
//! 浮层上的点击不落到后面的终端里（只处理本地动作）；滚轮不拦——滚轮穿过去，由终端的滚动照常处理一次，
//! 同时关闭浮层。点终端、拖选、鼠标上报同理：先关浮层，原事件照常处理一次。

use super::*;
use crate::app_theme::Tokens;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use zssh_core::terminal_assist::suggest::{Candidate, CandidateSource};

use super::assist_ops::SuggestView;

/// 浮层里一行的高度、顶部标题条的高度、四周留白（逻辑像素）。
pub(super) const ROW_H: f32 = 26.0;
pub(super) const HEADER_H: f32 = 28.0;
const PAD: f32 = 4.0;
/// 浮层和光标格子之间留的缝。
const GAP: f32 = 2.0;
/// 浮层的宽度范围。
const MIN_W: f32 = 300.0;
const MAX_W: f32 = 640.0;
/// 一行里除了命令文字之外要占的宽度：来源标签 + 填入按钮 + 内边距。
const ROW_EXTRA_W: f32 = 150.0;

/// 命令建议浮层上的文案。
#[derive(Clone, Copy, Debug)]
pub(super) enum SuggestMsg {
    Title,
    SourceHistory,
    SourceQuick,
    Fill,
    FillTip,
    SelectButton,
    SelectTip,
    SelectHint,
    ExitSelect,
}

impl Msg for SuggestMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::Title => "命令建议",
            Self::SourceHistory => "历史",
            Self::SourceQuick => "快捷命令",
            Self::Fill => "填入",
            Self::FillTip => "只填入后面的部分，不会回车执行",
            Self::SelectButton => "选择",
            Self::SelectTip => "选择建议：用方向键选，Tab 或 Enter 填入",
            Self::SelectHint => "↑↓ 选择 · Tab / Enter 填入 · Esc 关闭",
            Self::ExitSelect => "退出选择",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::Title => "Suggestions",
            Self::SourceHistory => "History",
            Self::SourceQuick => "Quick command",
            Self::Fill => "Fill in",
            Self::FillTip => "Only fills in the rest of the command; it is not executed",
            Self::SelectButton => "Select",
            Self::SelectTip => "Select a suggestion: arrow keys to pick, Tab or Enter to fill in",
            Self::SelectHint => "↑↓ select · Tab / Enter fill in · Esc close",
            Self::ExitSelect => "Exit",
        }
    }
}

/// 浮层里的一行（纯数据，测试用它断言"画什么"）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OverlayRow {
    /// 完整命令（同时也是悬浮提示的内容）。
    pub(crate) text: String,
    pub(crate) source: CandidateSource,
    pub(crate) selected: bool,
}

/// 整个浮层的模型。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OverlayModel {
    pub(crate) rows: Vec<OverlayRow>,
    pub(crate) select_mode: bool,
}

pub(crate) fn overlay_model(view: &SuggestView<'_>) -> OverlayModel {
    OverlayModel {
        rows: view
            .items
            .iter()
            .enumerate()
            .map(|(index, item): (usize, &Candidate)| OverlayRow {
                text: item.text.clone(),
                source: item.source,
                selected: view.selected == Some(index),
            })
            .collect(),
        select_mode: view.select_mode,
    }
}

/// 浮层的位置和大小（相对窗格左上角，逻辑像素）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Placement {
    pub(crate) left: f32,
    pub(crate) top: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    /// 向上翻转：底部放不下，画在光标那一行的上方。
    pub(crate) flipped: bool,
}

/// 浮层总高度：标题条 + 每行 + 上下留白。
pub(crate) fn overlay_height(rows: usize) -> f32 {
    HEADER_H + rows as f32 * ROW_H + PAD * 2.0
}

/// 锚定光标：默认在光标那一行的下面；底部放不下就翻到上面；上下都放不下就取空间大的一侧并夹在窗格内。
/// 水平方向从光标列开始，右边放不下就左移，始终留在窗格里。
pub(crate) fn place(
    cursor_x: f32,
    cursor_y: f32,
    cell_h: f32,
    rows: usize,
    content_w: f32,
    pane_w: f32,
    pane_h: f32,
) -> Placement {
    let height = overlay_height(rows);
    let width = content_w
        .clamp(MIN_W, MAX_W)
        .min((pane_w - PAD * 2.0).max(0.0));
    let left = cursor_x.min(pane_w - width - PAD).max(PAD);
    let below = cursor_y + cell_h + GAP;
    let fits_below = below + height <= pane_h;
    let above = cursor_y - GAP - height;
    let (top, flipped) = if fits_below {
        (below, false)
    } else if above >= 0.0 {
        (above, true)
    } else {
        // 两边都放不下：取空间大的一侧，夹在窗格里。
        let room_below = pane_h - below;
        let room_above = cursor_y - GAP;
        if room_below >= room_above {
            (below.min((pane_h - height).max(0.0)), false)
        } else {
            (0.0, true)
        }
    };
    Placement {
        left,
        top,
        width,
        height,
        flipped,
    }
}

/// 浮层里最长一行需要的宽度（等宽字：字符数 × 单元格宽度）。
pub(crate) fn content_width(model: &OverlayModel, cell_width: f32) -> f32 {
    let longest = model
        .rows
        .iter()
        .map(|row| row.text.chars().count())
        .max()
        .unwrap_or(0);
    longest as f32 * cell_width + ROW_EXTRA_W
}

fn source_label(source: CandidateSource) -> &'static str {
    t(match source {
        CandidateSource::History => SuggestMsg::SourceHistory,
        CandidateSource::QuickCommand => SuggestMsg::SourceQuick,
    })
}

/// 用户自己绑定的"选择建议"键（设置里存的是按键写法，如 `ctrl-alt-s`；空串 = 没绑）和这次按键是不是同一个组合。
pub(crate) fn bound_key_matches(stored: &str, keystroke: &::gpui::Keystroke) -> bool {
    if stored.is_empty() {
        return false;
    }
    ::gpui::Keystroke::parse_with_os(stored, ::gpui::OperatingSystem::Windows)
        .map(|bound| bound.key == keystroke.key && bound.modifiers == keystroke.modifiers)
        .unwrap_or(false)
}

/// 把一次按键写成设置里存的按键写法：`ctrl-alt-shift-cmd-fn-键`（与平台无关的固定拼法）。
/// 只有带 Ctrl / Alt / Cmd 之一的组合才能绑（光是一个字母或 Shift+字母是普通输入，不能被拦）；
/// 纯修饰键本身也不行。不合格返回 `None`。
pub(crate) fn binding_text(keystroke: &::gpui::Keystroke) -> Option<String> {
    let mods = &keystroke.modifiers;
    if !(mods.control || mods.alt || mods.platform) {
        return None;
    }
    if matches!(
        keystroke.key.as_str(),
        "" | "control" | "alt" | "shift" | "platform" | "function" | "escape" | "enter" | "tab"
    ) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    if mods.control {
        parts.push("ctrl");
    }
    if mods.alt {
        parts.push("alt");
    }
    if mods.shift {
        parts.push("shift");
    }
    if mods.platform {
        parts.push("cmd");
    }
    if mods.function {
        parts.push("fn");
    }
    parts.push(&keystroke.key);
    Some(parts.join("-"))
}

impl TerminalWorkspace {
    pub(crate) fn suggest_in_select_mode(&self) -> bool {
        self.pane().is_some_and(|pane| pane.assist.in_select_mode())
    }

    /// 显式进入选择模式（状态栏 / 浮层上的"选择建议"入口）。已经在选择模式里就退出。
    pub(crate) fn suggest_toggle_select(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(pane) = self.pane_mut() else {
            return false;
        };
        let changed = if pane.assist.in_select_mode() {
            pane.assist.exit_select_mode();
            true
        } else {
            pane.assist.enter_select_mode()
        };
        cx.notify();
        changed
    }

    /// 调试驱动：聚焦窗格现在展示的候选文字。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_suggest_texts(&self) -> Vec<String> {
        self.pane()
            .and_then(|pane| pane.assist.suggest_view())
            .map(|view| view.items.iter().map(|item| item.text.clone()).collect())
            .unwrap_or_default()
    }

    /// 调试驱动：选择模式里选中了第几条。
    #[cfg(debug_assertions)]
    pub(crate) fn dev_suggest_selected(&self) -> Option<usize> {
        self.pane()
            .and_then(|pane| pane.assist.suggest_view())
            .and_then(|view| view.selected)
    }

    /// 设置页改了命令建议的开关：马上套用到所有窗格，不等 60 ms 的节拍。
    /// 关掉自动建议会立即收起浮层和选择模式；关掉历史采集会清空内存里已有的历史。
    pub(crate) fn apply_assist_prefs(&mut self) {
        let active = self.tabs.active_index();
        self.sync_assist_restrictions(active);
    }

    /// 清除活动窗格所属目标的内存历史；返回清掉了几条。
    pub(crate) fn clear_history_current(&mut self) -> usize {
        self.pane_mut()
            .map_or(0, |pane| pane.assist.clear_target_history())
    }

    /// 清除全部内存历史（所有目标）；返回清掉了几条。
    pub(crate) fn clear_history_all(&mut self) -> usize {
        let cleared =
            zssh_core::terminal_assist::history::lock(&super::assist_ops::shared_history())
                .clear_all();
        self.suggest_dismiss();
        cleared
    }

    /// 关掉所有窗格的列表和选择模式（点终端、滚动、拖选、鼠标上报、开关关掉）。原事件由调用方照常处理。
    pub(super) fn suggest_dismiss(&mut self) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.assist.dismiss_suggestions();
            }
        }
    }

    /// 焦点离开终端：清掉接受键的 latch（不管松开事件有没有收到）。
    pub(super) fn suggest_focus_lost(&mut self) {
        for tab in self.tabs.items_mut() {
            for pane in &mut tab.panes {
                pane.assist.focus_lost();
            }
        }
    }

    /// 键盘入口：返回这次按键是否已被建议这一层消费（消费了就不再发给 shell）。
    ///
    /// 不显式进入选择模式时，Tab / Enter / 上下键（含 Alt、Ctrl+Alt 组合）一律不拦，原样走编码发给 shell。
    pub(super) fn suggest_key_down(&mut self, event: &KeyDownEvent) -> bool {
        let keystroke = &event.keystroke;
        let stored = crate::app_prefs::cached()
            .terminal_assist_select_key
            .clone();
        // 输入法正在预编辑时绑定键不生效（按键属于输入法）。
        let bound = self.ime_preedit.is_empty() && bound_key_matches(&stored, keystroke);
        let plain = !keystroke.modifiers.modified();
        let Some(pane) = self.pane_mut() else {
            return false;
        };
        pane.suggest_key(&keystroke.key, plain, event.is_held, bound)
    }

    /// 按键松开：释放接受键的 latch。
    pub(super) fn on_key_up(
        &mut self,
        event: &::gpui::KeyUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.active_mut() {
            for pane in &mut tab.panes {
                pane.assist.release_latch(&event.keystroke.key);
            }
        }
    }

    /// 浮层上"填入"：校验后只追加后缀，焦点回到终端，不发回车。
    pub(super) fn suggest_click_accept(
        &mut self,
        pane_id: PaneId,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self
            .tabs
            .active_mut()
            .and_then(|tab| tab.panes.iter_mut().find(|pane| pane.id == pane_id))
        {
            let _ = pane.accept_candidate(index);
        }
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn suggest_click_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.suggest_toggle_select(cx);
        window.focus(&self.focus, cx);
    }

    /// 聚焦窗格的候选浮层；没有候选（或窗格还没排过版）是 `None`。
    pub(super) fn suggest_overlay(
        &self,
        pane: &Pane,
        index: usize,
        rect: (f32, f32),
        mono: &::gpui::SharedString,
        tokens: &Tokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = pane.assist.suggest_view()?;
        if pane.snapshot.display_offset != 0 {
            return None;
        }
        let model = overlay_model(&view);
        let (pane_w, pane_h) = rect;
        let cursor_x = PANE_PAD + pane.snapshot.cursor_col as f32 * self.cell_width;
        let cursor_y = PANE_PAD + pane.snapshot.cursor_row as f32 * self.cell_height;
        let at = place(
            cursor_x,
            cursor_y,
            self.cell_height,
            model.rows.len(),
            content_width(&model, self.cell_width),
            pane_w,
            pane_h,
        );
        let entity = cx.entity();
        let pane_id = pane.id;
        let font_size = self.settings.font_size;
        let select_mode = model.select_mode;
        let rows = model.rows.iter().enumerate().map(|(row_index, row)| {
            let accept = entity.clone();
            let tip: ::gpui::SharedString = row.text.clone().into();
            let selected = row.selected;
            h_flex()
                .id(("suggest-row", index * 16 + row_index))
                .h(::gpui::px(ROW_H))
                .w_full()
                .px_2()
                .gap_2()
                .items_center()
                .when(selected, |d| d.bg(tokens.accent.alpha(0.22)))
                .child(
                    div()
                        .id(("suggest-text", index * 16 + row_index))
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(mono.clone())
                        .text_size(::gpui::px(font_size))
                        .text_color(tokens.text)
                        .child(row.text.clone())
                        .tooltip(move |window, cx| {
                            gpui_kit::component::tooltip::Tooltip::new(tip.clone())
                                .build(window, cx)
                        }),
                )
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(tokens.text_muted)
                        .child(source_label(row.source)),
                )
                .child(
                    Button::new(("suggest-fill", index * 16 + row_index))
                        .ghost()
                        .compact()
                        .label(t(SuggestMsg::Fill))
                        .tooltip(t(SuggestMsg::FillTip))
                        .on_click(move |_, window, cx| {
                            accept.update(cx, |this, cx| {
                                this.suggest_click_accept(pane_id, row_index, window, cx);
                            });
                        }),
                )
        });
        let toggle = entity.clone();
        let header = h_flex()
            .h(::gpui::px(HEADER_H))
            .w_full()
            .px_2()
            .gap_2()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(tokens.text_muted)
                    .child(if select_mode {
                        t(SuggestMsg::SelectHint)
                    } else {
                        t(SuggestMsg::Title)
                    }),
            )
            .child(
                Button::new(("suggest-select", index))
                    .ghost()
                    .compact()
                    .selected(select_mode)
                    .label(if select_mode {
                        t(SuggestMsg::ExitSelect)
                    } else {
                        t(SuggestMsg::SelectButton)
                    })
                    .tooltip(t(SuggestMsg::SelectTip))
                    .on_click(move |_, window, cx| {
                        toggle.update(cx, |this, cx| this.suggest_click_select(window, cx));
                    }),
            );
        Some(
            v_flex()
                .id(("suggest-overlay", index))
                .absolute()
                .left(::gpui::px(at.left))
                .top(::gpui::px(at.top))
                .w(::gpui::px(at.width))
                .h(::gpui::px(at.height))
                .py(::gpui::px(PAD))
                .rounded_md()
                .border_1()
                .border_color(tokens.border)
                .bg(tokens.surface)
                .shadow_md()
                .overflow_hidden()
                // 浮层上的点击只处理本地动作，不落到后面的终端里（不抢焦点、不开始选区）。
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
                .on_mouse_down(MouseButton::Middle, |_, _, cx| cx.stop_propagation())
                // 注意：不拦滚轮。滚轮穿过去由终端处理一次，同时关闭浮层。
                .child(header)
                .children(rows)
                .into_any_element(),
        )
    }
}
