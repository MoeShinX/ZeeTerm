//! 对话循环的测试：脚本化的 SSE 回放 + 内存里的假主机 / 假审批，不联网。

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::*;
use crate::ai::agent::builtin::catalog::{CapsOverride, ReasoningSupport};
use crate::ai::agent::builtin::config::{EndpointKind, OpenAiOptionsConfig, Tri};
use crate::ai::agent::builtin::provider::{ByteStream, HttpRequest, HttpResponse, TransportError};
use crate::ai::bridge::{
    ApprovalDecision, ApprovalRequest, Approver, AuditEntry, AuditLog, BridgeHost, ExecDecision,
    ExecMode, ExecModeProvider,
};
use crate::ai::files::testfs::MemFs;
use crate::ai::files::RemoteFs;
use crate::ssh::ExecCapture;

// ---------- 脚本化的传输层 ----------

enum Reply {
    /// 200 + SSE。
    Sse(String),
    /// 非 2xx。
    Status {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    },
    /// 先给一块数据，然后永远不再有数据（用来测停止）。
    Hang(String),
    /// 200 + JSON（`GET /models` 用）。
    Json(String),
    Fail(TransportError),
}

struct Body {
    chunks: VecDeque<Vec<u8>>,
    hang: bool,
}

impl ByteStream for Body {
    fn next_chunk<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<Vec<u8>>, TransportError>> {
        Box::pin(async move {
            if let Some(chunk) = self.chunks.pop_front() {
                return Ok(Some(chunk));
            }
            if self.hang {
                std::future::pending::<()>().await;
            }
            Ok(None)
        })
    }
}

#[derive(Default)]
struct ScriptedTransport {
    posts: Mutex<VecDeque<Reply>>,
    gets: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<HttpRequest>>,
    get_requests: Mutex<Vec<HttpRequest>>,
}

impl ScriptedTransport {
    fn push(&self, reply: Reply) {
        self.posts.lock().unwrap().push_back(reply);
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// 第 n 次请求的请求体（JSON）。
    fn body(&self, n: usize) -> Value {
        let requests = self.requests.lock().unwrap();
        serde_json::from_slice(&requests[n].body).expect("请求体是 JSON")
    }

    fn respond(reply: Reply) -> Result<HttpResponse, TransportError> {
        let (status, headers, chunks, hang) = match reply {
            Reply::Sse(text) => (
                200,
                vec![("content-type".to_owned(), "text/event-stream".to_owned())],
                vec![text.into_bytes()],
                false,
            ),
            Reply::Json(text) => (
                200,
                vec![("content-type".to_owned(), "application/json".to_owned())],
                vec![text.into_bytes()],
                false,
            ),
            Reply::Status {
                status,
                mut headers,
                body,
            } => {
                headers.push(("content-type".to_owned(), "application/json".to_owned()));
                (status, headers, vec![body.into_bytes()], false)
            }
            Reply::Hang(first) => (
                200,
                vec![("content-type".to_owned(), "text/event-stream".to_owned())],
                vec![first.into_bytes()],
                true,
            ),
            Reply::Fail(err) => return Err(err),
        };
        Ok(HttpResponse {
            status,
            headers,
            body: Box::new(Body {
                chunks: chunks.into(),
                hang,
            }),
        })
    }
}

impl Transport for ScriptedTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        self.requests.lock().unwrap().push(request);
        let reply = self.posts.lock().unwrap().pop_front();
        Box::pin(async move {
            match reply {
                Some(reply) => Self::respond(reply),
                None => Err(TransportError::Other("脚本里没有更多回复了".to_owned())),
            }
        })
    }

    fn get<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        self.get_requests.lock().unwrap().push(request);
        let reply = self.gets.lock().unwrap().pop_front();
        Box::pin(async move {
            match reply {
                Some(reply) => Self::respond(reply),
                None => Err(TransportError::Other("没有 GET 脚本".to_owned())),
            }
        })
    }
}

// ---------- SSE 样本 ----------

fn data(value: Value) -> String {
    format!("data: {value}\n\n")
}

