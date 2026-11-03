use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::*;

const STATE: &str = "state-0123456789abcdef";

async fn raw(addr: SocketAddr, request: &str) -> String {
    let mut stream = TcpStream::connect(addr).await.expect("连接");
    let _ = stream.write_all(request.as_bytes()).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
    String::from_utf8_lossy(&out).into_owned()
}

fn get(path_and_query: &str, host: &str) -> String {
    format!("GET {path_and_query} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
}

fn status(response: &str) -> u16 {
    response
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

struct Running {
    addr: SocketAddr,
    path: String,
    cancel: Arc<Notify>,
    handle: JoinHandle<Result<Callback, ConnectError>>,
}

impl Running {
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }

    /// 带正确路径的 GET。
    async fn callback(&self, query: &str) -> String {
        raw(
            self.addr,
            &get(&format!("{}?{query}", self.path), &self.host()),
        )
        .await
    }

    async fn finish(self) -> Result<Callback, ConnectError> {
        tokio::time::timeout(Duration::from_secs(5), self.handle)
            .await
            .expect("5 秒内要结束")
            .expect("任务没崩")
    }
}

async fn start(timeout: Duration) -> Running {
    let listener = CallbackListener::bind().await.expect("绑定");
    let addr = listener.local_addr().expect("地址");
    let path = listener.path().to_owned();
    let cancel = Arc::new(Notify::new());
    let waiter = cancel.clone();
    let handle = tokio::spawn(async move { listener.wait(STATE, timeout, &waiter).await });
    Running {
        addr,
        path,
        cancel,
        handle,
    }
}

#[tokio::test]
async fn it_binds_only_the_ipv4_loopback_on_a_random_port_with_a_random_path() {
    let a = CallbackListener::bind().await.unwrap();
    let b = CallbackListener::bind().await.unwrap();
    for listener in [&a, &b] {
        let addr = listener.local_addr().unwrap();
        assert_eq!(
            addr.ip().to_string(),
            "127.0.0.1",
            "只绑 127.0.0.1，不绑 0.0.0.0"
        );
        assert!(addr.port() >= 1024);
        assert!(listener.path().starts_with('/') && listener.path().ends_with("/cb"));
        assert!(
            listener.path().len() > 20,
            "随机段要够长：{}",
            listener.path()
        );
        assert_eq!(
            listener.redirect_uri(),
            format!("http://127.0.0.1:{}{}", addr.port(), listener.path())
        );
    }
    assert_ne!(a.path(), b.path());
    assert_ne!(a.port(), b.port());
}

#[tokio::test]
async fn a_good_callback_yields_the_code_and_closes_the_port() {
    let run = start(Duration::from_secs(30)).await;
    let response = run.callback(&format!("code=abc123&state={STATE}")).await;
    assert_eq!(status(&response), 200);
    assert!(response.contains("已授权"));
    assert!(response.contains("Content-Security-Policy"));
    // 页面里不回显授权码，也没有任何外部资源。
    assert!(!response.contains("abc123"));
    assert!(!response.contains("http://") && !response.contains("https://"));
    let addr = run.addr;
    assert_eq!(run.finish().await, Ok(Callback::Code("abc123".to_owned())));
    // 拿到回调后监听立刻关闭：端口连不上了（只接受一次请求）。
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(TcpStream::connect(addr).await.is_err(), "监听应已关闭");
}

#[tokio::test]
async fn a_wrong_state_or_a_missing_code_is_dropped_as_a_failed_authorization() {
    let cases = [
        "code=abc&state=wrong-state-value-xx",
        "code=abc",
        &format!("state={STATE}"),
        &format!("code=&state={STATE}"),
        &format!("code=abc%3Cscript%3E&state={STATE}"),
        &format!("code={}&state={STATE}", "a".repeat(600)),
        "",
    ];
    for query in cases {
        let run = start(Duration::from_secs(30)).await;
        let response = run.callback(query).await;
        assert_eq!(status(&response), 400, "{query}");
        assert!(response.contains("授权失败，请重试"));
        assert!(!response.contains("abc"), "不回显请求里的内容");
        assert_eq!(run.finish().await, Ok(Callback::Invalid), "{query}");
    }
}

#[tokio::test]
async fn the_user_denying_access_is_reported_and_other_provider_errors_are_failures() {
    let run = start(Duration::from_secs(30)).await;
    let response = run
        .callback(&format!("error=access_denied&state={STATE}"))
        .await;
    assert_eq!(status(&response), 200);
    assert_eq!(run.finish().await, Ok(Callback::Denied));
    let run = start(Duration::from_secs(30)).await;
    run.callback(&format!("error=server_error&state={STATE}"))
        .await;
    assert_eq!(run.finish().await, Ok(Callback::Invalid));
    // 拒绝回调的 state 也得对：不然随便谁都能取消用户的授权。
    let run = start(Duration::from_secs(30)).await;
    run.callback("error=access_denied&state=nope-nope-nope")
        .await;
    assert_eq!(run.finish().await, Ok(Callback::Invalid));
}

#[tokio::test]
async fn other_paths_methods_and_hosts_are_ignored_without_spending_the_authorization() {
    let run = start(Duration::from_secs(30)).await;
    let host = run.host();
    // 路径不对（猜不到随机段的页面碰不到回调，favicon 之类也是）。
    for path in [
        "/",
        "/cb",
        "/favicon.ico",
        "/x/cb?code=abc&state=".to_owned().as_str(),
    ] {
        let response = raw(run.addr, &get(path, &host)).await;
        assert_eq!(status(&response), 404, "{path}");
    }
    // 路径对但不是 GET：400 并忽略。
    for method in ["POST", "PUT", "DELETE", "HEAD"] {
        let request = format!(
            "{method} {}?code=abc&state={STATE} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            run.path
        );
        let response = raw(run.addr, &request).await;
        assert_eq!(status(&response), 400, "{method}");
    }
    // Host 头不是 127.0.0.1（DNS 重绑定）：400 并忽略。
    for evil in ["evil.example", "localhost:1", "127.0.0.1:1", "[::1]"] {
        let response = raw(
            run.addr,
            &get(&format!("{}?code=abc&state={STATE}", run.path), evil),
        )
        .await;
        assert_eq!(status(&response), 400, "{evil}");
    }
    // 没有 Host 头。
    let response = raw(
        run.addr,
        &format!("GET {}?code=abc&state={STATE} HTTP/1.1\r\n\r\n", run.path),
    )
    .await;
    assert_eq!(status(&response), 400);
    // 全是干扰之后，真正的回调还能成功。
    let response = run.callback(&format!("code=real&state={STATE}")).await;
    assert_eq!(status(&response), 200);
    assert_eq!(run.finish().await, Ok(Callback::Code("real".to_owned())));
}

#[tokio::test]
async fn only_the_first_valid_callback_counts() {
    let run = start(Duration::from_secs(30)).await;
    let (addr, path, host) = (run.addr, run.path.clone(), run.host());
    let first = run.callback(&format!("code=first&state={STATE}")).await;
    assert_eq!(status(&first), 200);
    // 监听已经关了：第二个要么连不上，要么被拒绝，绝不会再产生一个授权码。
    tokio::time::sleep(Duration::from_millis(50)).await;
    if let Ok(mut stream) = TcpStream::connect(addr).await {
        let _ = stream
            .write_all(get(&format!("{path}?code=second&state={STATE}"), &host).as_bytes())
            .await;
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut out)).await;
        assert!(!String::from_utf8_lossy(&out).contains("200 OK"));
    }
    assert_eq!(run.finish().await, Ok(Callback::Code("first".to_owned())));
}

