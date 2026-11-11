use std::sync::atomic::AtomicUsize;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Semaphore;

use super::*;

struct FixedMode(ExecMode);

impl ExecModeProvider for FixedMode {
    fn mode(&self) -> ExecMode {
        self.0
    }
}

struct ExecCall {
    session_id: String,
    command: String,
    wait: Duration,
    max_stream_bytes: usize,
}

fn exec_ok() -> ExecCapture {
    ExecCapture {
        stdout: "hello\n".into(),
        stderr: String::new(),
        stdout_truncated_bytes: 0,
        stderr_truncated_bytes: 0,
        exit_code: Some(0),
        timed_out: false,
        cancelled: false,
        duration_ms: 12,
    }
}

struct MockHost {
    exec_result: Mutex<Result<ExecCapture, String>>,
    last_exec: Mutex<Option<ExecCall>>,
    terminal_lines: Mutex<Vec<String>>,
    last_max_lines: Mutex<Option<usize>>,
    /// 终端执行的处置：默认后台（老行为）。
    placement: Mutex<ExecPlacement>,
    terminal_result: Mutex<Result<TerminalExecCapture, String>>,
    last_terminal: Mutex<Option<TerminalExecRequest>>,
    interrupted: Mutex<Vec<String>>,
    memory: Mutex<std::collections::HashMap<String, String>>,
    /// 目标 ID -> 拒绝原因（模拟非 SSH 主机）。
    refusals: Mutex<std::collections::HashMap<String, String>>,
}

impl MockHost {
    fn new() -> MockHost {
        MockHost {
            exec_result: Mutex::new(Ok(exec_ok())),
            last_exec: Mutex::new(None),
            terminal_lines: Mutex::new(vec!["one".into(), "two".into()]),
            memory: Mutex::new(std::collections::HashMap::new()),
            refusals: Mutex::new(std::collections::HashMap::new()),
            last_max_lines: Mutex::new(None),
            placement: Mutex::new(ExecPlacement::Background {
                reason: "测试默认走后台".to_string(),
            }),
            terminal_result: Mutex::new(Ok(TerminalExecCapture {
                output: "term-out".into(),
                exit_code: Some(0),
                duration_ms: 7,
                timed_out: false,
            })),
            last_terminal: Mutex::new(None),
            interrupted: Mutex::new(Vec::new()),
        }
    }

    fn set_terminal_placement(&self) {
        *self.placement.lock().unwrap() = ExecPlacement::Terminal;
    }

    fn set_exec_result(&self, result: Result<ExecCapture, String>) {
        *self.exec_result.lock().unwrap() = result;
    }

    fn last_exec(&self) -> Option<(String, String, Duration, usize)> {
        self.last_exec.lock().unwrap().as_ref().map(|call| {
            (
                call.session_id.clone(),
                call.command.clone(),
                call.wait,
                call.max_stream_bytes,
            )
        })
    }

    fn last_max_lines(&self) -> Option<usize> {
        *self.last_max_lines.lock().unwrap()
    }
}

impl BridgeHost for MockHost {
    fn target_refusal<'a>(&'a self, target_id: &'a str) -> BoxFuture<'a, Option<String>> {
        let reason = self.refusals.lock().unwrap().get(target_id).cloned();
        Box::pin(async move { reason })
    }

    fn read_memory<'a>(
        &'a self,
        target_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        let value = self.memory.lock().unwrap().get(target_id).cloned();
        Box::pin(async move { Ok(value) })
    }

    fn write_memory<'a>(
        &'a self,
        target_id: &'a str,
        notes: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.memory
            .lock()
            .unwrap()
            .insert(target_id.to_owned(), notes.to_owned());
        Box::pin(async { Ok(()) })
    }

    fn exec<'a>(
        &'a self,
        session_id: &'a str,
        command: &'a str,
        wait: Duration,
        max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        *self.last_exec.lock().unwrap() = Some(ExecCall {
            session_id: session_id.to_string(),
            command: command.to_string(),
            wait,
            max_stream_bytes,
        });
        let result = self.exec_result.lock().unwrap().clone();
        Box::pin(async move { result })
    }

    fn read_terminal<'a>(
        &'a self,
        _session_id: &'a str,
        max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        *self.last_max_lines.lock().unwrap() = Some(max_lines);
        let lines = self.terminal_lines.lock().unwrap().clone();
        Box::pin(async move { Ok(lines) })
    }

    fn exec_placement<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, ExecPlacement> {
        let placement = self.placement.lock().unwrap().clone();
        Box::pin(async move { placement })
    }

    fn run_in_terminal<'a>(
        &'a self,
        request: TerminalExecRequest,
    ) -> BoxFuture<'a, Result<TerminalExecCapture, String>> {
        *self.last_terminal.lock().unwrap() = Some(request);
        let result = self.terminal_result.lock().unwrap().clone();
        Box::pin(async move { result })
    }

    fn interrupt_terminal<'a>(&'a self, scope_id: &'a str) -> BoxFuture<'a, ()> {
        self.interrupted.lock().unwrap().push(scope_id.to_string());
        Box::pin(async {})
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ApproverBehavior {
    Allow,
    Deny,
    Never,
}

struct MockApprover {
    behavior: ApproverBehavior,
    calls: AtomicUsize,
    last: Mutex<Option<ApprovalRequest>>,
}

impl MockApprover {
    fn new(behavior: ApproverBehavior) -> MockApprover {
        MockApprover {
            behavior,
            calls: AtomicUsize::new(0),
            last: Mutex::new(None),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn last(&self) -> Option<ApprovalRequest> {
        self.last.lock().unwrap().clone()
    }
}

impl Approver for MockApprover {
    fn approve<'a>(&'a self, request: ApprovalRequest) -> BoxFuture<'a, ApprovalDecision> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some(request);
        let behavior = self.behavior;
        Box::pin(async move {
            match behavior {
                ApproverBehavior::Allow => ApprovalDecision::Allowed,
                ApproverBehavior::Deny => ApprovalDecision::Denied,
                ApproverBehavior::Never => std::future::pending().await,
            }
        })
    }
}

#[derive(Default)]
struct MockAudit {
    entries: Mutex<Vec<AuditEntry>>,
}

impl MockAudit {
    fn entries(&self) -> Vec<AuditEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl AuditLog for MockAudit {
    fn record(&self, entry: AuditEntry) {
        self.entries.lock().unwrap().push(entry);
    }
}

struct Harness {
    port: u16,
    bridge: Arc<Bridge>,
    host: Arc<MockHost>,
    approver: Arc<MockApprover>,
    audit: Arc<MockAudit>,
}

async fn harness(mode: ExecMode, behavior: ApproverBehavior, approval_wait: Duration) -> Harness {
    harness_with(Arc::new(FixedMode(mode)), behavior, approval_wait).await
}

/// 能随时锁定 / 解锁的模式来源（模拟界面的锁屏状态）。
struct LockableMode {
    mode: ExecMode,
    locked: std::sync::atomic::AtomicBool,
}

impl LockableMode {
    fn new(mode: ExecMode) -> Arc<LockableMode> {
        Arc::new(LockableMode {
            mode,
            locked: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn set_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::SeqCst);
    }
}

impl ExecModeProvider for LockableMode {
    fn mode(&self) -> ExecMode {
        self.mode
    }

    fn force_confirm(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }
}

async fn harness_with(
    mode: Arc<dyn ExecModeProvider>,
    behavior: ApproverBehavior,
    approval_wait: Duration,
) -> Harness {
    let host = Arc::new(MockHost::new());
    let approver = Arc::new(MockApprover::new(behavior));
    let audit = Arc::new(MockAudit::default());
    let bridge = Arc::new(
        Bridge::new(host.clone(), audit.clone(), approver.clone(), mode)
            .with_approval_wait(approval_wait),
    );
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("绑定端口");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(Arc::clone(&bridge).serve(listener));
    Harness {
        port,
        bridge,
        host,
        approver,
        audit,
    }
}

struct Client {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: tokio::net::tcp::OwnedWriteHalf,
    /// 请求里附带的对话能力密钥（握手成功后每条请求都要带）。
    capability: String,
}

impl Client {
    async fn connect(port: u16, connection_token: &str) -> Client {
        let stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("连上桥");
        let (reader, writer) = stream.into_split();
        let mut client = Client {
            reader: BufReader::new(reader),
            writer,
            capability: String::new(),
        };
        client.send(&json!({ "token": connection_token })).await;
        client
    }

    /// 连接层令牌 + 对话能力密钥都就位的客户端。
    async fn authenticated(port: u16, connection_token: &str, capability: &str) -> Client {
        let mut client = Client::connect(port, connection_token).await;
        client.capability = capability.to_string();
        client
    }

    async fn send(&mut self, value: &Value) {
        let mut line = serde_json::to_string(value).expect("序列化");
        line.push('\n');
        let _ = self.writer.write_all(line.as_bytes()).await;
        let _ = self.writer.flush().await;
    }

    async fn recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        match self.reader.read_line(&mut line).await {
            Ok(0) | Err(_) => None,
            Ok(_) => serde_json::from_str(line.trim()).ok(),
        }
    }

    async fn recv_within(&mut self, wait: Duration) -> Option<Value> {
        tokio::time::timeout(wait, self.recv())
            .await
            .expect("等服务端响应超时")
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(&json!({
            "id": id,
            "method": method,
            "params": params,
            "capability": self.capability,
        }))
        .await;
        self.recv_within(Duration::from_secs(5))
            .await
            .expect("服务端应回响应")
    }
}

fn result_of(response: &Value) -> &Value {
    assert!(response.get("error").is_none(), "不该报错：{response}");
    response.get("result").expect("要有 result")
}

fn error_of(response: &Value) -> &str {
    response
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("该报错：{response}"))
}

fn short_wait() -> Duration {
    Duration::from_millis(150)
}

#[test]
fn exec_mode_from_setting_defaults_to_manual() {
    assert_eq!(ExecMode::from_setting(None), ExecMode::Manual);
    assert_eq!(ExecMode::from_setting(Some("manual")), ExecMode::Manual);
    assert_eq!(ExecMode::from_setting(Some("auto")), ExecMode::Auto);
    assert_eq!(ExecMode::from_setting(Some("readonly")), ExecMode::ReadOnly);
    assert_eq!(ExecMode::from_setting(Some("垃圾")), ExecMode::Manual);
}

#[tokio::test]
async fn wrong_connection_token_closes_connection() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    let mut client = Client::connect(h.port, "bad-token").await;
    client
        .send(&json!({
            "id": 1,
            "method": METHOD_LIST_TARGETS,
            "params": {},
            "capability": "whatever",
        }))
        .await;
    assert!(
        client.recv_within(Duration::from_secs(2)).await.is_none(),
        "连接层令牌不对必须断开"
    );
}

#[tokio::test]
async fn unknown_capability_is_rejected_but_connection_survives() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let mut client = Client::authenticated(h.port, h.bridge.connection_token(), "deadbeef").await;
    let response = client.request(1, METHOD_LIST_TARGETS, json!({})).await;
    assert!(error_of(&response).contains("能力密钥无效"), "{response}");
    // 缺能力密钥也不行。
    client.capability = String::new();
    let response = client.request(2, METHOD_LIST_TARGETS, json!({})).await;
    assert!(
        error_of(&response).contains("缺少对话能力密钥"),
        "{response}"
    );
    // 握手没问题，连接还活着。
    client.capability = "deadbeef".to_string();
    let response = client.request(3, METHOD_LIST_TARGETS, json!({})).await;
    assert!(error_of(&response).contains("能力密钥无效"));
}

#[tokio::test]
async fn list_targets_returns_the_scope_targets_without_address() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    scope.add_target("s2", "db-1", "admin", "connected", true, false);

    let other = h.bridge.create_scope();
    other.add_target("s3", "secret", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client.request(1, METHOD_LIST_TARGETS, json!({})).await;
    let sessions = result_of(&response)["targets"]
        .as_array()
        .expect("sessions 数组")
        .clone();
    assert_eq!(sessions.len(), 2);
    let ids: Vec<&str> = sessions.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"s1") && ids.contains(&"s2"));
    assert!(!ids.contains(&"s3"), "别的 scope 的会话不能出现");
    for session in &sessions {
        let keys: Vec<&String> = session.as_object().unwrap().keys().collect();
        assert_eq!(
            keys.len(),
            6,
            "只允许 id/host_label/username/status/current/pinned：{session}"
        );
    }
    // 当前焦点那台排第一。
    assert_eq!(sessions[0]["id"], "s1");
    assert_eq!(sessions[0]["current"], true);
    assert_eq!(sessions[0]["pinned"], false);
}

