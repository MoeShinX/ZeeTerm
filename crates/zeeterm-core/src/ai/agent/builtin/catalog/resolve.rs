//! 合成：手动 > 接口自报 > models.dev（缓存或内置快照）> 规则兜底，逐字段合并。

use super::heuristic::heuristic;
use super::matcher::Catalog;
use super::types::{CapsOverride, CapsSource, ModelCaps, PartialCaps, ReasoningSupport};

/// 合成某个模型名的最终能力。
///
/// - 每个字段各自取优先级最高、且有值的那一层（0 视为没给）。
/// - `source` 记的是上下文窗口来自哪一层。
/// - 接口只说"能思考"（[`ReasoningSupport::Toggle`]）而 models.dev 有更具体的等级 / 预算时，
///   用 models.dev 的：接口自报的清单只能表达到"支不支持"，不该把已知的等级抹掉。
/// - 最大输出不会超过上下文窗口。
pub fn resolve(
    model_name: &str,
    manual: Option<CapsOverride>,
    endpoint: Option<PartialCaps>,
    db: &Catalog,
) -> ModelCaps {
    let manual = manual.unwrap_or_default();
    let endpoint = endpoint.unwrap_or_default();
    let fallback = heuristic(model_name);

    // 用户改选过的条目优先；键失效（数据更新后没了）就回到按名字匹配。
    let entry = manual
        .pick
        .as_deref()
        .and_then(|k| db.get(k))
        .or_else(|| db.find(model_name).map(|m| m.entry));
    let db_source = db.origin().caps_source();

    let positive = |v: Option<u32>| v.filter(|&n| n > 0);

    let (context_window, source) = if let Some(n) = positive(manual.context_window) {
        (n, CapsSource::Manual)
    } else if let Some(n) = positive(endpoint.context_window) {
        (n, CapsSource::Endpoint)
    } else if let Some(e) = entry {
        (e.context, db_source)
    } else {
        (fallback.context_window, CapsSource::Heuristic)
    };

    let max_output = positive(manual.max_output)
        .or(positive(endpoint.max_output))
        .or(entry.and_then(|e| positive(Some(e.output))))
        .unwrap_or(fallback.max_output)
        .min(context_window);

    let max_input = positive(manual.max_input)
        .or(positive(endpoint.max_input))
        .or(entry.and_then(|e| e.input));

    let db_reasoning = entry.map(|e| e.reasoning.clone());
    let reasoning = manual
        .reasoning
        .clone()
        .or_else(|| match (endpoint.reasoning.clone(), &db_reasoning) {
            (
                Some(ReasoningSupport::Toggle),
                Some(more @ (ReasoningSupport::Levels(_) | ReasoningSupport::Budget { .. })),
            ) => Some(more.clone()),
            (r, _) => r,
        })
        .or(db_reasoning)
        .unwrap_or(fallback.reasoning);

    let tool_call = manual
        .tool_call
        .or(endpoint.tool_call)
        .or(entry.map(|e| e.tool_call))
        .unwrap_or(fallback.tool_call);
    let image_input = manual
        .image_input
        .or(endpoint.image_input)
        .or(entry.map(|e| e.image))
        .unwrap_or(fallback.image_input);

    let cost = manual.cost.or(entry.and_then(|e| e.cost));
    let display_name = entry
        .map(|e| e.name.clone())
        .or(endpoint.display_name)
        .unwrap_or_else(|| model_name.trim().to_string());

    ModelCaps {
        context_window,
        max_input,
        max_output,
        reasoning,
        tool_call,
        image_input,
        cost,
        source,
        display_name,
        matched_id: entry.map(|e| e.key()),
    }
}
