//! 授权回调的本机回环监听（RFC 8252 的回环地址重定向）。
//!
//! 本机上任何网页、任何进程都能往这个端口发请求，所以逐条设防：
//!
//! - **只绑 `127.0.0.1`**（不绑 `0.0.0.0`、不绑 IPv6），随机端口；
//! - 路径里带一段随机的、不可猜的段（`/<随机>/cb`），别的路径一律 404，猜不到路径的页面碰不到回调；
//! - **只接受一次回调**：第一个合格的 GET 被认领后监听立刻关闭，之后的请求都连不上；
//! - 只接受 GET，别的方法回 400 并忽略；`Host` 头必须是 `127.0.0.1[:端口]`（防 DNS 重绑定）；
//! - `state` 对不上或没有 `code` / 错误参数：当作授权失败（丢弃，不换令牌），回一个"授权失败，请重试"的页面；
//! - 请求头最多读 16 KiB、最多等 10 秒，半开的连接占不住监听；
//! - 返回的页面是纯静态中文，不加载任何外部资源，也不回显请求里的任何内容；
//! - 5 分钟（调用方给的超时）没等到或用户点了取消，监听就关。

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinSet;

use super::connect::ConnectError;
use crate::sync::onedrive::pkce::constant_time_eq;

const MAX_HEAD_BYTES: usize = 16 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// 授权码最长这么多字符（Tokenway 的码是 32 字节随机数的文本形式）。
const MAX_CODE_CHARS: usize = 512;

/// 回调说了什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// 授权码。
    Code(String),
    /// 用户在网页上点了"拒绝"。
    Denied,
    /// 回调不合格（`state` 不对、缺 `code`、授权码格式不对、服务端报别的错）。
    Invalid,
}

pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
    path: String,
}

fn random_segment() -> Result<String, ConnectError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|err| ConnectError::Internal(format!("系统随机数不可用：{err}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

impl CallbackListener {
    /// 绑 `127.0.0.1:0`，挑一段随机路径。
    pub async fn bind() -> Result<Self, ConnectError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|err| ConnectError::Listen(err.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|err| ConnectError::Listen(err.to_string()))?
            .port();
        Ok(Self {
            listener,
            port,
            path: format!("/{}/cb", random_segment()?),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.listener.local_addr().ok()
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// 授权请求里的 `redirect_uri`。
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }

    /// 等回调：合格的回调 / 超时 / 取消，三选一。返回时监听已经关闭（端口释放）。
    pub async fn wait(
        self,
        expected_state: &str,
        timeout: Duration,
        cancel: &Notify,
    ) -> Result<Callback, ConnectError> {
        let (tx, mut rx) = mpsc::channel::<Callback>(1);
        let claimed = Arc::new(AtomicBool::new(false));
        let shared = Arc::new(Shared {
            state: expected_state.to_owned(),
            path: self.path.clone(),
            port: self.port,
        });
        let mut tasks = JoinSet::new();
        let listener = self.listener;
        tasks.spawn(accept_loop(listener, shared, tx, claimed));
        let result = tokio::select! {
            callback = rx.recv() => callback.ok_or_else(|| ConnectError::Listen("回调监听意外停止".to_owned())),
            _ = tokio::time::sleep(timeout) => Err(ConnectError::Timeout),
            _ = cancel.notified() => Err(ConnectError::Cancelled),
        };
        // 丢掉 JoinSet 会中止监听任务和还没处理完的连接，端口随之释放。
        drop(tasks);
        result
    }
}

struct Shared {
    state: String,
    path: String,
    port: u16,
}

async fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Callback>,
    claimed: Arc<AtomicBool>,
) {
    let mut connections = JoinSet::new();
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let (shared, tx, claimed) = (shared.clone(), tx.clone(), claimed.clone());
        connections.spawn(async move { handle(stream, &shared, tx, claimed).await });
        while connections.try_join_next().is_some() {}
    }
}

struct Request {
    method: String,
    target: String,
    host: Option<String>,
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut head = Vec::new();
    let mut buf = [0u8; 2048];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        if head.len() > MAX_HEAD_BYTES {
            return None;
        }
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let host = lines.take_while(|line| !line.is_empty()).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("host")
            .then(|| value.trim().to_owned())
    });
    Some(Request {
        method,
        target,
        host,
    })
}