#[tokio::test]
async fn non_ssh_targets_are_listed_as_unavailable_and_refused_by_every_executing_tool() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    h.host.refusals.lock().unwrap().insert(
        "host-9".to_string(),
        "这是一台 串口 主机，AI 助手只能在 SSH 主机上执行命令，不能操作它".to_string(),
    );
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", false, false);
    scope.add_target("host-9", "console-1", "", "pinned", true, true);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;

    // 列表：串口那台不在 targets 里，单独在 unavailable 里带原因。
    let response = client.request(1, METHOD_LIST_TARGETS, json!({})).await;
    let result = result_of(&response);
    let ids: Vec<&str> = result["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["s1"]);
    assert_eq!(result["unavailable"][0]["id"], "host-9");
    assert!(result["unavailable"][0]["reason"]
        .as_str()
        .unwrap()
        .contains("串口"));

    // 执行 / 读终端 / 文件工具：点名它或者它是当前焦点（没写 target_id），都在做任何事之前拒绝。
    for (id, method, params) in [
        (
            2,
            METHOD_RUN_COMMAND,
            json!({"target_id": "host-9", "command": "ls"}),
        ),
        (3, METHOD_RUN_COMMAND, json!({"command": "ls"})),
        (4, METHOD_READ_TERMINAL, json!({"target_id": "host-9"})),
        (
            5,
            METHOD_LIST_DIR,
            json!({"target_id": "host-9", "path": "/"}),
        ),
    ] {
        let response = client.request(id, method, params).await;
        assert!(
            error_of(&response).contains("只能在 SSH 主机上执行命令"),
            "{method}: {response}"
        );
    }
    assert!(h.host.last_exec().is_none(), "一条命令都不能发出去");

    // SSH 那台照常。
    let response = client
        .request(
            6,
            METHOD_RUN_COMMAND,
            json!({"target_id": "s1", "command": "ls"}),
        )
        .await;
    assert_eq!(result_of(&response)["exit_code"], 0);
}

#[tokio::test]
async fn retired_opencode_tools_are_denied_and_recovery_gets_a_fresh_capability() {
    use crate::ai::agent::AgentConversation;
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let (conversation, old_key) =
        crate::ai::agent::opencode::tests::retired_tool_scope_fixture(scope.clone()).await;
    let mut old_client = Client::authenticated(h.port, h.bridge.connection_token(), &old_key).await;
    let response = old_client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({"target_id":"s1", "command":"echo old-turn"}),
        )
        .await;
    assert!(error_of(&response).contains("能力密钥无效"));
    assert!(h.host.last_exec().is_none());

    conversation.send("继续", None).await.unwrap();
    let secrets = conversation.history_secrets();
    let new_key = secrets.last().unwrap();
    assert_ne!(new_key, &old_key);
    assert_ne!(new_key, scope.capability(), "根密钥不能交给模型");
    assert!(secrets.contains(&old_key), "撤销的密钥仍须从历史中抹掉");
    let mut fresh = Client::authenticated(h.port, h.bridge.connection_token(), new_key).await;
    let response = fresh
        .request(
            2,
            METHOD_RUN_COMMAND,
            json!({"target_id":"s1", "command":"echo new-turn"}),
        )
        .await;
    assert_eq!(result_of(&response)["exit_code"], 0);
    assert_eq!(h.host.last_exec().unwrap().1, "echo new-turn");

    let response = old_client
        .request(
            3,
            METHOD_RUN_COMMAND,
            json!({"target_id":"s1", "command":"echo late-old-turn"}),
        )
        .await;
    assert!(error_of(&response).contains("能力密钥无效"));
    assert_eq!(h.host.last_exec().unwrap().1, "echo new-turn");
}

#[test]
fn session_capability_revocation_invalidates_acquired_scopes_and_pending_tickets() {
    let bridge = manual_bridge(
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
        Arc::new(MockHost::new()),
    );
    let root = bridge.create_scope();
    root.add_target("s1", "web-1", "root", "connected", true, false);
    let old = root.agent_capability();
    let acquired = bridge.registry.lookup(old.value()).unwrap();
    let ticket = acquired.ticket("s1");
    let gate = ExecGate::new(&acquired, &ticket);
    assert!(gate.check().is_ok());
    old.revoke();
    assert!(gate.check().is_err(), "查表后、执行前才撤销也必须拦住");
    assert!(bridge.registry.lookup(old.value()).is_none());
    let fresh = root.agent_capability();
    let active = bridge.registry.lookup(fresh.value()).unwrap();
    let gate = ExecGate::new(&active, &active.ticket("s1"));
    old.revoke();
    assert!(gate.check().is_ok(), "重复撤销旧密钥不能误伤新会话");
    root.remove_target("s1");
    assert!(
        gate.check().is_err(),
        "界面的目标变更必须同步到 session 授权"
    );
    root.add_target("s1", "web-1", "root", "connected", true, false);
    let gate = ExecGate::new(&active, &active.ticket("s1"));
    bridge.stop_scope(root.scope_id());
    assert!(gate.check().is_err(), "手动停止也必须作用于子授权");
    root.destroy();
    assert!(active.is_destroyed(), "关对话后所有子授权必须失效");
}

#[tokio::test]
async fn revoking_session_capability_ends_its_pending_approval() {
    let h = harness(
        ExecMode::Manual,
        ApproverBehavior::Never,
        Duration::from_secs(30),
    )
    .await;
    let root = h.bridge.create_scope();
    root.add_target("s1", "web-1", "root", "connected", true, false);
    let grant = root.agent_capability();
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), grant.value()).await;
    let pending = tokio::spawn(async move {
        client
            .request(
                1,
                METHOD_RUN_COMMAND,
                json!({"target_id":"s1", "command":"ls"}),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while h.approver.calls() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    grant.revoke();
    let response = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap();
    assert!(error_of(&response).contains("拒绝"));
    assert!(h.host.last_exec().is_none());
}

#[tokio::test]
async fn cross_scope_session_access_is_rejected() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope_a = h.bridge.create_scope();
    scope_a.add_target("a-1", "web-a", "root", "connected", true, false);
    let scope_b = h.bridge.create_scope();
    scope_b.add_target("b-1", "web-b", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope_a.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "b-1", "command": "ls" }),
        )
        .await;
    assert!(error_of(&response).contains("不属于本次对话的作用范围"));
    let response = client
        .request(2, METHOD_READ_TERMINAL, json!({ "target_id": "b-1" }))
        .await;
    assert!(error_of(&response).contains("不属于本次对话的作用范围"));
    assert_eq!(h.approver.calls(), 0, "越权请求不该走到审批");
    assert_eq!(h.host.last_exec(), None);
}

#[tokio::test]
async fn manual_mode_allows_after_approval() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "ls -la" }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["stdout"], "hello\n");
    assert_eq!(result["exit_code"], 0);

    assert_eq!(h.approver.calls(), 1);
    let request = h.approver.last().expect("审批请求");
    assert_eq!(request.scope, scope.scope_id());
    assert_eq!(request.host_label, "web-1");
    assert_eq!(request.command, "ls -la");
    assert_eq!(request.danger, DangerLevel::Safe);

    let (session_id, command, wait, max_stream) = h.host.last_exec().expect("执行过");
    assert_eq!(session_id, "s1");
    assert_eq!(command, "ls -la");
    assert_eq!(
        wait,
        Duration::from_secs(60),
        "没给 timeout_sec 时默认 60 秒"
    );
    assert_eq!(max_stream, EXEC_MAX_STREAM_BYTES);

    let entries = h.audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].decision, ExecDecision::Allowed);
    assert_eq!(entries[0].danger, DangerLevel::Safe);
    assert_eq!(entries[0].exit_code, Some(0));
    assert_eq!(entries[0].duration_ms, Some(12));
    assert_eq!(entries[0].host_label, "web-1");
    assert_eq!(entries[0].scope, scope.scope_id());
}

#[tokio::test]
async fn manual_mode_denial_returns_user_rejected() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "ls" }),
        )
        .await;
    assert!(error_of(&response).contains("用户拒绝"));
    assert_eq!(h.host.last_exec(), None, "被拒绝就不能执行");

    let entries = h.audit.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].decision, ExecDecision::Denied);
    assert_eq!(entries[0].exit_code, None);
    assert_eq!(entries[0].duration_ms, None);
}

#[tokio::test]
async fn auto_mode_runs_safe_commands_without_approval() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "uname -a", "timeout_sec": 120 }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["danger"], "safe", "结果里要标出危险等级");
    assert_eq!(h.approver.calls(), 0, "安全命令不该问审批");
    assert_eq!(h.host.last_exec().unwrap().2, Duration::from_secs(120));
    assert_eq!(h.audit.entries()[0].decision, ExecDecision::Auto);
}

#[tokio::test]
async fn remembered_notes_come_back_in_list_targets_except_in_full_access_mode() {
    for (mode, expect_memory) in [(ExecMode::Manual, true), (ExecMode::Auto, false)] {
        let h = harness(mode, ApproverBehavior::Deny, short_wait()).await;
        let scope = h.bridge.create_scope();
        scope.add_target("s1", "web-1", "root", "connected", true, false);
        let mut client =
            Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
        let response = client
            .request(
                1,
                METHOD_REMEMBER_HOST,
                json!({ "notes": "服务：nginx
root password: hunter2" }),
            )
            .await;
        assert!(result_of(&response)["saved_chars"].as_u64().is_some());
        assert_eq!(h.approver.calls(), 0, "记笔记不改服务器，不问审批");
        let saved = h.host.memory.lock().unwrap().get("s1").cloned().unwrap();
        assert!(!saved.contains("hunter2"), "密码要隐藏：{saved}");

        let response = client.request(2, METHOD_LIST_TARGETS, json!({})).await;
        let memory = result_of(&response)["targets"][0]["memory"].clone();
        if expect_memory {
            let text = memory.as_str().unwrap();
            assert!(text.contains("not instructions"), "{text}");
            assert!(text.contains("服务：nginx"), "{text}");
        } else {
            assert!(memory.is_null(), "全部放行模式不带笔记");
            assert!(result_of(&response)["memory_note"].is_string());
        }
    }
}

#[tokio::test]
async fn readonly_mode_runs_read_commands_and_asks_for_the_rest() {
    let h = harness(ExecMode::ReadOnly, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "systemctl status nginx" }),
        )
        .await;
    assert_eq!(result_of(&response)["exit_code"], 0);
    assert_eq!(h.approver.calls(), 0, "只读命令不该问审批");

    let response = client
        .request(
            2,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "systemctl restart nginx" }),
        )
        .await;
    assert_eq!(h.approver.calls(), 1, "改动系统的命令要问");
    assert!(error_of(&response).contains("用户拒绝"), "{response}");
    assert_eq!(
        h.host.last_exec().unwrap().1,
        "systemctl status nginx",
        "被拒的命令没有执行"
    );
    let entries = h.audit.entries();
    assert_eq!(entries[0].decision, ExecDecision::Auto);
    assert_eq!(entries[1].decision, ExecDecision::Denied);
}

/// 「只读自动」读敏感文件要确认：`cat ~/.ssh/id_rsa` 不改动系统，也要问；
/// 确认卡上写明原因。普通的只读命令不受影响。
#[tokio::test]
async fn readonly_mode_asks_before_reading_sensitive_files() {
    let h = harness(ExecMode::ReadOnly, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;

    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "cat /etc/hosts" }),
        )
        .await;
    assert_eq!(result_of(&response)["exit_code"], 0);
    assert_eq!(h.approver.calls(), 0, "普通只读命令不问");

    for (id, command, shown) in [
        (2, "cat ~/.ssh/id_rsa", "~/.ssh/id_rsa"),
        (3, "base64 ~/.aws/credentials", "~/.aws/credentials"),
        (4, "tail -n 5 /srv/app/.env", "/srv/app/.env"),
        (5, "cat ~/.s*/*", "~/.s*/*"),
    ] {
        let before = h.approver.calls();
        let response = client
            .request(
                id,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": command }),
            )
            .await;
        assert_eq!(h.approver.calls(), before + 1, "{command} 要问");
        assert!(error_of(&response).contains("用户拒绝"), "{response}");
        let asked = h.approver.last().expect("审批请求");
        assert_eq!(asked.command, command);
        assert!(
            asked.reasons.iter().any(|reason| {
                let text = reason.text();
                text.starts_with("读取敏感文件：") && text.contains(shown)
            }),
            "{command}：{:?}",
            asked.reasons
        );
        assert_eq!(
            h.host.last_exec().unwrap().1,
            "cat /etc/hosts",
            "被拒的命令没有执行"
        );
    }
}