/// OpenAI 兼容：纯文本回复。
fn oa_text(text: &str, prompt: u64, completion: u64) -> String {
    let mut out = String::new();
    out.push_str(&data(
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text}}]}),
    ));
    out.push_str(&data(
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ));
    out.push_str(&data(
        json!({"choices":[],"usage":{"prompt_tokens":prompt,"completion_tokens":completion}}),
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

/// OpenAI 兼容：若干工具调用（`(id, name, 参数)`）。
fn oa_calls(text: &str, calls: &[(&str, &str, Value)], prompt: u64, completion: u64) -> String {
    let mut out = String::new();
    if !text.is_empty() {
        out.push_str(&data(
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text}}]}),
        ));
    }
    for (index, (id, name, args)) in calls.iter().enumerate() {
        out.push_str(&data(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":index,"id":id,"type":"function","function":{"name":name,"arguments":""}}]}}]})));
        out.push_str(&data(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":index,"function":{"arguments":args.to_string()}}]}}]})));
    }
    out.push_str(&data(
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ));
    out.push_str(&data(
        json!({"choices":[],"usage":{"prompt_tokens":prompt,"completion_tokens":completion}}),
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

/// OpenAI 兼容：思考 + 工具调用（DeepSeek 风格的 `reasoning_content`）。
fn oa_reasoning_call(reasoning: &str, id: &str, name: &str, args: Value) -> String {
    let mut out = String::new();
    out.push_str(&data(
        json!({"choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":reasoning}}]}),
    ));
    out.push_str(&data(json!({"choices":[{"index":0,"delta":{"tool_calls":[
        {"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]}}]})));
    out.push_str(&data(
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ));
    out.push_str(&data(
        json!({"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":20}}),
    ));
    out.push_str("data: [DONE]\n\n");
    out
}

fn ant_event(name: &str, value: Value) -> String {
    format!("event: {name}\ndata: {value}\n\n")
}

/// Anthropic：思考块（带签名）+ 一个工具调用。
fn ant_thinking_call(thinking: &str, signature: &str, id: &str, name: &str, args: Value) -> String {
    let mut out = String::new();
    out.push_str(&ant_event("message_start", json!({"type":"message_start","message":{"id":"m1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":300,"output_tokens":1}}})));
    out.push_str(&ant_event("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}})));
    out.push_str(&ant_event("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":thinking}})));
    out.push_str(&ant_event("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":signature}})));
    out.push_str(&ant_event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ));
    out.push_str(&ant_event("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}})));
    out.push_str(&ant_event("content_block_delta", json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":args.to_string()}})));
    out.push_str(&ant_event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":1}),
    ));
    out.push_str(&ant_event("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}})));
    out.push_str(&ant_event("message_stop", json!({"type":"message_stop"})));
    out
}

fn ant_text(text: &str) -> String {
    let mut out = String::new();
    out.push_str(&ant_event("message_start", json!({"type":"message_start","message":{"id":"m2","type":"message","role":"assistant","content":[],"usage":{"input_tokens":400,"output_tokens":1}}})));
    out.push_str(&ant_event(
        "content_block_start",
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
    ));
    out.push_str(&ant_event(
        "content_block_delta",
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
    ));
    out.push_str(&ant_event(
        "content_block_stop",
        json!({"type":"content_block_stop","index":0}),
    ));
    out.push_str(&ant_event("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}})));
    out.push_str(&ant_event("message_stop", json!({"type":"message_stop"})));
    out
}

// ---------- 假主机 / 假审批 / 假审计 ----------

struct FakeHost {
    exec_calls: Mutex<Vec<String>>,
    stdout: Mutex<String>,
    /// 执行永远不返回（测停止时工具卡在执行中）。
    exec_hangs: Mutex<bool>,
    fs: Arc<MemFs>,
}

impl FakeHost {
    fn new() -> Arc<FakeHost> {
        let fs = Arc::new(MemFs::new());
        fs.put_dir("/tmp");
        fs.put("/tmp/a.txt", b"alpha", 0o644);
        fs.put("/tmp/b.log", b"beta", 0o644);
        Arc::new(FakeHost {
            exec_calls: Mutex::new(Vec::new()),
            stdout: Mutex::new("hello\n".to_owned()),
            exec_hangs: Mutex::new(false),
            fs,
        })
    }

    fn commands(&self) -> Vec<String> {
        self.exec_calls.lock().unwrap().clone()
    }
}

impl BridgeHost for FakeHost {
    fn exec<'a>(
        &'a self,
        _session_id: &'a str,
        command: &'a str,
        _wait: Duration,
        _max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        self.exec_calls.lock().unwrap().push(command.to_owned());
        let stdout = self.stdout.lock().unwrap().clone();
        let hang = *self.exec_hangs.lock().unwrap();
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            Ok(ExecCapture {
                stdout,
                stderr: String::new(),
                stdout_truncated_bytes: 0,
                stderr_truncated_bytes: 0,
                exit_code: Some(0),
                timed_out: false,
                cancelled: false,
                duration_ms: 3,
            })
        })
    }

    fn read_terminal<'a>(
        &'a self,
        _session_id: &'a str,
        _max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async { Ok(vec!["$ ls".to_owned(), "file".to_owned()]) })
    }

    fn remote_fs<'a>(
        &'a self,
        _target_id: &'a str,
    ) -> BoxFuture<'a, Result<Arc<dyn RemoteFs>, String>> {
        let fs: Arc<dyn RemoteFs> = Arc::clone(&self.fs) as Arc<dyn RemoteFs>;
        Box::pin(async move { Ok(fs) })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Answer {
    Allow,
    Deny,
    Never,
}

struct FakeApprover {
    answer: Mutex<Answer>,
    asked: Mutex<Vec<String>>,
}

impl FakeApprover {
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

impl Approver for FakeApprover {
    fn approve<'a>(&'a self, request: ApprovalRequest) -> BoxFuture<'a, ApprovalDecision> {
        self.asked.lock().unwrap().push(request.command.clone());
        let answer = *self.answer.lock().unwrap();
        Box::pin(async move {
            match answer {
                Answer::Allow => ApprovalDecision::Allowed,
                Answer::Deny => ApprovalDecision::Denied,
                Answer::Never => std::future::pending().await,
            }
        })
    }
}

struct Mode(Mutex<ExecMode>);

impl ExecModeProvider for Mode {
    fn mode(&self) -> ExecMode {
        *self.0.lock().unwrap()
    }
}

#[derive(Default)]
struct Audit(Mutex<Vec<AuditEntry>>);

impl AuditLog for Audit {
    fn record(&self, entry: AuditEntry) {
        self.0.lock().unwrap().push(entry);
    }
}

// ---------- 测试台 ----------

const KEY: &str = "sk-test-secret-key-0001";

struct Rig {
    conv: BuiltinConversation,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    signals: mpsc::UnboundedReceiver<BuiltinSignal>,
    transport: Arc<ScriptedTransport>,
    host: Arc<FakeHost>,
    approver: Arc<FakeApprover>,
    audit: Arc<Audit>,
    mode: Arc<Mode>,
    bridge: Arc<Bridge>,
    _dir: Option<crate::test_util::TestDir>,
}

struct RigBuilder {
    kind: EndpointKind,
    model: &'static str,
    caps: CapsOverride,
    mode: ExecMode,
    answer: Answer,
    tuning: BuiltinTuning,
    with_dir: bool,
    resume: Option<String>,
    dir: Option<crate::test_util::TestDir>,
    openai_options: OpenAiOptionsConfig,
}

fn fast_tuning() -> BuiltinTuning {
    BuiltinTuning {
        retry_base_delay: Duration::from_millis(5),
        stop_grace: Duration::from_millis(300),
        models_timeout: Duration::from_secs(5),
        ..BuiltinTuning::default()
    }
}

impl RigBuilder {
    fn new() -> Self {
        RigBuilder {
            kind: EndpointKind::OpenAiChat,
            model: "test-model",
            caps: CapsOverride {
                context_window: Some(100_000),
                max_output: Some(8_000),
                tool_call: Some(true),
                reasoning: Some(ReasoningSupport::None),
                ..CapsOverride::default()
            },
            mode: ExecMode::Auto,
            answer: Answer::Allow,
            tuning: fast_tuning(),
            with_dir: false,
            resume: None,
            dir: None,
            openai_options: OpenAiOptionsConfig::default(),
        }
    }

    fn kind(mut self, kind: EndpointKind) -> Self {
        self.kind = kind;
        self
    }

    fn model(mut self, model: &'static str) -> Self {
        self.model = model;
        self
    }

    fn caps(mut self, edit: impl FnOnce(&mut CapsOverride)) -> Self {
        edit(&mut self.caps);
        self
    }

    fn mode(mut self, mode: ExecMode, answer: Answer) -> Self {
        self.mode = mode;
        self.answer = answer;
        self
    }

    fn tuning(mut self, edit: impl FnOnce(&mut BuiltinTuning)) -> Self {
        edit(&mut self.tuning);
        self
    }

    fn with_history(mut self) -> Self {
        self.with_dir = true;
        self
    }

    fn resume_from(mut self, dir: crate::test_util::TestDir, id: &str) -> Self {
        self.with_dir = true;
        self.dir = Some(dir);
        self.resume = Some(id.to_owned());
        self
    }

    fn build(self) -> Rig {
        let transport = Arc::new(ScriptedTransport::default());
        let host = FakeHost::new();
        let approver = Arc::new(FakeApprover {
            answer: Mutex::new(self.answer),
            asked: Mutex::new(Vec::new()),
        });
        let audit = Arc::new(Audit::default());
        let mode = Arc::new(Mode(Mutex::new(self.mode)));
        let bridge = Arc::new(Bridge::new(
            host.clone(),
            audit.clone(),
            approver.clone(),
            mode.clone(),
        ));
        let scope = bridge.create_scope();
        scope.add_target("s1", "web-1", "root", "connected", true, false);
        let endpoint = AiEndpoint {
            id: 1,
            uid: "ep-1".to_owned(),
            name: "测试接口".to_owned(),
            kind: self.kind,
            base_url: "https://api.example.test/v1".to_owned(),
            key_identity_id: None,
            has_key: true,
            default_model: Some(self.model.to_owned()),
            models: vec!["extra-model".to_owned()],
            overrides: BTreeMap::from([(self.model.to_owned(), self.caps)]),
            openai_options: self.openai_options,
            preset_id: None,
            created_at: 1,
            updated_at: 1,
        };
        let (events_tx, events) = mpsc::unbounded_channel();
        let (signals_tx, signals) = mpsc::unbounded_channel();
        let dir = if self.with_dir && self.dir.is_none() {
            Some(crate::test_util::TestDir::new("builtin-conv"))
        } else {
            self.dir
        };
        let conv = BuiltinConversation::new(BuiltinParams {
            endpoint,
            api_key: ApiKey::new(KEY),
            transport: transport.clone(),
            catalog: Arc::new(Catalog::empty()),
            bridge: bridge.clone(),
            scope,
            events: events_tx,
            signals: Some(signals_tx),
            data_dir: dir.as_ref().map(|d| d.to_path_buf()),
            resume_id: self.resume,
            tuning: self.tuning,
        });
        Rig {
            conv,
            events,
            signals,
            transport,
            host,
            approver,
            audit,
            mode,
            bridge,
            _dir: dir,
        }
    }
}

impl Rig {
    /// 发一条消息，收齐事件直到 `TurnEnd`。
    async fn say(&mut self, text: &str) -> Vec<AgentEvent> {
        self.say_with(text, None).await
    }

    async fn say_with(&mut self, text: &str, model: Option<&str>) -> Vec<AgentEvent> {
        self.conv.send(text, model).await.expect("发送");
        self.until_turn_end().await
    }

    async fn until_turn_end(&mut self) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(10), self.events.recv())
                .await
                .expect("等事件超时")
                .expect("事件通道断了");
            let end = event == AgentEvent::TurnEnd;
            out.push(event);
            if end {
                return out;
            }
        }
    }

    fn signals(&mut self) -> Vec<BuiltinSignal> {
        let mut out = Vec::new();
        while let Ok(signal) = self.signals.try_recv() {
            out.push(signal);
        }
        out
    }

    fn notices(signals: &[BuiltinSignal]) -> Vec<NoticeKind> {
        signals
            .iter()
            .filter_map(|s| match s {
                BuiltinSignal::Notice(n) => Some(n.kind),
                _ => None,
            })
            .collect()
    }
}

fn text_of(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn error_of(events: &[AgentEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        AgentEvent::Error { message } => Some(message.clone()),
        _ => None,
    })
}

fn tool_ends(events: &[AgentEvent]) -> Vec<(String, String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolEnd {
                call_id,
                output,
                error,
                ..
            } => Some((call_id.clone(), output.clone(), *error)),
            _ => None,
        })
        .collect()
}

fn messages_of(body: &Value) -> Vec<Value> {
    body["messages"].as_array().cloned().unwrap_or_default()
}

// ---------- 测试 ----------

#[tokio::test]
async fn a_plain_text_turn_streams_text_and_ends() {
    let mut rig = RigBuilder::new().build();
    rig.transport
        .push(Reply::Sse(oa_text("你好，我是助手。", 120, 9)));
    let events = rig.say("你好").await;
    assert_eq!(text_of(&events), "你好，我是助手。");
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnd));
    assert!(error_of(&events).is_none());
    assert_eq!(rig.transport.request_count(), 1);

    // 请求：系统提示最前、带工具清单、输出上限取模型上限、默认不带思考参数。
    let body = rig.transport.body(0);
    let messages = messages_of(&body);
    assert_eq!(messages[0]["role"], "system");
    assert!(messages[0]["content"]
        .as_str()
        .unwrap()
        .contains("服务器运维助手"));
    assert_eq!(messages[1], json!({"role":"user","content":"你好"}));
    assert_eq!(body["tools"].as_array().unwrap().len(), 8);
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["max_tokens"], 8000);
    assert!(body.get("reasoning_effort").is_none() && body.get("thinking").is_none());
    // 用量走旁路。
    let signals = rig.signals();
    let usage = signals
        .iter()
        .find_map(|s| match s {
            BuiltinSignal::Usage(u) => Some(u.clone()),
            _ => None,
        })
        .expect("有用量信号");
    assert_eq!((usage.used, usage.output, usage.window), (120, 9, 100_000));
    assert!(!usage.window_estimated, "窗口是手动填的");
    // 历史里有这一问一答。
    assert_eq!(rig.conv.message_count(), 2);
    assert!(!rig.conv.is_running());
}