#[tokio::test]
async fn waiting_times_out_and_can_be_cancelled_and_both_close_the_port() {
    let run = start(Duration::from_millis(150)).await;
    let addr = run.addr;
    assert_eq!(run.finish().await, Err(ConnectError::Timeout));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "超时后端口应已关闭"
    );

    let run = start(Duration::from_secs(60)).await;
    let addr = run.addr;
    run.cancel.notify_one();
    assert_eq!(run.finish().await, Err(ConnectError::Cancelled));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        TcpStream::connect(addr).await.is_err(),
        "取消后端口应已关闭"
    );
}

#[tokio::test]
async fn a_stalled_connection_does_not_block_the_real_callback() {
    let run = start(Duration::from_secs(30)).await;
    // 一个只连不发的半开连接。
    let _idle = TcpStream::connect(run.addr).await.unwrap();
    // 一个只发了半截请求头的连接。
    let mut half = TcpStream::connect(run.addr).await.unwrap();
    half.write_all(b"GET /x HTTP/1.1\r\nHost: 127.0.0.1")
        .await
        .unwrap();
    // 超长请求头：服务端直接放弃这个连接。
    let mut huge = TcpStream::connect(run.addr).await.unwrap();
    let junk = format!("GET / HTTP/1.1\r\nX: {}\r\n", "a".repeat(20_000));
    let _ = huge.write_all(junk.as_bytes()).await;
    let response = run.callback(&format!("code=ok&state={STATE}")).await;
    assert_eq!(status(&response), 200);
    assert_eq!(run.finish().await, Ok(Callback::Code("ok".to_owned())));
}
