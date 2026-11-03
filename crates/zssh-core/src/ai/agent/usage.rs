//! 上下文用量（V2-U15）：三种智能体各自报告的 token 用量，翻成同一个 [`ContextUsage`]，界面只显示
//! （面板标题栏的「上下文 ▬ 12% ∨」）。**只读解析**：不改给智能体的任何请求，也不影响回合状态。
//!
//! 各智能体的数据来源和「已用上下文」的口径（口径跟各自自己的界面一致）：
//! - codex：app-server 通知 `thread/tokenUsage/updated`，`tokenUsage.last` 是最近一次模型调用的用量，
//!   已用 = `last.totalTokens`；窗口大小直接带在 `tokenUsage.modelContextWindow` 里；
//! - Claude Code：stream-json 里 `assistant` 消息的 `message.usage`，已用 = `input_tokens` +
//!   `cache_creation_input_tokens` + `cache_read_input_tokens`；窗口大小协议里没有，按模型名兜底（估计）；
//! - opencode：SSE 里 assistant 的 `message.updated`，`info.tokens` 是这条消息的用量，已用 = 它自己的
//!   `total`（输入 + 输出 + 思考 + 缓存读写）；没有 `tokens`（或全是 0，消息还没跑完）就不报；窗口大小
//!   协议事件里没有，按模型名兜底（估计），认不出模型就不给窗口（界面只显示用了多少，不画百分比）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 超过这个百分比，进度条用警示色。
pub const WARN_PERCENT: f64 = 80.0;
/// 超过这个百分比，进度条用危险色。
pub const DANGER_PERCENT: f64 = 95.0;

/// 当前对话最近一轮的上下文用量。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    /// 已用上下文（token）。
    pub used: u64,
    /// 模型上下文窗口（token）；拿不到也没法兜底就是 `None`。
    #[serde(default)]
    pub window: Option<u64>,
    /// 窗口是按模型名估计的，不是智能体报的。
    #[serde(default)]
    pub window_estimated: bool,
    /// 本轮明细（拿得到哪些填哪些）。
    #[serde(default)]
    pub input: Option<u64>,
    #[serde(default)]
    pub output: Option<u64>,
    #[serde(default)]
    pub cache_read: Option<u64>,
    #[serde(default)]
    pub cache_write: Option<u64>,
    /// 这一轮用的模型（智能体报的原始名字）。
    #[serde(default)]
    pub model: Option<String>,
}

/// 进度条颜色档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    Normal,
    Warning,
    Danger,
}

impl ContextUsage {
    /// 已用占窗口的百分比（可以超过 100）；没有窗口或窗口是 0 就是 `None`。
    pub fn percent(&self) -> Option<f64> {
        let window = self.window.filter(|window| *window > 0)?;
        Some(self.used as f64 * 100.0 / window as f64)
    }

    /// 颜色档：超过 80% 警示，超过 95% 危险；没有百分比时按普通。
    pub fn level(&self) -> UsageLevel {
        match self.percent() {
            Some(percent) if percent > DANGER_PERCENT => UsageLevel::Danger,
            Some(percent) if percent > WARN_PERCENT => UsageLevel::Warning,
            _ => UsageLevel::Normal,
        }
    }

    /// 窗口没报就按模型名兜底（标成估计）；报了的不动。
    pub fn with_window_fallback(mut self) -> Self {
        if self.window.is_none() {
            if let Some(window) = self.model.as_deref().and_then(window_for_model) {
                self.window = Some(window);
                self.window_estimated = true;
            }
        }
        self
    }
}

/// 按模型名猜常见的上下文窗口（只是兜底，用的时候界面会标「估计」）。认不出返回 `None`。
pub fn window_for_model(model: &str) -> Option<u64> {
    let name = model.to_lowercase();
    // 显式带窗口档位的写法：`claude-opus-4[1m]`、`...-1m`。
    if name.contains("[1m]") || name.contains("-1m") || name.contains("1m-context") {
        return Some(1_000_000);
    }
    let has = |words: &[&str]| words.iter().any(|word| name.contains(word));
    if has(&["claude", "opus", "sonnet", "haiku"]) {
        return Some(200_000);
    }
    if has(&["gpt-5", "codex"]) {
        return Some(400_000);
    }
    if has(&["gpt-4.1"]) {
        return Some(1_000_000);
    }
    if has(&["gpt-4o"]) {
        return Some(128_000);
    }
    if has(&["gemini"]) {
        return Some(1_000_000);
    }
    if has(&["deepseek"]) {
        return Some(128_000);
    }
    None
}