#[tokio::test]
async fn one_tool_call_runs_through_the_bridge_and_the_result_goes_back() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Sse(oa_calls(
        "我看一下。",
        &[("call_1", "run_command", json!({"command": "uptime"}))],
        200,
        30,
    )));
    rig.transport
        .push(Reply::Sse(oa_text("系统已运行很久。", 300, 12)));
    let events = rig.say("机器怎么样").await;

    // 事件顺序：文字 → 工具开始 → 工具结束 → 文字 → 结束。
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match e {
            AgentEvent::TextDelta { .. } => "text",
            AgentEvent::ToolStart { .. } => "start",
            AgentEvent::ToolEnd { .. } => "end",
            AgentEvent::TurnEnd => "turn_end",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["text", "start", "end", "text", "turn_end"]);
    match &events[1] {
        AgentEvent::ToolStart {
            call_id,
            tool,
            input,
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(tool, "run_command");
            assert_eq!(input["command"], "uptime");
        }
        other => panic!("{other:?}"),
    }
    let ends = tool_ends(&events);
    assert_eq!(ends.len(), 1);
    assert!(!ends[0].2, "{}", ends[0].1);
    assert!(ends[0].1.contains("hello"), "{}", ends[0].1);
    assert!(
        ends[0].1.contains("[zssh exec "),
        "界面靠这行对回执行位置：{}",
        ends[0].1
    );
    assert_eq!(rig.host.commands(), ["uptime"]);

    // 第二次请求带着 assistant 的工具调用和 tool 结果。
    let second = messages_of(&rig.transport.body(1));
    let assistant = second.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    let tool = second.iter().find(|m| m["role"] == "tool").unwrap();
    assert_eq!(tool["tool_call_id"], "call_1");
    assert!(tool["content"].as_str().unwrap().contains("exit_code: 0"));
    // 审计里有这条命令（走的是真工具桥）。
    let audit = rig.audit.0.lock().unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].command, "uptime");
    assert_eq!(audit[0].decision, ExecDecision::Auto);
}

#[tokio::test]
async fn parallel_tool_calls_all_run_in_order_and_all_come_back() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[
            ("c1", "run_command", json!({"command": "df -h"})),
            ("c2", "list_dir", json!({"path": "/tmp"})),
        ],
        100,
        20,
    )));
    rig.transport
        .push(Reply::Sse(oa_text("都看完了。", 200, 5)));
    let events = rig.say("查磁盘和 /tmp").await;
    let starts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolStart { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(starts, ["c1", "c2"]);
    let ends = tool_ends(&events);
    assert_eq!(
        ends.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(),
        ["c1", "c2"]
    );
    assert!(ends.iter().all(|e| !e.2), "{ends:?}");
    assert!(
        ends[1].1.contains("a.txt"),
        "list_dir 走了假文件系统：{}",
        ends[1].1
    );
    let second = messages_of(&rig.transport.body(1));
    let tool_ids: Vec<&str> = second
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["tool_call_id"].as_str().unwrap())
        .collect();
    assert_eq!(tool_ids, ["c1", "c2"]);
}

#[tokio::test]
async fn a_user_denial_is_reported_to_the_model_and_nothing_runs() {
    let mut rig = RigBuilder::new()
        .mode(ExecMode::Manual, Answer::Deny)
        .build();
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[(
            "c1",
            "run_command",
            json!({"command": "rm -rf /var/log/old"}),
        )],
        100,
        20,
    )));
    rig.transport
        .push(Reply::Sse(oa_text("好的，我不删了。", 150, 8)));
    let events = rig.say("清理日志").await;
    let ends = tool_ends(&events);
    assert!(ends[0].2, "被拒绝是失败结果");
    assert!(ends[0].1.contains("用户拒绝"), "{}", ends[0].1);
    assert!(rig.host.commands().is_empty(), "拒绝后不能执行");
    assert_eq!(rig.approver.asked(), ["rm -rf /var/log/old"]);
    // 模型看到的是拒绝原因，之后正常回答。
    let second = messages_of(&rig.transport.body(1));
    let tool = second.iter().find(|m| m["role"] == "tool").unwrap();
    assert!(tool["content"].as_str().unwrap().contains("用户拒绝"));
    assert_eq!(text_of(&events), "好的，我不删了。");
    assert_eq!(
        rig.audit.0.lock().unwrap()[0].decision,
        ExecDecision::Denied
    );
}

#[tokio::test]
async fn manual_mode_asks_before_every_command_and_runs_after_approval() {
    let mut rig = RigBuilder::new()
        .mode(ExecMode::Manual, Answer::Allow)
        .build();
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[("c1", "run_command", json!({"command": "ls"}))],
        100,
        20,
    )));
    rig.transport.push(Reply::Sse(oa_text("好了。", 150, 8)));
    let events = rig.say("看看目录").await;
    assert_eq!(rig.approver.asked(), ["ls"], "手动模式每条都问");
    assert!(!tool_ends(&events)[0].2);
    assert_eq!(rig.host.commands(), ["ls"]);
    assert_eq!(
        rig.audit.0.lock().unwrap()[0].decision,
        ExecDecision::Allowed
    );
}

#[tokio::test]
async fn read_only_mode_behaves_like_it_does_for_external_agents() {
    let mut rig = RigBuilder::new()
        .mode(ExecMode::ReadOnly, Answer::Deny)
        .build();
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[
            ("c1", "run_command", json!({"command": "ls -la"})),
            ("c2", "run_command", json!({"command": "touch /tmp/x"})),
        ],
        100,
        20,
    )));
    rig.transport
        .push(Reply::Sse(oa_text("读到了，写被拒了。", 150, 8)));
    let events = rig.say("看看再建个文件").await;
    let ends = tool_ends(&events);
    assert!(!ends[0].2, "只读命令自动放行：{}", ends[0].1);
    assert!(ends[1].2 && ends[1].1.contains("用户拒绝"), "{}", ends[1].1);
    assert_eq!(rig.approver.asked(), ["touch /tmp/x"], "只有写命令去问");
    assert_eq!(rig.host.commands(), ["ls -la"]);
    // 写类工具照样暴露给模型（由桥决定要不要问），和外部智能体一致。
    let names: Vec<String> = rig.transport.body(0)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(names.contains(&"write_file".to_owned()));
}

#[tokio::test]
async fn the_mode_is_read_fresh_so_switching_to_manual_takes_effect_mid_conversation() {
    let mut rig = RigBuilder::new().mode(ExecMode::Auto, Answer::Deny).build();
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[("c1", "run_command", json!({"command": "a1"}))],
        10,
        5,
    )));
    rig.transport.push(Reply::Sse(oa_text("ok", 20, 5)));
    rig.say("一").await;
    assert_eq!(rig.host.commands(), ["a1"]);
    *rig.mode.0.lock().unwrap() = ExecMode::Manual;
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[("c2", "run_command", json!({"command": "a2"}))],
        10,
        5,
    )));
    rig.transport.push(Reply::Sse(oa_text("ok", 20, 5)));
    let events = rig.say("二").await;
    assert!(tool_ends(&events)[0].2, "改成手动之后要问，这里被拒");
    assert_eq!(rig.host.commands(), ["a1"]);
}