/// 审核 W01 / W02：粘连重定向吞掉命令名（`reboot>/dev/null`）、以及参数里没有敏感路径的秘密读取
/// （`getent shadow`、`jq -n env`、`history`、`systemctl cat`、`grep -r … /home`），
/// 「只读自动」和「逐条确认」两个模式都要问，卡片上带原因；正常诊断不受影响。
#[tokio::test]
async fn readonly_and_manual_cards_agree_on_glued_redirects_and_secret_sources() {
    use crate::error::reason_keys as rk;
    let asked_commands = [
        ("reboot>/dev/null", "cmd.power"),
        ("killall5>/dev/null", "cmd.killall5"),
        ("getent shadow", rk::CMD_SENSITIVE_ACCOUNT_DB),
        ("jq -n env", rk::CMD_SENSITIVE_ENVIRONMENT),
        ("jq -n '$ENV'", rk::CMD_SENSITIVE_ENVIRONMENT),
        ("history", rk::CMD_SENSITIVE_HISTORY),
        (
            "systemctl --user show-environment",
            rk::CMD_SENSITIVE_ENVIRONMENT,
        ),
        (
            "systemctl cat sample.service",
            rk::CMD_SENSITIVE_ENVIRONMENT,
        ),
        (
            "grep -r -A40 'PRIVATE KEY' /home",
            rk::CMD_SENSITIVE_RECURSIVE_SEARCH,
        ),
    ];
    let plain_commands = [
        "getent passwd",
        "jq . data.json",
        "systemctl status sample.service",
        "grep -r error /var/log",
        "ls -la 2>/dev/null",
        "dmesg 2>&1 | tail",
    ];
    for mode in [ExecMode::ReadOnly, ExecMode::Manual] {
        let h = harness(mode, ApproverBehavior::Deny, short_wait()).await;
        let scope = h.bridge.create_scope();
        scope.add_target("s1", "web-1", "root", "connected", true, false);
        let mut client =
            Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
        let mut id = 0;
        for (command, rule) in asked_commands {
            id += 1;
            let before = h.approver.calls();
            let response = client
                .request(
                    id,
                    METHOD_RUN_COMMAND,
                    json!({ "target_id": "s1", "command": command }),
                )
                .await;
            assert_eq!(h.approver.calls(), before + 1, "{mode:?}：{command} 要问");
            assert!(error_of(&response).contains("用户拒绝"), "{response}");
            let asked = h.approver.last().expect("审批请求");
            assert!(
                asked.reasons.iter().any(|reason| matches!(
                    reason,
                    ApprovalReason::Command { rule: key, .. } if key == rule
                )),
                "{mode:?}：{command} 的卡片要写明 {rule}：{:?}",
                asked.reasons
            );
        }
        if mode == ExecMode::ReadOnly {
            for command in plain_commands {
                id += 1;
                let before = h.approver.calls();
                let response = client
                    .request(
                        id,
                        METHOD_RUN_COMMAND,
                        json!({ "target_id": "s1", "command": command }),
                    )
                    .await;
                assert_eq!(result_of(&response)["exit_code"], 0, "{command}");
                assert_eq!(h.approver.calls(), before, "{command} 不该问");
            }
        }
    }
}

/// 「逐条确认」本来每条都问；读敏感文件的命令在卡片上额外写明原因，普通命令不加。
#[tokio::test]
async fn manual_mode_cards_say_when_a_command_reads_sensitive_files() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;

    client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "cat /etc/hosts" }),
        )
        .await;
    let plain = h.approver.last().expect("审批请求");
    assert!(
        !plain
            .reasons
            .iter()
            .any(|r| r.text().starts_with("读取敏感文件：")),
        "{:?}",
        plain.reasons
    );

    client
        .request(
            2,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "cat ~/.ssh/id_rsa" }),
        )
        .await;
    assert_eq!(h.approver.calls(), 2);
    let sensitive = h.approver.last().expect("审批请求");
    assert!(
        sensitive.reasons.iter().any(|r| {
            let text = r.text();
            text.starts_with("读取敏感文件：") && text.contains("~/.ssh/id_rsa")
        }),
        "{:?}",
        sensitive.reasons
    );
}

#[tokio::test]
async fn sensitive_reads_run_after_approval_and_need_no_prompt_in_auto_mode() {
    // 批准后照常执行，审计记“人工批准”。
    let h = harness(ExecMode::ReadOnly, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "cat ~/.ssh/id_rsa" }),
        )
        .await;
    assert_eq!(result_of(&response)["exit_code"], 0);
    assert_eq!(h.approver.calls(), 1);
    assert_eq!(h.audit.entries()[0].decision, ExecDecision::Allowed);

    // 「全部放行」不变。
    let h = harness(ExecMode::Auto, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "cat ~/.ssh/id_rsa" }),
        )
        .await;
    assert_eq!(result_of(&response)["exit_code"], 0);
    assert_eq!(h.approver.calls(), 0);
}

/// 锁屏时 AI 不能在服务器上产生副作用：不论哪种模式（含全部放行）、不论命令是否只读，
/// 都要确认；解锁后立即恢复原模式的行为。
#[tokio::test]
async fn while_locked_every_mode_asks_and_unlocking_restores_the_mode() {
    for (mode, command) in [
        (ExecMode::Auto, "systemctl restart nginx"),
        (ExecMode::Auto, "cat /etc/hosts"),
        (ExecMode::ReadOnly, "cat /etc/hosts"),
        (ExecMode::Manual, "cat /etc/hosts"),
    ] {
        let provider = LockableMode::new(mode);
        let h = harness_with(provider.clone(), ApproverBehavior::Allow, short_wait()).await;
        let scope = h.bridge.create_scope();
        scope.add_target("s1", "web-1", "root", "connected", true, false);
        let mut client =
            Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;

        provider.set_locked(true);
        let response = client
            .request(
                1,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": command }),
            )
            .await;
        assert_eq!(result_of(&response)["exit_code"], 0, "批准后照常执行");
        assert_eq!(h.approver.calls(), 1, "{mode:?} 锁屏时要问：{command}");
        let asked = h.approver.last().expect("审批请求");
        assert!(
            asked.reasons.iter().any(|r| r.text() == LOCKED_REASON),
            "卡片上写明锁屏：{:?}",
            asked.reasons
        );
        assert_eq!(h.audit.entries()[0].decision, ExecDecision::Allowed);

        // 解锁：恢复原模式。全部放行 / 只读命令不再问；手动模式照旧问。
        provider.set_locked(false);
        let before = h.approver.calls();
        client
            .request(
                2,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": command }),
            )
            .await;
        let expect_ask = match mode {
            ExecMode::Manual => true,
            ExecMode::Auto => false,
            ExecMode::ReadOnly => !crate::ai::readonly::is_read_only(command),
        };
        assert_eq!(
            h.approver.calls() - before,
            usize::from(expect_ask),
            "{mode:?} 解锁后恢复：{command}"
        );
    }
}

/// 锁着的时候审批超时按拒绝处理，原因写明“锁屏期间超时”；没锁时仍是普通的审批超时。
#[tokio::test]
async fn approval_timeout_while_locked_says_so_and_never_executes() {
    let provider = LockableMode::new(ExecMode::Auto);
    let h = harness_with(provider.clone(), ApproverBehavior::Never, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;

    provider.set_locked(true);
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "touch /tmp/x" }),
        )
        .await;
    assert!(error_of(&response).contains("锁屏期间超时"), "{response}");
    assert!(h.host.last_exec().is_none(), "超时按拒绝：没有执行");
    assert_eq!(h.audit.entries()[0].decision, ExecDecision::Timeout);

    // 没锁屏时的超时文案没变（手动模式会问，所以换一个手动模式的桥验证）。
    let h = harness(ExecMode::Manual, ApproverBehavior::Never, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "touch /tmp/x" }),
        )
        .await;
    assert!(error_of(&response).contains("审批超时"), "{response}");
}

/// 看终端内容也算：锁屏时要确认；没锁屏时不问。
#[tokio::test]
async fn reading_the_terminal_needs_confirmation_only_while_locked() {
    let provider = LockableMode::new(ExecMode::Auto);
    let h = harness_with(provider.clone(), ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;

    let response = client
        .request(1, METHOD_READ_TERMINAL, json!({ "target_id": "s1" }))
        .await;
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(h.approver.calls(), 0);

    provider.set_locked(true);
    let response = client
        .request(2, METHOD_READ_TERMINAL, json!({ "target_id": "s1" }))
        .await;
    assert!(error_of(&response).contains("用户拒绝"), "{response}");
    assert_eq!(h.approver.calls(), 1);

    provider.set_locked(false);
    client
        .request(3, METHOD_READ_TERMINAL, json!({ "target_id": "s1" }))
        .await;
    assert_eq!(h.approver.calls(), 1, "解锁后恢复");
}

/// AUDIT-2 A01：完整执行链。ReadOnly 模式 + 一律拒绝的审批替身：写操作的写法
/// （长选项缩写、短选项带值、子命令缩写……）一条都不能走到执行宿主，只读写法照常执行。
#[tokio::test]
async fn readonly_mode_never_executes_write_forms_without_approval() {
    let h = harness(ExecMode::ReadOnly, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;

    let mut id = 0;
    let write_forms = [
        "sort --output=out.txt input.txt",
        "sort --out=out.txt input.txt",
        "date -s2020-01-01",
        "hostname --file=/tmp/name",
        "ip l s dev eth0 down",
        "dmesg --read-clear",
        "journalctl --rot",
        "systemctl -p show restart nginx",
        "docker --config ps rm -f web",
        "echo x >/dev/null.audit",
        // AUDIT-3：要 shell 展开后才知道真实参数 / 展开发生在解析之前的写法
        "sort $'\\x2do' out.txt input.txt",
        "sort {-o,out.txt} input.txt",
        "printf -v x %s '-o out.txt'; sort $x input.txt",
        "sort $X input.txt",
        "sort *",
        "cat !!",
        "echo \"!!\"",
        "printf -v PROMPT_COMMAND %s x",
        "$X file",
        "echo $'\\'' ; touch f ; echo $'\\''",
    ];
    for command in write_forms {
        id += 1;
        let response = client
            .request(
                id,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": command }),
            )
            .await;
        assert!(
            error_of(&response).contains("用户拒绝"),
            "{command}: {response}"
        );
        assert_eq!(
            h.host.last_exec(),
            None,
            "写操作不该走到执行宿主：{command}"
        );
    }
    assert_eq!(
        h.approver.calls(),
        write_forms.len(),
        "每条写操作都要问审批"
    );

    let read_forms = [
        "sort -k2 file",
        "sort -t, -k2 file",
        "date",
        "date +%F",
        "date -u",
        "hostname",
        "hostname -I",
        "ip a",
        "ip addr show",
        "ip route",
        "ip -br link",
        // AUDIT-3：不看参数的命令带展开照常执行；引号里的特殊字符是字面量
        "cat /var/log/*.log",
        "ls -la $HOME",
        "echo $PATH",
        "rg 'error.*timeout' /var/log",
        "find /var/log -name '*.log'",
        "sort -t'$' -k2 file",
        "test ! -f x",
        "printf '%s\\n' x",
    ];
    for command in read_forms {
        id += 1;
        let response = client
            .request(
                id,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": command }),
            )
            .await;
        assert_eq!(result_of(&response)["exit_code"], 0, "{command}");
        assert_eq!(h.host.last_exec().unwrap().1, command, "只读命令照常执行");
    }
    assert_eq!(h.approver.calls(), write_forms.len(), "只读命令不问审批");
}

#[tokio::test]
async fn auto_mode_runs_dangerous_commands_without_approval() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "rm -rf /" }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["exit_code"], 0, "自动模式危险命令也直接执行");
    assert_eq!(result["danger"], "dangerous", "结果里要标出危险等级");
    assert_eq!(h.approver.calls(), 0, "自动模式不进审批");
    assert_eq!(h.approver.last(), None);
    assert_eq!(
        h.host.last_exec().unwrap().1,
        "rm -rf /",
        "危险命令真的执行了"
    );

    let entries = h.audit.entries();
    assert_eq!(
        entries[0].decision,
        ExecDecision::Auto,
        "审计决策记成自动放行"
    );
    assert_eq!(entries[0].danger, DangerLevel::Dangerous);
    assert_eq!(entries[0].exit_code, Some(0));
}

#[tokio::test]
async fn auto_mode_never_asks_the_approver() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let commands = ["uname -a", "rm -rf /", "reboot"];
    for (index, command) in commands.iter().enumerate() {
        let response = client
            .request(
                index as u64 + 1,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": command }),
            )
            .await;
        assert_eq!(
            result_of(&response)["exit_code"],
            0,
            "{command} 要直接执行：{response}"
        );
    }
    assert_eq!(h.approver.calls(), 0, "自动模式下审批接口一次都不该被调用");
    assert_eq!(h.approver.last(), None);

    let entries = h.audit.entries();
    assert_eq!(entries.len(), 3);
    assert!(
        entries
            .iter()
            .all(|entry| entry.decision == ExecDecision::Auto),
        "全部记成自动放行：{entries:?}"
    );
    assert_eq!(entries[0].danger, DangerLevel::Safe);
    assert_eq!(entries[1].danger, DangerLevel::Dangerous);
    assert_eq!(entries[2].danger, DangerLevel::Dangerous);
}

#[tokio::test]
async fn approval_timeout_denies_command() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Never, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "ls" }),
        )
        .await;
    assert!(error_of(&response).contains("审批超时"));
    assert_eq!(h.host.last_exec(), None);
    assert_eq!(h.audit.entries()[0].decision, ExecDecision::Timeout);
}

