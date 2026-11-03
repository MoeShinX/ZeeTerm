//! provider 层的集成式单测：每个 SSE 样本都用多种切块方式回放（整块、逐字节、随机块），
//! 结果必须完全一致；另外覆盖请求头 / 错误分类 / 传输层失败 / 真实 `reqwest` 传输（本机回环）。

use std::time::Duration;

use serde_json::{json, Value};

use super::testing::{random_chunks, Script, StubTransport};
use super::*;

const OPENAI_TEXT: &str = include_str!("fixtures/openai_text.sse");
const OPENAI_DEEPSEEK: &str = include_str!("fixtures/openai_reasoning_deepseek.sse");
const OPENAI_OPENROUTER: &str = include_str!("fixtures/openai_reasoning_openrouter.sse");
const OPENAI_TOOL_SINGLE: &str = include_str!("fixtures/openai_tool_call_single.sse");
const OPENAI_TOOL_PARALLEL: &str = include_str!("fixtures/openai_tool_call_parallel.sse");
const OPENAI_TOOL_SAME_INDEX: &str = include_str!("fixtures/openai_tool_call_same_index.sse");
const OPENAI_COMMENTS: &str = include_str!("fixtures/openai_comments_no_done.sse");
const OPENAI_MIDSTREAM_ERROR: &str = include_str!("fixtures/openai_error_midstream.sse");
const OPENAI_ERROR_401: &str = include_str!("fixtures/openai_error_401.json");
const GATEWAY_HTML: &str = include_str!("fixtures/gateway_error_page.html");
const ANTHROPIC_TEXT: &str = include_str!("fixtures/anthropic_text.sse");
const ANTHROPIC_THINKING: &str = include_str!("fixtures/anthropic_thinking_text.sse");
const ANTHROPIC_TOOL_USE: &str = include_str!("fixtures/anthropic_tool_use.sse");
const ANTHROPIC_MULTI: &str = include_str!("fixtures/anthropic_multi_block.sse");
const ANTHROPIC_OMITTED: &str = include_str!("fixtures/anthropic_omitted_thinking.sse");
const ANTHROPIC_ERROR_EVENT: &str = include_str!("fixtures/anthropic_error_event.sse");
const ANTHROPIC_ERROR_401: &str = include_str!("fixtures/anthropic_error_401.json");

const TEST_KEY: &str = "sk-test-SECRET-0123456789";

fn config(kind: ProviderKind) -> ProviderConfig {
    ProviderConfig::new(kind, "https://llm.example.test/v1", ApiKey::new(TEST_KEY))
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        system: "系统提示词".into(),
        messages: vec![Message::User {
            text: "你好".into(),
        }],
        tools: bridge_tool_specs(),
        max_output_tokens: 4096,
        reasoning: ReasoningParam::Off,
        temperature: None,
        cache_hint: false,
    }
}

type Outcome = (Result<AssistantTurn, ProviderError>, Vec<StreamEvent>);

async fn replay_script(kind: ProviderKind, script: Script) -> (Outcome, StubTransport) {
    let transport = StubTransport::new(script);
    let mut events = Vec::new();
    let result = stream_chat(&transport, &config(kind), &request("m"), &mut |e| {
        events.push(e)
    })
    .await;
    ((result, events), transport)
}

async fn replay(kind: ProviderKind, chunks: Vec<Vec<u8>>) -> Outcome {
    replay_script(kind, Script::sse(chunks)).await.0
}

/// 整块回放一次，再用逐字节和 40 种随机切法各回放一次，结果必须逐事件相同。
async fn replay_all_chunkings(kind: ProviderKind, fixture: &str) -> Outcome {
    let bytes = fixture.as_bytes();
    let whole = replay(kind, vec![bytes.to_vec()]).await;
    let one_byte: Vec<Vec<u8>> = bytes.iter().map(|b| vec![*b]).collect();
    assert_eq!(replay(kind, one_byte).await, whole, "逐字节切块结果不同");
    for seed in 1..=40u64 {
        let max = [3usize, 7, 16, 41, 130][(seed % 5) as usize];
        let chunks = random_chunks(bytes, seed, max);
        assert_eq!(
            replay(kind, chunks).await,
            whole,
            "随机切块（seed {seed}, 最大 {max} 字节）结果不同"
        );
    }
    whole
}

fn ok_turn(outcome: &Outcome) -> &AssistantTurn {
    match &outcome.0 {
        Ok(turn) => turn,
        Err(err) => panic!("应当成功，实际报错：{err}"),
    }
}

fn text(s: &str) -> StreamEvent {
    StreamEvent::TextDelta { text: s.into() }
}

fn reasoning(s: &str) -> StreamEvent {
    StreamEvent::ReasoningDelta { text: s.into() }
}

fn start(id: &str, name: &str) -> StreamEvent {
    StreamEvent::ToolCallStart {
        id: id.into(),
        name: name.into(),
    }
}

fn args(id: &str, s: &str) -> StreamEvent {
    StreamEvent::ToolCallArgsDelta {
        id: id.into(),
        json_fragment: s.into(),
    }
}