#[tokio::test]
async fn a_model_without_tool_support_gets_no_tools_and_a_notice() {
    let mut rig = RigBuilder::new()
        .caps(|c| c.tool_call = Some(false))
        .build();
    rig.transport
        .push(Reply::Sse(oa_text("我只能聊天。", 50, 6)));
    let events = rig.say("帮我重启 nginx").await;
    let body = rig.transport.body(0);
    assert!(body.get("tools").is_none(), "不发工具清单");
    let system = messages_of(&body)[0]["content"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(system.contains("只能聊天"), "{system}");
    assert_eq!(text_of(&events), "我只能聊天。");
    assert_eq!(Rig::notices(&rig.signals()), [NoticeKind::NoToolSupport]);
}

#[tokio::test]
async fn anthropic_thinking_signature_is_sent_back_with_the_tool_result() {
    let mut rig = RigBuilder::new()
        .kind(EndpointKind::Anthropic)
        .model("claude-test")
        .caps(|c| c.reasoning = Some(ReasoningSupport::Levels(vec!["low".into(), "high".into()])))
        .build();
    rig.transport.push(Reply::Sse(ant_thinking_call(
        "先看磁盘",
        "SIG-123",
        "toolu_1",
        "run_command",
        json!({"command": "df -h"}),
    )));
    rig.transport.push(Reply::Sse(ant_text("根分区还剩很多。")));
    let events = rig.say_with("磁盘", Some("claude-test/high")).await;
    assert_eq!(text_of(&events), "根分区还剩很多。");

    // 第一次请求：高档思考 → adaptive + effort。
    let first = rig.transport.body(0);
    assert_eq!(first["thinking"]["type"], "adaptive");
    assert_eq!(first["output_config"]["effort"], "high");
    // 第二次请求：assistant 的思考块带着原来的签名，tool_use / tool_result 配对。
    let second = rig.transport.body(1);
    let history = second["messages"].as_array().unwrap();
    let assistant = history.iter().find(|m| m["role"] == "assistant").unwrap();
    let blocks = assistant["content"].as_array().unwrap();
    assert_eq!(blocks[0]["type"], "thinking");
    assert_eq!(blocks[0]["signature"], "SIG-123");
    assert_eq!(blocks[0]["thinking"], "先看磁盘");
    assert!(blocks
        .iter()
        .any(|b| b["type"] == "tool_use" && b["id"] == "toolu_1"));
    let user = history.last().unwrap();
    assert!(user["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|b| b["type"] == "tool_result" && b["tool_use_id"] == "toolu_1"));
}

#[tokio::test]
async fn deepseek_reasoning_content_is_echoed_back_during_tool_calls() {
    let mut rig = RigBuilder::new()
        .model("deepseek-v4-flash")
        .caps(|c| c.reasoning = Some(ReasoningSupport::Levels(vec!["high".into()])))
        .build();
    rig.transport.push(Reply::Sse(oa_reasoning_call(
        "先 df",
        "c1",
        "run_command",
        json!({"command": "df"}),
    )));
    rig.transport.push(Reply::Sse(oa_text("好了", 80, 4)));
    let events = rig.say("磁盘").await;
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ReasoningDelta { text } if text == "先 df")));
    let second = messages_of(&rig.transport.body(1));
    let assistant = second.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(
        assistant["reasoning_content"], "先 df",
        "DeepSeek 工具调用回合必须回传思考"
    );
}

#[tokio::test]
async fn thinking_choice_and_output_limit_follow_the_capabilities() {
    let mut rig = RigBuilder::new()
        .caps(|c| {
            c.reasoning = Some(ReasoningSupport::Levels(vec![
                "low".into(),
                "medium".into(),
                "high".into(),
            ]));
            c.context_window = Some(20_000);
            c.max_output = Some(30_000);
        })
        .build();
    rig.transport.push(Reply::Sse(oa_text("a", 10, 1)));
    rig.say_with("hi", Some("test-model/medium")).await;
    let body = rig.transport.body(0);
    assert_eq!(body["reasoning_effort"], "medium");
    // max_output 被夹到窗口以内（A1 保证 ≤ 窗口），再与剩余预算取小。
    let limit = body["max_tokens"].as_u64().unwrap();
    assert!(limit <= 20_000 && limit > 10_000, "{limit}");
    // 没选就一个思考参数都不发。
    rig.transport.push(Reply::Sse(oa_text("b", 10, 1)));
    rig.say("again").await;
    assert!(rig.transport.body(1).get("reasoning_effort").is_none());
}

#[tokio::test]
async fn a_429_is_retried_with_backoff_and_then_succeeds() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Status {
        status: 429,
        headers: vec![],
        body: r#"{"error":{"message":"slow down","type":"rate_limit"}}"#.to_owned(),
    });
    rig.transport.push(Reply::Sse(oa_text("终于好了", 30, 4)));
    let events = rig.say("hi").await;
    let retries: Vec<(u32, String, Option<u64>)> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Retrying {
                attempt,
                reason,
                next_in_secs,
            } => Some((*attempt, reason.clone(), *next_in_secs)),
            _ => None,
        })
        .collect();
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].0, 1);
    assert!(retries[0].1.contains("限流"), "{}", retries[0].1);
    assert_eq!(text_of(&events), "终于好了");
    assert!(error_of(&events).is_none());
    assert_eq!(rig.transport.request_count(), 2);
}

#[tokio::test]
async fn three_failed_retries_give_up_with_the_shared_message() {
    let mut rig = RigBuilder::new().build();
    for _ in 0..4 {
        rig.transport.push(Reply::Status {
            status: 429,
            headers: vec![("retry-after".to_owned(), "0".to_owned())],
            body: r#"{"error":{"message":"slow down"}}"#.to_owned(),
        });
    }
    let events = rig.say("hi").await;
    let attempts: Vec<u32> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::Retrying { attempt, .. } => Some(*attempt),
            _ => None,
        })
        .collect();
    assert_eq!(attempts, [1, 2, 3], "最多重试 3 次");
    assert_eq!(rig.transport.request_count(), 4);
    let message = error_of(&events).expect("最后给错误");
    assert!(message.starts_with("重试了 3 次仍然失败："), "{message}");
    assert!(message.contains("限流"));
    // 失败的这一回合没有留下孤零零的用户消息。
    assert_eq!(rig.conv.message_count(), 0);
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnd));
}

#[test]
fn retry_waits_follow_the_exponential_schedule_and_honor_retry_after() {
    // 默认节奏 1s、2s、4s；对方给了 retry-after 就听它的，但有上限。
    let tuning = BuiltinTuning::default();
    let secs = |attempt, after: Option<u64>| {
        tuning
            .retry_delay(attempt, after.map(Duration::from_secs))
            .as_secs()
    };
    assert_eq!([secs(0, None), secs(1, None), secs(2, None)], [1, 2, 4]);
    assert_eq!(secs(0, Some(7)), 7);
    assert_eq!(secs(2, Some(3600)), 120, "对方要等一小时也最多等 120 秒");
    assert_eq!(secs(0, Some(0)), 0);
}

#[tokio::test]
async fn an_auth_error_is_not_retried_and_says_why() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Status {
        status: 401,
        headers: vec![],
        body: r#"{"error":{"message":"Incorrect API key","type":"invalid_request_error","code":"invalid_api_key"}}"#.to_owned(),
    });
    let events = rig.say("hi").await;
    assert_eq!(rig.transport.request_count(), 1, "401 不重试");
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::Retrying { .. })));
    let message = error_of(&events).unwrap();
    assert!(message.contains("API Key 不对"), "{message}");
    let failure = rig
        .signals()
        .into_iter()
        .find_map(|s| match s {
            BuiltinSignal::Failure(f) => Some(f),
            _ => None,
        })
        .unwrap();
    assert_eq!(failure.kind, ErrorKind::Auth);
    assert_eq!(failure.status, Some(401));
}