fn count(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64))
}

/// codex `thread/tokenUsage/updated` 的参数。
///
/// 明细口径：OpenAI 的 `inputTokens` 含缓存命中的部分，这里把「输入」拆成不含缓存的那部分，缓存读
/// 单独列，几项加起来才对得上。没有 `last`（协议里是必填，防万一）或总数是 0 就不报。
pub fn from_codex(params: &Value) -> Option<ContextUsage> {
    let usage = &params["tokenUsage"];
    let last = &usage["last"];
    let used = count(&last["totalTokens"]).filter(|used| *used > 0)?;
    let cached = count(&last["cachedInputTokens"]);
    let input = count(&last["inputTokens"]);
    Some(ContextUsage {
        used,
        window: count(&usage["modelContextWindow"]).filter(|window| *window > 0),
        window_estimated: false,
        input: input.map(|input| input.saturating_sub(cached.unwrap_or(0))),
        output: count(&last["outputTokens"]),
        cache_read: cached,
        cache_write: count(&last["cacheWriteInputTokens"]).filter(|n| *n > 0),
        model: None,
    })
}

/// Claude Code stream-json 里 `assistant` 消息的 `message`（取它的 `usage` 和 `model`）。
///
/// 全是 0 的（合成的占位消息）不报。窗口协议里没有，按模型名兜底。
pub fn from_claude(message: &Value) -> Option<ContextUsage> {
    let usage = &message["usage"];
    let input = count(&usage["input_tokens"]);
    let cache_write = count(&usage["cache_creation_input_tokens"]);
    let cache_read = count(&usage["cache_read_input_tokens"]);
    let used = input.unwrap_or(0) + cache_write.unwrap_or(0) + cache_read.unwrap_or(0);
    if used == 0 {
        return None;
    }
    Some(
        ContextUsage {
            used,
            window: None,
            window_estimated: false,
            input,
            output: count(&usage["output_tokens"]),
            cache_read,
            cache_write,
            model: message["model"]
                .as_str()
                .filter(|model| !model.is_empty())
                .map(str::to_owned),
        }
        .with_window_fallback(),
    )
}

