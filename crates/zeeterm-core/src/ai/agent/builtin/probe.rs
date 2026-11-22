//! 接口"测试"（V2-A4）：设置页里还没保存的表单值也能验证地址、Key 和模型能不能用。
//!
//! 两步，互不依赖：
//! 1. 拉 `GET /models`（[`fetch_models`]）：拿到接口的模型清单和它自报的能力；失败不算测试失败
//!    （很多网关不提供这个接口），只是少了下拉候选；
//! 2. 对选中的模型发一个极短的请求（"回复 OK"，输出上限很小、不带工具），**这一步的结果才决定
//!    测试成功与否**：错误按 A2 的分类给出中文原因。
//!
//! 传输层由调用方注入（产品里是按"软件网络"代理建的 `ReqwestTransport`，测试里是桩）。
//! 日志里不出现 Key、提示词和回复正文。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::catalog::{parse_models_endpoint, PartialCaps};
use super::provider::{
    build_models_request, stream_chat, ChatRequest, Message, ProviderConfig, ProviderError,
    ReasoningParam, StreamEvent, Transport, TransportError, Usage,
};

/// 拉模型清单最多等多久。
pub const MODELS_TIMEOUT: Duration = Duration::from_secs(20);
/// 极短请求最多等多久（含建立连接、等第一个字）。
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(60);
/// 极短请求的输出上限：够说一个 OK，又不会让思考型模型白烧 token。
pub const PROBE_MAX_OUTPUT_TOKENS: u32 = 32;
/// 模型清单响应体上限（再大的清单也不会过几 MB）。
const MODELS_BODY_LIMIT: usize = 4 * 1024 * 1024;

/// `GET {base}/models`：成功返回 `(模型 id, 接口自报的能力)`，顺序同接口返回。
///
/// 任何失败（连不上、非 2xx、不是 JSON、没有条目、超时、传输层不支持 GET）都是 `None`，
/// 调用方退回手填的清单。
pub async fn fetch_models(
    transport: &dyn Transport,
    config: &ProviderConfig,
    timeout: Duration,
) -> Option<Vec<(String, PartialCaps)>> {
    let request = build_models_request(config);
    let fetch = async {
        let mut response = transport.get(request).await.ok()?;
        if !(200..300).contains(&response.status) {
            return None;
        }
        let mut body = Vec::new();
        while body.len() < MODELS_BODY_LIMIT {
            match response.body.next_chunk().await {
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(_) => return None,
            }
        }
        let text = String::from_utf8_lossy(&body).into_owned();
        let parsed = parse_models_endpoint(&text);
        (!parsed.is_empty()).then_some(parsed)
    };
    tokio::time::timeout(timeout, fetch).await.ok().flatten()
}

/// 极短请求成功时的信息。
#[derive(Debug, Clone, PartialEq)]
pub struct ReplyProbe {
    /// 从发出请求到流收完的耗时。
    pub elapsed: Duration,
    /// 模型说的话（截短），可能为空（思考型模型把输出额度烧在思考上）。
    pub text: String,
    /// 接口回报的用量（有的话）。
    pub usage: Option<Usage>,
}

/// 一次"测试"的完整结果。
#[derive(Debug, Clone)]
pub struct EndpointProbe {
    /// 接口返回的模型清单（`None` = 没取到）。
    pub models: Option<Vec<(String, PartialCaps)>>,
    /// 极短请求的结果：`Ok` 才算测试成功。
    pub reply: Result<ReplyProbe, ProviderError>,
}

impl EndpointProbe {
    pub fn ok(&self) -> bool {
        self.reply.is_ok()
    }

    /// 接口自报的能力，按模型 id 查。
    pub fn remote_caps(&self) -> HashMap<String, PartialCaps> {
        self.models
            .iter()
            .flatten()
            .map(|(id, caps)| (id.clone(), caps.clone()))
            .collect()
    }