#[tokio::test]
async fn destroyed_scope_denies_pending_approval_and_rejects_later_requests() {
    let h = harness(
        ExecMode::Manual,
        ApproverBehavior::Never,
        Duration::from_secs(30),
    )
    .await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let capability = scope.capability().to_string();

    let mut pending = Client::authenticated(h.port, h.bridge.connection_token(), &capability).await;
    let mut later = Client::authenticated(h.port, h.bridge.connection_token(), &capability).await;
    assert!(
        result_of(&later.request(0, METHOD_LIST_TARGETS, json!({})).await).is_object(),
        "later 的握手要先就位"
    );

    let handle = tokio::spawn(async move {
        pending
            .request(
                1,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": "ls" }),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while h.approver.calls() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("审批要挂起");
    scope.destroy();

    let response = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("scope 销毁后审批要立刻结束")
        .expect("任务不 panic");
    assert!(error_of(&response).contains("用户拒绝"));
    assert_eq!(h.audit.entries()[0].decision, ExecDecision::Denied);

    let response = later.request(2, METHOD_LIST_TARGETS, json!({})).await;
    assert!(error_of(&response).contains("能力密钥无效或已过期"));

    // 连接层令牌仍然有效（进程没停），但能力密钥已从注册表摘除；用旧密钥的
    // 新连接一律拒绝。
    let mut fresh = Client::authenticated(h.port, h.bridge.connection_token(), &capability).await;
    let response = fresh.request(3, METHOD_LIST_TARGETS, json!({})).await;
    assert!(
        error_of(&response).contains("能力密钥无效或已过期"),
        "销毁后能力密钥必须失效：{response}"
    );
}

#[tokio::test]
async fn exec_timeout_reports_zssh_timeout() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    h.host.set_exec_result(Ok(ExecCapture {
        stdout: "部分输出".into(),
        stderr: String::new(),
        stdout_truncated_bytes: 0,
        stderr_truncated_bytes: 0,
        exit_code: None,
        timed_out: true,
        cancelled: false,
        duration_ms: 999,
    }));
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "sleep 5", "timeout_sec": 1 }),
        )
        .await;
    assert!(error_of(&response).contains("已超时"));
    let entries = h.audit.entries();
    assert_eq!(entries[0].decision, ExecDecision::Auto);
    assert_eq!(entries[0].exit_code, None);
    assert_eq!(entries[0].duration_ms, Some(999));
}

#[tokio::test]
async fn read_terminal_clamps_max_lines_and_returns_lines() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(1, METHOD_READ_TERMINAL, json!({ "target_id": "s1" }))
        .await;
    let result = result_of(&response);
    assert_eq!(h.host.last_max_lines(), Some(200), "默认 200 行");
    assert_eq!(result["lines"].as_array().unwrap().len(), 2);

    let response = client
        .request(
            2,
            METHOD_READ_TERMINAL,
            json!({ "session_id": "s1", "max_lines": 999_999 }),
        )
        .await;
    assert!(result_of(&response)["lines"].is_array());
    assert_eq!(h.host.last_max_lines(), Some(2000), "最多 2000 行");
}

#[tokio::test]
async fn unknown_method_and_garbage_return_errors() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client.request(1, "no_such_method", json!({})).await;
    assert!(error_of(&response).contains("未知方法"));

    client.send(&json!("这不是请求")).await;
    let garbage = client
        .recv_within(Duration::from_secs(5))
        .await
        .expect("要回错误");
    assert_eq!(garbage["id"], 0);
    assert!(error_of(&garbage).contains("无法解析"));

    let response = client.request(2, METHOD_LIST_TARGETS, json!({})).await;
    assert!(result_of(&response)["targets"].is_array(), "连接要活着");
}

#[tokio::test]
async fn host_failure_is_passed_through_and_audited() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    h.host.set_exec_result(Err("会话已断开".into()));
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "ls" }),
        )
        .await;
    assert!(error_of(&response).contains("会话已断开"));
    assert_eq!(h.audit.entries()[0].exit_code, None);
}

#[tokio::test]
async fn removed_session_disappears_from_scope() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    scope.add_target("s2", "db-1", "root", "connected", true, false);
    scope.remove_target("s1");

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client.request(1, METHOD_LIST_TARGETS, json!({})).await;
    let sessions = result_of(&response)["targets"].as_array().unwrap().clone();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], "s2");

    let response = client
        .request(
            2,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "ls" }),
        )
        .await;
    assert!(error_of(&response).contains("不属于本次对话的作用范围"));
}

#[tokio::test]
async fn runs_in_terminal_when_the_host_says_it_is_free() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    h.host.set_terminal_placement();
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "echo hi" }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["location"], "terminal");
    assert_eq!(result["stdout"], "term-out");
    assert_eq!(result["exit_code"], 0);
    // 没有走后台通道。
    assert_eq!(h.host.last_exec(), None);
    // 写进终端的是包装过的命令（带清输入前缀，前面有空格）。
    let request = h
        .host
        .last_terminal
        .lock()
        .unwrap()
        .clone()
        .expect("终端执行过");
    assert!(
        request.payload.contains("\x0b\x15"),
        "要先清掉输了一半的输入"
    );
    assert!(request.payload.contains("echo hi"));
    // 请求要带上这个对话的 scope id（AI-2d 1.4：停止按钮靠它认领命令）。
    assert_eq!(request.scope_id, scope.scope_id());

    let entries = h.audit.entries();
    assert_eq!(entries[0].location, Some(ExecLocation::Terminal));
    assert_eq!(entries[0].fallback_reason, None);
}

#[tokio::test]
async fn busy_terminal_falls_back_to_background_with_a_reason() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    // MockHost 默认返回 Background { reason }。
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "uname -a" }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["location"], "background");
    assert_eq!(result["fallback_reason"], "测试默认走后台");
    assert_eq!(result["stdout"], "hello\n");

    let entries = h.audit.entries();
    assert_eq!(entries[0].location, Some(ExecLocation::Background));
    assert_eq!(
        entries[0].fallback_reason.as_deref(),
        Some("测试默认走后台")
    );
}

#[tokio::test]
async fn target_outside_the_list_is_rejected() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    // 填了一个不在目标列表里的 ID：拒绝（哪怕它在主机库里）。
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s-other", "command": "ls" }),
        )
        .await;
    assert!(error_of(&response).contains("不属于本次对话的作用范围"));
    assert_eq!(h.host.last_exec(), None, "越权目标不该执行");
    assert!(h.host.last_terminal.lock().unwrap().is_none());
}

#[tokio::test]
async fn missing_target_id_uses_the_current_one() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    scope.add_target("s2", "db-1", "root", "connected", false, true);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    // 不填 target_id：默认用"当前"那台（s1）。
    let response = client
        .request(1, METHOD_RUN_COMMAND, json!({ "command": "ls" }))
        .await;
    assert!(result_of(&response).is_object());
    let (target, _, _, _) = h.host.last_exec().expect("执行过");
    assert_eq!(target, "s1", "不填就用当前焦点那台");
}

#[tokio::test]
async fn interrupt_targets_the_scope() {
    // 停止按钮传的是 scope id：宿主据此查"这个对话有没有命令在终端里跑"。
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    h.bridge.interrupt_terminal(scope.scope_id()).await;
    assert_eq!(
        h.host.interrupted.lock().unwrap().clone(),
        vec![scope.scope_id().to_string()]
    );
}

/// 会让终端行编辑器产生按键效果、或者让卡片显示和实际内容不一致的字符（R-8 第 1 节）。
///
/// 命令是当键盘输入打进终端的，单引号挡得住 shell 解释，挡不住 Ctrl+U（删掉已输入
/// 的前缀）、Ctrl+C/D、ESC、DEL、回车（提前提交）这些按键；C1 和方向控制字符还能
/// 让审批卡片显示的内容和实际执行的不一样。
const FORBIDDEN_CONTROL_CHARS: [(&str, &str, &str); 8] = [
    ("Ctrl+U", "ls\x15rm -rf ~", "U+0015"),
    ("Ctrl+C", "ls\x03", "U+0003"),
    ("Ctrl+D", "ls\x04", "U+0004"),
    ("ESC", "ls\x1b", "U+001B"),
    ("DEL", "ls\x7f", "U+007F"),
    ("CR", "ls\r", "U+000D"),
    ("C1 CSI", "ls\u{009b}31m", "U+009B"),
    ("RTL override", "ls\u{202e}rm -rf ~", "U+202E"),
];

#[tokio::test]
async fn control_char_commands_are_rejected_before_approval_and_write_nothing_to_the_terminal() {
    // 手动模式 + 终端空闲：修复前这条命令会走到审批、批准后写进终端。
    for (label, command, code) in FORBIDDEN_CONTROL_CHARS {
        let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
        h.host.set_terminal_placement();
        let scope = h.bridge.create_scope();
        scope.add_target("s1", "web-1", "root", "connected", true, false);

        let mut client =
            Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
        let response = client
            .request(
                1,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": command }),
            )
            .await;
        let message = error_of(&response);
        assert!(
            message.contains("控制字符"),
            "{label}：错误要说明是控制字符：{message}"
        );
        assert!(
            message.contains(code),
            "{label}：错误里要有码位 {code}：{message}"
        );
        assert_eq!(h.approver.calls(), 0, "{label}：审批之前就该拒绝");
        assert!(
            h.host.last_terminal.lock().unwrap().is_none(),
            "{label}：终端里不能写入任何字节"
        );
        assert_eq!(
            h.host.last_exec(),
            None,
            "{label}：后台通道也不能执行（终端执行和后台执行都算）"
        );
    }
}

#[tokio::test]
async fn control_char_commands_are_rejected_in_auto_mode_too() {
    // 自动模式没有审批这道关：控制字符也要在后台执行之前被拒。
    for (label, command, code) in FORBIDDEN_CONTROL_CHARS {
        let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
        // MockHost 默认返回 Background：这次走后台通道。
        let scope = h.bridge.create_scope();
        scope.add_target("s1", "web-1", "root", "connected", true, false);

        let mut client =
            Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
        let response = client
            .request(
                1,
                METHOD_RUN_COMMAND,
                json!({ "target_id": "s1", "command": command }),
            )
            .await;
        let message = error_of(&response);
        assert!(
            message.contains("控制字符"),
            "{label}：错误要说明是控制字符：{message}"
        );
        assert!(
            message.contains(code),
            "{label}：错误里要有码位 {code}：{message}"
        );
        assert_eq!(h.host.last_exec(), None, "{label}：自动模式也不能执行");
        assert!(
            h.host.last_terminal.lock().unwrap().is_none(),
            "{label}：终端里不能写入任何字节"
        );
    }
}

#[tokio::test]
async fn newline_and_tab_still_run_through_the_terminal() {
    let command = "printf 'a\\tb\\n'\necho done";
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    h.host.set_terminal_placement();
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": command }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["location"], "terminal");
    assert_eq!(h.approver.calls(), 1, "放行的命令照常走审批");
    let request = h
        .host
        .last_terminal
        .lock()
        .unwrap()
        .clone()
        .expect("换行和制表符要能执行");
    assert_eq!(request.command, command);
    assert!(
        request.payload.contains("a\\tb\\n") && request.payload.contains("echo done"),
        "命令原文要原样进包装脚本：{}",
        request.payload
    );
}

/// 终端行编辑器收到 Tab 会做自动补全（单引号里也一样），实际执行的命令会和审批时看到的
/// 不一样（R-7 第 0 节）。所以带真正 `\t` 的命令在终端空闲时也要改走后台通道。
#[tokio::test]
async fn tab_in_command_goes_to_background_not_terminal_in_manual_mode() {
    let command = "echo a\tb";
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    h.host.set_terminal_placement();
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": command }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["location"], "background");
    assert_eq!(
        result["fallback_reason"],
        "命令里有 Tab，终端会把它当成补全键"
    );
    assert_eq!(h.approver.calls(), 1, "照常走审批");
    assert!(
        h.host.last_terminal.lock().unwrap().is_none(),
        "终端里不能写入任何字节"
    );
    let (_, executed, _, _) = h.host.last_exec().expect("后台通道执行了");
    assert_eq!(executed, command, "后台通道拿到的是原文");

    let entries = h.audit.entries();
    assert_eq!(entries[0].location, Some(ExecLocation::Background));
    assert_eq!(
        entries[0].fallback_reason.as_deref(),
        Some("命令里有 Tab，终端会把它当成补全键")
    );
}

#[tokio::test]
async fn tab_in_command_goes_to_background_in_auto_mode_too() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    h.host.set_terminal_placement();
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "\techo indented" }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["location"], "background");
    assert!(result["fallback_reason"]
        .as_str()
        .unwrap_or_default()
        .contains("Tab"));
    assert!(h.host.last_terminal.lock().unwrap().is_none());
}

#[tokio::test]
async fn command_without_tab_still_uses_the_terminal() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    h.host.set_terminal_placement();
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": "echo a b\necho c" }),
        )
        .await;
    let result = result_of(&response);
    assert_eq!(result["location"], "terminal");
    assert_eq!(h.host.last_exec(), None, "没有 Tab 不该走后台");
    assert!(h.host.last_terminal.lock().unwrap().is_some());
}

#[tokio::test]
async fn newline_and_tab_still_run_in_the_background() {
    let command = "echo one\necho two\tend";
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);

    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "s1", "command": command }),
        )
        .await;
    assert_eq!(result_of(&response)["exit_code"], 0);
    let (_, executed, _, _) = h.host.last_exec().expect("后台通道要执行");
    assert_eq!(executed, command);
}

