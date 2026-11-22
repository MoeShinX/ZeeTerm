//! 内置助手的完整对话记录（V2-A3）：续接用。
//!
//! 界面的历史会话（[`crate::ai::history::HistoryRecord`]）只存给人看的摘要（工具结果最多 4KB、
//! 没有思考块）；续接要把**模型看到的**消息原样接上——Anthropic 的思考块签名、工具调用的 id 和
//! 参数、完整的工具结果——所以另存一份，和历史会话文件并排放（见
//! [`crate::ai::history::HistoryStore::transcript_save`]），靠历史记录里的 `agent_session_id`
//! （= [`super::BuiltinConversation::resume_id`]）对上。
//!
//! 格式自己带版本号；读不出来的记录当"没有"（续接退成新会话并提示），不能崩。写之前把 API Key
//! 抹掉（模型偶尔会复述系统里出现过的字符串，工具输出也可能带回来）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::provider::{AssistantMessage, Message, ReasoningBlock, ToolCall};
use crate::ai::history::MAX_TRANSCRIPT_BYTES;

/// 当前格式版本。
const FORMAT_VERSION: u32 = 1;
/// 正文里出现的密钥替换成这个。
const REDACTED: &str = "[已隐藏]";
/// 比这短的串不当密钥抹（空串、很短的串替换起来会误伤正文）。
const MIN_SECRET_LEN: usize = 8;

