//! 本机"假 Tokenway"（测试夹具，**不联网**，只监听 `127.0.0.1`）：授权页直接 302 回调、换令牌、
//! 余额状态、停用，外加假的 `/v1/models` 和 `/v1/chat/completions`（含 402 `zeeterm_requires_topup`
//! 和 401 两种失败）。核心库测试、原生应用的端到端测试和调试版的窗口截图都用它；产品路径里没有
//! 任何地方会启动它（调试版靠 `ZSSH_*` 钩子，release 里不存在这条路径）。
//!
//! 只做测试需要的 HTTP/1.1 子集：一个连接一个请求，`Connection: close`。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use reqwest::Url;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::connect::UrlOpener;
use super::presets::Preset;

/// 授权页收到请求后怎么办。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approve {
    /// 直接 302 回调（相当于用户秒点"授权"）。
    Auto,
    /// 卡住，直到 [`FakeTokenway::release`]（用来截"正在等待浏览器授权"的图）。
    Hold,
    /// 302 回 `error=access_denied`（用户点了"拒绝"）。
    Deny,
}

struct CodeRecord {
    challenge: String,
    redirect_uri: String,
}

struct State {
    approve: Approve,
    codes: HashMap<String, CodeRecord>,
    /// 签发过的令牌 → 是否已被停用。
    tokens: HashMap<String, bool>,
    last_key: Option<String>,
    /// 余额（分）。
    usable_cents: i64,
    total_cents: i64,
    needs_topup: bool,
    /// 让 status 接口固定返回这个状态码（测错误映射用）。
    status_override: Option<u16>,
    /// 收到过的请求："GET /path"（不含查询串和任何头、体）。
    requests: Vec<String>,
    /// 带了 `Authorization` 头的请求数（只记有没有，不记值）。
    authed_requests: usize,
    counter: u64,
    /// 授权页收到的设备名（最近一次）。
    last_device: Option<String>,
    last_authorize_query: Vec<(String, String)>,
}

pub struct FakeTokenway {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    hold: Arc<Notify>,
    task: JoinHandle<()>,
}

impl Drop for FakeTokenway {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|p| p.into_inner())
}