fn end(id: &str) -> StreamEvent {
    StreamEvent::ToolCallEnd { id: id.into() }
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> StreamEvent {
    StreamEvent::Usage {
        input,
        output,
        cache_read,
        cache_write,
    }
}

fn stop(reason: StopReason) -> StreamEvent {
    StreamEvent::Stop { reason }
}

// ---------------------------------------------------------------- OpenAI 兼容

#[tokio::test]
async fn openai_plain_text_with_trailing_usage() {
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_TEXT).await;
    assert_eq!(
        outcome.1,
        vec![
            text("你好"),
            text("，我是"),
            text("终端助手。😀"),
            // prompt_tokens 24 里有 8 个命中缓存：input 只算没命中的 16。
            usage(16, 9, 8, 0),
            stop(StopReason::EndTurn),
        ]
    );
    let turn = ok_turn(&outcome);
    assert_eq!(turn.text, "你好，我是终端助手。😀");
    assert_eq!(turn.usage.total_input(), 24);
    assert_eq!(turn.stop, Some(StopReason::EndTurn));
    assert!(turn.tool_calls.is_empty() && turn.reasoning.is_empty());
}

#[tokio::test]
async fn openai_deepseek_reasoning_content_then_text() {
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_DEEPSEEK).await;
    assert_eq!(
        outcome.1,
        vec![
            reasoning("用户问磁盘，"),
            reasoning("先看 df。"),
            text("根分区"),
            text("用了 40%。"),
            // 用量在带 finish_reason 的内容块里；命中 64、未命中 36。
            usage(36, 30, 64, 0),
            stop(StopReason::EndTurn),
        ]
    );
    let turn = ok_turn(&outcome);
    assert_eq!(
        turn.reasoning,
        vec![ReasoningBlock::Thinking {
            text: "用户问磁盘，先看 df。".into(),
            signature: None
        }]
    );
    assert_eq!(turn.text, "根分区用了 40%。");
}

#[tokio::test]
async fn openai_openrouter_reasoning_field_and_comment_lines() {
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_OPENROUTER).await;
    assert_eq!(
        outcome.1,
        vec![
            reasoning("先想一下"),
            reasoning("，再回答。"),
            text("答案是 42。"),
            usage(12, 20, 0, 0),
            stop(StopReason::EndTurn),
        ]
    );
}

#[tokio::test]
async fn openai_single_tool_call_arguments_are_stitched() {
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_TOOL_SINGLE).await;
    assert_eq!(
        outcome.1,
        vec![
            start("call_test_001", "run_command"),
            args("call_test_001", "{\"comm"),
            args("call_test_001", "and\": \"df -"),
            args("call_test_001", "h\", \"timeout_sec\": 30}"),
            end("call_test_001"),
            usage(150, 22, 0, 0),
            stop(StopReason::ToolUse),
        ]
    );
    let turn = ok_turn(&outcome);
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].id, "call_test_001");
    assert_eq!(turn.tool_calls[0].name, "run_command");
    assert_eq!(
        turn.tool_calls[0].arguments,
        Ok(json!({"command": "df -h", "timeout_sec": 30}))
    );
}

#[tokio::test]
async fn openai_parallel_tool_calls_interleaved_by_index() {
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_TOOL_PARALLEL).await;
    assert_eq!(
        outcome.1,
        vec![
            text("我同时查两项。"),
            start("call_test_a", "run_command"),
            args("call_test_a", "{\"command\":"),
            start("call_test_b", "read_file"),
            args("call_test_a", " \"uptime\"}"),
            args("call_test_b", "{\"path\": \"/etc/os-release\""),
            args("call_test_b", ", \"max_bytes\": 2048}"),
            end("call_test_a"),
            end("call_test_b"),
            usage(200, 60, 0, 0),
            stop(StopReason::ToolUse),
        ]
    );
    let turn = ok_turn(&outcome);
    assert_eq!(turn.text, "我同时查两项。");
    assert_eq!(turn.tool_calls.len(), 2);
    assert_eq!(
        turn.tool_calls[0].arguments,
        Ok(json!({"command": "uptime"}))
    );
    assert_eq!(
        turn.tool_calls[1].arguments,
        Ok(json!({"path": "/etc/os-release", "max_bytes": 2048}))
    );
}

#[tokio::test]
async fn openai_same_index_with_new_id_is_a_new_call_and_stop_becomes_tool_use() {
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_TOOL_SAME_INDEX).await;
    assert_eq!(
        outcome.1,
        vec![
            start("call_g1", "stat"),
            args("call_g1", "{\"path\":\"/tmp\"}"),
            end("call_g1"),
            start("call_g2", "list_dir"),
            args("call_g2", "{\"path\":\"/var\"}"),
            end("call_g2"),
            // 服务端报的是 stop，但有工具调用：按 ToolUse 处理。
            stop(StopReason::ToolUse),
        ]
    );
}