/// 一条连接的请求并发处理（R-8 第 10 节）：一条卡在审批上时，另一条必须立刻回。
#[tokio::test]
async fn a_pending_approval_does_not_block_other_requests_on_the_same_connection() {
    // 审批永远不回答；等待时间给足，测试自己不等它结束。
    let h = harness(
        ExecMode::Manual,
        ApproverBehavior::Never,
        Duration::from_secs(30),
    )
    .await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;

    // 第一条：run_command 卡在审批上，只发请求、不在这里等响应。
    client
        .send(&json!({
            "id": 1,
            "method": METHOD_RUN_COMMAND,
            "params": { "target_id": "s1", "command": "ls" },
            "capability": scope.capability(),
        }))
        .await;
    // 等桥确实进了审批（旧实现会一直串行处理，这条请求之后才轮到下一条）。
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while h.approver.calls() == 0 {
        assert!(tokio::time::Instant::now() < deadline, "第一条没进审批");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // 第二条：同一连接上的 list_targets 要立刻返回（不等第一条审批）。
    let response = client.request(2, METHOD_LIST_TARGETS, json!({})).await;
    assert_eq!(response["id"], 2, "响应要按 id 对应：{response}");
    let targets = result_of(&response)["targets"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(targets.len(), 1, "第二条要拿到自己的结果：{response}");
    assert_eq!(targets[0]["id"], "s1");
}

/// 宿主：read_terminal 卡在信号量上，用来制造「先到的请求后回」。
struct GatedHost {
    entered: Semaphore,
    release: Semaphore,
}

impl GatedHost {
    fn new() -> GatedHost {
        GatedHost {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
}

impl BridgeHost for GatedHost {
    fn exec<'a>(
        &'a self,
        _session_id: &'a str,
        _command: &'a str,
        _wait: Duration,
        _max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        Box::pin(async { Err("本测不执行命令".to_owned()) })
    }

    fn read_terminal<'a>(
        &'a self,
        _session_id: &'a str,
        _max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        self.entered.add_permits(1);
        Box::pin(async move {
            let _ = self.release.acquire().await;
            Ok(vec!["gated".to_string()])
        })
    }
}

/// 响应可以乱序：客户端按 id 认领自己的结果。
#[tokio::test]
async fn responses_are_matched_by_id_even_when_they_arrive_out_of_order() {
    let host = Arc::new(GatedHost::new());
    let bridge = Arc::new(Bridge::new(
        host.clone(),
        Arc::new(MockAudit::default()),
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
        Arc::new(FixedMode(ExecMode::Auto)),
    ));
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("绑定端口");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(Arc::clone(&bridge).serve(listener));
    let scope = bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(port, bridge.connection_token(), scope.capability()).await;

    // id 1：read_terminal 卡在宿主里；只发请求。
    client
        .send(&json!({
            "id": 1,
            "method": METHOD_READ_TERMINAL,
            "params": { "target_id": "s1" },
            "capability": scope.capability(),
        }))
        .await;
    // 等宿主确实进了 read_terminal，再发第二条（保证 id 1 先到、后回）。
    host.entered
        .acquire()
        .await
        .expect("信号量不该关闭")
        .forget();

    // id 2：list_targets 先回。
    let response = client.request(2, METHOD_LIST_TARGETS, json!({})).await;
    assert_eq!(response["id"], 2, "先回的是 id 2：{response}");

    // 放行 id 1，它的响应随后到，内容对得上。
    host.release.add_permits(1);
    let response = client
        .recv_within(Duration::from_secs(5))
        .await
        .expect("id 1 的响应要回来");
    assert_eq!(response["id"], 1, "后回的是 id 1：{response}");
    let lines = result_of(&response)["lines"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0], "gated");
}

#[test]
fn token_comparison_accepts_only_the_exact_token() {
    // 恒定时间比较的行为要和 `==` 一样准（R-8 第 11 节）。
    let token = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert!(constant_time_eq(token, token));
    assert!(constant_time_eq("", ""));
    assert!(!constant_time_eq(token, ""));
    assert!(!constant_time_eq("", token));
    assert!(!constant_time_eq(token, "bad-token"));
    // 长度不同。
    assert!(!constant_time_eq(token, &token[..token.len() - 1]));
    // 同长度、只有最后一个字符不同。
    let mut last_differs = token.to_string();
    let last = last_differs.pop().expect("非空");
    last_differs.push(if last == '0' { '1' } else { '0' });
    assert!(!constant_time_eq(token, &last_differs));
    // 同长度、只有第一个字符不同。
    let mut first_differs = token.to_string();
    let first = first_differs.remove(0);
    first_differs.insert(0, if first == '0' { '1' } else { '0' });
    assert!(!constant_time_eq(token, &first_differs));
}

#[tokio::test]
async fn wrong_connection_token_with_the_same_length_closes_the_connection() {
    // 真实入口：和正确令牌等长（只是内容不同）也要被拒（恒定时间比较不能判错）。
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    let valid = h.bridge.connection_token().to_string();
    let mut bad = valid.clone();
    let last = bad.pop().expect("令牌非空");
    bad.push(if last == '0' { '1' } else { '0' });
    assert_eq!(bad.len(), valid.len());
    assert_ne!(bad, valid);

    let mut client = Client::connect(h.port, &bad).await;
    client
        .send(&json!({
            "id": 1,
            "method": METHOD_LIST_TARGETS,
            "params": {},
            "capability": "whatever",
        }))
        .await;
    assert!(
        client.recv_within(Duration::from_secs(2)).await.is_none(),
        "等长但内容不对的连接层令牌必须断开连接"
    );
}

// ----- R-9：停止 / 移出目标让在途请求作废（F11、F12），输入限长（F15） -----

/// 审批人：等测试放行之后才回答"允许"，用来制造"批准之前目标被移出 / 点了停止"。
struct GateApprover {
    entered: Semaphore,
    release: Semaphore,
}

impl Approver for GateApprover {
    fn approve<'a>(&'a self, _request: ApprovalRequest) -> BoxFuture<'a, ApprovalDecision> {
        self.entered.add_permits(1);
        Box::pin(async move {
            let _ = self.release.acquire().await;
            ApprovalDecision::Allowed
        })
    }
}

fn manual_bridge(approver: Arc<dyn Approver>, host: Arc<dyn BridgeHost>) -> Arc<Bridge> {
    Arc::new(Bridge::new(
        host,
        Arc::new(MockAudit::default()),
        approver,
        Arc::new(FixedMode(ExecMode::Manual)),
    ))
}

fn approval(scope: &Scope) -> ApprovalRequest {
    ApprovalRequest {
        scope: scope.id.clone(),
        exec_id: "t-1".into(),
        target_id: "s1".into(),
        session_id: Some("s1".into()),
        host_label: "web-1".into(),
        command: "ls".into(),
        danger: DangerLevel::Safe,
        reasons: Vec::new(),
        file: None,
    }
}

#[test]
fn a_ticket_is_invalidated_by_removal_even_if_the_target_comes_back() {
    let bridge = manual_bridge(
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
        Arc::new(MockHost::new()),
    );
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let ticket = handle.scope.ticket("s1");
    assert_eq!(handle.scope.ticket_refusal(&ticket), None);

    handle.remove_target("s1");
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    assert!(
        handle.scope.ticket_refusal(&ticket).is_some(),
        "移出后又加回来，也不能按移出之前的批准执行"
    );
    let fresh = handle.scope.ticket("s1");
    assert_eq!(
        handle.scope.ticket_refusal(&fresh),
        None,
        "新的请求不受影响"
    );
}

#[tokio::test]
async fn an_approval_given_after_the_target_was_removed_is_refused() {
    let approver = Arc::new(GateApprover {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
    });
    let bridge = manual_bridge(approver.clone(), Arc::new(MockHost::new()));
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let scope = Arc::clone(&handle.scope);
    let ticket = scope.ticket("s1");

    let decide = {
        let bridge = Arc::clone(&bridge);
        let scope = Arc::clone(&scope);
        tokio::spawn(async move {
            let request = approval(&scope);
            bridge.decide(&scope, &ticket, request).await
        })
    };
    approver
        .entered
        .acquire()
        .await
        .expect("审批要挂起")
        .forget();
    // 卡片还开着时目标被移出：等审批的请求立即按拒绝结束，不用等人点。
    handle.remove_target("s1");
    let (decision, refusal) = tokio::time::timeout(Duration::from_secs(5), decide)
        .await
        .expect("移出目标后审批要立刻结束")
        .expect("任务不 panic");
    assert_eq!(decision, ExecDecision::Denied);
    assert!(refusal.unwrap_or_default().contains("移出"));
}

#[tokio::test]
async fn an_approval_that_lands_right_after_a_stop_is_refused() {
    // 自动模式没有审批可等：停止发生在拿到 ticket 和放行之间，放行前的复核也要拦住。
    let bridge = Arc::new(Bridge::new(
        Arc::new(MockHost::new()),
        Arc::new(MockAudit::default()),
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
        Arc::new(FixedMode(ExecMode::Auto)),
    ));
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let ticket = handle.scope.ticket("s1");
    bridge.stop_scope(&handle.scope.id);
    let (decision, refusal) = bridge
        .decide(&handle.scope, &ticket, approval(&handle.scope))
        .await;
    assert_eq!(decision, ExecDecision::Denied);
    assert!(refusal.unwrap_or_default().contains("停止"));
}

