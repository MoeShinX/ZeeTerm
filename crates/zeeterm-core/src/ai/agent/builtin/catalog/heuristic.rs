//! 规则兜底：数据库里找不到时，按模型家族给一组常见值。结果在界面上标"估计"。
//!
//! 数字宁小勿大：上下文偏大会让上下文预算失效、请求被拒；偏小只是压缩得早一点。
//! 完全不认识的名字：32K 上下文、4K 输出、不开思考；工具调用按"未知 = 允许尝试"，
//! 因为禁掉会让用户连试都没法试，而试错的代价只是模型返回一个错误。

use super::matcher::tokens;
use super::types::ReasoningSupport;

/// 兜底给出的能力。`known` 为 false 表示一个家族都没认出来（用的是通用默认值）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HeuristicCaps {
    pub context_window: u32,
    pub max_output: u32,
    pub reasoning: ReasoningSupport,
    pub tool_call: bool,
    pub image_input: bool,
    pub known: bool,
}

const UNKNOWN: HeuristicCaps = HeuristicCaps {
    context_window: 32_000,
    max_output: 4_096,
    reasoning: ReasoningSupport::None,
    tool_call: true,
    image_input: false,
    known: false,
};

fn caps(context: u32, output: u32, reasoning: ReasoningSupport, image: bool) -> HeuristicCaps {
    HeuristicCaps {
        context_window: context,
        max_output: output,
        reasoning,
        tool_call: true,
        image_input: image,
        known: true,
    }
}

fn levels(v: &[&str]) -> ReasoningSupport {
    ReasoningSupport::Levels(v.iter().map(|s| s.to_string()).collect())
}

pub(crate) fn heuristic(model_name: &str) -> HeuristicCaps {
    let last = model_name.rsplit('/').next().unwrap_or(model_name);
    let t = tokens(last);
    let has = |w: &str| t.iter().any(|x| x == w);
    // 名字里带这些，视觉输入大概率可用。
    let vision = has("vl") || has("vision") || has("omni") || has("4o");
    let Some(first) = t.first().map(String::as_str) else {
        return UNKNOWN;
    };

    if first == "claude" {
        // Claude 3 系列：输出上限小，3.7 之前不支持思考。
        if t.get(1).map(String::as_str) == Some("3") {
            let third = t.get(2).map(String::as_str);
            return match third {
                Some("7") => caps(
                    200_000,
                    32_000,
                    ReasoningSupport::Budget {
                        min: 1024,
                        max: 32_000,
                    },
                    true,
                ),
                Some("5") => caps(200_000, 8_192, ReasoningSupport::None, true),
                _ => caps(200_000, 4_096, ReasoningSupport::None, true),
            };
        }
        return caps(
            200_000,
            32_000,
            ReasoningSupport::Budget {
                min: 1024,
                max: 32_000,
            },
            true,
        );
    }
    if first == "gpt" || first == "chatgpt" || is_o_series(first) {
        if is_o_series(first) {
            return caps(200_000, 32_000, levels(&["low", "medium", "high"]), true);
        }
        let second = t.get(1).map(String::as_str).unwrap_or("");
        return match second {
            "5" | "6" => caps(200_000, 32_000, levels(&["low", "medium", "high"]), true),
            "4" | "4o" => caps(
                128_000,
                16_000,
                ReasoningSupport::None,
                second == "4o" || has("turbo") || has("vision"),
            ),
            "3" => caps(16_000, 4_096, ReasoningSupport::None, false),
            _ => caps(128_000, 16_000, ReasoningSupport::None, vision),
        };
    }
    if first == "gemini" || first == "gemma" {
        if first == "gemma" {
            return caps(128_000, 8_192, ReasoningSupport::None, true);
        }
        return caps(
            1_000_000,
            32_000,
            ReasoningSupport::Budget {
                min: 0,
                max: 24_576,
            },
            true,
        );
    }
    if first == "deepseek" {
        let thinks = has("reasoner")
            || has("r1")
            || t.iter()
                .any(|x| x.starts_with('v') && x != "v2" && x != "v3");
        let reasoning = if thinks {
            ReasoningSupport::Toggle
        } else {
            ReasoningSupport::None
        };
        return caps(128_000, 8_192, reasoning, false);
    }
    if first.starts_with("qwen") || first == "qwq" || first == "qvq" {
        let thinks =
            first == "qwq" || first == "qvq" || has("thinking") || first.starts_with("qwen3");
        let reasoning = if thinks {
            ReasoningSupport::Toggle
        } else {
            ReasoningSupport::None
        };
        return caps(128_000, 8_192, reasoning, vision);
    }
    if first == "kimi" || first == "moonshot" {
        return caps(128_000, 8_192, ReasoningSupport::None, vision || has("k2"));
    }
    if first == "glm" || first == "chatglm" {
        return caps(
            128_000,
            8_192,
            ReasoningSupport::Toggle,
            vision
                || t.iter().any(|x| {
                    x.len() > 1 && x.ends_with('v') && x.starts_with(|c: char| c.is_ascii_digit())
                }),
        );
    }
    if first == "minimax" {
        return caps(200_000, 16_000, ReasoningSupport::None, false);
    }
    if first == "grok" {
        return caps(128_000, 16_000, ReasoningSupport::None, true);
    }
    if first.starts_with("llama") {
        return caps(128_000, 4_096, ReasoningSupport::None, vision);
    }
    if matches!(
        first,
        "mistral" | "ministral" | "magistral" | "devstral" | "codestral" | "pixtral" | "voxtral"
    ) {
        return caps(128_000, 8_192, ReasoningSupport::None, first == "pixtral");
    }
    UNKNOWN
}

fn is_o_series(first: &str) -> bool {
    first.len() >= 2 && first.starts_with('o') && first[1..].chars().all(|c| c.is_ascii_digit())
}