#[tokio::test]
async fn the_tokenway_topup_code_passes_through_verbatim() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Status {
        status: 402,
        headers: vec![],
        body: r#"{"error":{"code":"zeeterm_requires_topup","message":"ZeeTerm 内置助手需要充值后使用"}}"#
            .to_owned(),
    });
    let events = rig.say("hi").await;
    assert_eq!(rig.transport.request_count(), 1);
    let message = error_of(&events).unwrap();
    assert!(
        message.contains("zeeterm_requires_topup"),
        "错误码原样带出：{message}"
    );
    assert!(message.contains("充值"));
    let failure = rig
        .signals()
        .into_iter()
        .find_map(|s| match s {
            BuiltinSignal::Failure(f) => Some(f),
            _ => None,
        })
        .unwrap();
    assert_eq!(failure.code.as_deref(), Some("zeeterm_requires_topup"));
    assert_eq!(failure.kind, ErrorKind::Quota);
}

#[tokio::test]
async fn stop_drops_the_stream_and_ends_the_turn() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Hang(data(
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"写到一半"}}]}),
    )));
    rig.conv.send("讲个长故事", None).await.unwrap();
    // 等第一块文字出来。
    let first = tokio::time::timeout(Duration::from_secs(5), rig.events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first,
        AgentEvent::TextDelta {
            text: "写到一半".into()
        }
    );
    rig.conv.stop().await.unwrap();
    let rest = rig.until_turn_end().await;
    assert_eq!(rest, [AgentEvent::TurnEnd], "停止以 TurnEnd 结束，不报错");
    assert!(!rig.conv.is_running());
    // 历史里留着写了一半的内容和一句说明，下一问还能接着聊。
    rig.transport.push(Reply::Sse(oa_text("继续", 50, 2)));
    rig.say("然后呢").await;
    let messages = messages_of(&rig.transport.body(1));
    let stopped = messages.iter().find(|m| {
        m["role"] == "assistant"
            && m["content"]
                .as_str()
                .is_some_and(|c| c.contains("写到一半"))
    });
    assert!(stopped.unwrap()["content"]
        .as_str()
        .unwrap()
        .contains("用户停止"));
    // 没有回合在跑时 stop 是空操作。
    rig.conv.stop().await.unwrap();
}

#[tokio::test]
async fn stop_while_waiting_for_approval_denies_and_never_executes() {
    let mut rig = RigBuilder::new()
        .mode(ExecMode::Manual, Answer::Never)
        .build();
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[
            ("c1", "run_command", json!({"command": "sleep 100"})),
            ("c2", "run_command", json!({"command": "echo second"})),
        ],
        10,
        5,
    )));
    rig.conv.send("跑个慢命令", None).await.unwrap();
    // 等到审批被问到。
    for _ in 0..200 {
        if !rig.approver.asked().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rig.approver.asked(), ["sleep 100"]);
    rig.conv.stop().await.unwrap();
    let events = rig.until_turn_end().await;
    let ends = tool_ends(&events);
    assert_eq!(ends.len(), 2, "两张卡都收尾：{ends:?}");
    assert!(ends.iter().all(|e| e.2));
    assert!(ends[0].1.contains("停止"), "{}", ends[0].1);
    assert!(rig.host.commands().is_empty(), "停止后什么都不能执行");
    assert_eq!(rig.transport.request_count(), 1, "不再继续发请求");
    assert!(error_of(&events).is_none());
    // 历史仍然合法：每个工具调用都有结果，可以接着问。
    rig.transport.push(Reply::Sse(oa_text("好", 20, 1)));
    rig.say("算了").await;
    let ids: Vec<String> = messages_of(&rig.transport.body(1))
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["tool_call_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, ["c1", "c2"]);
}

#[tokio::test]
async fn stop_while_a_command_is_executing_gives_up_waiting_after_the_grace() {
    let mut rig = RigBuilder::new().build();
    *rig.host.exec_hangs.lock().unwrap() = true;
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[("c1", "run_command", json!({"command": "tail -f x"}))],
        10,
        5,
    )));
    rig.conv.send("盯着日志", None).await.unwrap();
    for _ in 0..200 {
        if !rig.host.commands().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rig.host.commands(), ["tail -f x"]);
    rig.conv.stop().await.unwrap();
    let events = rig.until_turn_end().await;
    let ends = tool_ends(&events);
    assert!(ends[0].2 && ends[0].1.contains("停止"), "{ends:?}");
    assert!(!rig.conv.is_running());
}

#[tokio::test]
async fn a_second_message_while_a_turn_runs_is_refused_as_busy() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Hang(String::new()));
    rig.conv.send("一", None).await.unwrap();
    let err = rig.conv.send("二", None).await.unwrap_err();
    assert!(matches!(err, AgentError::Busy));
    rig.conv.stop().await.unwrap();
    rig.until_turn_end().await;
    assert!(rig.conv.send("   ", None).await.is_err(), "空消息不发");
}

#[tokio::test]
async fn at_most_fifty_tool_calls_per_turn_then_it_stops_and_says_so() {
    let mut rig = RigBuilder::new().build();
    // 一个回复里并行发 52 个调用：前 50 个执行，后面的不执行，回合停下。
    let calls: Vec<(String, String, Value)> = (0..52)
        .map(|i| (format!("c{i}"), "stat".to_owned(), json!({"path": "/tmp"})))
        .collect();
    let borrowed: Vec<(&str, &str, Value)> = calls
        .iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.clone()))
        .collect();
    rig.transport
        .push(Reply::Sse(oa_calls("", &borrowed, 10, 5)));
    let events = rig.say("全查一遍").await;
    let ends = tool_ends(&events);
    assert_eq!(ends.len(), 52);
    assert_eq!(ends.iter().filter(|e| !e.2).count(), 50);
    assert!(ends[50].1.contains("达到上限") && ends[51].1.contains("达到上限"));
    let message = error_of(&events).expect("停下并提示");
    assert!(message.contains("50 次"), "{message}");
    assert_eq!(rig.transport.request_count(), 1, "不再把结果发回去继续");
    // 历史仍然合法，下一条消息可以继续。
    rig.transport.push(Reply::Sse(oa_text("继续", 10, 1)));
    let events = rig.say("接着").await;
    assert!(error_of(&events).is_none());
    let tool_messages = messages_of(&rig.transport.body(1))
        .iter()
        .filter(|m| m["role"] == "tool")
        .count();
    assert_eq!(tool_messages, 52);
}

#[tokio::test]
async fn the_tool_call_budget_counts_across_rounds_of_one_turn() {
    let mut rig = RigBuilder::new().tuning(|t| t.max_tool_calls = 3).build();
    for i in 0..3 {
        rig.transport.push(Reply::Sse(oa_calls(
            "",
            &[(&format!("c{i}"), "stat", json!({"path": "/tmp"}))],
            10,
            5,
        )));
    }
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[("c3", "stat", json!({"path": "/tmp"}))],
        10,
        5,
    )));
    let events = rig.say("一直查").await;
    assert_eq!(rig.transport.request_count(), 4);
    let ends = tool_ends(&events);
    assert_eq!(ends.len(), 4);
    assert!(ends[3].2 && ends[3].1.contains("达到上限"));
    assert!(error_of(&events).unwrap().contains("3 次"));
}

/// 2026-11-11 审查：计数只在"参数合法且工具存在"的分支里加，模型反复发不存在的工具 /
/// 被截断的调用时永远碰不到上限，一直循环烧 token。所有调用都要计数。
#[tokio::test]
async fn unknown_tools_and_bad_arguments_also_count_toward_the_call_budget() {
    let mut rig = RigBuilder::new().tuning(|t| t.max_tool_calls = 3).build();
    for i in 0..4 {
        rig.transport.push(Reply::Sse(oa_calls(
            "",
            &[(&format!("c{i}"), "format_disk", json!({}))],
            10,
            5,
        )));
    }
    let events = rig.say("一直瞎调").await;
    assert_eq!(rig.transport.request_count(), 4, "第 4 次调用碰到上限就停");
    let ends = tool_ends(&events);
    assert_eq!(ends.len(), 4);
    assert!(ends[..3].iter().all(|e| e.1.contains("没有这个工具")));
    assert!(ends[3].2 && ends[3].1.contains("达到上限"), "{}", ends[3].1);
    assert!(error_of(&events).expect("要停下").contains("3 次"));
    assert!(rig.host.commands().is_empty());
}

