//! 内置助手的对话循环（V2-A3）：实现 [`AgentConversation`]，直接和第三方模型接口对话。
//!
//! 一个回合 = 循环 { 构造请求（系统提示 + 历史 + 工具清单）→ 流式拿回复 → 有工具调用就逐个交给
//! 工具桥执行（进程内，走和外部智能体同一个入口 [`Bridge::call_tool`]）→ 结果回填 → 继续 }，
//! 直到模型不再调工具，或者本回合的工具调用到了 [`BuiltinTuning::max_tool_calls`] 次。
//!
//! # 事件
//!
//! 界面已经认的 [`AgentEvent`]：正文 / 思考增量、`ToolStart` / `ToolEnd`、`Retrying`、`Error`、
//! `TurnEnd`、`ResumeFailed`。`AgentEvent` 里没有"用量""提示"这类事件（加变体会让界面里穷尽的
//! `match` 编译不过，界面是 A4 的事），所以这两样走另一条可选的旁路 [`BuiltinSignal`]。
//!
//! # 停止
//!
//! [`stop`](AgentConversation::stop)：丢掉进行中的流（连接随之关闭）；正在等批准 / 正在执行的工具
//! 交给工具桥的现有规则（`Bridge::stop_scope` 让等待中的审批按拒绝结束、后台命令关通道，
//! `interrupt_terminal` 给终端里跑着的命令发 Ctrl+C）；回合照常以 `TurnEnd` 结束。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

use super::catalog::{resolve, Catalog, ModelCaps, PartialCaps};
use super::config::AiEndpoint;
use super::context::{self, TOOL_RESULT_LIMIT};
use super::mapping;
use super::prompt;
use super::provider::{
    bridge_tool_specs, stream_chat, ApiKey, AssembledToolCall, AssistantTurn, ChatRequest,
    ErrorKind, Message, ProviderConfig, ProviderError, ReasoningParam, StopReason, StreamEvent,
    ToolSpec, Transport,
};
use super::transcript;
use crate::ai::agent::retry;
use crate::ai::agent::{AgentConversation, AgentError, AgentEvent, AgentModel, AgentModelCatalog};
use crate::ai::bridge::{Bridge, ScopeCapability, ScopeHandle};
use crate::ai::history::HistoryStore;
use crate::ssh::BoxFuture;

/// 历史目录里内置助手的目录名（[`crate::ai::history::AGENTS`] 里的一项）。
pub const HISTORY_AGENT: &str = "builtin";

/// 一些可调的数字（卡里定的默认值；测试里把等待时间调小）。
#[derive(Debug, Clone)]
pub struct BuiltinTuning {
    /// 重试的基础等待：第 N 次重试等 `base * 2^(N-1)`（1s、2s、4s）。
    pub retry_base_delay: Duration,
    /// 同一次请求最多重试几次。
    pub max_retries: u32,
    /// 对方给的 `retry-after` 最多等多久。
    pub retry_after_cap: Duration,
    /// 单回合最多调用几次工具。
    pub max_tool_calls: usize,
    /// 单条工具结果交给模型的字节上限。
    pub tool_result_limit: usize,
    /// 点停止后等正在执行的工具收尾多久（超过就不等了，结果丢掉）。
    pub stop_grace: Duration,
    /// 列模型（`GET /models`）最多等多久。
    pub models_timeout: Duration,
}

impl BuiltinTuning {
    /// 第 `attempt`（从 0 数）次失败之后等多久再试：对方给了 `retry-after` 就听它的（不超过上限），
    /// 否则 `base * 2^attempt`（默认 1s、2s、4s）。
    pub fn retry_delay(&self, attempt: u32, retry_after: Option<Duration>) -> Duration {
        retry_after
            .map(|wait| wait.min(self.retry_after_cap))
            .unwrap_or_else(|| self.retry_base_delay * 2u32.saturating_pow(attempt))
    }
}

impl Default for BuiltinTuning {
    fn default() -> Self {
        Self {
            retry_base_delay: Duration::from_secs(1),
            max_retries: retry::MAX_RETRIES,
            retry_after_cap: Duration::from_secs(120),
            max_tool_calls: 50,
            tool_result_limit: TOOL_RESULT_LIMIT,
            stop_grace: Duration::from_secs(10),
            models_timeout: Duration::from_secs(20),
        }
    }
}