    /// 模型 id 清单（接口顺序）。
    pub fn model_ids(&self) -> Vec<String> {
        self.models
            .iter()
            .flatten()
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// 回复正文最多保留这么多字符。
const REPLY_TEXT_MAX_CHARS: usize = 80;

/// 对 `model` 发一个极短请求。取消：丢掉这个 future。
pub async fn probe_reply(
    transport: &dyn Transport,
    config: &ProviderConfig,
    model: &str,
    timeout: Duration,
) -> Result<ReplyProbe, ProviderError> {
    let request = ChatRequest {
        model: model.to_owned(),
        system: "这是 ZeeTerm 的连通性测试，请只回复 OK。".to_owned(),
        messages: vec![Message::User {
            text: "回复 OK".to_owned(),
        }],
        tools: Vec::new(),
        max_output_tokens: PROBE_MAX_OUTPUT_TOKENS,
        reasoning: ReasoningParam::Unspecified,
        temperature: None,
        cache_hint: false,
    };
    let started = Instant::now();
    let mut text = String::new();
    let mut on_event = |event: StreamEvent| {
        if let StreamEvent::TextDelta { text: delta } = event {
            if text.chars().count() < REPLY_TEXT_MAX_CHARS {
                text.push_str(&delta);
            }
        }
    };
    let run = stream_chat(transport, config, &request, &mut on_event);
    let result = match tokio::time::timeout(timeout, run).await {
        Ok(result) => result,
        Err(_) => {
            return Err(ProviderError::from_transport(&TransportError::Timeout(
                format!("{} 秒内没有完成", timeout.as_secs()),
            )))
        }
    };
    let turn = result?;
    // 接口没回报用量时全是 0：当没有。
    let usage = (turn.usage != Usage::default()).then_some(turn.usage);
    let text: String = text.trim().chars().take(REPLY_TEXT_MAX_CHARS).collect();
    Ok(ReplyProbe {
        elapsed: started.elapsed(),
        text,
        usage,
    })
}

/// 设置页"测试"按钮：先拉模型清单，再对 `model` 发极短请求。
pub async fn test_endpoint(
    transport: &dyn Transport,
    config: &ProviderConfig,
    model: &str,
) -> EndpointProbe {
    let models = fetch_models(transport, config, MODELS_TIMEOUT).await;
    let reply = probe_reply(transport, config, model, REPLY_TIMEOUT).await;
    EndpointProbe { models, reply }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::agent::builtin::provider::testing::{Script, StubTransport};
    use crate::ai::agent::builtin::provider::{ApiKey, ProviderKind};

    fn openai_config() -> ProviderConfig {
        ProviderConfig::new(
            ProviderKind::OpenAiChat,
            "https://api.example.com/v1",
            ApiKey::new("sk-test-probe"),
        )
    }

    fn ok_sse() -> Vec<Vec<u8>> {
        let lines = [
            r#"data: {"id":"1","choices":[{"index":0,"delta":{"role":"assistant","content":"OK"}}]}"#,
            r#"data: {"id":"1","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":2,"total_tokens":14}}"#,
            "data: [DONE]",
        ];
        vec![lines.join("\n\n").into_bytes(), b"\n\n".to_vec()]
    }

    #[tokio::test]
    async fn a_short_reply_reports_text_usage_and_a_tiny_request() {
        let transport = StubTransport::new(Script::sse(ok_sse()));
        let probe = probe_reply(&transport, &openai_config(), "m", REPLY_TIMEOUT)
            .await
            .expect("成功");
        assert_eq!(probe.text, "OK");
        assert_eq!(probe.usage.map(|u| u.output), Some(2));
        let request = transport.last_request().expect("发过请求");
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("json");
        assert_eq!(body["model"], "m");
        // 不带工具；输出上限就是那个很小的值（OpenAI 兼容的字段名由接口选项决定，两个都认）。
        assert!(body
            .get("tools")
            .is_none_or(|tools| tools.as_array().is_none_or(|tools| tools.is_empty())));
        let limit = body["max_completion_tokens"]
            .as_u64()
            .or_else(|| body["max_tokens"].as_u64());
        assert_eq!(limit, Some(PROBE_MAX_OUTPUT_TOKENS as u64));
    }

    #[tokio::test]
    async fn a_rejected_key_comes_back_as_a_classified_error_without_the_key() {
        let transport = StubTransport::new(Script::body(
            401,
            "application/json",
            br#"{"error":{"message":"Incorrect API key provided: sk-test-probe","type":"invalid_request_error","code":"invalid_api_key"}}"#,
        ));
        let probe = test_endpoint(&transport, &openai_config(), "m").await;
        assert!(!probe.ok());
        let err = probe.reply.expect_err("失败");
        assert_eq!(
            err.kind,
            crate::ai::agent::builtin::provider::ErrorKind::Auth
        );
        assert!(!err.to_string().contains("sk-test-probe"));
        // 桩传输层不支持 GET：模型清单取不到不影响判断，只是 None。
        assert!(probe.models.is_none());
    }

    #[tokio::test]
    async fn the_models_list_failing_does_not_fail_the_test() {
        let transport = StubTransport::new(Script::sse(ok_sse()));
        let probe = test_endpoint(&transport, &openai_config(), "m").await;
        assert!(probe.ok());
        assert!(probe.models.is_none());
        assert!(probe.model_ids().is_empty());
    }
}