#[tokio::test]
async fn openai_comments_bad_lines_and_missing_done_are_tolerated() {
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_COMMENTS).await;
    assert_eq!(
        outcome.1,
        vec![
            text("第一段"),
            text("第二段"),
            usage(5, 4096, 0, 0),
            stop(StopReason::MaxTokens),
        ]
    );
}

#[tokio::test]
async fn openai_crlf_line_endings() {
    let crlf = OPENAI_TOOL_SINGLE
        .replace("\r\n", "\n")
        .replace('\n', "\r\n");
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, &crlf).await;
    assert_eq!(ok_turn(&outcome).tool_calls.len(), 1);
}

#[tokio::test]
async fn openai_done_without_finish_reason_infers_stop() {
    let stream =
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n";
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, stream).await;
    assert_eq!(outcome.1, vec![text("hi"), stop(StopReason::EndTurn)]);
}

#[tokio::test]
async fn openai_tool_call_without_id_gets_a_synthesized_one() {
    let stream = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"function\":{\"name\":\"stat\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, stream).await;
    let turn = ok_turn(&outcome);
    assert_eq!(turn.tool_calls.len(), 1);
    assert!(!turn.tool_calls[0].id.is_empty());
    assert_eq!(turn.tool_calls[0].arguments, Ok(json!({})));
}

#[tokio::test]
async fn openai_tool_name_repeated_on_every_fragment_starts_once() {
    let stream = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"stat\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"stat\",\"arguments\":\"\\\"/\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, stream).await;
    let starts = outcome
        .1
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolCallStart { .. }))
        .count();
    assert_eq!(starts, 1);
    assert_eq!(
        ok_turn(&outcome).tool_calls[0].arguments,
        Ok(json!({"path": "/"}))
    );
}

#[tokio::test]
async fn openai_midstream_error_event_is_delivered_then_returned() {
    let (result, events) =
        replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_MIDSTREAM_ERROR).await;
    let err = result.expect_err("应当报错");
    assert_eq!(err.kind, ErrorKind::Server);
    assert!(err.retryable);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0], text("处理到一半"));
    assert_eq!(events[1], StreamEvent::Error { error: err });
}

#[tokio::test]
async fn openai_stream_cut_before_finish_is_interrupted_and_retryable() {
    let cut = OPENAI_TEXT
        .find("\"finish_reason\":\"stop\"")
        .unwrap_or(OPENAI_TEXT.len());
    let (result, events) = replay(
        ProviderKind::OpenAiChat,
        vec![OPENAI_TEXT.as_bytes()[..cut].to_vec()],
    )
    .await;
    let err = result.expect_err("应当报错");
    assert_eq!(err.kind, ErrorKind::Interrupted);
    assert!(err.retryable);
    assert_eq!(
        events,
        vec![text("你好"), text("，我是"), text("终端助手。😀")]
    );
}

#[tokio::test]
async fn openai_invalid_tool_arguments_are_reported_not_hidden() {
    // 输出被长度上限截断，参数 JSON 没写完。
    let stream = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\": \\\"/tmp/a\\\", \\\"cont\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, stream).await;
    let turn = ok_turn(&outcome);
    assert_eq!(turn.stop, Some(StopReason::MaxTokens));
    assert!(turn.tool_calls[0].arguments.is_err());
    // 放回历史时记成空对象，下一次请求不会因为非法 JSON 被拒。
    assert_eq!(turn.to_message().tool_calls[0].arguments, json!({}));
}

// ---------------------------------------------------------------- Anthropic

#[tokio::test]
async fn anthropic_plain_text_and_cache_usage() {
    let outcome = replay_all_chunkings(ProviderKind::Anthropic, ANTHROPIC_TEXT).await;
    assert_eq!(
        outcome.1,
        vec![
            usage(25, 1, 400, 100),
            text("你好"),
            text("，我是终端助手。😀"),
            // message_delta 里的 output_tokens 是累计值，其余沿用 message_start 的。
            usage(25, 15, 400, 100),
            stop(StopReason::EndTurn),
        ]
    );
    let turn = ok_turn(&outcome);
    assert_eq!(
        turn.usage,
        Usage {
            input: 25,
            output: 15,
            cache_read: 400,
            cache_write: 100
        }
    );
    assert_eq!(turn.usage.total_input(), 525);
}

#[tokio::test]
async fn anthropic_thinking_with_signature_then_text() {
    let outcome = replay_all_chunkings(ProviderKind::Anthropic, ANTHROPIC_THINKING).await;
    let turn = ok_turn(&outcome);
    assert_eq!(
        turn.reasoning,
        vec![ReasoningBlock::Thinking {
            text: "I need to find the GCD of 1071 and 462 using the Euclidean algorithm.\n\n1071 = 2 × 462 + 147\n462 = 3 × 147 + 21\n147 = 7 × 21 + 0".into(),
            signature: Some("EqQBCgIYAhIMtestsignaturebase64AAAA==".into()),
        }]
    );
    assert_eq!(
        turn.text,
        "The greatest common divisor of 1071 and 462 is **21**."
    );
    assert!(outcome.1.contains(&StreamEvent::ReasoningSignature {
        signature: "EqQBCgIYAhIMtestsignaturebase64AAAA==".into()
    }));
    assert_eq!(turn.usage.output, 120);
}

