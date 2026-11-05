//! 面板标题栏的「上下文」用量指示（V2-U15）的显示规则：纯函数，不依赖 GPUI。
//!
//! 数据来自各智能体翻译层报的 [`ContextUsage`]（来源和口径见 `zssh_core::ai::agent::usage`），这里只决定
//! 怎么显示：有窗口大小就画进度条 + 百分比，超过 80% 警示色、超过 95% 危险色；没有窗口大小（认不出模型）
//! 只写用了多少 token；还没有数据（新对话、没发消息）写「—」。

use zssh_core::ai::agent::usage::{ContextUsage, UsageLevel};

use super::ChatMsg;
use crate::i18n::{Msg, t};

/// 上下文用量明细里的行名和「估计」标记。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum UsageMsg {
    Window,
    Input,
    Output,
    CacheRead,
    CacheWrite,
    Model,
    Estimated,
}

impl Msg for UsageMsg {
    fn zh(&self) -> &'static str {
        let key = *self;
        match key {
            Self::Window => "已用 / 窗口",
            Self::Input => "输入",
            Self::Output => "输出",
            Self::CacheRead => "缓存读",
            Self::CacheWrite => "缓存写",
            Self::Model => "模型",
            Self::Estimated => " 估计",
        }
    }

    fn en(&self) -> &'static str {
        let key = *self;
        match key {
            Self::Window => "Used / Window",
            Self::Input => "Input",
            Self::Output => "Output",
            Self::CacheRead => "Cache Read",
            Self::CacheWrite => "Cache Write",
            Self::Model => "Model",
            Self::Estimated => " est.",
        }
    }
}

/// 标题栏指示器的内容。
#[derive(Clone, Debug, PartialEq)]
pub enum MeterView {
    /// 还没有用量数据：「上下文 —」，不能点开。
    Empty,
    /// 有已用数、没有窗口大小：只写「上下文 12.3k」，不画进度条。
    Tokens { text: String },
    /// 进度条 + 百分比。`fraction` 在 0 到 1 之间（超过 100% 也只画满）。
    Bar {
        fraction: f32,
        percent_text: String,
        level: UsageLevel,
    },
}

pub fn meter(usage: Option<&ContextUsage>) -> MeterView {
    let Some(usage) = usage else {
        return MeterView::Empty;
    };
    match usage.percent() {
        Some(percent) => MeterView::Bar {
            fraction: (percent / 100.0).clamp(0.0, 1.0) as f32,
            percent_text: percent_text(percent),
            level: usage.level(),
        },
        None => MeterView::Tokens {
            text: compact(usage.used),
        },
    }
}

/// 百分比文字：不到 1% 写「<1%」，别的四舍五入到整数（超过 100% 照写，让人看得出爆了）。
pub fn percent_text(percent: f64) -> String {
    if percent <= 0.0 {
        "0%".to_owned()
    } else if percent < 1.0 {
        "<1%".to_owned()
    } else {
        format!("{}%", percent.round() as u64)
    }
}

/// 带千位分隔的完整数字：`12345` → `12,345`。
pub fn full(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// 紧凑写法：`950`、`12.3k`、`1.2M`。
pub fn compact(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => trim_zero(format!("{:.1}", n as f64 / 1_000.0), "k"),
        _ => trim_zero(format!("{:.1}", n as f64 / 1_000_000.0), "M"),
    }
}

fn trim_zero(text: String, unit: &str) -> String {
    format!("{}{unit}", text.strip_suffix(".0").unwrap_or(&text))
}