/// 创建对话要的东西。
pub struct BuiltinParams {
    /// 接口配置（A3 的 `Database::ai_endpoint_get` / `ai_endpoint_list` 取）。
    pub endpoint: AiEndpoint,
    /// 这个接口的 Key（`Database::ai_endpoint_api_key`）；没有 Key 的接口给空。
    pub api_key: ApiKey,
    /// HTTP 层。生产用 [`super::provider::ReqwestTransport`]（A4 按"软件网络"代理设置建）。
    pub transport: Arc<dyn Transport>,
    /// 模型能力库（A1）。
    pub catalog: Arc<Catalog>,
    /// 工具桥。工具调用进程内直接走它，安全确认 / 执行模式 / 只读 / 目标校验都由它负责。
    pub bridge: Arc<Bridge>,
    /// 本对话的作用范围（目标列表在里面，界面往里加减主机）。
    pub scope: ScopeHandle,
    /// 事件出口。
    pub events: mpsc::UnboundedSender<AgentEvent>,
    /// 旁路：用量、提示、失败详情（可选）。
    pub signals: Option<mpsc::UnboundedSender<BuiltinSignal>>,
    /// 数据目录（`ZSSH_DATA_DIR` 解析好的那个）；给了就把完整对话记录存在
    /// `<数据目录>\ai-history\builtin\`，没给就不落盘（也不能续接）。
    pub data_dir: Option<std::path::PathBuf>,
    /// 续接：上次 [`AgentConversation::resume_id`] 给的值。读不到记录就退成新会话并发
    /// [`AgentEvent::ResumeFailed`]。
    pub resume_id: Option<String>,
    pub tuning: BuiltinTuning,
}

/// 旁路信号。
#[derive(Debug, Clone, PartialEq)]
pub enum BuiltinSignal {
    /// 一次请求的用量（界面的"上下文"指示器用）。
    Usage(TurnUsage),
    /// 需要告诉用户的提示（不是错误）。
    Notice(BuiltinNotice),
    /// 这个回合因为接口错误失败了（和 `AgentEvent::Error` 同时发；这里是结构化的，界面按
    /// `code` 认 `zeeterm_requires_topup` 之类，别拿文字匹配）。
    Failure(FailureInfo),
}

/// 一次请求的用量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnUsage {
    pub model: String,
    /// 这次请求喂给模型的总 token（含缓存读写）。
    pub used: u64,
    pub output: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// 模型的上下文窗口。
    pub window: u32,
    /// 窗口是估计的（识别不到、按模型家族兜底）。
    pub window_estimated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// 较早的对话被压缩成了摘要。
    Compacted,
    /// 这个模型不支持工具调用，这次只能聊天。
    NoToolSupport,
    /// 回复被输出长度上限截断了。
    OutputTruncated,
    /// 续接时发现上次有工具调用没拿到结果，已经补上。
    HistoryRepaired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinNotice {
    pub kind: NoticeKind,
    /// 直接面向用户展示的中文。
    pub text: String,
}

/// 失败详情。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureInfo {
    pub kind: ErrorKind,
    /// 服务端的错误码原文（如 `zeeterm_requires_topup`）。
    pub code: Option<String>,
    pub status: Option<u16>,
}