#[tokio::test]
async fn bad_arguments_and_unknown_tools_become_error_results_not_bridge_calls() {
    let mut rig = RigBuilder::new().build();
    // 参数被截断（不是合法 JSON）+ 不存在的工具。
    let truncated = format!(
        "{}{}",
        data(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":0,"id":"c1","type":"function","function":{"name":"run_command","arguments":"{\"command\": \"ls"}}]}}]})),
        data(json!({"choices":[{"index":0,"delta":{"tool_calls":[
            {"index":1,"id":"c2","type":"function","function":{"name":"format_disk","arguments":"{}"}}]}}]})),
    ) + &data(json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}))
        + "data: [DONE]\n\n";
    rig.transport.push(Reply::Sse(truncated));
    rig.transport.push(Reply::Sse(oa_text("明白", 30, 2)));
    let events = rig.say("x").await;
    let ends = tool_ends(&events);
    assert!(
        ends[0].2 && ends[0].1.contains("不是合法的 JSON"),
        "{}",
        ends[0].1
    );
    assert!(
        ends[1].2 && ends[1].1.contains("没有这个工具"),
        "{}",
        ends[1].1
    );
    assert!(rig.host.commands().is_empty());
    assert!(rig.audit.0.lock().unwrap().is_empty());
    // 第二次请求里 assistant 的无效参数被换成 {}，请求仍然合法。
    let second = messages_of(&rig.transport.body(1));
    let assistant = second.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["tool_calls"][0]["function"]["arguments"], "{}");
}

#[tokio::test]
async fn very_long_tool_output_is_cut_before_it_reaches_the_model() {
    let mut rig = RigBuilder::new().build();
    *rig.host.stdout.lock().unwrap() = "x".repeat(300_000);
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[("c1", "run_command", json!({"command": "cat big"}))],
        10,
        5,
    )));
    rig.transport.push(Reply::Sse(oa_text("太长了", 30, 2)));
    let events = rig.say("读大文件").await;
    let output = &tool_ends(&events)[0].1;
    assert!(output.len() < 70 * 1024, "{}", output.len());
    assert!(output.contains("结果太长"), "要注明省略了");
    assert!(output.ends_with(|c: char| c.is_ascii_alphanumeric() || c == ']'));
    let sent = messages_of(&rig.transport.body(1));
    let tool = sent.iter().find(|m| m["role"] == "tool").unwrap();
    assert!(tool["content"].as_str().unwrap().len() < 70 * 1024);
    assert!(
        tool["content"].as_str().unwrap().contains("[zssh exec "),
        "尾部的执行编号标记还在"
    );
}

#[tokio::test]
async fn old_tool_output_is_truncated_first_when_the_context_is_getting_full() {
    let dir = crate::test_util::TestDir::new("builtin-trunc");
    let big: String = (1..=3000).map(|i| format!("log line {i}\n")).collect();
    let history = vec![
        Message::User {
            text: "查日志".into(),
        },
        Message::Assistant(super::super::provider::AssistantMessage {
            text: String::new(),
            reasoning: Vec::new(),
            tool_calls: vec![super::super::provider::ToolCall {
                id: "old1".into(),
                name: "run_command".into(),
                arguments: json!({"command": "cat log"}),
            }],
        }),
        Message::ToolResult {
            call_id: "old1".into(),
            content: big.clone(),
            is_error: false,
        },
        Message::Assistant(super::super::provider::AssistantMessage {
            text: "日志看完了".into(),
            ..Default::default()
        }),
    ];
    // 3000 行输出约 8000 token；窗口 12000、输出 1000 → 预算约 8900，80% 是 7100：不截就超，截完就够。
    let id = save_history(&dir, &history);
    let mut rig = RigBuilder::new()
        .caps(|c| {
            c.context_window = Some(12_000);
            c.max_output = Some(1_000);
        })
        .resume_from(dir, &id)
        .build();
    rig.transport.push(Reply::Sse(oa_text("好", 500, 2)));
    let events = rig.say("总结一下").await;
    assert!(error_of(&events).is_none());
    assert_eq!(
        rig.transport.request_count(),
        1,
        "截短后就够了，不需要压缩请求"
    );
    let sent = messages_of(&rig.transport.body(0));
    let tool = sent.iter().find(|m| m["role"] == "tool").unwrap();
    let content = tool["content"].as_str().unwrap();
    assert!(content.contains("已截断 2920 行"), "{content}");
    assert!(content.contains("log line 1\n") && content.contains("log line 3000"));
    assert!(!content.contains("log line 100\n"));
    assert!(Rig::notices(&rig.signals()).is_empty());
}

fn save_history(dir: &crate::test_util::TestDir, messages: &[Message]) -> String {
    let id = "bi_seeded".to_owned();
    let bytes = transcript::encode(&id, Some("test-model"), messages, &[]).unwrap();
    HistoryStore::new(dir)
        .transcript_save(HISTORY_AGENT, &id, &bytes)
        .unwrap();
    id
}

#[tokio::test]
async fn crossing_eighty_percent_compresses_earlier_turns_with_the_same_model() {
    let mut rig = RigBuilder::new()
        .caps(|c| {
            c.context_window = Some(10_000);
            c.max_output = Some(1_000);
        })
        .build();
    // 第一回合的响应报出很大的用量：接口说上下文已经用了 6000 token（预算约 6950 的 86%）。
    rig.transport
        .push(Reply::Sse(oa_text("第一个答案", 6_000, 50)));
    rig.say("第一个问题").await;
    assert!(Rig::notices(&rig.signals()).is_empty());

    // 第二回合：先发一个压缩请求（提示词是压缩提示词、没有工具），再发真正的请求。
    rig.transport.push(Reply::Sse(oa_text(
        "摘要：用户问过第一个问题，已回答。",
        700,
        30,
    )));
    rig.transport
        .push(Reply::Sse(oa_text("第二个答案", 300, 10)));
    let events = rig.say("第二个问题").await;
    assert_eq!(text_of(&events).matches("第二个答案").count(), 1);
    assert_eq!(rig.transport.request_count(), 3);
    let compact_body = rig.transport.body(1);
    let compact_messages = messages_of(&compact_body);
    assert!(compact_messages[0]["content"]
        .as_str()
        .unwrap()
        .contains("上下文检查点压缩"));
    assert!(compact_body.get("tools").is_none(), "压缩请求不带工具");
    assert!(compact_messages[1]["content"]
        .as_str()
        .unwrap()
        .contains("第一个问题"));
    assert_eq!(compact_body["model"], "test-model", "用同一个模型");

    // 真正的请求里，较早的对话已经换成摘要，最近的用户消息原样在后面。
    let real = messages_of(&rig.transport.body(2));
    let users: Vec<&str> = real
        .iter()
        .filter(|m| m["role"] == "user")
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(users.len(), 2);
    assert!(users[0].starts_with("[较早的对话已压缩]"));
    assert!(users[0].contains("摘要：用户问过第一个问题"));
    assert_eq!(users[1], "第二个问题");
    assert!(!real
        .iter()
        .any(|m| m["content"].as_str() == Some("第一个问题")));
    assert_eq!(Rig::notices(&rig.signals()), [NoticeKind::Compacted]);
}

#[tokio::test]
async fn a_failed_compression_leaves_the_history_alone_and_the_turn_goes_on() {
    let mut rig = RigBuilder::new()
        .caps(|c| {
            c.context_window = Some(10_000);
            c.max_output = Some(1_000);
        })
        .build();
    rig.transport.push(Reply::Sse(oa_text("答一", 6_000, 50)));
    rig.say("问一").await;
    rig.transport.push(Reply::Status {
        status: 500,
        headers: vec![],
        body: "{}".to_owned(),
    });
    // 压缩请求的 500 会被重试 3 次：全失败后放弃压缩，真正的请求照发。
    for _ in 0..3 {
        rig.transport.push(Reply::Status {
            status: 500,
            headers: vec![],
            body: "{}".to_owned(),
        });
    }
    rig.transport.push(Reply::Sse(oa_text("答二", 6_200, 10)));
    let events = rig.say("问二").await;
    assert_eq!(text_of(&events), "答二");
    assert!(error_of(&events).is_none());
    let last = messages_of(&rig.transport.body(rig.transport.request_count() - 1));
    assert!(
        last.iter().any(|m| m["content"].as_str() == Some("问一")),
        "原文还在"
    );
    assert!(Rig::notices(&rig.signals()).is_empty());
}