#[tokio::test]
async fn stop_ends_a_pending_approval() {
    let bridge = manual_bridge(
        Arc::new(MockApprover::new(ApproverBehavior::Never)),
        Arc::new(MockHost::new()),
    );
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let scope = Arc::clone(&handle.scope);
    let ticket = scope.ticket("s1");
    let decide = {
        let bridge = Arc::clone(&bridge);
        let scope = Arc::clone(&scope);
        tokio::spawn(async move {
            let request = approval(&scope);
            bridge.decide(&scope, &ticket, request).await
        })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    bridge.stop_scope(&scope.id);
    let (decision, refusal) = tokio::time::timeout(Duration::from_secs(5), decide)
        .await
        .expect("停止后审批要立刻结束")
        .expect("任务不 panic");
    assert_eq!(decision, ExecDecision::Denied);
    assert!(refusal.unwrap_or_default().contains("停止"));
}

/// 宿主：按生产实现的顺序走——开始前核对 gate → （可选）等"自动连接"→ 连上后再核对 gate
/// → 取消标志没置位才"发出"命令 → 命令一直跑，直到取消标志置位（模拟关闭执行通道）。
/// `sent` 记命令真正发出去的次数；`placement_gate` / `connect_gate` 让测试卡住探测 / 连接。
struct CancellableHost {
    started: Semaphore,
    sent: AtomicUsize,
    hold_connect: AtomicBool,
    connecting: Semaphore,
    connect_gate: Semaphore,
    hold_placement: AtomicBool,
    placing: Semaphore,
    placement_gate: Semaphore,
    terminal_runs: AtomicUsize,
    released: AtomicUsize,
    /// 设了就让 run_in_terminal 按这个原因失败（模拟会话任务那边的写入结果）。
    terminal_error: Mutex<Option<String>>,
}

impl CancellableHost {
    fn new() -> CancellableHost {
        CancellableHost {
            started: Semaphore::new(0),
            sent: AtomicUsize::new(0),
            hold_connect: AtomicBool::new(false),
            connecting: Semaphore::new(0),
            connect_gate: Semaphore::new(0),
            hold_placement: AtomicBool::new(false),
            placing: Semaphore::new(0),
            placement_gate: Semaphore::new(0),
            terminal_runs: AtomicUsize::new(0),
            released: AtomicUsize::new(0),
            terminal_error: Mutex::new(None),
        }
    }
}

impl BridgeHost for CancellableHost {
    fn exec<'a>(
        &'a self,
        _session_id: &'a str,
        _command: &'a str,
        _wait: Duration,
        _max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        Box::pin(async { Err("本测走 exec_cancellable".to_owned()) })
    }

    fn exec_cancellable<'a>(
        &'a self,
        _session_id: &'a str,
        _command: &'a str,
        _wait: Duration,
        _max_stream_bytes: usize,
        gate: &'a ExecGate,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        Box::pin(async move {
            gate.check()?;
            if self.hold_connect.load(Ordering::SeqCst) {
                self.connecting.add_permits(1);
                let _ = self.connect_gate.acquire().await;
            }
            gate.check()?;
            if gate.cancel_flag().load(Ordering::SeqCst) {
                return Err("已取消，命令没有发出".to_owned());
            }
            self.sent.fetch_add(1, Ordering::SeqCst);
            self.started.add_permits(1);
            while !gate.cancel_flag().load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok(ExecCapture {
                exit_code: None,
                cancelled: true,
                ..exec_ok()
            })
        })
    }

    fn read_terminal<'a>(
        &'a self,
        _session_id: &'a str,
        _max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn exec_placement<'a>(&'a self, _session_id: &'a str) -> BoxFuture<'a, ExecPlacement> {
        Box::pin(async move {
            if !self.hold_placement.load(Ordering::SeqCst) {
                return ExecPlacement::Background {
                    reason: "测试走后台".into(),
                };
            }
            // 模拟第一次在终端执行前探测远端 shell：卡住，等测试放行。
            self.placing.add_permits(1);
            let _ = self.placement_gate.acquire().await;
            ExecPlacement::Terminal
        })
    }

    fn run_in_terminal<'a>(
        &'a self,
        request: TerminalExecRequest,
    ) -> BoxFuture<'a, Result<TerminalExecCapture, String>> {
        Box::pin(async move {
            request.gate.check()?;
            if let Some(message) = self.terminal_error.lock().ok().and_then(|e| e.clone()) {
                return Err(message);
            }
            self.terminal_runs.fetch_add(1, Ordering::SeqCst);
            Ok(TerminalExecCapture {
                output: String::new(),
                exit_code: Some(0),
                duration_ms: 1,
                timed_out: false,
            })
        })
    }

    fn release_terminal<'a>(&'a self, _target_id: &'a str) -> BoxFuture<'a, ()> {
        self.released.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

fn auto_bridge(host: Arc<CancellableHost>) -> Arc<Bridge> {
    Arc::new(Bridge::new(
        host,
        Arc::new(MockAudit::default()),
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
        Arc::new(FixedMode(ExecMode::Auto)),
    ))
}

fn spawn_run(
    bridge: &Arc<Bridge>,
    scope: &Arc<Scope>,
) -> tokio::task::JoinHandle<Result<serde_json::Value, String>> {
    let bridge = Arc::clone(bridge);
    let scope = Arc::clone(scope);
    tokio::spawn(async move {
        bridge
            .run_command(
                &scope,
                json!({ "target_id": "s1", "command": "rm -f /tmp/x" }),
                "t-1",
            )
            .await
    })
}

#[tokio::test]
async fn stop_while_auto_connecting_never_sends_the_command() {
    // R-9 F12：停止发生在批准之后、自动连接完成之前：连上之后不能再把命令发出去。
    let host = Arc::new(CancellableHost::new());
    host.hold_connect.store(true, Ordering::SeqCst);
    let bridge = auto_bridge(host.clone());
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let run = spawn_run(&bridge, &handle.scope);
    host.connecting
        .acquire()
        .await
        .expect("要开始连接")
        .forget();
    bridge.stop_scope(&handle.scope.id);
    host.connect_gate.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("要结束")
        .expect("任务不 panic");
    assert!(result.expect_err("停止了").contains("停止"));
    assert_eq!(host.sent.load(Ordering::SeqCst), 0, "命令不能发出去");
}

#[tokio::test]
async fn removing_the_target_while_auto_connecting_never_sends_the_command() {
    // R-9 F11：批准之后、自动连接完成之前目标被移出：旧请求不能再在原目标执行。
    let host = Arc::new(CancellableHost::new());
    host.hold_connect.store(true, Ordering::SeqCst);
    let bridge = auto_bridge(host.clone());
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let run = spawn_run(&bridge, &handle.scope);
    host.connecting
        .acquire()
        .await
        .expect("要开始连接")
        .forget();
    handle.remove_target("s1");
    host.connect_gate.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("要结束")
        .expect("任务不 panic");
    assert!(result.expect_err("目标已移出").contains("移出"));
    assert_eq!(host.sent.load(Ordering::SeqCst), 0, "命令不能发出去");
}

#[tokio::test]
async fn removing_the_target_while_probing_the_shell_never_writes_to_the_terminal() {
    // R-9 F11：exec_placement 正在探测 shell 时目标被移出：不写终端，抢到的终端要还回去。
    let host = Arc::new(CancellableHost::new());
    host.hold_placement.store(true, Ordering::SeqCst);
    let bridge = auto_bridge(host.clone());
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let run = spawn_run(&bridge, &handle.scope);
    host.placing.acquire().await.expect("要开始探测").forget();
    handle.remove_target("s1");
    host.placement_gate.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("要结束")
        .expect("任务不 panic");
    assert!(result.expect_err("目标已移出").contains("移出"));
    assert_eq!(host.terminal_runs.load(Ordering::SeqCst), 0, "不能写终端");
    assert_eq!(host.sent.load(Ordering::SeqCst), 0, "也不能换后台通道执行");
    assert_eq!(host.released.load(Ordering::SeqCst), 1, "终端占位要还回去");
}

#[tokio::test]
async fn stop_while_probing_the_shell_never_writes_to_the_terminal() {
    let host = Arc::new(CancellableHost::new());
    host.hold_placement.store(true, Ordering::SeqCst);
    let bridge = auto_bridge(host.clone());
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let run = spawn_run(&bridge, &handle.scope);
    host.placing.acquire().await.expect("要开始探测").forget();
    bridge.stop_scope(&handle.scope.id);
    host.placement_gate.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("要结束")
        .expect("任务不 panic");
    assert!(result.expect_err("停止了").contains("停止"));
    assert_eq!(host.terminal_runs.load(Ordering::SeqCst), 0);
    assert_eq!(host.sent.load(Ordering::SeqCst), 0);
    assert_eq!(host.released.load(Ordering::SeqCst), 1);
}

/// 2026-11-11 审查：停止只作废"停止之前"开始的请求，外部智能体还没收到中断时发来的新调用
/// 会领到新代次、在"全部放行"下照常执行。停止后要进入"已停止"态，拒绝一切新调用，
/// 直到用户下一次发消息。
#[tokio::test]
async fn tool_calls_arriving_after_stop_are_refused_until_the_user_sends_again() {
    let host = Arc::new(CancellableHost::new());
    // 走终端通道：后台通道的替身要等取消标志才返回。
    host.hold_placement.store(true, Ordering::SeqCst);
    host.placement_gate.add_permits(4);
    let bridge = auto_bridge(host.clone());
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let params = json!({ "target_id": "s1", "command": "ls /tmp" });
    let executed = |host: &CancellableHost| {
        host.terminal_runs.load(Ordering::SeqCst) + host.sent.load(Ordering::SeqCst)
    };

    bridge.stop_scope(handle.scope_id());
    for (method, params) in [
        ("run_command", params.clone()),
        ("list_targets", json!({})),
        ("read_terminal", json!({ "target_id": "s1" })),
    ] {
        let outcome = bridge.call_tool(handle.capability(), method, params).await;
        assert!(outcome.is_error, "停止之后的 {method} 要被拒绝");
        assert!(
            outcome.text.contains("停止") && outcome.text.contains("不要重试"),
            "要明确告诉智能体别重试：{}",
            outcome.text
        );
    }
    assert_eq!(executed(&host), 0, "停止之后的命令不能执行");

    // 用户再发消息：恢复
    bridge.resume_scope(handle.scope_id());
    let outcome = bridge
        .call_tool(handle.capability(), "run_command", params)
        .await;
    assert!(!outcome.is_error, "恢复之后要能执行：{}", outcome.text);
    assert_eq!(executed(&host), 1);
}

/// 智能体 session 换密钥时撤销旧密钥，不能让整个对话进入"已停止"态。
#[tokio::test]
async fn revoking_a_session_capability_does_not_halt_the_conversation() {
    let host = Arc::new(CancellableHost::new());
    let bridge = auto_bridge(host.clone());
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let old = handle.agent_capability();
    let new = handle.agent_capability();
    old.revoke();
    let outcome = bridge
        .call_tool(new.value(), "list_targets", json!({}))
        .await;
    assert!(!outcome.is_error, "新密钥照常可用：{}", outcome.text);
}

#[tokio::test]
async fn stop_closes_a_running_background_command() {
    let host = Arc::new(CancellableHost::new());
    let bridge = Arc::new(Bridge::new(
        host.clone(),
        Arc::new(MockAudit::default()),
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
        Arc::new(FixedMode(ExecMode::Auto)),
    ));
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let scope = Arc::clone(&handle.scope);
    let run = {
        let bridge = Arc::clone(&bridge);
        let scope = Arc::clone(&scope);
        tokio::spawn(async move {
            bridge
                .run_command(
                    &scope,
                    json!({ "target_id": "s1", "command": "sleep 600" }),
                    "t-1",
                )
                .await
        })
    };
    host.started.acquire().await.expect("命令要开始").forget();
    bridge.stop_scope(&scope.id);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("停止后后台命令要立刻结束")
        .expect("任务不 panic");
    let message = result.expect_err("停止的命令不能当成正常完成");
    assert!(message.contains("停止"), "{message}");
    assert!(message.contains("执行通道已关闭"), "{message}");
}

#[tokio::test]
async fn removing_the_target_does_not_kill_a_command_that_is_already_running() {
    // 切换终端标签会把原来的会话移出对话：已经在跑的后台命令不能因此被关掉通道。
    let host = Arc::new(CancellableHost::new());
    let bridge = Arc::new(Bridge::new(
        host.clone(),
        Arc::new(MockAudit::default()),
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
        Arc::new(FixedMode(ExecMode::Auto)),
    ));
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let scope = Arc::clone(&handle.scope);
    let run = {
        let bridge = Arc::clone(&bridge);
        let scope = Arc::clone(&scope);
        tokio::spawn(async move {
            bridge
                .run_command(
                    &scope,
                    json!({ "target_id": "s1", "command": "sleep 600" }),
                    "t-1",
                )
                .await
        })
    };
    host.started.acquire().await.expect("命令要开始").forget();
    handle.remove_target("s1");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!run.is_finished(), "移出目标不该打断已经在跑的命令");
    // 停止仍然能让它结束。
    bridge.stop_scope(&scope.id);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("停止后要结束")
        .expect("任务不 panic");
    assert!(result.expect_err("停止的命令").contains("停止"));
}