/// 取消开关（可以 `await`）。
struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    fn new() -> Self {
        Self {
            flag: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    /// 等到被取消为止。可以安全地被 `select!` 丢掉。
    async fn cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

struct Running {
    cancel: Arc<Cancel>,
    handle: Option<JoinHandle<()>>,
}

struct State {
    /// 模型看到的历史（已完成的回合 + 本回合到目前为止的）。
    messages: Vec<Message>,
    /// 最近一次响应之后：(消息条数, 当时上下文占用 token = 输入 + 输出)。压缩 / 截断改了历史就作废。
    anchor: Option<(usize, u64)>,
    caps: HashMap<String, ModelCaps>,
    /// `GET /models` 的结果（接口自报的能力）。
    remote: Option<HashMap<String, PartialCaps>>,
    remote_order: Vec<String>,
    running: Option<Running>,
    last_model: Option<String>,
    closed: bool,
}

struct Inner {
    session_id: String,
    endpoint: AiEndpoint,
    config: ProviderConfig,
    transport: Arc<dyn Transport>,
    catalog: Arc<Catalog>,
    bridge: Arc<Bridge>,
    scope: ScopeHandle,
    capability: Arc<ScopeCapability>,
    events: mpsc::UnboundedSender<AgentEvent>,
    signals: Option<mpsc::UnboundedSender<BuiltinSignal>>,
    history: Option<HistoryStore>,
    tuning: BuiltinTuning,
    state: Mutex<State>,
}

/// 内置助手的一个对话。
pub struct BuiltinConversation {
    inner: Arc<Inner>,
}

/// 回合怎么结束的（不含正常结束）。
enum Abort {
    Stopped,
    Failed(String),
}

/// 一次流式请求（含重试）的失败。
enum StepError {
    Stopped(String),
    Failed(ProviderError, bool),
}

impl BuiltinConversation {
    pub fn new(params: BuiltinParams) -> Self {
        let BuiltinParams {
            endpoint,
            api_key,
            transport,
            catalog,
            bridge,
            scope,
            events,
            signals,
            data_dir,
            resume_id,
            tuning,
        } = params;
        let history = data_dir.as_deref().map(HistoryStore::new);
        let config = endpoint.provider_config(api_key);

        let mut session_id = format!("bi_{}", uuid::Uuid::new_v4().simple());
        let mut messages = Vec::new();
        let mut last_model = None;
        let mut resume_failed = false;
        let mut repaired = 0;
        if let Some(resume) = resume_id.filter(|id| !id.trim().is_empty()) {
            match load_transcript(history.as_ref(), &resume) {
                Some(mut record) => {
                    repaired = transcript::repair_tool_pairs(&mut record.messages);
                    session_id = resume;
                    last_model = record.model;
                    messages = record.messages;
                }
                None => resume_failed = true,
            }
        }

        let capability = scope.agent_capability();
        let this = Self {
            inner: Arc::new(Inner {
                session_id,
                endpoint,
                config,
                transport,
                catalog,
                bridge,
                scope,
                capability,
                events,
                signals,
                history,
                tuning,
                state: Mutex::new(State {
                    messages,
                    anchor: None,
                    caps: HashMap::new(),
                    remote: None,
                    remote_order: Vec::new(),
                    running: None,
                    last_model,
                    closed: false,
                }),
            }),
        };
        if resume_failed {
            this.inner.emit(AgentEvent::ResumeFailed);
        }
        if repaired > 0 {
            this.inner.notice(
                NoticeKind::HistoryRepaired,
                "上次对话有工具调用没拿到结果（程序在它返回之前就关了），已按失败处理。",
            );
        }
        this
    }

    /// 某个模型的能力（识别结果有缓存）。界面用它决定思考强度下拉、上下文指示器的分母、
    /// 是不是能用工具。`GET /models` 拿到过接口自报的数据就会参与合成。
    pub fn model_caps(&self, model: &str) -> ModelCaps {
        self.inner.model_caps(model)
    }

    /// 上次用的模型（续接时界面可以默认选它）。
    pub fn last_model(&self) -> Option<String> {
        self.inner.lock().last_model.clone()
    }

    /// 现在的历史条数（诊断 / 测试用）。
    pub fn message_count(&self) -> usize {
        self.inner.lock().messages.len()
    }

    /// 有回合在跑吗。
    pub fn is_running(&self) -> bool {
        self.inner.lock().running.is_some()
    }
}

fn load_transcript(
    history: Option<&HistoryStore>,
    session_id: &str,
) -> Option<transcript::Transcript> {
    let history = history?;
    let bytes = match history.transcript_load(HISTORY_AGENT, session_id) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return None,
        Err(err) => {
            tracing::warn!(error = %err, "ZeePilot 的对话记录读不了，按新会话处理");
            return None;
        }
    };
    match transcript::decode(&bytes) {
        Ok(record) => Some(record),
        Err(err) => {
            tracing::warn!(error = %err, "ZeePilot 的对话记录读不出来，按新会话处理");
            None
        }
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn emit(&self, event: AgentEvent) {
        // 接收端没了（面板关了）就算了。
        let _ = self.events.send(event);
    }

    fn signal(&self, signal: BuiltinSignal) {
        if let Some(signals) = &self.signals {
            let _ = signals.send(signal);
        }
    }

    fn notice(&self, kind: NoticeKind, text: &str) {
        self.signal(BuiltinSignal::Notice(BuiltinNotice {
            kind,
            text: text.to_owned(),
        }));
    }

    fn known_models(&self) -> Vec<String> {
        let state = self.lock();
        let mut models: Vec<String> = self.endpoint.models.clone();
        models.extend(self.endpoint.default_model.clone());
        models.extend(state.remote_order.iter().cloned());
        models
    }

    fn model_caps(&self, model: &str) -> ModelCaps {
        let mut state = self.lock();
        if let Some(caps) = state.caps.get(model) {
            return caps.clone();
        }
        let manual = self.endpoint.overrides.get(model).cloned();
        let remote = state
            .remote
            .as_ref()
            .and_then(|map| map.get(model))
            .cloned();
        let caps = resolve(model, manual, remote, &self.catalog);
        state.caps.insert(model.to_owned(), caps.clone());
        caps
    }

    /// 存完整对话记录（失败只记日志，不影响对话）。
    fn persist(&self) {
        let Some(history) = &self.history else { return };
        let (messages, model) = {
            let state = self.lock();
            (state.messages.clone(), state.last_model.clone())
        };
        if messages.is_empty() {
            return;
        }
        let key = self.config.api_key.expose().to_owned();
        let secrets: Vec<&str> = if key.is_empty() {
            Vec::new()
        } else {
            vec![&key]
        };
        let bytes =
            match transcript::encode(&self.session_id, model.as_deref(), &messages, &secrets) {
                Ok(bytes) => bytes,
                Err(err) => {
                    tracing::warn!(error = %err, "ZeePilot 的对话记录序列化失败");
                    return;
                }
            };
        if let Err(err) = history.transcript_save(HISTORY_AGENT, &self.session_id, &bytes) {
            tracing::warn!(error = %err, "ZeePilot 的对话记录保存失败");
        }
    }

    /// `GET /models` + 手填的模型，合成模型清单。
    async fn list_models(&self) -> AgentModelCatalog {
        let fetched = self.fetch_remote_models().await;
        let mut ids: Vec<String> = Vec::new();
        let mut push = |id: &str| {
            let id = id.trim();
            if !id.is_empty() && !ids.iter().any(|m| m == id) {
                ids.push(id.to_owned());
            }
        };
        if let Some(default) = &self.endpoint.default_model {
            push(default);
        }
        for model in &self.endpoint.models {
            push(model);
        }
        if let Some(fetched) = &fetched {
            for (id, _) in fetched {
                push(id);
            }
        }
        if let Some(fetched) = fetched {
            let mut state = self.lock();
            state.remote_order = fetched.iter().map(|(id, _)| id.clone()).collect();
            state.remote = Some(fetched.into_iter().collect());
            // 接口自报的能力现在才知道：之前缓存的识别结果作废重算。
            state.caps.clear();
        }
        let models = ids
            .into_iter()
            .map(|id| {
                let caps = self.model_caps(&id);
                let name = if caps.display_name.trim().is_empty() {
                    id.clone()
                } else {
                    caps.display_name.clone()
                };
                AgentModel {
                    id,
                    name,
                    unavailable: false,
                }
            })
            .collect();
        AgentModelCatalog {
            models,
            default: self.endpoint.default_model.clone(),
            default_label: self.endpoint.default_model.clone(),
        }
    }

    /// 拉接口的模型清单；任何失败（连不上、鉴权、不是 JSON、没有条目）都是 `None`，
    /// 调用方退回手填的。
    async fn fetch_remote_models(&self) -> Option<Vec<(String, PartialCaps)>> {
        super::probe::fetch_models(
            self.transport.as_ref(),
            &self.config,
            self.tuning.models_timeout,
        )
        .await
    }
}

impl AgentConversation for BuiltinConversation {
    fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    fn resume_id(&self) -> Option<String> {
        // 还没有任何消息就没有可续接的东西。
        let has_messages = !self.inner.lock().messages.is_empty();
        has_messages.then(|| self.inner.session_id.clone())
    }

    fn history_secrets(&self) -> Vec<String> {
        let key = self.inner.config.api_key.expose();
        if key.is_empty() {
            Vec::new()
        } else {
            vec![key.to_owned()]
        }
    }

    fn models<'a>(&'a self) -> BoxFuture<'a, Result<AgentModelCatalog, AgentError>> {
        Box::pin(async move { Ok(self.inner.list_models().await) })
    }

    fn send<'a>(
        &'a self,
        text: &'a str,
        model: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            if text.trim().is_empty() {
                return Err(AgentError::Other("消息是空的".to_owned()));
            }
            let cancel = Arc::new(Cancel::new());
            {
                let mut state = self.inner.lock();
                if state.closed {
                    return Err(AgentError::Other("这个对话已经关闭".to_owned()));
                }
                if state.running.is_some() {
                    return Err(AgentError::Busy);
                }
                state.running = Some(Running {
                    cancel: Arc::clone(&cancel),
                    handle: None,
                });
            }
            // 用户发了新消息：解除上一次停止设下的"已停止"态，这个回合的工具调用才能执行。
            self.inner.bridge.resume_scope(self.inner.scope.scope_id());
            let inner = Arc::clone(&self.inner);
            let text = text.to_owned();
            let selection = model.map(str::to_owned);
            let task_cancel = Arc::clone(&cancel);
            let handle = tokio::spawn(async move {
                inner.run_turn(text, selection, task_cancel).await;
            });
            let mut state = self.inner.lock();
            match state.running.as_mut() {
                // 回合还在跑：把句柄记下来，shutdown 要等它。
                Some(running) if Arc::ptr_eq(&running.cancel, &cancel) => {
                    running.handle = Some(handle);
                }
                // 回合已经跑完了（任务比这里还快）：句柄不用留。
                _ => drop(handle),
            }
            Ok(())
        })
    }

    fn stop<'a>(&'a self) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            self.inner.stop_turn().await;
            Ok(())
        })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let handle = {
                let mut state = self.inner.lock();
                state.closed = true;
                state.running.as_mut().and_then(|r| r.handle.take())
            };
            self.inner.stop_turn().await;
            if let Some(handle) = handle {
                let wait = self.inner.tuning.stop_grace + Duration::from_secs(2);
                let _ = tokio::time::timeout(wait, handle).await;
            }
            // 这个对话以后不会再调工具：收回它的密钥。
            self.inner.capability.revoke();
            self.inner.persist();
        })
    }
}