#[tokio::test]
async fn a_context_too_long_error_triggers_one_compression_and_a_retry() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Sse(oa_text("答一", 100, 5)));
    rig.say("问一").await;
    // 第二个请求被接口拒绝：上下文超长。
    rig.transport.push(Reply::Status {
        status: 400,
        headers: vec![],
        body: r#"{"error":{"message":"This model's maximum context length is 8192 tokens","code":"context_length_exceeded"}}"#.to_owned(),
    });
    rig.transport
        .push(Reply::Sse(oa_text("摘要：问一已答。", 100, 10)));
    rig.transport.push(Reply::Sse(oa_text("答二", 100, 5)));
    let events = rig.say("问二").await;
    assert_eq!(text_of(&events), "答二");
    assert!(error_of(&events).is_none());
    assert_eq!(rig.transport.request_count(), 4);
    assert_eq!(Rig::notices(&rig.signals()), [NoticeKind::Compacted]);
}

#[tokio::test]
async fn resume_id_continues_the_conversation_with_signatures_intact() {
    let dir = crate::test_util::TestDir::new("builtin-resume");
    let (id, dir) = {
        let mut rig = RigBuilder::new()
            .kind(EndpointKind::Anthropic)
            .model("claude-test")
            .caps(|c| c.reasoning = Some(ReasoningSupport::Levels(vec!["high".into()])))
            .resume_from(dir, "ignored-new")
            .build();
        assert!(
            rig.conv.resume_id().is_none(),
            "还没有消息就没有可续接的东西"
        );
        rig.transport.push(Reply::Sse(ant_thinking_call(
            "想想",
            "SIG-RESUME",
            "toolu_9",
            "run_command",
            json!({"command": "uptime"}),
        )));
        rig.transport.push(Reply::Sse(ant_text("运行正常")));
        let events = rig.say_with("看看运行时间", Some("claude-test/high")).await;
        assert!(events.contains(&AgentEvent::TurnEnd));
        let id = rig.conv.resume_id().expect("回合后有续接标识");
        assert!(crate::ai::history::valid_agent_session_id(&id));
        (id, rig._dir.take().unwrap())
    };

    // 新对话从同一个数据目录续接。
    let mut rig = RigBuilder::new()
        .kind(EndpointKind::Anthropic)
        .model("claude-test")
        .caps(|c| c.reasoning = Some(ReasoningSupport::Levels(vec!["high".into()])))
        .resume_from(dir, &id)
        .build();
    assert_eq!(rig.conv.session_id(), id);
    assert_eq!(rig.conv.message_count(), 4);
    assert_eq!(rig.conv.last_model().as_deref(), Some("claude-test"));
    assert!(rig.events.try_recv().is_err(), "续接成功不发 ResumeFailed");
    rig.transport.push(Reply::Sse(ant_text("接着聊")));
    rig.say_with("再看一次", Some("claude-test/high")).await;
    let body = rig.transport.body(0);
    let history = body["messages"].as_array().unwrap();
    let assistant = history.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(
        assistant["content"][0]["signature"], "SIG-RESUME",
        "签名原样回传"
    );
    assert!(history
        .iter()
        .any(|m| m["content"].to_string().contains("看看运行时间")));
    assert_eq!(history.last().unwrap()["content"][0]["text"], "再看一次");
}

#[tokio::test]
async fn resuming_an_unknown_or_broken_record_falls_back_to_a_new_conversation() {
    // 记录不存在。
    let dir = crate::test_util::TestDir::new("builtin-resume-missing");
    let mut rig = RigBuilder::new().resume_from(dir, "bi_gone").build();
    assert_eq!(rig.events.try_recv().unwrap(), AgentEvent::ResumeFailed);
    assert_ne!(rig.conv.session_id(), "bi_gone");
    assert_eq!(rig.conv.message_count(), 0);

    // 记录坏了。
    let dir = crate::test_util::TestDir::new("builtin-resume-broken");
    HistoryStore::new(&dir)
        .transcript_save(HISTORY_AGENT, "bi_bad", b"{ not json")
        .unwrap();
    let mut rig = RigBuilder::new().resume_from(dir, "bi_bad").build();
    assert_eq!(rig.events.try_recv().unwrap(), AgentEvent::ResumeFailed);
    // 没带数据目录也不会崩。
    let rig = RigBuilder::new().build();
    assert!(rig.conv.resume_id().is_none());
}

#[tokio::test]
async fn a_resumed_history_with_a_dangling_tool_call_is_repaired() {
    let dir = crate::test_util::TestDir::new("builtin-resume-repair");
    let history = vec![
        Message::User { text: "做".into() },
        Message::Assistant(super::super::provider::AssistantMessage {
            tool_calls: vec![super::super::provider::ToolCall {
                id: "lost".into(),
                name: "stat".into(),
                arguments: json!({"path": "/tmp"}),
            }],
            ..Default::default()
        }),
    ];
    let id = save_history(&dir, &history);
    let mut rig = RigBuilder::new().resume_from(dir, &id).build();
    assert_eq!(Rig::notices(&rig.signals()), [NoticeKind::HistoryRepaired]);
    rig.transport.push(Reply::Sse(oa_text("好", 10, 1)));
    rig.say("继续").await;
    let sent = messages_of(&rig.transport.body(0));
    assert!(sent
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "lost"));
}

#[tokio::test]
async fn the_api_key_never_reaches_the_saved_history() {
    let mut rig = RigBuilder::new().with_history().build();
    assert_eq!(rig.conv.history_secrets(), [KEY]);
    *rig.host.stdout.lock().unwrap() = format!("secret={KEY}\n");
    rig.transport.push(Reply::Sse(oa_calls(
        "",
        &[(
            "c1",
            "run_command",
            json!({"command": format!("echo {KEY}")}),
        )],
        10,
        5,
    )));
    rig.transport
        .push(Reply::Sse(oa_text(&format!("你的 key 是 {KEY}"), 30, 4)));
    rig.say(&format!("记住 {KEY}")).await;
    let id = rig.conv.resume_id().unwrap();
    let bytes = HistoryStore::new(rig._dir.as_ref().unwrap())
        .transcript_load(HISTORY_AGENT, &id)
        .unwrap()
        .expect("存了对话记录");
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains(KEY), "{text}");
    assert!(text.contains("[已隐藏]"));
    // 界面历史用的抹除表就是这个。
    assert_eq!(rig.conv.history_secrets(), [KEY]);
}

#[tokio::test]
async fn models_merges_the_endpoint_report_with_hand_added_models() {
    let rig = RigBuilder::new().build();
    rig.transport.gets.lock().unwrap().push_back(Reply::Json(
        json!({"data":[
            {"id":"remote-a","context_length":64000,"supported_parameters":["tools","reasoning"]},
            {"id":"test-model"},
            {"id":"remote-b"}
        ]})
        .to_string(),
    ));
    let catalog = rig.conv.models().await.unwrap();
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["test-model", "extra-model", "remote-a", "remote-b"]);
    assert_eq!(catalog.default.as_deref(), Some("test-model"));
    // 接口自报的能力参与合成（没有手动值的模型）。
    let caps = rig.conv.model_caps("remote-a");
    assert_eq!(caps.context_window, 64_000);
    assert!(caps.tool_call);
    // 请求带了鉴权头，地址是 base/models。
    let requests = rig.transport.get_requests.lock().unwrap();
    assert_eq!(requests[0].url, "https://api.example.test/v1/models");
    assert!(requests[0]
        .headers
        .iter()
        .any(|(k, v)| k == "authorization" && v == &format!("Bearer {KEY}")));
}

#[tokio::test]
async fn models_falls_back_to_the_hand_added_list_when_the_endpoint_fails() {
    for reply in [
        Reply::Status {
            status: 401,
            headers: vec![],
            body: "{}".into(),
        },
        Reply::Json("not json".into()),
        Reply::Fail(TransportError::Connect("refused".into())),
    ] {
        let rig = RigBuilder::new().build();
        rig.transport.gets.lock().unwrap().push_back(reply);
        let catalog = rig.conv.models().await.unwrap();
        let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["test-model", "extra-model"]);
    }
    // 传输层根本不支持 GET 也一样。
    let rig = RigBuilder::new().build();
    assert_eq!(rig.conv.models().await.unwrap().models.len(), 2);
}

#[tokio::test]
async fn model_capabilities_are_cached_and_manual_values_win() {
    let rig = RigBuilder::new()
        .caps(|c| c.context_window = Some(123_456))
        .build();
    let first = rig.conv.model_caps("test-model");
    assert_eq!(first.context_window, 123_456);
    assert_eq!(
        first.source,
        crate::ai::agent::builtin::catalog::CapsSource::Manual
    );
    assert_eq!(rig.conv.model_caps("test-model"), first);
    // 没有手动值的模型走兜底：窗口标成估计。
    let guess = rig.conv.model_caps("totally-unknown-model");
    assert_eq!(
        guess.source,
        crate::ai::agent::builtin::catalog::CapsSource::Heuristic
    );
}