impl FakeTokenway {
    /// 在 `127.0.0.1` 的随机端口起服务。
    pub async fn start() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let state = Arc::new(Mutex::new(State {
            approve: Approve::Auto,
            codes: HashMap::new(),
            tokens: HashMap::new(),
            last_key: None,
            usable_cents: 1234,
            total_cents: 1234,
            needs_topup: false,
            status_override: None,
            requests: Vec::new(),
            authed_requests: 0,
            counter: 0,
            last_device: None,
            last_authorize_query: Vec::new(),
        }));
        let hold = Arc::new(Notify::new());
        let task = tokio::spawn({
            let (state, hold) = (state.clone(), hold.clone());
            async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (state, hold) = (state.clone(), hold.clone());
                    tokio::spawn(async move { serve(stream, addr, state, hold).await });
                }
            }
        });
        Ok(Self {
            addr,
            state,
            hold,
            task,
        })
    }

    pub fn origin(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// 指向这个假服务的预设（地址全是本机回环，要配 `UrlPolicy::with_http_loopback_for_tests()` 才放行）。
    pub fn preset(&self) -> Preset {
        let origin = self.origin();
        Preset {
            id: "tokenway".to_owned(),
            name: "Tokenway".to_owned(),
            recommended: true,
            tagline: Some("国内直连，按量计费".to_owned()),
            connect_url: Some(format!("{origin}/connect/zeeterm")),
            token_url: Some(format!("{origin}/api/connect/token")),
            status_url: Some(format!("{origin}/api/connect/status")),
            revoke_url: Some(format!("{origin}/api/connect/revoke")),
            openai_base: Some(format!("{origin}/v1")),
            anthropic_base: Some(origin.clone()),
            register_url: Some(format!("{origin}/register?source=zeeterm")),
            topup_url: Some(format!("{origin}/dashboard/topup")),
        }
    }

    pub fn set_approve(&self, mode: Approve) {
        lock(&self.state).approve = mode;
    }

    /// 放行卡在授权页的请求（`Approve::Hold` 时）。
    pub fn release(&self) {
        // 存一张许可：请求还没到也不会漏。
        self.hold.notify_one();
    }

    pub fn set_balance(&self, usable_cents: i64, total_cents: i64) {
        let mut state = lock(&self.state);
        state.usable_cents = usable_cents;
        state.total_cents = total_cents;
        state.needs_topup = usable_cents <= 0;
    }

    /// 没有可用的充值余额：status 里 `needs_topup`，对话回 402。
    pub fn set_needs_topup(&self, needs: bool) {
        let mut state = lock(&self.state);
        state.needs_topup = needs;
        if needs {
            state.usable_cents = 0;
        }
    }

    pub fn set_status_override(&self, status: Option<u16>) {
        lock(&self.state).status_override = status;
    }

    /// 相当于用户在网站上改了密码 / 停用了授权：所有已签发的令牌立刻作废。
    pub fn revoke_all(&self) {
        for revoked in lock(&self.state).tokens.values_mut() {
            *revoked = true;
        }
    }

    /// 最近签发的令牌（测试用来核对"保险库里存的就是它"）。
    pub fn last_key(&self) -> Option<String> {
        lock(&self.state).last_key.clone()
    }

    pub fn key_is_revoked(&self, key: &str) -> Option<bool> {
        lock(&self.state).tokens.get(key).copied()
    }

    pub fn requests(&self) -> Vec<String> {
        lock(&self.state).requests.clone()
    }

    pub fn count(&self, needle: &str) -> usize {
        lock(&self.state)
            .requests
            .iter()
            .filter(|line| line.contains(needle))
            .count()
    }

    pub fn authed_requests(&self) -> usize {
        lock(&self.state).authed_requests
    }

    pub fn last_device(&self) -> Option<String> {
        lock(&self.state).last_device.clone()
    }

    pub fn last_authorize_query(&self) -> Vec<(String, String)> {
        lock(&self.state).last_authorize_query.clone()
    }
}

/// 假浏览器：把授权页地址 GET 一遍（reqwest 会跟着 302 走到 ZeeTerm 的回环监听），
/// 相当于用户在网页上点了"授权"（或"拒绝"，看服务端的 [`Approve`]）。
#[derive(Clone, Default)]
pub struct FakeBrowser {
    opened: Arc<Mutex<Vec<String>>>,
}

impl FakeBrowser {
    pub fn new() -> Self {
        Self::default()
    }