impl Inner {
    /// 停止当前回合：取消流、让工具桥作废这个对话在这一刻之前的请求并给终端里的命令发 Ctrl+C。
    async fn stop_turn(&self) {
        let cancel = self
            .lock()
            .running
            .as_ref()
            .map(|running| Arc::clone(&running.cancel));
        let Some(cancel) = cancel else { return };
        cancel.cancel();
        let scope_id = self.scope.scope_id().to_owned();
        self.bridge.stop_scope(&scope_id);
        self.bridge.interrupt_terminal(&scope_id).await;
    }

    async fn run_turn(
        self: Arc<Self>,
        text: String,
        selection: Option<String>,
        cancel: Arc<Cancel>,
    ) {
        let outcome = self.turn_body(&text, selection.as_deref(), &cancel).await;
        match outcome {
            Ok(()) | Err(Abort::Stopped) => {}
            Err(Abort::Failed(message)) => self.emit(AgentEvent::Error { message }),
        }
        self.persist();
        // 先清掉"在跑"再发 TurnEnd：界面收到 TurnEnd 就可能立刻发下一条。
        self.lock().running = None;
        self.emit(AgentEvent::TurnEnd);
    }

    async fn turn_body(
        &self,
        text: &str,
        selection: Option<&str>,
        cancel: &Cancel,
    ) -> Result<(), Abort> {
        // 选模型：界面给的 > 接口的默认模型。
        let raw = selection
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| self.endpoint.default_model.clone())
            .ok_or_else(|| {
                Abort::Failed(
                    "还没有选模型：请在接口设置里填默认模型，或在输入框里选一个模型。".to_owned(),
                )
            })?;
        let (model, choice) = mapping::parse_model_selection(&raw, &self.known_models());
        let caps = self.model_caps(&model);
        let reasoning = mapping::reasoning_param(self.endpoint.kind, &caps, &choice);
        let tools_enabled = caps.tool_call;
        let tools: Vec<ToolSpec> = if tools_enabled {
            bridge_tool_specs()
        } else {
            self.notice(
                NoticeKind::NoToolSupport,
                "该模型不支持工具调用，只能聊天，不能操作服务器。",
            );
            Vec::new()
        };
        let system = prompt::system_prompt(tools_enabled);

