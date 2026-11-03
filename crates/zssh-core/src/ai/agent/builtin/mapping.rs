//! 能力 → 请求参数的映射（V2-A3）：用户在界面选的模型 + 思考强度，加上 A1 算出的 [`ModelCaps`]，
//! 变成 provider 层的 [`ReasoningParam`] 和输出上限。
//!
//! # 模型选择串
//!
//! `AgentConversation::send` 只有一个 `model: Option<&str>`。内置助手沿用 Claude Code 驱动的写法
//! `模型/强度`：最后一个 `/` 后面是已知的强度记号才拆（`gpt-5.5/high`、`claude-opus-5/off`、
//! `deepseek-v4/budget:4000`）；本来就带 `/` 的模型名（`deepseek-ai/DeepSeek-V4`）不会被误拆，
//! 因为后缀不是强度记号。整串正好是接口里已知的模型名时不拆。

use super::catalog::{ModelCaps, ReasoningSupport};
use super::config::EndpointKind;
use super::provider::ReasoningParam;

/// 用户选的思考强度。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ThinkingChoice {
    /// 没选：思考参数一个都不发，模型按服务端默认走。
    #[default]
    Default,
    /// 关闭思考。
    Off,
    /// 开（只能开关的模型）。
    On,
    /// 某个强度档（`low` / `medium` / `high` / `xhigh` / `max` / `minimal`）。
    Level(String),
    /// 直接给 token 预算。
    Budget(u32),
}

/// 已知的强度记号（小写）。
const LEVEL_TOKENS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];
/// 强度由低到高的顺序，选的档模型不支持时按它就近取。
const LEVEL_ORDER: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 解析 `模型/强度`。`known_models` 是接口里已知的模型名（整串命中就不拆）。
pub fn parse_model_selection(raw: &str, known_models: &[String]) -> (String, ThinkingChoice) {
    let raw = raw.trim();
    if known_models.iter().any(|m| m == raw) {
        return (raw.to_owned(), ThinkingChoice::Default);
    }
    if let Some((name, suffix)) = raw.rsplit_once('/') {
        if !name.is_empty() {
            if let Some(choice) = parse_choice(suffix) {
                return (name.to_owned(), choice);
            }
        }
    }
    (raw.to_owned(), ThinkingChoice::Default)
}

fn parse_choice(token: &str) -> Option<ThinkingChoice> {
    let lower = token.trim().to_ascii_lowercase();
    match lower.as_str() {
        "off" | "none" | "disabled" => Some(ThinkingChoice::Off),
        "on" | "enabled" => Some(ThinkingChoice::On),
        level if LEVEL_TOKENS.contains(&level) => Some(ThinkingChoice::Level(lower)),
        other => other
            .strip_prefix("budget:")
            .and_then(|n| n.trim().parse::<u32>().ok())
            .map(ThinkingChoice::Budget),
    }
}

