//! AI 面板输入框底部那一行（执行模式、模型、思考强度、发送 / 停止）的版面规则：纯函数，不依赖 GPUI，
//! 画的时候按这里算出来的表走，窄面板下怎么收缩也在这里定下来并有单测。
//!
//! 三个选择本身的存储和判定（按智能体记忆、模型 / 强度联动、`ai_exec_mode`）都不在这里，
//! 仍是 `main_window::model_picker` 和 `AgentChatDemo` 里原来那些。

use zssh_core::ai::bridge::ExecMode;

/// 工具行里的一项（从左到右）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposerItem {
    Mode,
    Model,
    /// 思考强度：`Full` 显示文字，`IconOnly` 只显示图标（悬停显示当前值）。
    Effort(EffortStyle),
    /// 右边的发送 / 停止（中间的弹性空白不算项）。
    Action(ActionKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffortStyle {
    Full,
    IconOnly,
}

/// 发送按钮：回合进行中变成停止。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Send,
    Stop,
}

/// 执行模式的色调：「全部放行」用警示色，其余是普通的次要文字色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeTone {
    Neutral,
    Caution,
}

pub fn mode_tone(mode: ExecMode) -> ModeTone {
    match mode {
        ExecMode::Auto => ModeTone::Caution,
        ExecMode::Manual | ExecMode::ReadOnly => ModeTone::Neutral,
    }
}

/// 一行里估算的宽度：中文 / 全角按 13、其余按 7（12px 字号），不求精确，只用来判断窄面板要不要收缩。
fn text_width(text: &str) -> f32 {
    text.chars()
        .map(|c| if c.is_ascii() { 7.0 } else { 13.0 })
        .sum()
}

/// 带箭头和内边距的下拉按钮在文字之外占的宽度（按 12 到 13px 字号在窗口里量出来的）。
const BUTTON_CHROME: f32 = 42.0;
/// 执行模式按钮前面多一个图标。
const MODE_ICON: f32 = 26.0;
/// 模型名最少留这么宽（再窄就只剩省略号），先把它让出来，不够再收强度。
const MODEL_MIN_WIDTH: f32 = 84.0;
/// 只剩图标的强度按钮宽度。
const ICON_BUTTON: f32 = 30.0;
/// 发送 / 停止按钮宽度。
const ACTION_WIDTH: f32 = 30.0;
/// 工具行左右内边距加输入卡片的边框和外边距（面板宽度里扣掉）。
const ROW_CHROME: f32 = 16.0 + 2.0 + 16.0;
/// 按钮之间的间隙和估算误差留的余量：宁可让模型名多省略一点，也别让一行折成两行。
const SLACK: f32 = 16.0;

/// 面板宽度 `panel_width`（还没量到是 `None`）下强度怎么显示：先让模型名省略，模型名已经缩到
/// 最小宽度还放不下，就把强度收成只显示图标。
pub fn effort_style(panel_width: Option<f32>, mode_label: &str, effort_label: &str) -> EffortStyle {
    let Some(width) = panel_width else {
        return EffortStyle::Full;
    };
    let available = width - ROW_CHROME - ACTION_WIDTH - SLACK;
    let need = text_width(mode_label)
        + BUTTON_CHROME
        + MODE_ICON
        + MODEL_MIN_WIDTH
        + text_width(effort_label)
        + BUTTON_CHROME;
    if need > available {
        EffortStyle::IconOnly
    } else {
        EffortStyle::Full
    }
}

/// 模型按钮最多占多宽：面板宽度扣掉模式、强度、发送按钮和内边距，剩下的都给它（再多也不超过 260），
/// 文字放不下就省略。还没量到面板宽度（第一帧）按 220。
pub fn model_max_width(
    panel_width: Option<f32>,
    mode_label: &str,
    effort: Option<(EffortStyle, &str)>,
) -> f32 {
    let Some(width) = panel_width else {
        return 220.0;
    };
    let effort_width = match effort {
        None => 0.0,
        Some((EffortStyle::Full, label)) => text_width(label) + BUTTON_CHROME,
        Some((EffortStyle::IconOnly, _)) => ICON_BUTTON,
    };
    let left = width
        - ROW_CHROME
        - ACTION_WIDTH
        - SLACK
        - (text_width(mode_label) + BUTTON_CHROME + MODE_ICON)
        - effort_width;
    left.clamp(MODEL_MIN_WIDTH, 260.0)
}