    /// 打开过的地址。
    pub fn opened(&self) -> Vec<String> {
        self.opened
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl UrlOpener for FakeBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        self.opened
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(url.to_owned());
        let url = url.to_owned();
        // 授权页可能被 `Hold` 住，所以放到后台去 GET，不挡住 `open` 返回。
        tokio::spawn(async move {
            let client = reqwest::Client::builder().no_proxy().build();
            if let Ok(client) = client {
                let _ = client.get(&url).send().await;
            }
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

struct Req {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn read_req(stream: &mut TcpStream) -> Option<Req> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if data.len() > 64 * 1024 {
            return None;
        }
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }
    let url = Url::parse(&format!("http://fake{target}")).ok()?;
    Some(Req {
        method,
        path: url.path().to_owned(),
        query: url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect(),
        headers,
        body,
    })
}

struct Resp {
    status: u16,
    content_type: &'static str,
    location: Option<String>,
    body: Vec<u8>,
}

impl Resp {
    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            location: None,
            body: value.to_string().into_bytes(),
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self::json(status, json!({"error": {"code": code, "message": message}}))
    }

    fn redirect(location: String) -> Self {
        Self {
            status: 302,
            content_type: "text/plain",
            location: Some(location),
            body: Vec::new(),
        }
    }
}

async fn write_resp(stream: &mut TcpStream, resp: Resp) {
    let reason = match resp.status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        402 => "Payment Required",
        404 => "Not Found",
        429 => "Too Many Requests",
        _ => "Error",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n",
        resp.status,
        resp.content_type,
        resp.body.len()
    );
    if let Some(location) = &resp.location {
        head.push_str(&format!("Location: {location}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&resp.body).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    let _ = getrandom::fill(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn bearer(req: &Req) -> Option<String> {
    req.headers
        .get("authorization")?
        .strip_prefix("Bearer ")
        .map(|key| key.trim().to_owned())
}

fn loopback_redirect(uri: &str) -> bool {
    Url::parse(uri).is_ok_and(|url| {
        url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port().is_some_and(|port| port >= 1024)
            && url.query().is_none()
    })
}

async fn serve(
    mut stream: TcpStream,
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    hold: Arc<Notify>,
) {
    let Some(req) = read_req(&mut stream).await else {
        return;
    };
    {
        let mut guard = lock(&state);
        guard.requests.push(format!("{} {}", req.method, req.path));
        if req.headers.contains_key("authorization") {
            guard.authed_requests += 1;
        }
    }
    let resp = route(&req, addr, &state, &hold).await;
    write_resp(&mut stream, resp).await;
}

async fn route(req: &Req, addr: SocketAddr, state: &Arc<Mutex<State>>, hold: &Arc<Notify>) -> Resp {
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/connect/zeeterm") => authorize_page(req, state, hold).await,
        ("POST", "/api/connect/token") => token(req, addr, state),
        ("GET", "/api/connect/status") => status(req, state),
        ("POST", "/api/connect/revoke") => revoke(req, state),
        ("GET", "/v1/models") => models(req, state),
        ("POST", "/v1/chat/completions") => chat(req, state),
        _ => Resp::error(404, "not_found", "没有这个路径"),
    }
}

async fn authorize_page(req: &Req, state: &Arc<Mutex<State>>, hold: &Arc<Notify>) -> Resp {
    let get = |name: &str| {
        req.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    let (redirect_uri, oauth_state, challenge) =
        (get("redirect_uri"), get("state"), get("code_challenge"));
    if !loopback_redirect(&redirect_uri) || get("code_challenge_method") != "S256" {
        return Resp::error(400, "invalid_request", "回调地址或 PKCE 参数不对");
    }
    {
        let mut guard = lock(state);
        guard.last_device = Some(get("device")).filter(|d| !d.is_empty());
        guard.last_authorize_query = req.query.clone();
    }
    let mode = lock(state).approve;
    if mode == Approve::Hold {
        hold.notified().await;
    }
    let mode = lock(state).approve;
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    if mode == Approve::Deny {
        return Resp::redirect(format!(
            "{redirect_uri}{sep}error=access_denied&state={oauth_state}"
        ));
    }
    let code = random_hex(24);
    lock(state).codes.insert(
        code.clone(),
        CodeRecord {
            challenge,
            redirect_uri: redirect_uri.clone(),
        },
    );
    Resp::redirect(format!(
        "{redirect_uri}{sep}code={code}&state={oauth_state}"
    ))
}

fn token(req: &Req, addr: SocketAddr, state: &Arc<Mutex<State>>) -> Resp {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    if body["grant_type"] != "authorization_code" {
        return Resp::error(400, "unsupported_grant_type", "只支持 authorization_code");
    }
    let text = |name: &str| body[name].as_str().unwrap_or("").to_owned();
    let (code, verifier, redirect_uri) =
        (text("code"), text("code_verifier"), text("redirect_uri"));
    let mut guard = lock(state);
    // 授权码先作废再核对，和真服务端一样：对不上也不能再拿同一个码重试。
    let Some(record) = guard.codes.remove(&code) else {
        return Resp::error(
            400,
            "invalid_grant",
            "授权码无效、已用过或已过期，请在 ZeeTerm 里重新连接",
        );
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if challenge != record.challenge || record.redirect_uri != redirect_uri {
        return Resp::error(
            400,
            "invalid_grant",
            "授权码无效、已用过或已过期，请在 ZeeTerm 里重新连接",
        );
    }
    guard.counter += 1;
    let key = format!("sk-test-zt-{}", random_hex(12));
    guard.tokens.insert(key.clone(), false);
    guard.last_key = Some(key.clone());
    let origin = format!("http://{addr}");
    Resp::json(
        200,
        json!({
            "api_key": key,
            "token_id": guard.counter,
            "base_url_openai": format!("{origin}/v1"),
            "base_url_anthropic": origin,
            "expires_at": "2027-05-01T00:00:00Z",
            "quota_limit": 50,
        }),
    )
}

/// 令牌是否有效：`Ok(())` 有效，`Err` 是要回的 401。
fn check_key(req: &Req, state: &Arc<Mutex<State>>) -> Result<(), Resp> {
    let guard = lock(state);
    match bearer(req).and_then(|key| guard.tokens.get(&key).copied()) {
        Some(false) => Ok(()),
        _ => Err(Resp::error(
            401,
            "invalid_token",
            "令牌已停用或已过期，请在 ZeeTerm 里重新连接",
        )),
    }
}

fn status(req: &Req, state: &Arc<Mutex<State>>) -> Resp {
    if let Some(code) = lock(state).status_override {
        return Resp::error(code, "rate_limited", "请求太频繁，请稍后再试");
    }
    if let Err(resp) = check_key(req, state) {
        return resp;
    }
    let guard = lock(state);
    let yuan = |cents: i64| cents as f64 / 100.0;
    Resp::json(
        200,
        json!({
            "balance_paid": yuan(guard.usable_cents),
            "balance_total": yuan(guard.total_cents),
            "zeeterm_usable": yuan(guard.usable_cents),
            "quota_limit": 50,
            "used": 1.25,
            "expires_at": "2027-05-01T00:00:00Z",
            "needs_topup": guard.needs_topup,
        }),
    )
}

fn revoke(req: &Req, state: &Arc<Mutex<State>>) -> Resp {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let key = body["api_key"].as_str().unwrap_or("").to_owned();
    let mut guard = lock(state);
    match guard.tokens.get_mut(&key) {
        Some(revoked) => {
            *revoked = true;
            Resp::json(200, json!({"ok": true}))
        }
        None => Resp::error(401, "invalid_token", "不是有效的 ZeeTerm 令牌"),
    }
}

fn models(req: &Req, state: &Arc<Mutex<State>>) -> Resp {
    if let Err(resp) = check_key(req, state) {
        return resp;
    }
    Resp::json(
        200,
        json!({"object": "list", "data": [
            {"id": "text-embedding-fake", "object": "model"},
            {"id": "fake-chat-pro", "object": "model"},
            {"id": "fake-chat-mini", "object": "model"},
        ]}),
    )
}

fn chat(req: &Req, state: &Arc<Mutex<State>>) -> Resp {
    if let Err(resp) = check_key(req, state) {
        return resp;
    }
    if lock(state).needs_topup {
        return Resp::error(
            402,
            "zeeterm_requires_topup",
            "ZeeTerm 内置助手需要充值后使用，赠送额度可在网页和其他客户端试用",
        );
    }
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let last_user = body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .and_then(|m| m["content"].as_str().map(str::to_owned))
        })
        .unwrap_or_default();
    let chunk = |delta: Value, usage: Option<Value>| {
        let mut value = json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk",
            "model": "fake-chat-pro", "choices": [{"index": 0, "delta": delta}]});
        if let Some(usage) = usage {
            value["usage"] = usage;
            value["choices"] = json!([]);
        }
        format!("data: {value}\n\n")
    };
    let mut sse = String::new();
    sse.push_str(&chunk(json!({"role": "assistant", "content": ""}), None));
    sse.push_str(&chunk(
        json!({"content": format!("收到：{last_user}")}),
        None,
    ));
    sse.push_str(&format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl-fake", "object": "chat.completion.chunk", "model": "fake-chat-pro",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
    ));
    sse.push_str(&chunk(
        json!({}),
        Some(json!({"prompt_tokens": 12, "completion_tokens": 6, "total_tokens": 18})),
    ));
    sse.push_str("data: [DONE]\n\n");
    Resp {
        status: 200,
        content_type: "text/event-stream",
        location: None,
        body: sse.into_bytes(),
    }
}