/// 界面上这个模型可选的思考强度记号（拼在模型名后面用）。不支持思考调节就是空。
pub fn thinking_options(caps: &ModelCaps) -> Vec<String> {
    match &caps.reasoning {
        ReasoningSupport::None => Vec::new(),
        ReasoningSupport::Toggle => vec!["off".to_owned(), "on".to_owned()],
        ReasoningSupport::Levels(levels) => levels
            .iter()
            // `none` 是"关"，界面统一叫 off。
            .map(|l| {
                if l == "none" {
                    "off".to_owned()
                } else {
                    l.clone()
                }
            })
            .collect(),
        ReasoningSupport::Budget { .. } => ["off", "low", "medium", "high"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
    }
}

/// 把用户的选择变成请求里的思考参数。
///
/// - 用户没选 → `Unspecified`（什么都不发）；
/// - 模型没有可调的思考（`None`，含"常开不可调"）→ 用户没明确要关就不发；要关发 `Off`；
/// - `Levels` → `Effort(档)`，档不在清单里就近取；`Off` 时清单有 `none` 的发 `Effort("none")`
///   （OpenAI 系），Anthropic 不发任何东西（思考强制开 / 只能 adaptive 的新模型发 `disabled` 会 400）；
/// - `Budget{min,max}` → `Budget(n)`，档位按区间比例换算，用户直接给的值夹在区间内；
/// - `Toggle` → 开发 `Effort("high")`（DeepSeek 一类的开关会因此打开），关发 `Off`。
pub fn reasoning_param(
    kind: EndpointKind,
    caps: &ModelCaps,
    choice: &ThinkingChoice,
) -> ReasoningParam {
    if *choice == ThinkingChoice::Default {
        return ReasoningParam::Unspecified;
    }
    match &caps.reasoning {
        ReasoningSupport::None => match choice {
            ThinkingChoice::Off => ReasoningParam::Off,
            _ => ReasoningParam::Unspecified,
        },
        ReasoningSupport::Toggle => match choice {
            ThinkingChoice::Off => ReasoningParam::Off,
            _ => ReasoningParam::Effort("high".to_owned()),
        },
        ReasoningSupport::Levels(levels) => match choice {
            ThinkingChoice::Off => {
                if kind == EndpointKind::OpenAiChat && levels.iter().any(|l| l == "none") {
                    ReasoningParam::Effort("none".to_owned())
                } else if kind == EndpointKind::Anthropic {
                    ReasoningParam::Unspecified
                } else {
                    ReasoningParam::Off
                }
            }
            ThinkingChoice::Level(level) => nearest_level(levels, level)
                .map_or(ReasoningParam::Unspecified, |l| {
                    ReasoningParam::Effort(l.to_owned())
                }),
            // "开"、直接给预算：没有更具体的信息，取中间档。
            ThinkingChoice::On | ThinkingChoice::Budget(_) => nearest_level(levels, "medium")
                .map_or(ReasoningParam::Unspecified, |l| {
                    ReasoningParam::Effort(l.to_owned())
                }),
            ThinkingChoice::Default => ReasoningParam::Unspecified,
        },
        ReasoningSupport::Budget { min, max } => match choice {
            ThinkingChoice::Off => ReasoningParam::Off,
            ThinkingChoice::Budget(n) => ReasoningParam::Budget((*n).clamp(*min, *max.max(min))),
            ThinkingChoice::Level(level) => ReasoningParam::Budget(budget_for(level, *min, *max)),
            ThinkingChoice::On => ReasoningParam::Budget(budget_for("medium", *min, *max)),
            ThinkingChoice::Default => ReasoningParam::Unspecified,
        },
    }
}

/// 档位换算成预算：在 `[min, max]` 里按 25% / 50% / 75% / 100% 取。
fn budget_for(level: &str, min: u32, max: u32) -> u32 {
    let max = max.max(min);
    let fraction = match level {
        "minimal" => 0.0,
        "low" => 0.25,
        "medium" => 0.5,
        "high" => 0.75,
        _ => 1.0,
    };
    let span = f64::from(max - min);
    (f64::from(min) + span * fraction).round() as u32
}

/// 清单里和 `want` 最接近的档：先取不高于它的最高档，没有就取清单里最低的。
fn nearest_level<'a>(levels: &'a [String], want: &str) -> Option<&'a str> {
    let rank = |level: &str| LEVEL_ORDER.iter().position(|l| *l == level);
    if let Some(found) = levels.iter().find(|l| l.as_str() == want) {
        return Some(found.as_str());
    }
    let want_rank = rank(want)?;
    let mut below: Option<(&str, usize)> = None;
    let mut lowest: Option<(&str, usize)> = None;
    for level in levels {
        // 不认识的档（以后新出的）只在精确命中时用，不参与就近取。
        let Some(r) = rank(level) else { continue };
        if r <= want_rank && below.is_none_or(|(_, best)| r > best) {
            below = Some((level.as_str(), r));
        }
        if lowest.is_none_or(|(_, best)| r < best) {
            lowest = Some((level.as_str(), r));
        }
    }
    below.or(lowest).map(|(level, _)| level)
}

/// 单次请求的输出上限：模型最大输出与上下文剩余预算取小，但不低于 [`MIN_OUTPUT_TOKENS`]。
pub fn max_output_tokens(caps: &ModelCaps, remaining_budget: u64) -> u32 {
    let remaining = u32::try_from(remaining_budget).unwrap_or(u32::MAX);
    caps.max_output.min(remaining).max(MIN_OUTPUT_TOKENS)
}