/// 下拉里的明细：（名称，值）。拿得到哪些显示哪些；`fallback_model` 是面板上选中的模型名，用量里没带
/// 模型名时用它（codex 的用量通知不带模型）。只展示，不加任何操作。
pub fn detail_lines(
    usage: &ContextUsage,
    fallback_model: Option<&str>,
) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let used = match (usage.window, usage.percent()) {
        (Some(window), Some(percent)) => format!(
            "{} / {}{}{}{}{}",
            full(usage.used),
            full(window),
            if usage.window_estimated {
                t(UsageMsg::Estimated)
            } else {
                ""
            },
            t(ChatMsg::ParenLeft),
            percent_text(percent),
            t(ChatMsg::ParenRight)
        ),
        _ => full(usage.used),
    };
    out.push((t(UsageMsg::Window), used));
    for (name, value) in [
        (t(UsageMsg::Input), usage.input),
        (t(UsageMsg::Output), usage.output),
        (t(UsageMsg::CacheRead), usage.cache_read),
        (t(UsageMsg::CacheWrite), usage.cache_write),
    ] {
        if let Some(value) = value {
            out.push((name, full(value)));
        }
    }
    if let Some(model) = usage
        .model
        .as_deref()
        .or(fallback_model)
        .filter(|model| !model.is_empty())
    {
        out.push((t(UsageMsg::Model), model.to_owned()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(used: u64, window: Option<u64>) -> ContextUsage {
        ContextUsage {
            used,
            window,
            ..ContextUsage::default()
        }
    }

    #[test]
    fn no_data_shows_a_dash_and_no_window_shows_only_tokens() {
        assert_eq!(meter(None), MeterView::Empty);
        assert_eq!(
            meter(Some(&usage(12_345, None))),
            MeterView::Tokens {
                text: "12.3k".into()
            }
        );
    }

    #[test]
    fn bar_fraction_percent_and_colour_thresholds() {
        let view = |used| meter(Some(&usage(used, Some(200_000))));
        assert_eq!(
            view(2_000),
            MeterView::Bar {
                fraction: 0.01,
                percent_text: "1%".into(),
                level: UsageLevel::Normal
            }
        );
        assert!(
            matches!(view(100), MeterView::Bar { ref percent_text, .. } if percent_text == "<1%")
        );
        assert!(
            matches!(
                view(160_000),
                MeterView::Bar {
                    level: UsageLevel::Normal,
                    ..
                }
            ),
            "刚好 80%"
        );
        assert!(matches!(
            view(160_002),
            MeterView::Bar {
                level: UsageLevel::Warning,
                ..
            }
        ));
        assert!(matches!(
            view(190_002),
            MeterView::Bar {
                level: UsageLevel::Danger,
                ..
            }
        ));
        // 超过窗口：条画满、颜色危险、百分比照实写。
        let MeterView::Bar {
            fraction,
            percent_text,
            level,
        } = view(260_000)
        else {
            panic!("应该有进度条");
        };
        assert_eq!(
            (fraction, percent_text.as_str(), level),
            (1.0, "130%", UsageLevel::Danger)
        );
    }

    #[test]
    fn number_formatting() {
        assert_eq!(full(0), "0");
        assert_eq!(full(999), "999");
        assert_eq!(full(12_345), "12,345");
        assert_eq!(full(1_234_567), "1,234,567");
        assert_eq!(compact(950), "950");
        assert_eq!(compact(1_000), "1k");
        assert_eq!(compact(12_345), "12.3k");
        assert_eq!(compact(1_260_000), "1.3M");
        assert_eq!(percent_text(0.0), "0%");
        assert_eq!(percent_text(0.4), "<1%");
        assert_eq!(percent_text(6.2), "6%");
    }

    #[test]
    fn detail_lists_what_is_known_and_marks_an_estimated_window() {
        let full_usage = ContextUsage {
            used: 4_511,
            window: Some(200_000),
            window_estimated: true,
            input: Some(2),
            output: Some(5),
            cache_read: Some(3_719),
            cache_write: Some(790),
            model: Some("claude-opus-5-5".into()),
        };
        let lines = detail_lines(&full_usage, Some("Opus"));
        assert_eq!(
            lines,
            vec![
                ("已用 / 窗口", "4,511 / 200,000 估计（2%）".to_owned()),
                ("输入", "2".to_owned()),
                ("输出", "5".to_owned()),
                ("缓存读", "3,719".to_owned()),
                ("缓存写", "790".to_owned()),
                ("模型", "claude-opus-5-5".to_owned()),
            ]
        );
        // codex：窗口是它报的（不标估计），缺的项不列，模型名用面板上选中的。
        let codex = ContextUsage {
            used: 23_000,
            window: Some(258_400),
            input: Some(2_000),
            output: Some(1_000),
            cache_read: Some(20_000),
            ..ContextUsage::default()
        };
        let lines = detail_lines(&codex, Some("GPT-5.5 · medium"));
        assert_eq!(
            lines[0],
            ("已用 / 窗口", "23,000 / 258,400（9%）".to_owned())
        );
        assert!(!lines.iter().any(|(name, _)| *name == "缓存写"));
        assert_eq!(lines.last(), Some(&("模型", "GPT-5.5 · medium".to_owned())));
        // 没有窗口也没有模型：只有已用。
        assert_eq!(
            detail_lines(&usage(500, None), None),
            vec![("已用 / 窗口", "500".to_owned())]
        );
    }
}