/// 后台执行的发命令边界（ssh 层）拿 ExecGate 当 ExecGuard 用：开通道期间目标被移出，
/// `invalidated` 要醒、`check` 要拒（审计 C04：以前只传停止标志，移出拦不住）。
#[tokio::test]
async fn removing_the_target_while_the_channel_opens_wakes_the_exec_guard() {
    use crate::ssh::connection::ExecGuard;
    let bridge = auto_bridge(Arc::new(CancellableHost::new()));
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let gate = ExecGate::new(&handle.scope, &handle.scope.ticket("s1"));
    let guard: &dyn ExecGuard = &gate;
    assert_eq!(guard.check(), Ok(()));
    let waiting = {
        let gate = gate.clone();
        tokio::spawn(async move { ExecGuard::invalidated(&gate).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!waiting.is_finished(), "还有效时不能醒");
    handle.remove_target("s1");
    let reason = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("移出目标要叫醒等开通道的一方")
        .expect("不 panic");
    assert!(reason.contains("移出"), "{reason}");
    assert!(guard.check().expect_err("已移出").contains("移出"));
    assert!(
        !guard.stop_requested(),
        "移出目标不算停止：已经发出去的命令不因此关通道"
    );
}

#[tokio::test]
async fn stop_while_the_channel_opens_wakes_the_exec_guard_and_requests_a_stop() {
    use crate::ssh::connection::ExecGuard;
    let bridge = auto_bridge(Arc::new(CancellableHost::new()));
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let gate = ExecGate::new(&handle.scope, &handle.scope.ticket("s1"));
    let waiting = {
        let gate = gate.clone();
        tokio::spawn(async move { ExecGuard::invalidated(&gate).await })
    };
    tokio::time::sleep(Duration::from_millis(30)).await;
    bridge.stop_scope(&handle.scope.id);
    let reason = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("停止要叫醒等开通道的一方")
        .expect("不 panic");
    assert!(reason.contains("停止"), "{reason}");
    // 取消标志还没被 run_background 置位，也要算停止。
    assert!(!gate.cancel_flag().load(Ordering::SeqCst));
    assert!(ExecGuard::stop_requested(&gate));
}

#[tokio::test]
async fn a_terminal_write_interrupted_by_closing_the_session_is_not_rerun_in_the_background() {
    // 会话任务认领了 AI 命令、写到一半会话被关：命令可能已经进了终端，不能退回后台通道
    // 再执行一遍。
    let host = Arc::new(CancellableHost::new());
    host.hold_placement.store(true, Ordering::SeqCst);
    host.placement_gate.add_permits(1);
    *host.terminal_error.lock().expect("锁") = Some(terminal_write::WRITE_INTERRUPTED.to_owned());
    let bridge = auto_bridge(host.clone());
    let handle = bridge.create_scope();
    handle.add_target("s1", "web-1", "root", "connected", true, false);
    let run = spawn_run(&bridge, &handle.scope);
    let result = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("要结束")
        .expect("任务不 panic");
    assert_eq!(
        result.expect_err("写到一半"),
        terminal_write::WRITE_INTERRUPTED
    );
    assert_eq!(host.sent.load(Ordering::SeqCst), 0, "不能换后台通道重跑");
}

#[tokio::test]
async fn many_authenticated_connections_are_all_served() {
    // 握手名额只限制没认证的连接：claude 每个对话一个 zssh-mcp，多开对话不能被拒。
    let h = harness(
        ExecMode::Auto,
        ApproverBehavior::Allow,
        Duration::from_secs(1),
    )
    .await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut clients = Vec::new();
    for _ in 0..(MAX_PENDING_HANDSHAKES + 4) {
        let mut client =
            Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
        let response = client.request(1, METHOD_LIST_TARGETS, json!({})).await;
        assert!(result_of(&response).is_object(), "{response}");
        clients.push(client);
    }
    for client in &mut clients {
        let response = client.request(2, METHOD_LIST_TARGETS, json!({})).await;
        assert!(result_of(&response).is_object(), "{response}");
    }
}

#[tokio::test]
async fn reconnecting_does_not_grant_a_fresh_request_budget() {
    // R-9 F15：每条连接各有上限还不够——在几条连接上各压满待审批的请求、断开、再连，
    // 旧请求还在等审批，新连接不能再拿到额度。
    let h = harness(
        ExecMode::Manual,
        ApproverBehavior::Never,
        Duration::from_secs(60),
    )
    .await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let per_connection = MAX_IN_FLIGHT_PER_CONNECTION;
    let connections = MAX_IN_FLIGHT_TOTAL / per_connection;
    for round in 0..connections {
        let mut client =
            Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
        for i in 0..per_connection {
            client
                .send(&json!({
                    "id": i + 1,
                    "method": METHOD_RUN_COMMAND,
                    "params": { "target_id": "s1", "command": "ls" },
                    "capability": scope.capability(),
                }))
                .await;
        }
        let expected = (round + 1) * per_connection;
        tokio::time::timeout(Duration::from_secs(10), async {
            while h.approver.calls() < expected {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("这一批请求都要挂在审批上");
        // 断开：请求还在等审批，名额不能跟着连接一起还回去。
        drop(client);
    }
    let mut fresh =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    let response = fresh.request(999, METHOD_LIST_TARGETS, json!({})).await;
    assert!(
        error_of(&response).contains("太多"),
        "全局名额用完时新连接也要被拒：{response}"
    );

    // 作用范围销毁后挂着的请求都结束，名额回来了。
    scope.destroy();
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    fresh.capability = scope.capability().to_string();
    let response = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let response = fresh.request(1000, METHOD_LIST_TARGETS, json!({})).await;
            if response.get("error").is_none() || !error_of(&response).contains("太多") {
                return response;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("名额要还回来");
    assert!(result_of(&response).is_object(), "{response}");
}

#[tokio::test]
async fn bounded_line_reads_stop_at_the_limit() {
    async fn read(input: &[u8], max: usize) -> BoundedLine {
        let mut reader = BufReader::new(input);
        read_bounded_line(&mut reader, max).await.expect("读")
    }
    assert_eq!(read(b"abc\nrest", 8).await, BoundedLine::Line("abc".into()));
    assert_eq!(read(b"", 8).await, BoundedLine::Eof);
    assert_eq!(
        read(b"12345678\n", 8).await,
        BoundedLine::Line("12345678".into())
    );
    assert_eq!(read(b"123456789\n", 8).await, BoundedLine::TooLong);
    assert_eq!(read(b"tail", 8).await, BoundedLine::Line("tail".into()));
}

#[tokio::test]
async fn an_oversized_handshake_line_closes_the_connection() {
    let h = harness(
        ExecMode::Auto,
        ApproverBehavior::Allow,
        Duration::from_secs(1),
    )
    .await;
    let stream = TcpStream::connect(("127.0.0.1", h.port))
        .await
        .expect("连上桥");
    let (reader, mut writer) = stream.into_split();
    // 没有换行、比握手上限大得多的一串：服务端读到上限就该断开，而不是一直攒。
    let junk = vec![b'a'; HANDSHAKE_MAX_BYTES * 4];
    let _ = writer.write_all(&junk).await;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("服务端要很快断开");
    assert!(matches!(read, Ok(0) | Err(_)), "连接应已关闭：{read:?}");
}

// ---- AI-SCOPE-2：一台主机一个逻辑目标、绑定会话、回报实际执行位置 ----

/// 能区分"绑定的会话"和"这台主机别的会话"的替身宿主：记录每次执行落在哪个会话、
/// 收到的执行位置回报；`alive` 里没有的会话算已经关了。
struct BindingHost {
    alive: Mutex<std::collections::HashSet<String>>,
    /// 目标 ID → 这台主机已经连上的会话（没有绑定时的后备）。
    host_sessions: Mutex<HashMap<String, String>>,
    exec_sessions: Mutex<Vec<String>>,
    read_sessions: Mutex<Vec<String>>,
    reports: Mutex<Vec<ExecTargetReport>>,
    /// 为真时 `execution_session` 进入后停住，等 `release` 才返回（模拟找会话 / 等连接）。
    block: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl BindingHost {
    fn new(alive: &[&str], host_sessions: &[(&str, &str)]) -> Arc<BindingHost> {
        Arc::new(BindingHost {
            alive: Mutex::new(alive.iter().map(|s| (*s).to_owned()).collect()),
            host_sessions: Mutex::new(
                host_sessions
                    .iter()
                    .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                    .collect(),
            ),
            exec_sessions: Mutex::new(Vec::new()),
            read_sessions: Mutex::new(Vec::new()),
            reports: Mutex::new(Vec::new()),
            block: AtomicBool::new(false),
            entered: Notify::new(),
            release: Notify::new(),
        })
    }

    fn close(&self, session: &str) {
        self.alive.lock().unwrap().remove(session);
    }

    fn reports(&self) -> Vec<ExecTargetReport> {
        self.reports.lock().unwrap().clone()
    }
}

impl BridgeHost for BindingHost {
    fn execution_session<'a>(
        &'a self,
        target_id: &'a str,
        bound: Option<&'a str>,
    ) -> BoxFuture<'a, Option<String>> {
        let alive = self.alive.lock().unwrap();
        let session = bound
            .filter(|id| alive.contains(*id))
            .map(str::to_owned)
            .or_else(|| alive.contains(target_id).then(|| target_id.to_owned()))
            .or_else(|| self.host_sessions.lock().unwrap().get(target_id).cloned());
        drop(alive);
        Box::pin(async move {
            if self.block.load(Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            session
        })
    }

    fn report_target(&self, report: ExecTargetReport) {
        self.reports.lock().unwrap().push(report);
    }

    fn exec<'a>(
        &'a self,
        session_id: &'a str,
        _command: &'a str,
        _wait: Duration,
        _max_stream_bytes: usize,
    ) -> BoxFuture<'a, Result<ExecCapture, String>> {
        self.exec_sessions
            .lock()
            .unwrap()
            .push(session_id.to_owned());
        Box::pin(async { Ok(exec_ok()) })
    }

    fn read_terminal<'a>(
        &'a self,
        session_id: &'a str,
        _max_lines: usize,
    ) -> BoxFuture<'a, Result<Vec<String>, String>> {
        self.read_sessions
            .lock()
            .unwrap()
            .push(session_id.to_owned());
        Box::pin(async { Ok(vec!["line".to_owned()]) })
    }
}

fn binding_bridge(
    host: Arc<BindingHost>,
    mode: ExecMode,
    approver: Arc<MockApprover>,
) -> Arc<Bridge> {
    Arc::new(Bridge::new(
        host,
        Arc::new(MockAudit::default()),
        approver,
        Arc::new(FixedMode(mode)),
    ))
}

fn host_target(id: &str, label: &str, current: bool, session: Option<&str>) -> TargetSummary {
    TargetSummary {
        id: id.to_owned(),
        host_label: label.to_owned(),
        username: "root".to_owned(),
        status: if session.is_some() {
            "connected"
        } else {
            "pinned"
        }
        .to_owned(),
        current,
        pinned: false,
        session_id: session.map(str::to_owned),
    }
}

async fn serve(bridge: &Arc<Bridge>) -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("绑定端口");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(Arc::clone(bridge).serve(listener));
    port
}

#[tokio::test]
async fn run_command_executes_on_the_bound_session_and_falls_back_after_it_closes() {
    // 这台主机开了两个终端：sess-1 是别的，sess-2 是界面绑定的（焦点所在）。
    let host = BindingHost::new(&["sess-1", "sess-2"], &[("host-7", "sess-1")]);
    let bridge = binding_bridge(
        host.clone(),
        ExecMode::Auto,
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
    );
    let scope = bridge.create_scope();
    scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-2")));
    let port = serve(&bridge).await;
    let mut client =
        Client::authenticated(port, bridge.connection_token(), scope.capability()).await;

    let response = client
        .request(1, METHOD_RUN_COMMAND, json!({ "command": "ls" }))
        .await;
    result_of(&response);
    assert_eq!(
        *host.exec_sessions.lock().unwrap(),
        vec!["sess-2".to_owned()],
        "有绑定会话就落在它上面，不用这台主机别的会话"
    );
    let reports = host.reports();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].tool, "run_command");
    assert_eq!(reports[0].key, "ls");
    assert_eq!(reports[0].target_id, "host-7");
    assert_eq!(reports[0].session_id.as_deref(), Some("sess-2"));
    assert_eq!(reports[0].host_label, "web-1");

    // 绑定的会话关了（界面还没来得及同步）：回退到这台主机别的已连会话。
    host.close("sess-2");
    let response = client
        .request(2, METHOD_RUN_COMMAND, json!({ "command": "ls" }))
        .await;
    result_of(&response);
    assert_eq!(host.exec_sessions.lock().unwrap()[1], "sess-1");
    assert_eq!(host.reports()[1].session_id.as_deref(), Some("sess-1"));

    // 一个会话都没有：交给宿主按逻辑目标自己连（回报里会话为空）。
    host.close("sess-1");
    host.host_sessions.lock().unwrap().clear();
    let response = client
        .request(3, METHOD_RUN_COMMAND, json!({ "command": "ls" }))
        .await;
    result_of(&response);
    assert_eq!(host.exec_sessions.lock().unwrap()[2], "host-7");
    assert_eq!(host.reports()[2].session_id, None);
    assert_eq!(host.reports()[2].target_id, "host-7");
}