/// opencode `message.updated` 的 `properties.info`（只看 assistant 消息）。
pub fn from_opencode(info: &Value) -> Option<ContextUsage> {
    if info["role"].as_str() != Some("assistant") {
        return None;
    }
    let tokens = &info["tokens"];
    if !tokens.is_object() {
        return None;
    }
    let input = count(&tokens["input"]);
    let output = count(&tokens["output"]);
    let reasoning = count(&tokens["reasoning"]);
    let cache_read = count(&tokens["cache"]["read"]);
    let cache_write = count(&tokens["cache"]["write"]);
    let used = count(&tokens["total"]).unwrap_or_else(|| {
        input.unwrap_or(0)
            + output.unwrap_or(0)
            + reasoning.unwrap_or(0)
            + cache_read.unwrap_or(0)
            + cache_write.unwrap_or(0)
    });
    // 消息刚开始（还在生成）时 tokens 全是 0：不报，别把上一轮的用量冲成 0。
    if used == 0 {
        return None;
    }
    let model = info["modelID"]
        .as_str()
        .filter(|model| !model.is_empty())
        .map(
            |model| match info["providerID"].as_str().filter(|p| !p.is_empty()) {
                Some(provider) => format!("{provider}/{model}"),
                None => model.to_owned(),
            },
        );
    Some(
        ContextUsage {
            used,
            window: None,
            window_estimated: false,
            input,
            // 思考的 token 也是输出的一部分。
            output: match (output, reasoning) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
            },
            cache_read,
            cache_write,
            model,
        }
        .with_window_fallback(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(used: u64, window: Option<u64>) -> ContextUsage {
        ContextUsage {
            used,
            window,
            ..ContextUsage::default()
        }
    }

    #[test]
    fn percent_and_level_thresholds() {
        assert_eq!(usage(1_000, Some(200_000)).percent(), Some(0.5));
        assert_eq!(usage(1_000, None).percent(), None);
        assert_eq!(usage(1_000, Some(0)).percent(), None, "窗口是 0 不除");
        let level = |used| usage(used, Some(100)).level();
        assert_eq!(level(0), UsageLevel::Normal);
        assert_eq!(level(80), UsageLevel::Normal, "刚好 80% 还不算超过");
        assert_eq!(level(81), UsageLevel::Warning);
        assert_eq!(level(95), UsageLevel::Warning, "刚好 95% 还不算超过");
        assert_eq!(level(96), UsageLevel::Danger);
        assert_eq!(level(130), UsageLevel::Danger, "超过 100% 也是危险");
        assert_eq!(usage(5_000, None).level(), UsageLevel::Normal);
    }

    #[test]
    fn window_fallback_by_model_name_marks_the_estimate() {
        let known = ContextUsage {
            model: Some("claude-sonnet-5".into()),
            ..usage(10, None)
        }
        .with_window_fallback();
        assert_eq!(known.window, Some(200_000));
        assert!(known.window_estimated);
        let reported = ContextUsage {
            model: Some("claude-sonnet-5".into()),
            ..usage(10, Some(1_000_000))
        }
        .with_window_fallback();
        assert_eq!(reported.window, Some(1_000_000), "智能体报了的不改");
        assert!(!reported.window_estimated);
        let unknown = ContextUsage {
            model: Some("some-local-model".into()),
            ..usage(10, None)
        }
        .with_window_fallback();
        assert_eq!(unknown.window, None, "认不出的模型不猜");
        assert!(!unknown.window_estimated);
        assert_eq!(window_for_model("opus[1m]"), Some(1_000_000));
        assert_eq!(window_for_model("anthropic/claude-sonnet-5"), Some(200_000));
        assert_eq!(window_for_model("openai/gpt-5.5"), Some(400_000));
        assert_eq!(window_for_model(""), None);
    }

    /// 录制的 Claude Code 回合里最后一条带正文的 assistant 消息（真实样本）。
    #[test]
    fn claude_usage_from_the_recorded_turn() {
        const SAMPLE: &str = include_str!("fixtures/claude/claude_turn.jsonl");
        let mut found = Vec::new();
        for line in SAMPLE.lines().filter(|l| !l.trim().is_empty()) {
            let message: Value = serde_json::from_str(line).expect("样本是合法 JSON");
            if message["type"] == "assistant" {
                if let Some(parsed) = from_claude(&message["message"]) {
                    found.push(parsed);
                }
            }
        }
        assert!(!found.is_empty(), "样本里有带用量的 assistant 消息");
        let first = &found[0];
        // 第一个回合：input 2 + 缓存写 790 + 缓存读 3719。
        assert_eq!(first.used, 2 + 790 + 3719);
        assert_eq!(first.input, Some(2));
        assert_eq!(first.cache_write, Some(790));
        assert_eq!(first.cache_read, Some(3719));
        assert_eq!(first.output, Some(5));
        assert_eq!(first.model.as_deref(), Some("claude-opus-5-5"));
        assert_eq!(first.window, Some(200_000), "协议里没有窗口：按模型名兜底");
        assert!(first.window_estimated);
        // 第二个回合的上下文更大（缓存读变多）。
        assert!(found.last().expect("有").used > first.used);
    }

    #[test]
    fn claude_synthetic_zero_usage_is_ignored() {
        let message = json!({"model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0}});
        assert_eq!(from_claude(&message), None);
        assert_eq!(from_claude(&json!({"content": []})), None);
    }

    /// 录制的 opencode 回合里的 assistant `message.updated`（真实样本）。
    #[test]
    fn opencode_usage_from_the_recorded_turn() {
        const SAMPLE: &str = include_str!("fixtures/opencode_sse_turn.jsonl");
        let mut parsed = Vec::new();
        let mut zero_messages = 0;
        for line in SAMPLE.lines().filter(|l| !l.trim().is_empty()) {
            let event: Value = serde_json::from_str(line).expect("样本是合法 JSON");
            if event["type"] != "message.updated" {
                continue;
            }
            let info = &event["properties"]["info"];
            match from_opencode(info) {
                Some(usage) => parsed.push(usage),
                None if info["role"] == "assistant" => zero_messages += 1,
                None => {}
            }
        }
        assert!(zero_messages > 0, "消息刚开始时 tokens 全是 0，不报");
        // 样本里 assistant 跑了两步（工具调用一步、最后回复一步），上下文一步比一步大；以最后一步为准。
        assert_eq!(parsed[0].used, 16317, "用 opencode 自己给的 total");
        assert_eq!(parsed[0].input, Some(76));
        assert_eq!(parsed[0].output, Some(50 + 63), "思考算进输出");
        let last = parsed.last().expect("样本里有带用量的 assistant 消息");
        assert_eq!(last.used, 16346);
        assert_eq!(last.input, Some(82));
        assert_eq!(last.output, Some(8));
        assert_eq!(last.cache_read, Some(16256));
        assert_eq!(last.cache_write, Some(0));
        assert_eq!(last.model.as_deref(), Some("deepseek/deepseek-v4-pro"));
        assert_eq!(last.window, Some(128_000));
        assert!(last.window_estimated);
    }

    #[test]
    fn opencode_user_messages_and_missing_tokens_are_ignored() {
        assert_eq!(
            from_opencode(&json!({"role":"user","tokens":{"total":5}})),
            None
        );
        assert_eq!(from_opencode(&json!({"role":"assistant"})), None);
        // 没有 total 时自己加。
        let usage = from_opencode(&json!({
            "role": "assistant",
            "tokens": {"input": 10, "output": 2, "reasoning": 0, "cache": {"read": 5, "write": 1}}
        }))
        .expect("有用量");
        assert_eq!(usage.used, 18);
        assert_eq!(usage.window, None, "没有模型名：不猜窗口");
    }

    /// 按导出的 schema（`ThreadTokenUsageUpdatedNotification`）构造的样本：录制的回合里没有这条通知，
    /// 所以样本文件是照 schema 写的，字段名和形状由 `codex::protocol` 的 schema 单测对着导出文件核对。
    #[test]
    fn codex_usage_from_the_schema_sample() {
        const SAMPLE: &str = include_str!("fixtures/codex/token_usage_updated.json");
        let params: Value = serde_json::from_str(SAMPLE).expect("样本是合法 JSON");
        let usage = from_codex(&params).expect("有用量");
        assert_eq!(usage.used, 23_000, "已用 = last.totalTokens");
        assert_eq!(usage.window, Some(258_400), "窗口是 codex 自己报的");
        assert!(!usage.window_estimated);
        assert_eq!(
            usage.input,
            Some(2_000),
            "输入 = inputTokens 去掉缓存命中的部分"
        );
        assert_eq!(usage.cache_read, Some(20_000));
        assert_eq!(usage.output, Some(1_000));
        assert_eq!(usage.cache_write, None, "0 不列");
        assert_eq!(usage.percent().map(|p| p.round() as u64), Some(9));
    }

    #[test]
    fn codex_without_usable_numbers_reports_nothing() {
        assert_eq!(from_codex(&json!({})), None);
        let zero = json!({"tokenUsage": {"last": {"totalTokens": 0}, "total": {}}});
        assert_eq!(from_codex(&zero), None);
        // 窗口缺失（协议里可以是 null）：照样报已用，只是没有窗口。
        let no_window = json!({"tokenUsage": {"last": {"totalTokens": 5, "inputTokens": 3,
            "cachedInputTokens": 0, "outputTokens": 2, "reasoningOutputTokens": 0}, "modelContextWindow": null}});
        let usage = from_codex(&no_window).expect("有用量");
        assert_eq!((usage.used, usage.window), (5, None));
    }
}
