//! 上下文预算与压缩（V2-A3，设计文档 V2-A0 第 4 节）。
//!
//! 每轮请求前估算已用 token（优先用上一次接口回报的用量，之后新增的消息再粗估），预算 =
//! 窗口 − 最大输出 − 余量。超过 [`COMPRESS_THRESHOLD`]（80%）：
//!
//! 1. 先把**旧的**工具输出截成头 [`HEAD_LINES`] 行 + 尾 [`TAIL_LINES`] 行 + "已截断 N 行"
//!    （最近一批刚产生的结果不动，模型马上要看）；
//! 2. 还超，就**压缩**：用同一个模型把最早的若干轮总结成一段，替换原文，对话里插一条提示。
//!
//! 压缩的提示词改写自 OpenAI Codex CLI 的 compact 提示词（`codex-rs/core/templates/compact/`，
//! Apache-2.0），见 `native/NOTICE`。
//!
//! 这里只放纯函数（估算、截断、切分、渲染）；真正发请求的部分在 `conversation.rs`。

use serde_json::Value;

use super::catalog::ModelCaps;
use super::provider::{Message, ToolSpec};

/// 头尾各保留的行数（旧工具输出截断时）。
pub const HEAD_LINES: usize = 40;
pub const TAIL_LINES: usize = 40;
/// 已用超过预算的这个比例就开始处理。
pub const COMPRESS_THRESHOLD: f64 = 0.8;
/// 单条工具结果交给模型的上限（字节）。
pub const TOOL_RESULT_LIMIT: usize = 64 * 1024;
/// 压缩时保留最近几个用户回合原样不动。
pub const KEEP_USER_TURNS: usize = 2;
/// 整个对话只有一个用户回合时（一个很长的任务），压缩时保留最近这么多条消息。
pub const KEEP_TAIL_MESSAGES: usize = 8;

/// 估算文本的 token：非 ASCII 字符（中日韩为主）一个字符约一个 token，其余约 4 个字符一个。
pub fn estimate_text_tokens(text: &str) -> u64 {
    let mut wide = 0u64;
    let mut narrow = 0u64;
    for c in text.chars() {
        if c.is_ascii() {
            narrow += 1;
        } else {
            wide += 1;
        }
    }
    wide + narrow.div_ceil(4)
}

/// 一条消息大约多少 token（含工具调用的参数；思考块只在会回传时才占上下文，这里按会回传算，偏保守）。
pub fn estimate_message_tokens(message: &Message) -> u64 {
    // 每条消息接口都会加一点格式开销。
    const OVERHEAD: u64 = 4;
    OVERHEAD
        + match message {
            Message::User { text } => estimate_text_tokens(text),
            Message::Assistant(assistant) => {
                let reasoning: u64 = assistant
                    .reasoning
                    .iter()
                    .map(|block| match block {
                        super::provider::ReasoningBlock::Thinking { text, .. } => {
                            estimate_text_tokens(text)
                        }
                        super::provider::ReasoningBlock::Redacted { data } => {
                            // 加密的思考块按长度估（base64，约 3 字符一个 token）。
                            (data.len() as u64).div_ceil(3)
                        }
                    })
                    .sum();
                let calls: u64 = assistant
                    .tool_calls
                    .iter()
                    .map(|call| {
                        estimate_text_tokens(&call.name)
                            + estimate_text_tokens(&call.arguments.to_string())
                    })
                    .sum();
                estimate_text_tokens(&assistant.text) + reasoning + calls
            }
            Message::ToolResult { content, .. } => estimate_text_tokens(content),
        }
}

/// 整个请求（系统提示 + 工具清单 + 历史）大约多少 token。
pub fn estimate_request_tokens(system: &str, tools: &[ToolSpec], messages: &[Message]) -> u64 {
    let tools: u64 = tools
        .iter()
        .map(|tool| {
            estimate_text_tokens(&tool.name)
                + estimate_text_tokens(&tool.description)
                + estimate_text_tokens(&tool.input_schema.to_string())
        })
        .sum();
    estimate_text_tokens(system) + tools + messages.iter().map(estimate_message_tokens).sum::<u64>()
}