#[tokio::test]
async fn the_binding_can_move_without_removing_the_target() {
    // 焦点换到这台主机的另一个终端：界面再同步一次同一个目标（绑定变了），在途请求的
    // 票据不受影响（不算移出再加回），新请求落在新绑定的会话上。
    let host = BindingHost::new(&["sess-1", "sess-2"], &[]);
    let bridge = binding_bridge(
        host.clone(),
        ExecMode::Auto,
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
    );
    let scope = bridge.create_scope();
    scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-1")));
    let ticket = scope.scope.ticket("host-7");
    scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-2")));
    assert_eq!(scope.scope.ticket_refusal(&ticket), None);

    let port = serve(&bridge).await;
    let mut client =
        Client::authenticated(port, bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(1, METHOD_READ_TERMINAL, json!({ "target_id": "host-7" }))
        .await;
    result_of(&response);
    assert_eq!(
        *host.read_sessions.lock().unwrap(),
        vec!["sess-2".to_owned()]
    );
}

#[tokio::test]
async fn without_a_current_target_the_chosen_one_runs_and_is_reported() {
    // 切到空白标签：两个目标都不是"当前"，模型省略 target_id —— 选哪个的规则不变
    // （列表第一项），但选中的是谁要回报出去，且回报的会话就是实际执行的会话。
    let host = BindingHost::new(&["sess-a", "sess-b"], &[]);
    let bridge = binding_bridge(
        host.clone(),
        ExecMode::Auto,
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
    );
    let scope = bridge.create_scope();
    scope.add_target_summary(host_target("host-2", "db-1", false, Some("sess-b")));
    scope.add_target_summary(host_target("host-1", "web-1", false, Some("sess-a")));
    let port = serve(&bridge).await;
    let mut client =
        Client::authenticated(port, bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(1, METHOD_RUN_COMMAND, json!({ "command": "uptime" }))
        .await;
    result_of(&response);
    let reports = host.reports();
    assert_eq!(reports.len(), 1);
    let (label, session) = match reports[0].target_id.as_str() {
        "host-1" => ("web-1", "sess-a"),
        "host-2" => ("db-1", "sess-b"),
        other => panic!("不认识的目标：{other}"),
    };
    assert_eq!(reports[0].host_label, label);
    assert_eq!(reports[0].session_id.as_deref(), Some(session));
    assert_eq!(
        *host.exec_sessions.lock().unwrap(),
        vec![session.to_owned()],
        "回报的会话就是实际执行的会话"
    );
}

#[tokio::test]
async fn an_approval_carries_the_resolved_target_and_session() {
    let host = BindingHost::new(&["sess-2"], &[]);
    let approver = Arc::new(MockApprover::new(ApproverBehavior::Allow));
    let bridge = binding_bridge(host.clone(), ExecMode::Manual, approver.clone());
    let scope = bridge.create_scope();
    scope.add_target_summary(host_target("host-7", "web-1", false, Some("sess-2")));
    let port = serve(&bridge).await;
    let mut client =
        Client::authenticated(port, bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(
            1,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "host-7", "command": "ls" }),
        )
        .await;
    result_of(&response);
    let request = approver.last().expect("手动模式要审批");
    assert_eq!(request.target_id, "host-7");
    assert_eq!(request.session_id.as_deref(), Some("sess-2"));
    assert_eq!(host.reports()[0].session_id.as_deref(), Some("sess-2"));
}

#[tokio::test]
async fn every_tool_call_gets_its_own_exec_id_in_the_report_and_the_result() {
    let host = BindingHost::new(&["sess-1", "sess-2"], &[]);
    let bridge = binding_bridge(
        host.clone(),
        ExecMode::Auto,
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
    );
    let scope = bridge.create_scope();
    scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-1")));
    let port = serve(&bridge).await;
    let mut client =
        Client::authenticated(port, bridge.connection_token(), scope.capability()).await;

    // 同一条命令（都省略 target_id）调两次，中间切换绑定会话：两次各有自己的编号。
    let first = client
        .request(1, METHOD_RUN_COMMAND, json!({ "command": "uptime" }))
        .await;
    scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-2")));
    let second = client
        .request(2, METHOD_RUN_COMMAND, json!({ "command": "uptime" }))
        .await;
    let terminal = client.request(3, METHOD_READ_TERMINAL, json!({})).await;

    let reports = host.reports();
    assert_eq!(reports.len(), 3);
    let ids: Vec<&str> = [&first, &second, &terminal]
        .iter()
        .map(|response| {
            result_of(response)["exec_id"]
                .as_str()
                .expect("结果里要有执行编号")
        })
        .collect();
    for (report, id) in reports.iter().zip(&ids) {
        assert_eq!(&report.exec_id, id, "回报和结果里的编号一致");
    }
    assert_eq!(reports[0].session_id.as_deref(), Some("sess-1"));
    assert_eq!(reports[1].session_id.as_deref(), Some("sess-2"));
    assert_ne!(ids[0], ids[1], "同一条命令的两次调用编号不同");
    assert_ne!(ids[1], ids[2]);
    // 编号只含十六进制和连字符：能安全地放进一行标记。
    assert!(ids
        .iter()
        .all(|id| id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')));

    // 不带目标的方法没有编号。
    let list = client.request(4, METHOD_LIST_TARGETS, json!({})).await;
    assert!(result_of(&list).get("exec_id").is_none());
}

#[tokio::test]
async fn a_refused_call_still_returns_its_exec_id_and_the_approval_carries_it() {
    let host = BindingHost::new(&["sess-2"], &[]);
    let approver = Arc::new(MockApprover::new(ApproverBehavior::Deny));
    let bridge = binding_bridge(host.clone(), ExecMode::Manual, approver.clone());
    let scope = bridge.create_scope();
    scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-2")));
    let port = serve(&bridge).await;
    let mut client =
        Client::authenticated(port, bridge.connection_token(), scope.capability()).await;
    let response = client
        .request(1, METHOD_RUN_COMMAND, json!({ "command": "rm -rf /tmp/x" }))
        .await;
    let error = response.get("error").expect("被拒绝要报错");
    let exec_id = error["exec_id"].as_str().expect("错误里也带执行编号");
    assert_eq!(host.reports()[0].exec_id, exec_id);
    assert_eq!(
        approver.last().expect("手动模式要审批").exec_id,
        exec_id,
        "审批事件、回报、错误里的编号是同一个"
    );

    // 目标不在列表里：还没解析出目标，没有回报，但错误里仍带编号（界面对不上就不理）。
    let response = client
        .request(
            2,
            METHOD_RUN_COMMAND,
            json!({ "target_id": "nope", "command": "ls" }),
        )
        .await;
    assert!(response["error"]["exec_id"].is_string(), "{response}");
    assert_eq!(host.reports().len(), 1);
}

#[tokio::test]
async fn the_bound_session_is_not_shown_to_the_model() {
    let host = BindingHost::new(&["sess-2"], &[]);
    let bridge = binding_bridge(
        host,
        ExecMode::Manual,
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
    );
    let scope = bridge.create_scope();
    scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-2")));
    let port = serve(&bridge).await;
    let mut client =
        Client::authenticated(port, bridge.connection_token(), scope.capability()).await;
    let response = client.request(1, METHOD_LIST_TARGETS, json!({})).await;
    let text = result_of(&response).to_string();
    assert!(text.contains("host-7"));
    assert!(!text.contains("sess-2"), "绑定的会话 ID 不给模型看：{text}");
}

// ---- 解析会话期间的取消（停止 / 目标移出再加回）----

/// 发一条请求，停在宿主 `execution_session` 里时对作用范围做 `disturb`，再放行；返回响应。
async fn request_disturbed_while_resolving(
    method: &'static str,
    params: Value,
    disturb: impl FnOnce(&ScopeHandle, &Arc<Bridge>),
) -> (
    Result<Value, String>,
    Arc<BindingHost>,
    Vec<ExecTargetReport>,
) {
    let host = BindingHost::new(&["sess-2"], &[]);
    host.block.store(true, Ordering::SeqCst);
    let bridge = binding_bridge(
        host.clone(),
        ExecMode::Auto,
        Arc::new(MockApprover::new(ApproverBehavior::Allow)),
    );
    let scope = bridge.create_scope();
    scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-2")));
    let request = RequestEnvelope {
        id: 1,
        method: method.to_owned(),
        capability: scope.capability().to_owned(),
        params,
    };
    let task = {
        let bridge = Arc::clone(&bridge);
        tokio::spawn(async move { bridge.dispatch(&request).await })
    };
    tokio::time::timeout(Duration::from_secs(5), host.entered.notified())
        .await
        .expect("请求应该走到解析会话");
    disturb(&scope, &bridge);
    host.release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("请求应该结束")
        .expect("任务不该 panic");
    let reports = host.reports();
    (result, host, reports)
}

#[tokio::test]
async fn a_stop_while_the_session_is_being_resolved_prevents_execution() {
    for (method, params) in [
        (METHOD_RUN_COMMAND, json!({ "command": "touch /tmp/x" })),
        (METHOD_READ_TERMINAL, json!({})),
    ] {
        let (result, host, reports) =
            request_disturbed_while_resolving(method, params, |scope, bridge| {
                bridge.stop_scope(scope.scope_id());
            })
            .await;
        let message = result.expect_err("停止之后不能执行");
        assert!(message.contains("停止"), "{method}：{message}");
        assert!(host.exec_sessions.lock().unwrap().is_empty(), "{method}");
        assert!(host.read_sessions.lock().unwrap().is_empty(), "{method}");
        assert!(reports.is_empty(), "已经作废的请求不再回报：{method}");
    }
}

#[tokio::test]
async fn removing_and_re_adding_the_target_while_resolving_still_invalidates_the_request() {
    for (method, params) in [
        (METHOD_RUN_COMMAND, json!({ "command": "touch /tmp/x" })),
        (METHOD_READ_TERMINAL, json!({})),
    ] {
        let (result, host, _) = request_disturbed_while_resolving(method, params, |scope, _| {
            scope.remove_target("host-7");
            scope.add_target_summary(host_target("host-7", "web-1", true, Some("sess-2")));
        })
        .await;
        let message = result.expect_err("移出再加回之后旧请求仍然无效");
        assert!(message.contains("移出"), "{method}：{message}");
        assert!(host.exec_sessions.lock().unwrap().is_empty(), "{method}");
        assert!(host.read_sessions.lock().unwrap().is_empty(), "{method}");
    }
}

#[tokio::test]
async fn without_a_disturbance_the_same_request_executes_once() {
    let (result, host, reports) = request_disturbed_while_resolving(
        METHOD_RUN_COMMAND,
        json!({ "command": "ls" }),
        |_, _| {},
    )
    .await;
    result.expect("没有干扰就正常执行");
    assert_eq!(
        *host.exec_sessions.lock().unwrap(),
        vec!["sess-2".to_owned()]
    );
    assert_eq!(reports.len(), 1);
}

// ---------- 进程内调用（内置助手，V2-A3）----------
//
// 内置助手不起 zssh-mcp，直接 `Bridge::call_tool`。它和 IPC 路径共用 `handle_request`，
// 下面的测试证明：同样的输入得到同样的文本，审批 / 执行模式 / 只读 / 目标校验 / 审计一个
// 不少，被拒绝时不会执行。

fn strip_marker(text: &str) -> String {
    text.lines()
        .filter(|line| !line.starts_with("[zssh exec "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn in_process_call_gives_the_same_text_as_the_mcp_path_for_every_tool() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let mut client =
        Client::authenticated(h.port, h.bridge.connection_token(), scope.capability()).await;
    for (index, (method, params)) in [
        (METHOD_LIST_TARGETS, json!({})),
        (METHOD_RUN_COMMAND, json!({ "command": "echo hi" })),
        (METHOD_READ_TERMINAL, json!({ "max_lines": 5 })),
    ]
    .into_iter()
    .enumerate()
    {
        let response = client
            .request(index as u64 + 1, method, params.clone())
            .await;
        let via_ipc = crate::ai::tool_text::format_tool_result(method, result_of(&response));
        let in_process = h.bridge.call_tool(scope.capability(), method, params).await;
        assert!(!in_process.is_error, "{method}：{}", in_process.text);
        assert_eq!(
            strip_marker(&in_process.text),
            strip_marker(&via_ipc),
            "{method}"
        );
        assert_eq!(
            in_process.exec_id.is_some(),
            method_carries_exec_id(method),
            "{method}"
        );
    }
}

#[tokio::test]
async fn in_process_manual_mode_asks_and_a_denial_never_executes() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let outcome = h
        .bridge
        .call_tool(
            scope.capability(),
            METHOD_RUN_COMMAND,
            json!({ "command": "rm -rf /tmp/x" }),
        )
        .await;
    assert!(outcome.is_error);
    assert!(outcome.text.contains("用户拒绝"), "{}", outcome.text);
    assert!(
        outcome.text.contains("[zssh exec "),
        "失败也带编号：{}",
        outcome.text
    );
    assert_eq!(h.approver.calls(), 1, "手动模式必须问");
    assert!(h.host.last_exec().is_none(), "拒绝后不能执行");
    let audit = h.audit.entries();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].decision, ExecDecision::Denied);
    assert_eq!(audit[0].command, "rm -rf /tmp/x");
}

#[tokio::test]
async fn in_process_manual_mode_runs_after_approval_and_audits() {
    let h = harness(ExecMode::Manual, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let outcome = h
        .bridge
        .call_tool(
            scope.capability(),
            METHOD_RUN_COMMAND,
            json!({ "command": "ls" }),
        )
        .await;
    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("exit_code: 0"), "{}", outcome.text);
    assert_eq!(h.approver.calls(), 1);
    assert_eq!(h.host.last_exec().map(|e| e.1), Some("ls".to_owned()));
    assert_eq!(h.audit.entries()[0].decision, ExecDecision::Allowed);
}

#[tokio::test]
async fn in_process_readonly_mode_only_auto_allows_read_only_commands() {
    let h = harness(ExecMode::ReadOnly, ApproverBehavior::Deny, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let read = h
        .bridge
        .call_tool(
            scope.capability(),
            METHOD_RUN_COMMAND,
            json!({ "command": "ls -la" }),
        )
        .await;
    assert!(!read.is_error, "只读命令自动放行：{}", read.text);
    assert_eq!(h.approver.calls(), 0);
    // 写命令仍要问（这里被拒）。
    let write = h
        .bridge
        .call_tool(
            scope.capability(),
            METHOD_RUN_COMMAND,
            json!({ "command": "touch /tmp/x" }),
        )
        .await;
    assert!(
        write.is_error && write.text.contains("用户拒绝"),
        "{}",
        write.text
    );
    assert_eq!(h.approver.calls(), 1);
}

#[tokio::test]
async fn in_process_calls_are_checked_against_capability_scope_and_refusals() {
    let h = harness(ExecMode::Auto, ApproverBehavior::Allow, short_wait()).await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    scope.add_target("s9", "serial-1", "", "connected", false, false);
    h.host
        .refusals
        .lock()
        .unwrap()
        .insert("s9".to_owned(), "这是一台串口主机".to_owned());

    let bad_key = h
        .bridge
        .call_tool("deadbeef", METHOD_LIST_TARGETS, json!({}))
        .await;
    assert!(
        bad_key.is_error && bad_key.text.contains("能力密钥无效"),
        "{}",
        bad_key.text
    );
    let missing = h.bridge.call_tool("", METHOD_LIST_TARGETS, json!({})).await;
    assert!(
        missing.text.contains("缺少对话能力密钥"),
        "{}",
        missing.text
    );

    let outside = h
        .bridge
        .call_tool(
            scope.capability(),
            METHOD_RUN_COMMAND,
            json!({ "command": "ls", "target_id": "not-mine" }),
        )
        .await;
    assert!(
        outside.is_error && outside.text.contains("不属于本次对话"),
        "{}",
        outside.text
    );
    let refused = h
        .bridge
        .call_tool(
            scope.capability(),
            METHOD_RUN_COMMAND,
            json!({ "command": "ls", "target_id": "s9" }),
        )
        .await;
    assert!(
        refused.is_error && refused.text.contains("串口"),
        "{}",
        refused.text
    );
    let control = h
        .bridge
        .call_tool(
            scope.capability(),
            METHOD_RUN_COMMAND,
            json!({ "command": "ls\u{15}rm" }),
        )
        .await;
    assert!(control.is_error, "控制字符照样拒绝：{}", control.text);
    assert!(h.host.last_exec().is_none(), "这些调用一个都不能执行");

    let unknown = h
        .bridge
        .call_tool(scope.capability(), "format_disk", json!({}))
        .await;
    assert!(
        unknown.is_error && unknown.text.contains("未知方法"),
        "{}",
        unknown.text
    );

    // 作用范围销毁之后，进程内调用同样失效。
    scope.destroy();
    let dead = h
        .bridge
        .call_tool(scope.capability(), METHOD_LIST_TARGETS, json!({}))
        .await;
    assert!(dead.is_error, "{}", dead.text);
}

#[tokio::test]
async fn in_process_stop_ends_a_pending_approval_as_a_denial() {
    let h = harness(
        ExecMode::Manual,
        ApproverBehavior::Never,
        Duration::from_secs(30),
    )
    .await;
    let scope = h.bridge.create_scope();
    scope.add_target("s1", "web-1", "root", "connected", true, false);
    let bridge = Arc::clone(&h.bridge);
    let capability = scope.capability().to_owned();
    let call = tokio::spawn(async move {
        bridge
            .call_tool(
                &capability,
                METHOD_RUN_COMMAND,
                json!({ "command": "sleep 5" }),
            )
            .await
    });
    // 等它停在审批上。
    for _ in 0..100 {
        if h.approver.calls() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.approver.calls(), 1);
    h.bridge.stop_scope(scope.scope_id());
    let outcome = tokio::time::timeout(Duration::from_secs(5), call)
        .await
        .expect("停止后应很快结束")
        .unwrap();
    assert!(
        outcome.is_error && outcome.text.contains("停止"),
        "{}",
        outcome.text
    );
    assert!(h.host.last_exec().is_none());
}