#[tokio::test]
async fn anthropic_tool_use_fragments() {
    let outcome = replay_all_chunkings(ProviderKind::Anthropic, ANTHROPIC_TOOL_USE).await;
    assert_eq!(
        outcome.1,
        vec![
            usage(472, 2, 0, 0),
            text("Okay"),
            text(", let's check the disk:"),
            start("toolu_test_0001", "run_command"),
            args("toolu_test_0001", "{\"command\":"),
            args("toolu_test_0001", " \"df"),
            args("toolu_test_0001", " -h\"}"),
            end("toolu_test_0001"),
            usage(472, 89, 0, 0),
            stop(StopReason::ToolUse),
        ]
    );
    let turn = ok_turn(&outcome);
    assert_eq!(turn.text, "Okay, let's check the disk:");
    assert_eq!(
        turn.tool_calls[0].arguments,
        Ok(json!({"command": "df -h"}))
    );
}

#[tokio::test]
async fn anthropic_multi_block_mix_with_redacted_and_unknown_blocks() {
    let outcome = replay_all_chunkings(ProviderKind::Anthropic, ANTHROPIC_MULTI).await;
    let turn = ok_turn(&outcome);
    assert_eq!(
        turn.reasoning,
        vec![
            ReasoningBlock::Thinking {
                text: "两件事可以并行。".into(),
                signature: Some("sigFirstBlockAAAA".into())
            },
            ReasoningBlock::Redacted {
                data: "EmwKAhgBEgyredactedDataBlob".into()
            },
        ]
    );
    assert_eq!(turn.text, "我先查两项。");
    // server_tool_use 块（含它的 input_json_delta）整个被忽略。
    assert_eq!(turn.tool_calls.len(), 2);
    assert_eq!(turn.tool_calls[0].id, "toolu_test_a");
    assert_eq!(
        turn.tool_calls[0].arguments,
        Ok(json!({"command": "uptime"}))
    );
    // 没有任何参数片段的工具调用 = 空对象。
    assert_eq!(turn.tool_calls[1].name, "list_targets");
    assert_eq!(turn.tool_calls[1].arguments, Ok(json!({})));
    assert_eq!(
        turn.usage,
        Usage {
            input: 10,
            output: 77,
            cache_read: 2048,
            cache_write: 0
        }
    );
    assert_eq!(turn.stop, Some(StopReason::ToolUse));
    let ends = outcome
        .1
        .iter()
        .filter(|e| matches!(e, StreamEvent::ToolCallEnd { .. }))
        .count();
    assert_eq!(ends, 2);
}

#[tokio::test]
async fn anthropic_omitted_thinking_keeps_the_signature() {
    let outcome = replay_all_chunkings(ProviderKind::Anthropic, ANTHROPIC_OMITTED).await;
    let turn = ok_turn(&outcome);
    assert_eq!(
        turn.reasoning,
        vec![ReasoningBlock::Thinking {
            text: String::new(),
            signature: Some("sigOmittedAAAA".into())
        }]
    );
    assert_eq!(turn.stop, Some(StopReason::MaxTokens));
    // 没有可显示的思考增量。
    assert!(!outcome
        .1
        .iter()
        .any(|e| matches!(e, StreamEvent::ReasoningDelta { .. })));
}

#[tokio::test]
async fn anthropic_error_event_after_partial_text() {
    let (result, events) =
        replay_all_chunkings(ProviderKind::Anthropic, ANTHROPIC_ERROR_EVENT).await;
    let err = result.expect_err("应当报错");
    assert_eq!(err.kind, ErrorKind::Overloaded);
    assert!(err.retryable);
    assert_eq!(events.last(), Some(&StreamEvent::Error { error: err }));
    assert!(events.contains(&text("写到一半")));
}

#[tokio::test]
async fn anthropic_stream_cut_before_message_delta_is_interrupted() {
    let cut = ANTHROPIC_TEXT
        .find("event: message_delta")
        .unwrap_or(ANTHROPIC_TEXT.len());
    let (result, _) = replay(
        ProviderKind::Anthropic,
        vec![ANTHROPIC_TEXT.as_bytes()[..cut].to_vec()],
    )
    .await;
    let err = result.expect_err("应当报错");
    assert_eq!((err.kind, err.retryable), (ErrorKind::Interrupted, true));
}

#[tokio::test]
async fn anthropic_ping_only_stream_is_not_a_reply() {
    let (result, _) = replay(
        ProviderKind::Anthropic,
        vec![b"event: ping\ndata: {\"type\": \"ping\"}\n\n".to_vec()],
    )
    .await;
    assert_eq!(result.expect_err("应当报错").kind, ErrorKind::Interrupted);
}

// ---------------------------------------------------------------- 往返：回合 → 历史 → 请求体