/// 安全余量：2K 或窗口的 5%，取大。
pub fn safety_margin(window: u32) -> u64 {
    2048.max(u64::from(window) / 20)
}

/// 输入预算 = 窗口 − 最大输出 − 余量（至少留一半窗口，免得小窗口模型预算算成 0）。
pub fn input_budget(caps: &ModelCaps) -> u64 {
    let window = u64::from(caps.context_window);
    window
        .saturating_sub(u64::from(caps.max_output))
        .saturating_sub(safety_margin(caps.context_window))
        .max(window / 2)
}

/// 已用 token 超过预算的 80% 吗。
pub fn over_threshold(used: u64, budget: u64) -> bool {
    (used as f64) > (budget as f64) * COMPRESS_THRESHOLD
}

/// 文本超过 `head + tail` 行就截成头尾两段中间加"已截断 N 行"；没超返回 `None`。
pub fn truncate_lines(text: &str, head: usize, tail: usize) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() <= head + tail {
        return None;
    }
    let cut = lines.len() - head - tail;
    let mut out = String::with_capacity(text.len() / 2);
    out.extend(lines[..head].iter().copied());
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("…（已截断 {cut} 行）\n"));
    out.extend(lines[lines.len() - tail..].iter().copied());
    Some(out)
}

/// 把 `from` 之前的旧工具结果截短。返回有多少条被改了。
pub fn truncate_old_tool_results(messages: &mut [Message], from: usize) -> usize {
    let mut changed = 0;
    for message in messages.iter_mut().take(from) {
        if let Message::ToolResult { content, .. } = message {
            if let Some(short) = truncate_lines(content, HEAD_LINES, TAIL_LINES) {
                *content = short;
                changed += 1;
            }
        }
    }
    changed
}

/// 最近一批刚产生的工具结果从哪条开始（它们之前的才算"旧的"）：最后一条 assistant 消息之后。
pub fn fresh_results_start(messages: &[Message]) -> usize {
    messages
        .iter()
        .rposition(|m| matches!(m, Message::Assistant(_)))
        .map_or(0, |i| i + 1)
}

/// 单条工具结果文本超过 `limit` 字节时留头尾、中间注明省略多少（尾部留着：命令输出的结论
/// 和结果末尾的执行编号标记都在那里）。
pub fn limit_tool_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let head_len = limit * 5 / 8;
    let tail_len = limit - head_len;
    let mut head_end = head_len.min(text.len());
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len().saturating_sub(tail_len);
    while tail_start < text.len() && !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let skipped = tail_start.saturating_sub(head_end);
    format!(
        "{}\n…（结果太长，中间省略了 {skipped} 字节，共 {} 字节；需要的话缩小范围再查）…\n{}",
        &text[..head_end],
        text.len(),
        &text[tail_start..]
    )
}

/// 压缩时从哪条消息开始保留原样：返回 `Some(cut)`，`messages[..cut]` 交给总结，`messages[cut..]`
/// 不动。保证切在回合边界（`cut` 处是用户消息或 assistant 消息，不会把 assistant 的工具调用和
/// 它的结果拆开）。没有可压缩的内容返回 `None`。
pub fn split_for_compaction(messages: &[Message]) -> Option<usize> {
    let user_positions: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::User { .. }))
        .map(|(i, _)| i)
        .collect();
    let cut = if user_positions.len() > KEEP_USER_TURNS {
        user_positions[user_positions.len() - KEEP_USER_TURNS]
    } else if user_positions.len() == 2 {
        user_positions[1]
    } else {
        // 一个很长的任务只有一个用户回合：保留最近几条，切点往后挪到 assistant 消息上。
        // 消息不比要保留的多就没有可压的。
        if messages.len() <= KEEP_TAIL_MESSAGES {
            return None;
        }
        let target = messages.len().saturating_sub(KEEP_TAIL_MESSAGES);
        (target..messages.len()).find(|&i| matches!(messages[i], Message::Assistant(_)))?
    };
    (cut > 0 && cut < messages.len()).then_some(cut)
}