        {
            let mut state = self.lock();
            state.messages.push(Message::User {
                text: text.to_owned(),
            });
            state.last_model = Some(model.clone());
        }
        let mut got_reply = false;
        let mut calls_done = 0usize;
        let mut truncated_notice = false;
        let mut forced_compactions = 0u32;

        let result: Result<(), Abort> = async {
            loop {
                if cancel.is_cancelled() {
                    return Err(Abort::Stopped);
                }
                self.prepare_context(&caps, &model, &system, &tools, cancel)
                    .await;
                if cancel.is_cancelled() {
                    return Err(Abort::Stopped);
                }
                let request = self.build_request(&model, &caps, &system, &tools, &reasoning);
                let turn = match self.stream_with_retry(&request, cancel, false).await {
                    Ok(turn) => turn,
                    Err(StepError::Stopped(partial)) => {
                        self.record_stopped_reply(&partial);
                        return Err(Abort::Stopped);
                    }
                    Err(StepError::Failed(error, exhausted)) => {
                        // 对话太长：压缩一次再试这条请求。
                        if error.kind == ErrorKind::ContextTooLong
                            && forced_compactions < 2
                            && self.compact(&caps, &model, cancel, true).await
                        {
                            forced_compactions += 1;
                            continue;
                        }
                        return Err(self.fail(&error, exhausted));
                    }
                };
                got_reply = true;
                self.after_response(&model, &caps, &turn);
                if turn.stop == Some(StopReason::MaxTokens) && !truncated_notice {
                    truncated_notice = true;
                    self.notice(
                        NoticeKind::OutputTruncated,
                        "回复被模型的输出长度上限截断了，可以让它继续。",
                    );
                }
                self.lock().messages.push(Message::Assistant(turn.to_message()));
                if turn.tool_calls.is_empty() {
                    return Ok(());
                }
                match self
                    .run_tool_calls(&turn.tool_calls, &mut calls_done, cancel)
                    .await
                {
                    ToolRound::Continue => {}
                    ToolRound::Stopped => return Err(Abort::Stopped),
                    ToolRound::LimitReached => {
                        return Err(Abort::Failed(format!(
                            "这一轮已经调用了 {} 次工具，达到上限，已停下。请检查一下进展，需要的话再发一条消息让我继续。",
                            self.tuning.max_tool_calls
                        )));
                    }
                }
            }
        }
        .await;