fn host_ok(host: &str, port: u16) -> bool {
    host == "127.0.0.1" || host == format!("127.0.0.1:{port}")
}

const PAGE_STYLE: &str = "body{font-family:'Segoe UI','Microsoft YaHei',sans-serif;display:flex;\
align-items:center;justify-content:center;height:100vh;margin:0;background:#f5f5f5;color:#222}\
@media(prefers-color-scheme:dark){body{background:#1e1e1e;color:#e6e6e6}}\
main{text-align:center;padding:24px}h1{font-size:20px;font-weight:600;margin:0 0 8px}\
p{margin:0;font-size:14px;opacity:.8}";

fn page(title: &str, hint: &str) -> String {
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<title>ZeeTerm</title><style>{PAGE_STYLE}</style></head>\
<body><main><h1>{title}</h1><p>{hint}</p></main></body></html>"
    )
}

pub fn success_page() -> String {
    page("已授权", "可以回到 ZeeTerm 了，这个页面可以关闭。")
}

fn denied_page() -> String {
    page(
        "已取消授权",
        "没有连接 Tokenway。可以回到 ZeeTerm，这个页面可以关闭。",
    )
}

fn failure_page() -> String {
    page(
        "授权失败，请重试",
        "请回到 ZeeTerm 重新点「用 Tokenway 连接」。",
    )
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\n\
X-Content-Type-Options: nosniff\r\n\
Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\n\
Connection: close\r\n\r\n",
        body.len()
    );
    // 浏览器可能已经走了，写失败不是错误。
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;
}

fn code_is_sane(code: &str) -> bool {
    !code.is_empty()
        && code.chars().count() <= MAX_CODE_CHARS
        && code.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '=')
        })
}

async fn handle(
    mut stream: TcpStream,
    shared: &Shared,
    tx: mpsc::Sender<Callback>,
    claimed: Arc<AtomicBool>,
) {
    let Ok(Some(request)) = tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await
    else {
        return;
    };
    if !request
        .host
        .as_deref()
        .is_some_and(|host| host_ok(host, shared.port))
    {
        respond(&mut stream, 400, &failure_page()).await;
        return;
    }
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{}", request.target)) else {
        respond(&mut stream, 404, &failure_page()).await;
        return;
    };
    if url.path() != shared.path {
        respond(&mut stream, 404, &failure_page()).await;
        return;
    }
    if request.method != "GET" {
        // 只接受 GET：别的方法回 400 并忽略（不消耗这次授权）。
        respond(&mut stream, 400, &failure_page()).await;
        return;
    }
    if claimed.load(Ordering::SeqCst) {
        respond(&mut stream, 404, &failure_page()).await;
        return;
    }
    let (mut state, mut code, mut error) = (None, None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "state" => state = Some(value.into_owned()),
            "code" => code = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            _ => {}
        }
    }
    let state_ok = state
        .as_deref()
        .is_some_and(|given| constant_time_eq(given, &shared.state));
    let (callback, status, body) = if !state_ok {
        (Callback::Invalid, 400, failure_page())
    } else if let Some(error) = error {
        if error == "access_denied" {
            (Callback::Denied, 200, denied_page())
        } else {
            (Callback::Invalid, 400, failure_page())
        }
    } else {
        match code {
            Some(code) if code_is_sane(&code) => (Callback::Code(code), 200, success_page()),
            _ => (Callback::Invalid, 400, failure_page()),
        }
    };
    if claimed.swap(true, Ordering::SeqCst) {
        // 两个请求几乎同时到：只有先认领的算数。
        respond(&mut stream, 404, &failure_page()).await;
        return;
    }
    // 先把页面完整写给浏览器，再把结果交出去（交出去之后监听会被关掉）。
    respond(&mut stream, status, &body).await;
    let _ = tx.send(callback).await;
}

#[cfg(test)]
mod tests;