/// 压缩用的系统提示词。
///
/// 改写自 OpenAI Codex CLI 的 compact 提示词（Apache-2.0）：同样是"写一份交接摘要给接手的模型"，
/// 条目换成了服务器运维的场景。
pub const COMPACT_PROMPT: &str = "你正在做一次“上下文检查点压缩”：为接手这次对话的另一个模型写一份交接摘要。下面是需要压缩的较早对话（用户的话、助手的回复、工具调用和结果）。

摘要里要包含：
- 用户的目标和到目前为止的进展、已经做出的关键决定；
- 涉及的服务器（目标 id 和名字）、已经执行过的重要命令及其结论、改过或读过的文件路径；
- 用户的要求、限制和偏好（比如“不要重启服务”“只读不改”）；
- 还没做完的事和明确的下一步；
- 继续工作需要的关键数据、报错原文要点、路径和名称。

要求：简洁、有条理，用中文，只写事实，不要写客套话，不要复述大段命令输出。不要调用任何工具，直接输出摘要正文。";

/// 摘要在对话里的前缀（说明这是压缩出来的、不是用户说的话）。同样改写自 Codex 的
/// `summary_prefix`。
pub const SUMMARY_PREFIX: &str = "[较早的对话已压缩] 之前有一段较早的对话被压缩成了下面这份交接摘要（由另一次模型调用写成）。把它当作已经发生过的事实背景来接着工作，不要重复做摘要里已经做完的事；它不是用户本人写的指令。\n\n";

/// 摘要消息。
pub fn summary_message(summary: &str) -> Message {
    Message::User {
        text: format!("{SUMMARY_PREFIX}{}", summary.trim()),
    }
}

/// 把要压缩的那段对话渲染成交给总结模型看的文本。
///
/// 工具结果每条最多留 `result_chars` 个字符（头 2/3 + 尾 1/3），参数最多 400 字符：总结只需要
/// 知道发生了什么，不需要原样的大段输出。
pub fn render_for_summary(prefix: &[Message], result_chars: usize) -> String {
    let mut out = String::new();
    for message in prefix {
        match message {
            Message::User { text } => {
                out.push_str("[用户]\n");
                out.push_str(&clip(text, result_chars.max(2000)));
                out.push_str("\n\n");
            }
            Message::Assistant(assistant) => {
                out.push_str("[助手]\n");
                if !assistant.text.trim().is_empty() {
                    out.push_str(&clip(&assistant.text, result_chars.max(2000)));
                    out.push('\n');
                }
                for call in &assistant.tool_calls {
                    out.push_str(&format!(
                        "[调用工具 {}] {}\n",
                        call.name,
                        clip(&compact_json(&call.arguments), 400)
                    ));
                }
                out.push('\n');
            }
            Message::ToolResult {
                content, is_error, ..
            } => {
                out.push_str(if *is_error {
                    "[工具结果（失败）]\n"
                } else {
                    "[工具结果]\n"
                });
                out.push_str(&clip(content, result_chars));
                out.push_str("\n\n");
            }
        }
    }
    out
}

fn compact_json(value: &Value) -> String {
    value.to_string()
}