#[derive(Debug, Serialize, Deserialize)]
struct Wire {
    version: u32,
    session_id: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    messages: Vec<WireMessage>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum WireMessage {
    User {
        text: String,
    },
    Assistant {
        #[serde(default)]
        text: String,
        #[serde(default)]
        reasoning: Vec<WireReasoning>,
        #[serde(default)]
        tool_calls: Vec<WireCall>,
    },
    ToolResult {
        call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireReasoning {
    Thinking {
        text: String,
        #[serde(default)]
        signature: Option<String>,
    },
    Redacted {
        data: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct WireCall {
    id: String,
    name: String,
    arguments: Value,
}

/// 读出来的记录。
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub session_id: String,
    /// 上一次用的模型（续接时界面可以默认选它）。
    pub model: Option<String>,
    pub messages: Vec<Message>,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscriptError {
    #[error("对话记录读不出来：{0}")]
    Corrupt(String),
    #[error("对话记录的格式版本（{0}）比这个版本新，不能续接")]
    TooNew(u32),
}

fn to_wire(message: &Message) -> WireMessage {
    match message {
        Message::User { text } => WireMessage::User { text: text.clone() },
        Message::Assistant(a) => WireMessage::Assistant {
            text: a.text.clone(),
            reasoning: a
                .reasoning
                .iter()
                .map(|block| match block {
                    ReasoningBlock::Thinking { text, signature } => WireReasoning::Thinking {
                        text: text.clone(),
                        signature: signature.clone(),
                    },
                    ReasoningBlock::Redacted { data } => {
                        WireReasoning::Redacted { data: data.clone() }
                    }
                })
                .collect(),
            tool_calls: a
                .tool_calls
                .iter()
                .map(|call| WireCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect(),
        },
        Message::ToolResult {
            call_id,
            content,
            is_error,
        } => WireMessage::ToolResult {
            call_id: call_id.clone(),
            content: content.clone(),
            is_error: *is_error,
        },
    }
}

fn from_wire(message: WireMessage) -> Message {
    match message {
        WireMessage::User { text } => Message::User { text },
        WireMessage::Assistant {
            text,
            reasoning,
            tool_calls,
        } => Message::Assistant(AssistantMessage {
            text,
            reasoning: reasoning
                .into_iter()
                .map(|block| match block {
                    WireReasoning::Thinking { text, signature } => {
                        ReasoningBlock::Thinking { text, signature }
                    }
                    WireReasoning::Redacted { data } => ReasoningBlock::Redacted { data },
                })
                .collect(),
            tool_calls: tool_calls
                .into_iter()
                .map(|call| ToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: call.arguments,
                })
                .collect(),
        }),
        WireMessage::ToolResult {
            call_id,
            content,
            is_error,
        } => Message::ToolResult {
            call_id,
            content,
            is_error,
        },
    }
}

/// 把 JSON 里所有字符串里的密钥替换掉。
fn redact_value(value: &mut Value, secrets: &[&str]) {
    match value {
        Value::String(text) => {
            for secret in secrets {
                if secret.len() >= MIN_SECRET_LEN && text.contains(secret) {
                    *text = text.replace(secret, REDACTED);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| redact_value(v, secrets)),
        Value::Object(map) => map.values_mut().for_each(|v| redact_value(v, secrets)),
        _ => {}
    }
}

/// 序列化成要存盘的字节。`secrets` 里的串（API Key）从所有文字里抹掉；超过
/// [`MAX_TRANSCRIPT_BYTES`] 时从最早的回合开始丢，直到放得下。
pub fn encode(
    session_id: &str,
    model: Option<&str>,
    messages: &[Message],
    secrets: &[&str],
) -> Result<Vec<u8>, TranscriptError> {
    let mut kept: &[Message] = messages;
    loop {
        let wire = Wire {
            version: FORMAT_VERSION,
            session_id: session_id.to_owned(),
            model: model.map(str::to_owned),
            messages: kept.iter().map(to_wire).collect(),
        };
        let mut value =
            serde_json::to_value(&wire).map_err(|e| TranscriptError::Corrupt(e.to_string()))?;
        redact_value(&mut value, secrets);
        let bytes =
            serde_json::to_vec(&value).map_err(|e| TranscriptError::Corrupt(e.to_string()))?;
        if bytes.len() <= MAX_TRANSCRIPT_BYTES || kept.len() <= 2 {
            return Ok(bytes);
        }
        // 丢掉最早的一个回合（到下一条用户消息为止；找不到就只丢第一条之外的一半）。
        let next_user = kept
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, m)| matches!(m, Message::User { .. }))
            .map(|(i, _)| i);
        let drop = next_user.unwrap_or(kept.len() / 2).max(1);
        kept = &kept[drop..];
    }
}

/// 读存盘的字节。
pub fn decode(bytes: &[u8]) -> Result<Transcript, TranscriptError> {
    let wire: Wire =
        serde_json::from_slice(bytes).map_err(|e| TranscriptError::Corrupt(e.to_string()))?;
    if wire.version > FORMAT_VERSION {
        return Err(TranscriptError::TooNew(wire.version));
    }
    let messages: Vec<Message> = wire.messages.into_iter().map(from_wire).collect();
    check_tool_pairs(&messages)?;
    Ok(Transcript {
        session_id: wire.session_id,
        model: wire.model,
        messages,
    })
}

/// 续接前核对：每个 assistant 的工具调用后面都有对应的结果（接口会拒绝缺结果的历史）。
/// 缺的补一条"没有拿到结果"的失败结果——比如上次在工具执行到一半时被关掉。
pub fn check_tool_pairs(messages: &[Message]) -> Result<(), TranscriptError> {
    // 只核对结构能不能用；缺结果的由 repair_tool_pairs 补，这里只拒绝明显损坏的（结果没有调用）。
    let mut open: Vec<&str> = Vec::new();
    for message in messages {
        match message {
            Message::Assistant(a) => {
                open.clear();
                open.extend(a.tool_calls.iter().map(|c| c.id.as_str()));
            }
            Message::ToolResult { call_id, .. } => {
                if !open.contains(&call_id.as_str()) {
                    return Err(TranscriptError::Corrupt(
                        "有工具结果找不到对应的调用".to_owned(),
                    ));
                }
            }
            Message::User { .. } => open.clear(),
        }
    }
    Ok(())
}

/// 给缺结果的工具调用补一条失败结果，保证历史能发给接口。
pub fn repair_tool_pairs(messages: &mut Vec<Message>) -> usize {
    let mut repaired = 0;
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut pending: Vec<String> = Vec::new();
    let flush = |pending: &mut Vec<String>, out: &mut Vec<Message>, repaired: &mut usize| {
        for call_id in pending.drain(..) {
            out.push(Message::ToolResult {
                call_id,
                content: "（上一次对话在这个工具返回结果之前就结束了，没有拿到结果）".to_owned(),
                is_error: true,
            });
            *repaired += 1;
        }
    };
    for message in messages.drain(..) {
        match &message {
            Message::ToolResult { call_id, .. } => pending.retain(|id| id != call_id),
            _ => flush(&mut pending, &mut out, &mut repaired),
        }
        if let Message::Assistant(a) = &message {
            pending = a.tool_calls.iter().map(|c| c.id.clone()).collect();
        }
        out.push(message);
    }
    flush(&mut pending, &mut out, &mut repaired);
    *messages = out;
    repaired
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn sample() -> Vec<Message> {
        vec![
            Message::User {
                text: "看看磁盘".into(),
            },
            Message::Assistant(AssistantMessage {
                text: "我来查".into(),
                reasoning: vec![
                    ReasoningBlock::Thinking {
                        text: "先 df".into(),
                        signature: Some("sig-abc".into()),
                    },
                    ReasoningBlock::Redacted {
                        data: "enc-data".into(),
                    },
                ],
                tool_calls: vec![ToolCall {
                    id: "call_1".into(),
                    name: "run_command".into(),
                    arguments: json!({"command": "df -h"}),
                }],
            }),
            Message::ToolResult {
                call_id: "call_1".into(),
                content: "exit_code: 0\n[zssh exec ab-1]".into(),
                is_error: false,
            },
            Message::Assistant(AssistantMessage {
                text: "磁盘够用".into(),
                ..AssistantMessage::default()
            }),
        ]
    }

    #[test]
    fn round_trip_keeps_signatures_ids_and_arguments() {
        let messages = sample();
        let bytes = encode("sess_1", Some("m-1"), &messages, &[]).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back.session_id, "sess_1");
        assert_eq!(back.model.as_deref(), Some("m-1"));
        assert_eq!(back.messages, messages);
    }

    #[test]
    fn the_api_key_is_erased_everywhere_before_saving() {
        let key = "sk-live-0123456789";
        let mut messages = sample();
        messages.push(Message::User {
            text: format!("我的 key 是 {key}"),
        });
        messages.push(Message::Assistant(AssistantMessage {
            text: format!("收到 {key}"),
            reasoning: vec![ReasoningBlock::Thinking {
                text: format!("用户给了 {key}"),
                signature: None,
            }],
            tool_calls: vec![ToolCall {
                id: "c2".into(),
                name: "run_command".into(),
                arguments: json!({"command": format!("echo {key}")}),
            }],
        }));
        messages.push(Message::ToolResult {
            call_id: "c2".into(),
            content: format!("{key}\n"),
            is_error: false,
        });
        let bytes = encode("s", None, &messages, &[key, "短"]).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains(key), "{text}");
        assert!(text.contains(REDACTED));
        // 太短的"密钥"不抹，免得误伤。
        assert!(text.contains("看看磁盘"));
    }

    #[test]
    fn a_newer_format_and_garbage_are_refused_without_panicking() {
        let too_new = json!({"version": 99, "session_id": "s", "messages": []});
        assert!(matches!(
            decode(&serde_json::to_vec(&too_new).unwrap()),
            Err(TranscriptError::TooNew(99))
        ));
        assert!(matches!(
            decode(b"not json"),
            Err(TranscriptError::Corrupt(_))
        ));
        let orphan = json!({"version": 1, "session_id": "s", "messages": [
            {"role": "tool_result", "call_id": "x", "content": "c"}
        ]});
        assert!(matches!(
            decode(&serde_json::to_vec(&orphan).unwrap()),
            Err(TranscriptError::Corrupt(_))
        ));
    }

    #[test]
    fn unknown_fields_and_missing_optional_fields_are_tolerated() {
        let raw = json!({"version": 1, "session_id": "s", "future": 1, "messages": [
            {"role": "user", "text": "hi", "extra": true},
            {"role": "assistant"},
        ]});
        let back = decode(&serde_json::to_vec(&raw).unwrap()).unwrap();
        assert_eq!(back.messages.len(), 2);
    }

    #[test]
    fn missing_tool_results_are_filled_in_so_the_history_can_be_sent() {
        let mut messages = vec![
            Message::User { text: "做".into() },
            Message::Assistant(AssistantMessage {
                tool_calls: vec![
                    ToolCall {
                        id: "a".into(),
                        name: "stat".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "stat".into(),
                        arguments: json!({}),
                    },
                ],
                ..AssistantMessage::default()
            }),
            Message::ToolResult {
                call_id: "a".into(),
                content: "ok".into(),
                is_error: false,
            },
        ];
        assert_eq!(repair_tool_pairs(&mut messages), 1);
        assert_eq!(messages.len(), 4);
        match &messages[3] {
            Message::ToolResult {
                call_id, is_error, ..
            } => assert!(call_id == "b" && *is_error),
            other => panic!("{other:?}"),
        }
        // 已经配齐的不动。
        assert_eq!(repair_tool_pairs(&mut messages), 0);
        // 后面直接跟用户消息时也补。
        let mut messages = vec![
            Message::Assistant(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "z".into(),
                    name: "stat".into(),
                    arguments: json!({}),
                }],
                ..AssistantMessage::default()
            }),
            Message::User {
                text: "下一句".into(),
            },
        ];
        assert_eq!(repair_tool_pairs(&mut messages), 1);
        assert!(matches!(messages[1], Message::ToolResult { .. }));
        assert!(matches!(messages[2], Message::User { .. }));
    }

    #[test]
    fn an_oversized_transcript_drops_the_oldest_turns() {
        let big = "x".repeat(MAX_TRANSCRIPT_BYTES / 3);
        let messages: Vec<Message> = (0..6)
            .flat_map(|i| {
                [
                    Message::User {
                        text: format!("问题 {i} {big}"),
                    },
                    Message::Assistant(AssistantMessage {
                        text: format!("答 {i}"),
                        ..AssistantMessage::default()
                    }),
                ]
            })
            .collect();
        let bytes = encode("s", None, &messages, &[]).unwrap();
        assert!(bytes.len() <= MAX_TRANSCRIPT_BYTES);
        let back = decode(&bytes).unwrap();
        assert!(back.messages.len() < messages.len());
        assert!(
            matches!(back.messages[0], Message::User { .. }),
            "从回合边界开始"
        );
        assert_eq!(back.messages.last(), messages.last(), "最近的留着");
    }
}
