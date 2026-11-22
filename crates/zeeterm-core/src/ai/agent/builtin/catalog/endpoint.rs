//! 接口自报：解析 OpenAI 兼容 `GET {base}/models` 的返回。只解析，不发请求。
//!
//! 普通网关只返回 `id`；OpenRouter 风格的会带 `context_length`、
//! `top_provider.max_completion_tokens`、`supported_parameters`、`architecture.input_modalities`。
//! 有的网关用别的字段名（`context_window`、`max_output_tokens` ……），能认的都认。

use serde_json::Value;

use super::types::{PartialCaps, ReasoningSupport};

/// 解析 `/models` 的响应体。认 `{"data":[…]}`、`{"models":[…]}` 和裸数组；
/// 解析失败或没有可用条目返回空。条目里 id 缺失的丢掉，其余字段缺失就是 None。
pub fn parse_models_endpoint(json: &str) -> Vec<(String, PartialCaps)> {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    let list = match &root {
        Value::Array(a) => Some(a),
        Value::Object(o) => o
            .get("data")
            .or_else(|| o.get("models"))
            .and_then(Value::as_array),
        _ => None,
    };
    let Some(list) = list else {
        return Vec::new();
    };
    list.iter().filter_map(parse_one).collect()
}

fn parse_one(item: &Value) -> Option<(String, PartialCaps)> {
    // 个别实现（Ollama 原生接口）把 id 叫 name / model。
    let id = ["id", "model", "name"]
        .iter()
        .find_map(|k| item.get(k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();

    let top = item.get("top_provider");
    let context_window = first_u32(
        item,
        &[
            "context_length",
            "context_window",
            "max_context_length",
            "max_model_len",
        ],
    )
    .or_else(|| top.and_then(|t| first_u32(t, &["context_length"])));
    let max_output = first_u32(
        item,
        &["max_output_tokens", "max_completion_tokens", "max_output"],
    )
    .or_else(|| top.and_then(|t| first_u32(t, &["max_completion_tokens"])));
    let max_input = first_u32(item, &["max_input_tokens", "max_input"]);

    let params: Option<Vec<&str>> = item
        .get("supported_parameters")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
        .map(|a| a.iter().filter_map(Value::as_str).collect());
    // 只有接口真给了参数清单才下"不支持"的结论；没给就是不知道。
    let (reasoning, tool_call) = match &params {
        Some(p) => {
            let thinks = p
                .iter()
                .any(|x| matches!(*x, "reasoning" | "include_reasoning" | "reasoning_effort"));
            let tools = p.iter().any(|x| matches!(*x, "tools" | "tool_choice"));
            // 清单只能说明"能思考"，等级 / 预算要靠 models.dev 补；合成时会让更具体的数据优先。
            let r = if thinks {
                ReasoningSupport::Toggle
            } else {
                ReasoningSupport::None
            };
            (Some(r), Some(tools))
        }
        None => (None, None),
    };

    let image_input = item
        .pointer("/architecture/input_modalities")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).any(|m| m == "image"))
        .or_else(|| {
            item.pointer("/architecture/modality")
                .and_then(Value::as_str)
                // 形如 `text+image->text`：箭头左边是输入。
                .map(|m| m.split("->").next().unwrap_or("").contains("image"))
        });

    // 显示名：OpenRouter 的 name 形如 `Anthropic: Claude Opus 5`；和 id 一样的不算。
    let display_name = item
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty() && *n != id)
        .map(str::to_string);

    Some((
        id,
        PartialCaps {
            context_window,
            max_input,
            max_output,
            reasoning,
            tool_call,
            image_input,
            display_name,
        },
    ))
}

/// 取第一个存在且为正整数的字段（数字或数字字符串；0 视为没给）。
fn first_u32(v: &Value, keys: &[&str]) -> Option<u32> {
    keys.iter().find_map(|k| {
        let f = v.get(k)?;
        let n = f
            .as_u64()
            .or_else(|| {
                f.as_f64()
                    .filter(|x| *x >= 0.0 && x.fract() == 0.0)
                    .map(|x| x as u64)
            })
            .or_else(|| f.as_str().and_then(|s| s.trim().parse::<u64>().ok()))?;
        u32::try_from(n).ok().filter(|&n| n > 0)
    })
}