#[tokio::test]
async fn a_model_name_with_a_slash_is_not_mistaken_for_a_thinking_suffix() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Sse(oa_text("a", 5, 1)));
    rig.say_with("hi", Some("deepseek-ai/DeepSeek-V4")).await;
    assert_eq!(rig.transport.body(0)["model"], "deepseek-ai/DeepSeek-V4");
    rig.transport.push(Reply::Sse(oa_text("b", 5, 1)));
    rig.say_with("hi", Some("extra-model/high")).await;
    assert_eq!(rig.transport.body(1)["model"], "extra-model");
}

#[tokio::test]
async fn a_missing_model_is_a_clear_error() {
    // 造一个没有默认模型的接口。
    let transport = Arc::new(ScriptedTransport::default());
    let host = FakeHost::new();
    let approver = Arc::new(FakeApprover {
        answer: Mutex::new(Answer::Allow),
        asked: Mutex::new(Vec::new()),
    });
    let bridge = Arc::new(Bridge::new(
        host,
        Arc::new(Audit::default()),
        approver,
        Arc::new(Mode(Mutex::new(ExecMode::Auto))),
    ));
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let conv = BuiltinConversation::new(BuiltinParams {
        endpoint: AiEndpoint {
            id: 1,
            uid: "e".into(),
            name: "x".into(),
            kind: EndpointKind::OpenAiChat,
            base_url: "https://x.example.test/v1".into(),
            key_identity_id: None,
            has_key: false,
            default_model: None,
            models: vec![],
            overrides: BTreeMap::new(),
            openai_options: OpenAiOptionsConfig {
                echo_reasoning_content: Tri::Off,
                ..Default::default()
            },
            preset_id: None,
            created_at: 1,
            updated_at: 1,
        },
        api_key: ApiKey::new(""),
        transport,
        catalog: Arc::new(Catalog::empty()),
        scope: bridge.create_scope(),
        bridge,
        events: events_tx,
        signals: None,
        data_dir: None,
        resume_id: None,
        tuning: fast_tuning(),
    });
    conv.send("hi", None).await.unwrap();
    let mut got = Vec::new();
    loop {
        let event = events.recv().await.unwrap();
        let end = event == AgentEvent::TurnEnd;
        got.push(event);
        if end {
            break;
        }
    }
    assert!(error_of(&got).unwrap().contains("还没有选模型"));
    assert!(
        conv.history_secrets().is_empty(),
        "没有 Key 就没有要抹的东西"
    );
}

#[tokio::test]
async fn shutdown_stops_a_running_turn_and_revokes_the_capability() {
    let mut rig = RigBuilder::new().build();
    rig.transport.push(Reply::Hang(String::new()));
    rig.conv.send("hi", None).await.unwrap();
    rig.conv.shutdown().await;
    let events = rig.until_turn_end().await;
    assert_eq!(events.last(), Some(&AgentEvent::TurnEnd));
    assert!(
        rig.conv.send("again", None).await.is_err(),
        "关闭后不能再发"
    );
    // 密钥收回之后，工具桥不再认这个对话。
    let outcome = rig
        .bridge
        .call_tool("deadbeef", "list_targets", json!({}))
        .await;
    assert!(outcome.is_error);
}

#[test]
fn nothing_secret_is_logged_during_a_turn() {
    use std::io::Write;

    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Capture;
        fn make_writer(&'a self) -> Capture {
            self.clone()
        }
    }

    let captured = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(Capture(captured.clone()))
        .finish();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    runtime.block_on(async {
        let mut rig = RigBuilder::new().with_history().build();
        *rig.host.stdout.lock().unwrap() = "TOOL-OUTPUT-MARKER\n".to_owned();
        rig.transport.push(Reply::Status {
            status: 500,
            headers: vec![],
            body: format!("{{\"error\":{{\"message\":\"boom {KEY}\"}}}}"),
        });
        rig.transport.push(Reply::Sse(oa_calls(
            "",
            &[("c1", "run_command", json!({"command": "echo hi"}))],
            10,
            5,
        )));
        rig.transport
            .push(Reply::Sse(oa_text("REPLY-MARKER", 30, 2)));
        rig.say("PROMPT-MARKER 请执行").await;
        rig.conv.shutdown().await;
    });
    drop(guard);
    let logs = String::from_utf8_lossy(&captured.lock().unwrap()).into_owned();
    for secret in [KEY, "PROMPT-MARKER", "REPLY-MARKER", "TOOL-OUTPUT-MARKER"] {
        assert!(!logs.contains(secret), "日志里出现了 {secret}：\n{logs}");
    }
}

#[test]
fn the_defaults_match_the_card() {
    let tuning = BuiltinTuning::default();
    assert_eq!(tuning.max_tool_calls, 50);
    assert_eq!(tuning.max_retries, 3);
    assert_eq!(tuning.retry_base_delay, Duration::from_secs(1));
    assert_eq!(tuning.tool_result_limit, 64 * 1024);
}

/// 联网自测：真接口 + 假主机（内存文件系统，不连任何真服务器），跑一个完整的工具回合——
/// 让模型用 `list_dir` 看 /tmp 里有什么。设了 `ZSSH_AI_TEST_BASE` / `ZSSH_AI_TEST_KEY` /
/// `ZSSH_AI_TEST_MODEL` / `ZSSH_AI_TEST_KIND`（`openai` | `anthropic`）才会跑：
/// `cargo test -p zssh-core live_tool_round_trip -- --ignored --nocapture`。
#[tokio::test]
#[ignore = "联网自测：需要设置 ZSSH_AI_TEST_BASE / KEY / MODEL / KIND"]
async fn live_tool_round_trip() {
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
        "openai" => EndpointKind::OpenAiChat,
        "anthropic" => EndpointKind::Anthropic,
        other => {
            eprintln!("跳过：ZSSH_AI_TEST_KIND 要么 openai 要么 anthropic，收到 {other:?}");
            return;
        }
    };
    let host = FakeHost::new();
    let approver = Arc::new(FakeApprover {
        answer: Mutex::new(Answer::Allow),
        asked: Mutex::new(Vec::new()),
    });
    let bridge = Arc::new(Bridge::new(
        host.clone(),
        Arc::new(Audit::default()),
        approver,
        Arc::new(Mode(Mutex::new(ExecMode::Auto))),
    ));
    let scope = bridge.create_scope();
    scope.add_target("s1", "test-host", "tester", "connected", true, false);
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let conv = BuiltinConversation::new(BuiltinParams {
        endpoint: AiEndpoint {
            id: 1,
            uid: "live".into(),
            name: "联网自测".into(),
            kind,
            base_url: base,
            key_identity_id: None,
            has_key: true,
            default_model: Some(model),
            models: vec![],
            overrides: BTreeMap::new(),
            openai_options: OpenAiOptionsConfig::default(),
            preset_id: None,
            created_at: 1,
            updated_at: 1,
        },
        api_key: ApiKey::new(key),
        transport: Arc::new(
            crate::ai::agent::builtin::provider::ReqwestTransport::build(None).expect("客户端"),
        ),
        catalog: Arc::new(Catalog::builtin()),
        bridge,
        scope,
        events: events_tx,
        signals: None,
        data_dir: None,
        resume_id: None,
        tuning: BuiltinTuning::default(),
    });
    conv.send(
        "请用 list_dir 工具列出服务器上 /tmp 目录里的文件，然后告诉我文件名。",
        None,
    )
    .await
    .expect("发送");
    let mut got = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(180), events.recv())
            .await
            .expect("等事件超时")
            .expect("事件通道断了");
        let end = event == AgentEvent::TurnEnd;
        got.push(event);
        if end {
            break;
        }
    }
    // 只打印规模，不打印回复正文。
    eprintln!(
        "联网自测：{} 个事件，{} 次工具调用，回复 {} 字符",
        got.len(),
        tool_ends(&got).len(),
        text_of(&got).chars().count()
    );
    assert!(error_of(&got).is_none(), "{:?}", error_of(&got));
    let ends = tool_ends(&got);
    assert!(!ends.is_empty() && ends.iter().any(|e| !e.2 && e.1.contains("a.txt")));
    assert!(text_of(&got).contains("a.txt"));
}