/// 工具行有哪些项、什么次序。`has_effort`：当前模型有强度可选（沿用 `model_picker` 的判定，
/// 没有就整个隐藏，不占位）。
pub fn composer_items(
    has_effort: bool,
    running: bool,
    panel_width: Option<f32>,
    mode_label: &str,
    effort_label: &str,
) -> Vec<ComposerItem> {
    let mut items = vec![ComposerItem::Mode, ComposerItem::Model];
    if has_effort {
        items.push(ComposerItem::Effort(effort_style(
            panel_width,
            mode_label,
            effort_label,
        )));
    }
    items.push(ComposerItem::Action(if running {
        ActionKind::Stop
    } else {
        ActionKind::Send
    }));
    items
}

#[cfg(test)]
mod tests {
    // 不能 `use super::*`：它会带进 gpui 的 `test` 宏，盖住内置的 `#[test]`。
    use super::{
        ActionKind, ComposerItem, EffortStyle, ModeTone, composer_items, effort_style, mode_tone,
        model_max_width,
    };
    use zssh_core::ai::bridge::ExecMode;

    #[test]
    fn the_row_runs_mode_model_effort_then_send() {
        assert_eq!(
            composer_items(true, false, Some(480.0), "逐条确认", "Medium"),
            [
                ComposerItem::Mode,
                ComposerItem::Model,
                ComposerItem::Effort(EffortStyle::Full),
                ComposerItem::Action(ActionKind::Send),
            ]
        );
    }

    #[test]
    fn the_effort_item_is_hidden_when_the_model_has_no_effort() {
        assert_eq!(
            composer_items(false, false, Some(480.0), "逐条确认", "默认强度"),
            [
                ComposerItem::Mode,
                ComposerItem::Model,
                ComposerItem::Action(ActionKind::Send),
            ]
        );
    }

    #[test]
    fn send_turns_into_stop_while_a_turn_is_running() {
        let items = composer_items(true, true, Some(480.0), "只读自动", "High");
        assert_eq!(
            items.last(),
            Some(&ComposerItem::Action(ActionKind::Stop)),
            "停止在最右边"
        );
        assert_eq!(items.len(), 4, "其余的项不变");
    }

    #[test]
    fn a_wide_panel_keeps_the_effort_label_and_a_narrow_one_collapses_it_to_an_icon() {
        assert_eq!(
            effort_style(Some(520.0), "逐条确认", "Medium"),
            EffortStyle::Full
        );
        assert_eq!(
            effort_style(Some(300.0), "逐条确认", "Medium"),
            EffortStyle::IconOnly
        );
        // 还没量到面板宽度（第一帧）：按完整样式画。
        assert_eq!(effort_style(None, "逐条确认", "Medium"), EffortStyle::Full);
    }

    #[test]
    fn the_collapse_only_gets_more_eager_as_the_panel_narrows() {
        let mut collapsed_at = None;
        for width in (200..=600).rev().step_by(10) {
            let style = effort_style(Some(width as f32), "全部放行", "XHigh");
            match (style, collapsed_at) {
                (EffortStyle::IconOnly, None) => collapsed_at = Some(width),
                (EffortStyle::Full, Some(first)) => {
                    panic!("宽 {width} 又恢复了完整样式（上面 {first} 已经收缩）")
                }
                _ => {}
            }
        }
        assert!(collapsed_at.is_some(), "窄到一定程度一定会收缩");
    }

    #[test]
    fn only_the_allow_all_mode_is_drawn_as_a_caution() {
        assert_eq!(mode_tone(ExecMode::Auto), ModeTone::Caution);
        assert_eq!(mode_tone(ExecMode::Manual), ModeTone::Neutral);
        assert_eq!(mode_tone(ExecMode::ReadOnly), ModeTone::Neutral);
    }

    #[test]
    fn the_model_name_gets_squeezed_first_and_never_below_its_minimum() {
        let wide = model_max_width(Some(520.0), "逐条确认", Some((EffortStyle::Full, "Medium")));
        let narrow = model_max_width(Some(380.0), "逐条确认", Some((EffortStyle::Full, "Medium")));
        assert!(
            wide > narrow,
            "面板越窄，模型名能占的宽度越小：{wide} {narrow}"
        );
        let tiny = model_max_width(
            Some(200.0),
            "全部放行",
            Some((EffortStyle::IconOnly, "High")),
        );
        assert_eq!(tiny, super::MODEL_MIN_WIDTH, "再窄也给省略号留一点地方");
        assert_eq!(model_max_width(None, "逐条确认", None), 220.0);
        // 强度收成图标之后，模型名能多占一些。
        let full = model_max_width(Some(340.0), "逐条确认", Some((EffortStyle::Full, "Medium")));
        let icon = model_max_width(
            Some(340.0),
            "逐条确认",
            Some((EffortStyle::IconOnly, "Medium")),
        );
        assert!(icon > full);
    }
}