        if result.is_err() && !got_reply {
            // 第一次请求就失败 / 被停：这条消息没有得到任何回应，从历史里拿掉，
            // 用户重发同一句话时模型不会看到两遍。
            let mut state = self.lock();
            if matches!(state.messages.last(), Some(Message::User { text: last }) if last == text) {
                state.messages.pop();
            }
        }
        result
    }

    fn build_request(
        &self,
        model: &str,
        caps: &ModelCaps,
        system: &str,
        tools: &[ToolSpec],
        reasoning: &ReasoningParam,
    ) -> ChatRequest {
        // 已用多少 token：有上一次响应的用量就用它，之后新增的消息再粗估。
        let used = self.estimate_used(system, tools);
        let messages = self.lock().messages.clone();
        let budget_left = u64::from(caps.context_window)
            .saturating_sub(used + context::safety_margin(caps.context_window));
        ChatRequest {
            model: model.to_owned(),
            system: system.to_owned(),
            messages,
            tools: tools.to_vec(),
            max_output_tokens: mapping::max_output_tokens(caps, budget_left),
            reasoning: reasoning.clone(),
            temperature: None,
            // 系统提示词在最前面，不随模型变：让接口缓存它。
            cache_hint: true,
        }
    }

    /// 一次响应回来之后：记用量、发旁路信号、更新估算锚点。
    fn after_response(&self, model: &str, caps: &ModelCaps, turn: &AssistantTurn) {
        let usage = turn.usage;
        let used = usage.total_input();
        if used > 0 {
            let len_after = self.lock().messages.len() + 1;
            // 锚点 = 这次请求的输入 + 模型这次的输出（紧接着会被追加进历史的那条 assistant 消息）。
            self.lock().anchor = Some((len_after, used + usage.output));
            self.signal(BuiltinSignal::Usage(TurnUsage {
                model: model.to_owned(),
                used,
                output: usage.output,
                input: usage.input,
                cache_read: usage.cache_read,
                cache_write: usage.cache_write,
                window: caps.context_window,
                window_estimated: !matches!(
                    caps.source,
                    super::catalog::CapsSource::Manual
                        | super::catalog::CapsSource::Endpoint
                        | super::catalog::CapsSource::ModelsDev
                        | super::catalog::CapsSource::Builtin
                ),
            }));
        }
    }

    /// 被停止时没来得及收完的回复：已经流出来的正文留着，补一句说明，历史里不留一个孤零零的用户消息。
    fn record_stopped_reply(&self, partial: &str) {
        let text = if partial.trim().is_empty() {
            "（用户停止了这次回复）".to_owned()
        } else {
            format!("{partial}\n\n（用户停止了这次回复）")
        };
        let mut state = self.lock();
        state
            .messages
            .push(Message::Assistant(super::provider::AssistantMessage {
                text,
                reasoning: Vec::new(),
                tool_calls: Vec::new(),
            }));
        state.anchor = None;
    }

    /// 接口错误 → 面向用户的失败；同时发结构化的旁路信号。
    fn fail(&self, error: &ProviderError, exhausted: bool) -> Abort {
        self.signal(BuiltinSignal::Failure(FailureInfo {
            kind: error.kind,
            code: error.code.clone(),
            status: error.status,
        }));
        Abort::Failed(failure_message(error, exhausted))
    }

    /// 发请求，带重试：429 / 5xx / 连接中断按指数退避重试，不可重试的直接返回。
    ///
    /// `quiet`：不把正文 / 思考增量发给界面（压缩时模型写的摘要不是给用户看的回复）。
    async fn stream_with_retry(
        &self,
        request: &ChatRequest,
        cancel: &Cancel,
        quiet: bool,
    ) -> Result<AssistantTurn, StepError> {
        let max = self.tuning.max_retries;
        let mut attempt = 0u32;
        loop {
            let mut partial = String::new();
            let result = {
                let mut on_event = |event: StreamEvent| match event {
                    StreamEvent::TextDelta { text } if !quiet => {
                        partial.push_str(&text);
                        self.emit(AgentEvent::TextDelta { text });
                    }
                    StreamEvent::ReasoningDelta { text } if !quiet => {
                        self.emit(AgentEvent::ReasoningDelta { text });
                    }
                    _ => {}
                };
                tokio::select! {
                    result = stream_chat(&*self.transport, &self.config, request, &mut on_event) => Some(result),
                    _ = cancel.cancelled() => None,
                }
            };
            let error = match result {
                None => return Err(StepError::Stopped(partial)),
                Some(Ok(turn)) => return Ok(turn),
                Some(Err(error)) => error,
            };
            if !error.retryable || attempt >= max {
                return Err(StepError::Failed(
                    error.clone(),
                    error.retryable && attempt >= max,
                ));
            }
            let delay = self.tuning.retry_delay(attempt, error.retry_after);
            attempt += 1;
            self.emit(AgentEvent::Retrying {
                attempt,
                reason: error.message.clone(),
                next_in_secs: Some(delay.as_secs()),
            });
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = cancel.cancelled() => return Err(StepError::Stopped(String::new())),
            }
        }
    }

    /// 每轮请求前：估算上下文，超过预算的 80% 先截旧工具输出，还超就压缩。
    async fn prepare_context(
        &self,
        caps: &ModelCaps,
        model: &str,
        system: &str,
        tools: &[ToolSpec],
        cancel: &Cancel,
    ) {
        let budget = context::input_budget(caps);
        if !context::over_threshold(self.estimate_used(system, tools), budget) {
            return;
        }
        {
            let mut state = self.lock();
            let from = context::fresh_results_start(&state.messages);
            if context::truncate_old_tool_results(&mut state.messages, from) > 0 {
                state.anchor = None;
            }
        }
        if !context::over_threshold(self.estimate_used(system, tools), budget) {
            return;
        }
        self.compact(caps, model, cancel, false).await;
    }

    /// 现在的上下文大约占多少 token：有上一次响应的用量就用它，之后新增的消息再粗估。
    fn estimate_used(&self, system: &str, tools: &[ToolSpec]) -> u64 {
        let state = self.lock();
        match state.anchor {
            Some((len, tokens)) if len <= state.messages.len() => {
                tokens
                    + state.messages[len..]
                        .iter()
                        .map(context::estimate_message_tokens)
                        .sum::<u64>()
            }
            _ => context::estimate_request_tokens(system, tools, &state.messages),
        }
    }

    /// 压缩较早的对话：用同一个模型把最早的若干轮总结成一段，替换原文。成功返回 `true`。
    /// 失败（没有可压的、总结请求出错、被停止）返回 `false`，对话原样不动。
    async fn compact(&self, caps: &ModelCaps, model: &str, cancel: &Cancel, forced: bool) -> bool {
        let (cut, prefix) = {
            let state = self.lock();
            let Some(cut) = context::split_for_compaction(&state.messages) else {
                return false;
            };
            (cut, state.messages[..cut].to_vec())
        };
        // 渲染出来的文本本身也要放得进窗口：放不下就把每条结果再压短。
        let window = u64::from(caps.context_window);
        let mut result_chars = 4000usize;
        let mut rendered = context::render_for_summary(&prefix, result_chars);
        while context::estimate_text_tokens(&rendered) > window * 6 / 10 && result_chars > 200 {
            result_chars /= 2;
            rendered = context::render_for_summary(&prefix, result_chars);
        }
        let request = ChatRequest {
            model: model.to_owned(),
            system: context::COMPACT_PROMPT.to_owned(),
            messages: vec![Message::User { text: rendered }],
            tools: Vec::new(),
            max_output_tokens: mapping::max_output_tokens(caps, 8192).min(8192),
            reasoning: ReasoningParam::Unspecified,
            temperature: None,
            cache_hint: false,
        };
        let turn = match self.stream_with_retry(&request, cancel, true).await {
            Ok(turn) => turn,
            Err(StepError::Stopped(_)) => return false,
            Err(StepError::Failed(error, _)) => {
                tracing::warn!(kind = ?error.kind, forced, "ZeePilot 压缩对话失败，继续用原来的历史");
                return false;
            }
        };
        let summary = turn.text.trim();
        if summary.is_empty() {
            tracing::warn!("ZeePilot 压缩对话得到了空摘要，继续用原来的历史");
            return false;
        }
        {
            let mut state = self.lock();
            if state.messages.len() < cut {
                return false;
            }
            let mut rest = state.messages.split_off(cut);
            state.messages.clear();
            state.messages.push(context::summary_message(summary));
            state.messages.append(&mut rest);
            state.anchor = None;
        }
        self.notice(NoticeKind::Compacted, "已压缩较早的对话。");
        // 被压缩掉的内容不会再回来：马上存盘，别等回合结束。
        self.persist();
        true
    }

    /// 逐个执行模型这一步发起的工具调用（进程内走工具桥），结果回填进历史。
    async fn run_tool_calls(
        &self,
        calls: &[AssembledToolCall],
        calls_done: &mut usize,
        cancel: &Cancel,
    ) -> ToolRound {
        // 先把所有调用的卡片都亮出来，再一个个执行。
        for call in calls {
            self.emit(AgentEvent::ToolStart {
                call_id: call.id.clone(),
                tool: call.name.clone(),
                input: call.arguments.clone().unwrap_or_else(|_| json!({})),
            });
        }
        let mut limit_hit = false;
        let known: Vec<String> = bridge_tool_specs().into_iter().map(|t| t.name).collect();
        for call in calls {
            let (content, is_error) = if cancel.is_cancelled() {
                ("用户停止了这一轮，这次调用没有执行。".to_owned(), true)
            } else if *calls_done >= self.tuning.max_tool_calls {
                limit_hit = true;
                (
                    format!(
                        "这一轮已经调用了 {} 次工具，达到上限，这次调用没有执行。",
                        self.tuning.max_tool_calls
                    ),
                    true,
                )
            } else {
                // 所有调用都计数，不管参数合不合法、工具存不存在：模型反复发被截断的 / 不存在的
                // 调用时，只数"能执行的"就永远碰不到上限，一直循环烧 token。
                *calls_done += 1;
                match &call.arguments {
                    Err(raw) => (
                        format!(
                            "工具参数不是合法的 JSON 对象，没有执行（可能是输出被截断了）。收到的开头：{}",
                            raw.chars().take(200).collect::<String>()
                        ),
                        true,
                    ),
                    Ok(_) if !known.iter().any(|name| name == &call.name) => (
                        format!("没有这个工具：{}。可用的工具：{}。", call.name, known.join("、")),
                        true,
                    ),
                    Ok(arguments) => {
                        self.invoke_tool(&call.name, arguments.clone(), cancel).await
                    }
                }
            };
            let content = context::limit_tool_text(&content, self.tuning.tool_result_limit);
            self.emit(AgentEvent::ToolEnd {
                call_id: call.id.clone(),
                tool: call.name.clone(),
                output: content.clone(),
                error: is_error,
            });
            self.lock().messages.push(Message::ToolResult {
                call_id: call.id.clone(),
                content,
                is_error,
            });
        }
        if cancel.is_cancelled() {
            ToolRound::Stopped
        } else if limit_hit {
            ToolRound::LimitReached
        } else {
            ToolRound::Continue
        }
    }

    /// 在工具桥上执行一次调用。
    ///
    /// 调用放进单独的任务里跑：用户点停止时我们不再等它，但**不能把它丢掉**——桥那边的请求要
    /// 自己走完收尾（终端占位、临时文件），和外部智能体连接断了请求照常跑完是同一个道理。
    /// 停止同时作废了这个对话的在途请求，所以它会很快自己结束。
    async fn invoke_tool(&self, name: &str, arguments: Value, cancel: &Cancel) -> (String, bool) {
        let bridge = Arc::clone(&self.bridge);
        let capability = self.capability.value().to_owned();
        let method = name.to_owned();
        let mut task =
            tokio::spawn(async move { bridge.call_tool(&capability, &method, arguments).await });
        let outcome = tokio::select! {
            joined = &mut task => joined,
            _ = cancel.cancelled() => {
                // 停止已经通知过桥；给它一点时间收尾，超时就不等了（任务自己会走完）。
                match tokio::time::timeout(self.tuning.stop_grace, &mut task).await {
                    Ok(joined) => joined,
                    Err(_) => return ("用户停止了这一轮，这次调用被中断了。".to_owned(), true),
                }
            }
        };
        match outcome {
            Ok(outcome) => (outcome.text, outcome.is_error),
            Err(err) => {
                tracing::warn!(error = %err, "ZeePilot 的工具调用任务异常结束");
                ("工具调用异常结束了。".to_owned(), true)
            }
        }
    }
}

enum ToolRound {
    Continue,
    Stopped,
    LimitReached,
}

/// 面向用户的失败文字：中文原因 + 服务端原文 + 错误码（原样，界面 / A5 按码识别）。
fn failure_message(error: &ProviderError, exhausted: bool) -> String {
    let reason = error.message.trim().trim_end_matches('。').to_owned();
    let mut text = if exhausted {
        retry::retry_limit_message(&reason)
    } else {
        error.message.clone()
    };
    if let Some(detail) = &error.detail {
        if !detail.is_empty() {
            text.push_str(&format!("（服务端返回：{detail}）"));
        }
    }
    if let Some(code) = &error.code {
        if !text.contains(code.as_str()) {
            text.push_str(&format!("（错误码：{code}）"));
        }
    }
    text
}

#[cfg(test)]
mod tests;