/// 上下文快用完时也至少给这么多输出空间，否则模型连一句话都说不完整（预算已经按 80% 触发
/// 过压缩，走到这里说明压缩也没腾出更多空间，让接口自己去判断超不超）。
pub const MIN_OUTPUT_TOKENS: u32 = 512;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::agent::builtin::catalog::CapsSource;

    fn caps(reasoning: ReasoningSupport) -> ModelCaps {
        ModelCaps {
            context_window: 200_000,
            max_input: None,
            max_output: 32_000,
            reasoning,
            tool_call: true,
            image_input: false,
            cost: None,
            source: CapsSource::Heuristic,
            display_name: "m".into(),
            matched_id: None,
        }
    }

    fn levels(list: &[&str]) -> ReasoningSupport {
        ReasoningSupport::Levels(list.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn selection_strings_split_only_on_known_suffixes() {
        let known = vec!["acme/model".to_owned()];
        let parse = |raw: &str| parse_model_selection(raw, &known);
        assert_eq!(
            parse("gpt-5.5"),
            ("gpt-5.5".into(), ThinkingChoice::Default)
        );
        assert_eq!(
            parse("gpt-5.5/high"),
            ("gpt-5.5".into(), ThinkingChoice::Level("high".into()))
        );
        assert_eq!(
            parse("claude-opus-5/OFF"),
            ("claude-opus-5".into(), ThinkingChoice::Off)
        );
        assert_eq!(
            parse("m/budget:4000"),
            ("m".into(), ThinkingChoice::Budget(4000))
        );
        assert_eq!(parse("m/on"), ("m".into(), ThinkingChoice::On));
        assert_eq!(parse("m/none"), ("m".into(), ThinkingChoice::Off));
        // 本来就带斜杠的模型名不误拆。
        assert_eq!(
            parse("deepseek-ai/DeepSeek-V4"),
            ("deepseek-ai/DeepSeek-V4".into(), ThinkingChoice::Default)
        );
        assert_eq!(
            parse("acme/model"),
            ("acme/model".into(), ThinkingChoice::Default)
        );
        assert_eq!(
            parse("org/model/high"),
            ("org/model".into(), ThinkingChoice::Level("high".into()))
        );
        assert_eq!(parse("/high"), ("/high".into(), ThinkingChoice::Default));
        assert_eq!(
            parse("m/budget:x"),
            ("m/budget:x".into(), ThinkingChoice::Default)
        );
    }

    #[test]
    fn nothing_chosen_sends_nothing_for_every_kind_of_model() {
        for reasoning in [
            ReasoningSupport::None,
            ReasoningSupport::Toggle,
            levels(&["low", "high"]),
            ReasoningSupport::Budget {
                min: 1024,
                max: 32_000,
            },
        ] {
            for kind in [EndpointKind::OpenAiChat, EndpointKind::Anthropic] {
                assert_eq!(
                    reasoning_param(kind, &caps(reasoning.clone()), &ThinkingChoice::Default),
                    ReasoningParam::Unspecified
                );
            }
        }
    }

    #[test]
    fn levels_map_to_effort_and_snap_to_the_nearest_supported_level() {
        let c = caps(levels(&["low", "medium", "high"]));
        let pick = |level: &str| {
            reasoning_param(
                EndpointKind::OpenAiChat,
                &c,
                &ThinkingChoice::Level(level.into()),
            )
        };
        assert_eq!(pick("high"), ReasoningParam::Effort("high".into()));
        assert_eq!(
            pick("xhigh"),
            ReasoningParam::Effort("high".into()),
            "往下就近"
        );
        assert_eq!(pick("max"), ReasoningParam::Effort("high".into()));
        assert_eq!(
            pick("minimal"),
            ReasoningParam::Effort("low".into()),
            "没有更低的取最低"
        );
        let only_high = caps(levels(&["high", "max"]));
        assert_eq!(
            reasoning_param(
                EndpointKind::OpenAiChat,
                &only_high,
                &ThinkingChoice::Level("low".into())
            ),
            ReasoningParam::Effort("high".into())
        );
    }

    #[test]
    fn turning_thinking_off_depends_on_the_kind_of_model() {
        // OpenAI 系：清单里有 none 就发 none。
        let openai = caps(levels(&["none", "low", "high"]));
        assert_eq!(
            reasoning_param(EndpointKind::OpenAiChat, &openai, &ThinkingChoice::Off),
            ReasoningParam::Effort("none".into())
        );
        // 没有 none 的（DeepSeek 一类）发 Off，provider 层会用 thinking 开关关掉。
        let deepseek = caps(levels(&["low", "high", "max"]));
        assert_eq!(
            reasoning_param(EndpointKind::OpenAiChat, &deepseek, &ThinkingChoice::Off),
            ReasoningParam::Off
        );
        // Anthropic：发 disabled 会让强制思考的新模型 400，所以什么都不发。
        let claude = caps(levels(&["low", "medium", "high", "xhigh", "max"]));
        assert_eq!(
            reasoning_param(EndpointKind::Anthropic, &claude, &ThinkingChoice::Off),
            ReasoningParam::Unspecified
        );
    }

    #[test]
    fn budget_models_convert_levels_and_clamp_user_values() {
        let c = caps(ReasoningSupport::Budget {
            min: 1000,
            max: 9000,
        });
        let param = |choice: ThinkingChoice| reasoning_param(EndpointKind::Anthropic, &c, &choice);
        assert_eq!(
            param(ThinkingChoice::Level("low".into())),
            ReasoningParam::Budget(3000)
        );
        assert_eq!(
            param(ThinkingChoice::Level("medium".into())),
            ReasoningParam::Budget(5000)
        );
        assert_eq!(
            param(ThinkingChoice::Level("high".into())),
            ReasoningParam::Budget(7000)
        );
        assert_eq!(
            param(ThinkingChoice::Level("max".into())),
            ReasoningParam::Budget(9000)
        );
        assert_eq!(
            param(ThinkingChoice::Level("minimal".into())),
            ReasoningParam::Budget(1000)
        );
        assert_eq!(
            param(ThinkingChoice::Budget(100)),
            ReasoningParam::Budget(1000)
        );
        assert_eq!(
            param(ThinkingChoice::Budget(50_000)),
            ReasoningParam::Budget(9000)
        );
        assert_eq!(
            param(ThinkingChoice::Budget(4321)),
            ReasoningParam::Budget(4321)
        );
        assert_eq!(param(ThinkingChoice::On), ReasoningParam::Budget(5000));
        assert_eq!(param(ThinkingChoice::Off), ReasoningParam::Off);
    }

    #[test]
    fn toggle_and_unsupported_models() {
        let toggle = caps(ReasoningSupport::Toggle);
        assert_eq!(
            reasoning_param(EndpointKind::OpenAiChat, &toggle, &ThinkingChoice::On),
            ReasoningParam::Effort("high".into())
        );
        assert_eq!(
            reasoning_param(EndpointKind::OpenAiChat, &toggle, &ThinkingChoice::Off),
            ReasoningParam::Off
        );
        // 不支持（含常开不可调）：用户要关才发 Off，别的一律不发。
        let none = caps(ReasoningSupport::None);
        assert_eq!(
            reasoning_param(
                EndpointKind::OpenAiChat,
                &none,
                &ThinkingChoice::Level("high".into())
            ),
            ReasoningParam::Unspecified
        );
        assert_eq!(
            reasoning_param(EndpointKind::OpenAiChat, &none, &ThinkingChoice::On),
            ReasoningParam::Unspecified
        );
        assert_eq!(
            reasoning_param(EndpointKind::OpenAiChat, &none, &ThinkingChoice::Off),
            ReasoningParam::Off
        );
    }

    #[test]
    fn thinking_options_follow_the_capability() {
        assert!(thinking_options(&caps(ReasoningSupport::None)).is_empty());
        assert_eq!(
            thinking_options(&caps(ReasoningSupport::Toggle)),
            ["off", "on"]
        );
        assert_eq!(
            thinking_options(&caps(levels(&["none", "low", "high"]))),
            ["off", "low", "high"]
        );
        assert_eq!(
            thinking_options(&caps(ReasoningSupport::Budget { min: 1, max: 2 })),
            ["off", "low", "medium", "high"]
        );
    }

    #[test]
    fn output_limit_is_the_smaller_of_model_and_remaining_budget() {
        let c = caps(ReasoningSupport::None);
        assert_eq!(max_output_tokens(&c, 1_000_000), 32_000);
        assert_eq!(max_output_tokens(&c, 10_000), 10_000);
        assert_eq!(
            max_output_tokens(&c, 0),
            MIN_OUTPUT_TOKENS,
            "预算见底也给个下限"
        );
        assert_eq!(max_output_tokens(&c, u64::MAX), 32_000);
    }
}