#[tokio::test]
async fn anthropic_turn_round_trips_into_the_next_request() {
    let outcome = replay_all_chunkings(ProviderKind::Anthropic, ANTHROPIC_MULTI).await;
    let assistant = ok_turn(&outcome).to_message();
    let mut req = request("claude-test");
    req.messages.push(Message::Assistant(assistant));
    req.messages.push(Message::ToolResult {
        call_id: "toolu_test_a".into(),
        content: "up 3 days".into(),
        is_error: false,
    });
    req.messages.push(Message::ToolResult {
        call_id: "toolu_test_b".into(),
        content: "[]".into(),
        is_error: false,
    });
    let body = anthropic::build_body(&req, &AnthropicOptions::default());
    let blocks = &body["messages"][1]["content"];
    let types: Vec<&str> = blocks
        .as_array()
        .map(|b| b.iter().filter_map(|x| x["type"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        types,
        [
            "thinking",
            "redacted_thinking",
            "text",
            "tool_use",
            "tool_use"
        ]
    );
    assert_eq!(blocks[0]["signature"], "sigFirstBlockAAAA");
    assert_eq!(blocks[1]["data"], "EmwKAhgBEgyredactedDataBlob");
    assert_eq!(
        body["messages"][2]["content"].as_array().map(Vec::len),
        Some(2)
    );
}

#[tokio::test]
async fn deepseek_turn_round_trips_with_reasoning_content() {
    let mut req = request("deepseek-v4-flash");
    let outcome = replay_all_chunkings(ProviderKind::OpenAiChat, OPENAI_DEEPSEEK).await;
    req.messages
        .push(Message::Assistant(ok_turn(&outcome).to_message()));
    let body = openai_chat::build_body(&req, &OpenAiOptions::default());
    assert_eq!(
        body["messages"][2]["reasoning_content"],
        "用户问磁盘，先看 df。"
    );
    assert_eq!(body["messages"][2]["content"], "根分区用了 40%。");
}

// ---------------------------------------------------------------- HTTP 错误与传输失败

#[tokio::test]
async fn http_401_is_auth_not_retryable_for_both_kinds() {
    for (kind, body) in [
        (ProviderKind::OpenAiChat, OPENAI_ERROR_401),
        (ProviderKind::Anthropic, ANTHROPIC_ERROR_401),
    ] {
        let ((result, events), _) =
            replay_script(kind, Script::body(401, "application/json", body.as_bytes())).await;
        let err = result.expect_err("应当报错");
        assert_eq!(err.kind, ErrorKind::Auth, "{kind:?}");
        assert!(!err.retryable);
        assert_eq!(err.status, Some(401));
        assert!(events.is_empty());
    }
}

#[tokio::test]
async fn http_errors_are_classified() {
    let cases: [(u16, &str, ErrorKind, bool); 6] = [
        (
            403,
            r#"{"error":{"message":"forbidden"}}"#,
            ErrorKind::Auth,
            false,
        ),
        (
            404,
            r#"{"error":{"message":"model not found"}}"#,
            ErrorKind::NotFound,
            false,
        ),
        (
            429,
            r#"{"error":{"message":"slow down"}}"#,
            ErrorKind::RateLimited,
            true,
        ),
        (
            500,
            r#"{"error":{"message":"oops"}}"#,
            ErrorKind::Server,
            true,
        ),
        (503, "", ErrorKind::Overloaded, true),
        (
            400,
            r#"{"error":{"message":"bad"}}"#,
            ErrorKind::BadRequest,
            false,
        ),
    ];
    for (status, body, kind, retryable) in cases {
        let script =
            Script::body(status, "application/json", body.as_bytes()).header("retry-after", "12");
        let ((result, _), _) = replay_script(ProviderKind::OpenAiChat, script).await;
        let err = result.expect_err("应当报错");
        assert_eq!(
            (err.kind, err.retryable),
            (kind, retryable),
            "HTTP {status}"
        );
        if status == 429 {
            assert_eq!(err.retry_after, Some(Duration::from_secs(12)));
        }
    }
}

#[tokio::test]
async fn http_error_body_that_echoes_the_key_is_scrubbed() {
    let body = format!(r#"{{"error":{{"message":"Invalid key {TEST_KEY}"}}}}"#);
    let ((result, _), _) = replay_script(
        ProviderKind::OpenAiChat,
        Script::body(401, "application/json", body.as_bytes()),
    )
    .await;
    let shown = result.expect_err("应当报错").to_string();
    assert!(
        !shown.contains(TEST_KEY) && !shown.contains("SECRET"),
        "{shown}"
    );
}

#[tokio::test]
async fn html_bodies_are_not_sse() {
    // 200 + HTML（填错了地址 / 网关登录页）。
    let ((result, events), _) = replay_script(
        ProviderKind::OpenAiChat,
        Script::body(200, "text/html; charset=utf-8", GATEWAY_HTML.as_bytes()),
    )
    .await;
    let err = result.expect_err("应当报错");
    assert_eq!((err.kind, err.retryable), (ErrorKind::NotSse, false));
    assert!(err.message.contains("网页"), "{}", err.message);
    assert!(events.is_empty());
    // 没带 content-type 也一样认得出来，而且不管怎么切块。
    for seed in 1..=10u64 {
        let script = Script {
            status: 200,
            headers: vec![],
            chunks: random_chunks(GATEWAY_HTML.as_bytes(), seed, 5),
            then_error: None,
            post_error: None,
        };
        let ((result, _), _) = replay_script(ProviderKind::Anthropic, script).await;
        assert_eq!(
            result.expect_err("应当报错").kind,
            ErrorKind::NotSse,
            "seed {seed}"
        );
    }
    // 502 + HTML：是网关 5xx，可重试，不回显网页内容。
    let ((result, _), _) = replay_script(
        ProviderKind::OpenAiChat,
        Script::body(502, "text/html", GATEWAY_HTML.as_bytes()),
    )
    .await;
    let err = result.expect_err("应当报错");
    assert_eq!(
        (err.kind, err.retryable, err.detail),
        (ErrorKind::Server, true, None)
    );
}

#[tokio::test]
async fn plain_json_completion_and_empty_body_are_not_sse() {
    let json_reply = br#"{"id":"x","choices":[{"message":{"content":"hi"}}]}"#;
    let ((result, _), _) = replay_script(
        ProviderKind::OpenAiChat,
        Script::body(200, "application/json", json_reply),
    )
    .await;
    let err = result.expect_err("应当报错");
    assert_eq!(err.kind, ErrorKind::NotSse);
    assert!(err.message.contains("JSON"));
    let ((result, _), _) = replay_script(ProviderKind::OpenAiChat, Script::sse(vec![])).await;
    let err = result.expect_err("应当报错");
    assert_eq!(err.kind, ErrorKind::NotSse);
    assert!(err.message.contains("空内容"));
    // 200 + 错误 JSON：按里面的错误类型分类（这里是过载，可重试）。
    let ((result, _), _) = replay_script(
        ProviderKind::Anthropic,
        Script::body(
            200,
            "application/json",
            br#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
        ),
    )
    .await;
    let err = result.expect_err("应当报错");
    assert_eq!((err.kind, err.retryable), (ErrorKind::Overloaded, true));
}

#[tokio::test]
async fn sse_without_content_type_header_still_works() {
    let script = Script {
        status: 200,
        headers: vec![],
        chunks: random_chunks(OPENAI_TEXT.as_bytes(), 3, 9),
        then_error: None,
        post_error: None,
    };
    let ((result, _), _) = replay_script(ProviderKind::OpenAiChat, script).await;
    assert_eq!(
        result.map(|t| t.text).ok().as_deref(),
        Some("你好，我是终端助手。😀")
    );
}

#[tokio::test]
async fn redirect_is_reported_with_the_target() {
    let script = Script::body(301, "text/html", b"moved").header(
        "location",
        "https://elsewhere.example.test/v1/chat/completions",
    );
    let ((result, _), _) = replay_script(ProviderKind::OpenAiChat, script).await;
    let err = result.expect_err("应当报错");
    assert_eq!((err.kind, err.retryable), (ErrorKind::Redirect, false));
    assert!(err.message.contains("elsewhere.example.test"));
}

#[tokio::test]
async fn connection_failure_and_mid_stream_drop() {
    let mut script = Script::sse(vec![]);
    script.post_error = Some(TransportError::Connect("dns error".into()));
    let ((result, _), _) = replay_script(ProviderKind::OpenAiChat, script).await;
    let err = result.expect_err("应当报错");
    assert_eq!((err.kind, err.retryable), (ErrorKind::Connect, true));

    // 收了一半，连接被重置。
    let mut script = Script::sse(random_chunks(&OPENAI_TEXT.as_bytes()[..700], 5, 50));
    script.then_error = Some(TransportError::Interrupted("connection reset".into()));
    let ((result, events), _) = replay_script(ProviderKind::OpenAiChat, script).await;
    let err = result.expect_err("应当报错");
    assert_eq!((err.kind, err.retryable), (ErrorKind::Interrupted, true));
    assert!(!events.is_empty(), "断开之前的内容已经交出去了");

    let mut script = Script::sse(vec![]);
    script.post_error = Some(TransportError::Timeout("slow".into()));
    let ((result, _), _) = replay_script(ProviderKind::Anthropic, script).await;
    assert_eq!(result.expect_err("应当报错").kind, ErrorKind::Timeout);
}

// ---------------------------------------------------------------- 请求构造（经 stream_chat 发出去的样子）

#[tokio::test]
async fn openai_request_url_headers_and_body() {
    let (_, transport) = replay_script(
        ProviderKind::OpenAiChat,
        Script::sse(vec![OPENAI_TEXT.as_bytes().to_vec()]),
    )
    .await;
    let sent = transport.last_request().expect("发出了请求");
    assert_eq!(sent.url, "https://llm.example.test/v1/chat/completions");
    let header = |name: &str| {
        sent.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        header("authorization"),
        Some(format!("Bearer {TEST_KEY}").as_str())
    );
    assert_eq!(header("accept"), Some("text/event-stream"));
    assert_eq!(header("content-type"), Some("application/json"));
    assert!(header("x-api-key").is_none());
    let body: Value = serde_json::from_slice(&sent.body).unwrap_or(Value::Null);
    assert_eq!(body["stream"], true);
    assert_eq!(body["tools"].as_array().map(Vec::len), Some(8));
    assert_eq!(body["messages"][0]["role"], "system");
}

#[tokio::test]
async fn anthropic_request_url_headers_and_body() {
    let (_, transport) = replay_script(
        ProviderKind::Anthropic,
        Script::sse(vec![ANTHROPIC_TEXT.as_bytes().to_vec()]),
    )
    .await;
    let sent = transport.last_request().expect("发出了请求");
    assert_eq!(sent.url, "https://llm.example.test/v1/messages");
    let header = |name: &str| {
        sent.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(header("x-api-key"), Some(TEST_KEY));
    assert_eq!(header("anthropic-version"), Some("2023-06-01"));
    assert!(header("authorization").is_none());
    let body: Value = serde_json::from_slice(&sent.body).unwrap_or(Value::Null);
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["system"], "系统提示词");
}

#[test]
fn empty_key_sends_no_auth_header_and_extra_headers_pass_through() {
    let mut cfg = ProviderConfig::new(
        ProviderKind::OpenAiChat,
        "http://local.example.test/v1",
        ApiKey::default(),
    );
    cfg.extra_headers
        .push(("HTTP-Referer".into(), "https://app.example.test".into()));
    let sent = build_http_request(&cfg, &request("m"))
        .map(|r| r.headers)
        .unwrap_or_default();
    assert!(!sent.iter().any(|(k, _)| k == "authorization"));
    assert!(sent.iter().any(|(k, _)| k == "HTTP-Referer"));
}

#[test]
fn debug_output_never_contains_key_or_conversation() {
    let cfg = config(ProviderKind::Anthropic);
    let req = request("m");
    let sent = build_http_request(&cfg, &req).expect("构造请求");
    for shown in [
        format!("{cfg:?}"),
        format!("{sent:?}"),
        format!("{req:?}"),
        format!("{:?}", cfg.api_key),
    ] {
        assert!(!shown.contains("SECRET"), "{shown}");
        assert!(
            !shown.contains("系统提示词") && !shown.contains("你好"),
            "{shown}"
        );
    }
}

// ---------------------------------------------------------------- 真实 reqwest 传输（本机回环服务）

mod loopback {
    use super::*;
    use std::net::Ipv4Addr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 起一个只服务一次连接的本机 HTTP 服务：读完请求，按 `pieces` 分段写响应，返回收到的请求原文。
    async fn serve_once(pieces: Vec<Vec<u8>>) -> (u16, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let handle = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return Vec::new();
            };
            let mut received = Vec::new();
            let mut buf = [0u8; 4096];
            while let Ok(n) = socket.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                received.extend_from_slice(&buf[..n]);
                if let Some(at) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&received[..at]).to_lowercase();
                    let want: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if received.len() >= at + 4 + want {
                        break;
                    }
                }
            }
            for piece in pieces {
                if socket.write_all(&piece).await.is_err() {
                    break;
                }
                let _ = socket.flush().await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let _ = socket.shutdown().await;
            received
        });
        (port, handle)
    }

    fn http_head(status_line: &str, extra: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status_line}\r\n{extra}connection: close\r\n\r\n").into_bytes()
    }

    #[tokio::test]
    async fn real_transport_streams_an_sse_reply() {
        let mut pieces = vec![http_head("200 OK", "content-type: text/event-stream\r\n")];
        pieces.extend(random_chunks(OPENAI_TOOL_PARALLEL.as_bytes(), 11, 200));
        let (port, server) = serve_once(pieces).await;
        let transport = ReqwestTransport::build(None).expect("客户端");
        let cfg = ProviderConfig::new(
            ProviderKind::OpenAiChat,
            format!("http://{}:{port}/v1", Ipv4Addr::LOCALHOST),
            ApiKey::new(TEST_KEY),
        );
        let mut events = Vec::new();
        let turn = stream_chat(&transport, &cfg, &request("m"), &mut |e| events.push(e))
            .await
            .expect("应当成功");
        assert_eq!(turn.tool_calls.len(), 2);
        assert_eq!(turn.stop, Some(StopReason::ToolUse));
        let received = String::from_utf8_lossy(&server.await.unwrap_or_default()).to_string();
        assert!(
            received.starts_with("POST /v1/chat/completions HTTP/1.1"),
            "{received}"
        );
        let lower = received.to_lowercase();
        assert!(lower.contains(&format!(
            "authorization: bearer {}",
            TEST_KEY.to_lowercase()
        )));
        assert!(lower.contains("accept: text/event-stream"));
    }

    #[tokio::test]
    async fn real_transport_maps_401_and_redirect() {
        let body = ANTHROPIC_ERROR_401;
        let (port, _server) = serve_once(vec![
            http_head(
                "401 Unauthorized",
                &format!(
                    "content-type: application/json\r\ncontent-length: {}\r\n",
                    body.len()
                ),
            ),
            body.as_bytes().to_vec(),
        ])
        .await;
        let transport = ReqwestTransport::build(None).expect("客户端");
        let cfg = ProviderConfig::new(
            ProviderKind::Anthropic,
            format!("http://{}:{port}", Ipv4Addr::LOCALHOST),
            ApiKey::new(TEST_KEY),
        );
        let err = stream_chat(&transport, &cfg, &request("m"), &mut |_| {})
            .await
            .expect_err("应当报错");
        assert_eq!(err.kind, ErrorKind::Auth);

        let (port, _server) = serve_once(vec![http_head(
            "301 Moved Permanently",
            "location: https://elsewhere.example.test/v1/messages\r\ncontent-length: 0\r\n",
        )])
        .await;
        let cfg = ProviderConfig::new(
            ProviderKind::Anthropic,
            format!("http://{}:{port}", Ipv4Addr::LOCALHOST),
            ApiKey::new(TEST_KEY),
        );
        let err = stream_chat(&transport, &cfg, &request("m"), &mut |_| {})
            .await
            .expect_err("应当报错");
        assert_eq!(err.kind, ErrorKind::Redirect);
    }

    #[tokio::test]
    async fn real_transport_reports_connection_refused() {
        // 先占一个端口再释放，保证这个端口上没人监听。
        let port = {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let transport = ReqwestTransport::build(None).expect("客户端");
        let cfg = ProviderConfig::new(
            ProviderKind::OpenAiChat,
            format!("http://{}:{port}/v1", Ipv4Addr::LOCALHOST),
            ApiKey::new(TEST_KEY),
        );
        let err = stream_chat(&transport, &cfg, &request("m"), &mut |_| {})
            .await
            .expect_err("应当报错");
        assert_eq!((err.kind, err.retryable), (ErrorKind::Connect, true));
    }

    #[tokio::test]
    async fn real_transport_server_hangs_up_mid_stream() {
        let mut pieces = vec![http_head("200 OK", "content-type: text/event-stream\r\n")];
        pieces.push(OPENAI_TEXT.as_bytes()[..300].to_vec());
        let (port, _server) = serve_once(pieces).await;
        let transport = ReqwestTransport::build(None).expect("客户端");
        let cfg = ProviderConfig::new(
            ProviderKind::OpenAiChat,
            format!("http://{}:{port}/v1", Ipv4Addr::LOCALHOST),
            ApiKey::new(TEST_KEY),
        );
        let err = stream_chat(&transport, &cfg, &request("m"), &mut |_| {})
            .await
            .expect_err("应当报错");
        assert!(err.retryable, "{err}");
    }
}