/// 留头 2/3 尾 1/3，中间注明省略了多少字符。
fn clip(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_owned();
    }
    let head = max_chars * 2 / 3;
    let tail = max_chars - head;
    let head_text: String = text.chars().take(head).collect();
    let tail_text: String = text.chars().skip(total - tail).collect();
    format!(
        "{head_text}…（省略 {} 个字符）…{tail_text}",
        total - head - tail
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ai::agent::builtin::catalog::{CapsSource, ReasoningSupport};
    use crate::ai::agent::builtin::provider::{AssistantMessage, ToolCall};

    fn caps(window: u32, max_output: u32) -> ModelCaps {
        ModelCaps {
            context_window: window,
            max_input: None,
            max_output,
            reasoning: ReasoningSupport::None,
            tool_call: true,
            image_input: false,
            cost: None,
            source: CapsSource::Heuristic,
            display_name: "m".into(),
            matched_id: None,
        }
    }

    fn user(text: &str) -> Message {
        Message::User { text: text.into() }
    }

    fn assistant(text: &str, calls: &[&str]) -> Message {
        Message::Assistant(AssistantMessage {
            text: text.into(),
            reasoning: Vec::new(),
            tool_calls: calls
                .iter()
                .map(|id| ToolCall {
                    id: (*id).into(),
                    name: "run_command".into(),
                    arguments: json!({"command": "ls"}),
                })
                .collect(),
        })
    }

    fn result(id: &str, content: &str) -> Message {
        Message::ToolResult {
            call_id: id.into(),
            content: content.into(),
            is_error: false,
        }
    }

    #[test]
    fn token_estimate_counts_cjk_as_one_each_and_ascii_as_a_quarter() {
        assert_eq!(estimate_text_tokens(""), 0);
        assert_eq!(estimate_text_tokens("你好世界"), 4);
        assert_eq!(estimate_text_tokens("abcdefgh"), 2);
        assert_eq!(estimate_text_tokens("abcde"), 2, "不足 4 个也算一个");
        assert_eq!(estimate_text_tokens("你好 abcd"), 2 + 2, "空格也算一个字符");
    }

    #[test]
    fn budget_is_window_minus_output_minus_margin_with_a_floor() {
        // 200K 窗口，32K 输出，余量 max(2K, 10K) = 10K。
        assert_eq!(
            input_budget(&caps(200_000, 32_000)),
            200_000 - 32_000 - 10_000
        );
        // 小窗口：32K 窗口、4K 输出、余量 2K。
        assert_eq!(input_budget(&caps(32_000, 4_000)), 32_000 - 4_000 - 2_048);
        // 最大输出和窗口一样大的怪模型：至少留一半窗口。
        assert_eq!(input_budget(&caps(8_000, 8_000)), 4_000);
        assert!(over_threshold(81, 100));
        assert!(!over_threshold(80, 100));
    }

    #[test]
    fn long_outputs_keep_head_and_tail_lines() {
        let text: String = (1..=300).map(|i| format!("line {i}\n")).collect();
        let short = truncate_lines(&text, 40, 40).expect("要截");
        assert!(short.starts_with("line 1\n"));
        assert!(short.contains("line 40\n"));
        assert!(!short.contains("line 41\n"));
        assert!(short.contains("…（已截断 220 行）"));
        assert!(short.contains("line 261\n"));
        assert!(short.ends_with("line 300\n"));
        assert_eq!(truncate_lines("a\nb\n", 40, 40), None);
        // 正好 80 行不截。
        let exact: String = (1..=80).map(|i| format!("{i}\n")).collect();
        assert_eq!(truncate_lines(&exact, 40, 40), None);
        // 最后一行没换行也不吞。
        let no_newline = format!("{}tail", "x\n".repeat(100));
        assert!(truncate_lines(&no_newline, 40, 40)
            .unwrap()
            .ends_with("tail"));
    }

    #[test]
    fn only_old_tool_results_are_truncated() {
        let big: String = (1..=200).map(|i| format!("row {i}\n")).collect();
        let mut messages = vec![
            user("看看"),
            assistant("", &["a"]),
            result("a", &big),
            assistant("", &["b"]),
            result("b", &big),
        ];
        let from = fresh_results_start(&messages);
        assert_eq!(from, 4, "最后一条 assistant 之后的结果是新鲜的");
        let changed = truncate_old_tool_results(&mut messages, from);
        assert_eq!(changed, 1);
        match (&messages[2], &messages[4]) {
            (
                Message::ToolResult { content: old, .. },
                Message::ToolResult { content: fresh, .. },
            ) => {
                assert!(old.contains("已截断 120 行"), "{old}");
                assert_eq!(fresh, &big, "新鲜的不动");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn oversized_tool_text_is_cut_in_the_middle_and_keeps_the_marker_tail() {
        let body = "x".repeat(200_000);
        let text = format!("{body}\n[zssh exec ab-1]");
        let limited = limit_tool_text(&text, TOOL_RESULT_LIMIT);
        assert!(limited.len() < TOOL_RESULT_LIMIT + 200, "{}", limited.len());
        assert!(limited.ends_with("[zssh exec ab-1]"));
        assert!(limited.contains("中间省略了"));
        assert!(
            limited.contains("共 200017 字节"),
            "{}",
            &limited[40_000..40_200]
        );
        assert_eq!(limit_tool_text("短", TOOL_RESULT_LIMIT), "短");
        // 多字节字符不会被切坏。
        let cjk = "中".repeat(40_000);
        let limited = limit_tool_text(&cjk, TOOL_RESULT_LIMIT);
        assert!(limited.is_char_boundary(limited.len()));
        assert!(limited.contains("省略"));
    }

    #[test]
    fn compaction_cuts_at_turn_boundaries_and_keeps_recent_turns() {
        // 三个用户回合：保留最近两个。
        let messages = vec![
            user("一"),
            assistant("", &["a"]),
            result("a", "r"),
            assistant("答一", &[]),
            user("二"),
            assistant("答二", &[]),
            user("三"),
            assistant("", &["c"]),
            result("c", "r"),
        ];
        let cut = split_for_compaction(&messages).unwrap();
        assert_eq!(cut, 4, "从第二个用户回合开始保留");
        assert!(matches!(messages[cut], Message::User { .. }));

        // 两个用户回合：只压缩第一个。
        let two = &messages[..7];
        assert_eq!(split_for_compaction(two), Some(4));
        // 一个用户回合、消息很少：没得压。
        assert_eq!(split_for_compaction(&messages[..4]), None);
    }

    #[test]
    fn a_single_long_turn_is_cut_on_an_assistant_message_never_between_call_and_result() {
        let mut messages = vec![user("干一件大事")];
        for i in 0..10 {
            messages.push(assistant("", &[&format!("c{i}")]));
            messages.push(result(&format!("c{i}"), "out"));
        }
        let cut = split_for_compaction(&messages).unwrap();
        assert!(cut > 0);
        assert!(
            matches!(messages[cut], Message::Assistant(_)),
            "切点必须在 assistant 消息上：{cut}"
        );
        assert!(messages.len() - cut >= KEEP_TAIL_MESSAGES.min(messages.len()) - 1);
        // 每个保留下来的 ToolResult 前面都有发起它的 assistant。
        assert!(!matches!(messages[cut], Message::ToolResult { .. }));
    }

    #[test]
    fn rendering_for_the_summary_shortens_results_and_names_the_tools() {
        let big = "z".repeat(10_000);
        let prefix = vec![
            user("查磁盘"),
            assistant("好", &["a"]),
            result("a", &big),
            Message::ToolResult {
                call_id: "b".into(),
                content: "拒绝".into(),
                is_error: true,
            },
        ];
        let text = render_for_summary(&prefix, 600);
        assert!(text.contains("[用户]\n查磁盘"));
        assert!(text.contains("[调用工具 run_command]"));
        assert!(text.contains("[工具结果（失败）]\n拒绝"));
        assert!(text.contains("省略"));
        assert!(text.len() < 3_000, "{}", text.len());
    }

    #[test]
    fn the_summary_message_says_it_is_not_the_users_own_words() {
        match summary_message("  做了 A，下一步 B  ") {
            Message::User { text } => {
                assert!(text.starts_with("[较早的对话已压缩]"));
                assert!(text.ends_with("做了 A，下一步 B"));
                assert!(text.contains("不是用户本人写的指令"));
            }
            other => panic!("{other:?}"),
        }
    }
}