// ---------------------------------------------------------------- 可选的联网自测

/// 设了 `ZSSH_AI_TEST_BASE` / `ZSSH_AI_TEST_KEY` / `ZSSH_AI_TEST_MODEL` / `ZSSH_AI_TEST_KIND`
/// （`openai` | `anthropic`）才会真的发请求：
/// `cargo test -p zssh-core live_round_trip -- --ignored --nocapture`。
#[tokio::test]
#[ignore = "联网自测：需要设置 ZSSH_AI_TEST_BASE / KEY / MODEL / KIND"]
async fn live_round_trip() {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let (Some(base), Some(key), Some(model), Some(kind)) = (
        var("ZSSH_AI_TEST_BASE"),
        var("ZSSH_AI_TEST_KEY"),
        var("ZSSH_AI_TEST_MODEL"),
        var("ZSSH_AI_TEST_KIND"),
    ) else {
        eprintln!("跳过：没有设置 ZSSH_AI_TEST_BASE / ZSSH_AI_TEST_KEY / ZSSH_AI_TEST_MODEL / ZSSH_AI_TEST_KIND");
        return;
    };
    let kind = match kind.to_lowercase().as_str() {
        "openai" => ProviderKind::OpenAiChat,
        "anthropic" => ProviderKind::Anthropic,
        other => {
            eprintln!("跳过：ZSSH_AI_TEST_KIND 要么 openai 要么 anthropic，收到 {other:?}");
            return;
        }
    };
    let cfg = ProviderConfig::new(kind, base, ApiKey::new(key));
    let transport = ReqwestTransport::build(None).expect("客户端");
    let mut req = request(&model);
    req.system = "Answer in as few words as possible.".into();
    req.messages = vec![Message::User {
        text: "Reply with the single word: pong".into(),
    }];
    req.tools.clear();
    req.max_output_tokens = 256;
    let mut events = 0usize;
    let turn = stream_chat(&transport, &cfg, &req, &mut |_| events += 1)
        .await
        .expect("联网请求失败");
    // 只打印规模，不打印回复正文。
    eprintln!(
        "联网自测通过：{events} 个事件，回复 {} 字符，用量 {:?}，结束原因 {:?}",
        turn.text.chars().count(),
        turn.usage,
        turn.stop
    );
    assert!(events > 0 && turn.stop.is_some());
}
