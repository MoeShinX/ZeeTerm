//! 验收用的本机桩（只在 debug 构建里存在，release 里没有这个模块）。
//!
//! 一个进程内的 HTTP 服务器，只听 `127.0.0.1` 的随机端口，同一个端口按路径前缀扮演 core 同步要连的
//! 四类存储：`/dav`（WebDAV）、`/s3`（S3 兼容）、`/gh`（GitHub Gist 接口）、`/od`（OneDrive 的授权页、
//! 令牌接口和账号接口，只够走完「打开授权网址 → 回调 → 换令牌 → 读账号名」，同步包的读写不在里面）。
//! 四类都不连任何真实云存储。
//!
//! 桩自己不懂同步包的内容：WebDAV 和 S3 当成带 ETag 的键值存储（认 `If-Match` / `If-None-Match`），
//! Gist 当成带版本号的单文件。要的行为都用开关做：并发写（读完之后云端被别处写了一次）、限流、
//! OneDrive 授权被拒 / 一直不回调。凭据是固定的常量，填错了桩就按各家的样子拒绝。
//!
//! 同一个进程里还有一台「另一台电脑」（[`peer_run`]）：它有自己的数据目录和设备 ID，用同一个桩
//! 当云端，用来造出需要处理的局面——第二台电脑解开已有数据、对方删了很多记录（防误删确认）、
//! 两边改了同一条记录（冲突）。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use zssh_core::model::{AuthMethod, HostDraft, ProxyMode};
use zssh_core::store::Database;
use zssh_core::sync::github::GithubConfig;
use zssh_core::sync::guard::DeletionDecision;
use zssh_core::sync::onedrive::browser::BrowserOpener;
use zssh_core::sync::onedrive::oauth::Endpoints;
use zssh_core::sync::s3::S3Config;
use zssh_core::sync::sealer::{DpapiSealer, SecretSealer};
use zssh_core::sync::service::{self, StorageConfig, SyncContext};
use zssh_core::sync::webdav::WebDavConfig;

/// WebDAV 的用户名 / 密码。
pub(crate) const DAV_USER: &str = "stub-user";
pub(crate) const DAV_PASS: &str = "stub-pass";
/// S3 的 Access Key / Secret / Bucket。
pub(crate) const S3_ACCESS: &str = "STUBACCESSKEY";
pub(crate) const S3_SECRET: &str = "stub-secret-key";
pub(crate) const S3_BUCKET: &str = "stub-bucket";
/// GitHub 令牌。
pub(crate) const GH_TOKEN: &str = "ghp_faketoken0123456789";
/// OneDrive 的 client ID（桩只认格式，不认值）。
pub(crate) const OD_CLIENT_ID: &str = "11111111-2222-3333-4444-555555555555";
/// 验收里所有「另一台电脑」和界面用的同步主密码。
pub(crate) const MASTER_PASSWORD: &str = "stub-master-pass-1";

/// OneDrive 授权页收到请求时怎么回。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OneDriveMode {
    /// 用户同意：跳回回环地址并带上授权码。
    Approve,
    /// 用户拒绝：跳回回环地址并带上 `error=access_denied`。
    Deny,
    /// 浏览器一直不回来（用来验「取消登录」）。
    Hang,
}

/// 桩里一次请求的记录（断言用）。
#[derive(Clone, Debug)]
pub(crate) struct Req {
    pub method: String,
    pub path: String,
    pub if_match: Option<String>,
    pub if_none_match: Option<String>,
}

#[derive(Default)]
struct Gist {
    content: String,
    version: u64,
}

pub(crate) struct State {
    objects: HashMap<String, (Vec<u8>, u64)>,
    next_etag: u64,
    gists: HashMap<String, Gist>,
    next_gist: u64,
    log: Vec<Req>,
    /// 下一次读同步包之后，云端被「另一台电脑」写一次（ETag / 版本号变了，内容不变）。
    pub touch_after_next_read: bool,
    /// 接下来还有几个请求回 429（core 遇到临时错误会自己重试几次，要多回几个才会失败）。
    pub rate_limit_left: u32,
    /// 云端把凭据撤销了：本地设置没变，但每个请求都按「凭据无效」回（令牌过期 / 被撤销的样子）。
    pub auth_revoked: bool,
    /// 回每个请求之前先睡多久（毫秒）：让一次同步在途里停几秒，验「同步进行中退出要确认」。
    pub delay_ms: u64,
    od_mode: OneDriveMode,
    /// 打开过的授权网址（只记录，断言用）。
    od_opened: Vec<String>,
    /// OneDrive 授权页收到的 client_id。
    od_client_seen: Vec<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            objects: HashMap::new(),
            next_etag: 0,
            gists: HashMap::new(),
            next_gist: 0,
            log: Vec::new(),
            touch_after_next_read: false,
            rate_limit_left: 0,
            auth_revoked: false,
            delay_ms: 0,
            od_mode: OneDriveMode::Approve,
            od_opened: Vec::new(),
            od_client_seen: Vec::new(),
        }
    }
}

/// 一个桩服务器。测试里每个用例自己起一个（互不干扰）；驱动脚本用进程里唯一的那个（[`global`]）。
#[derive(Clone)]
pub(crate) struct StubServer {
    base: String,
    state: Arc<Mutex<State>>,
}

static GLOBAL: OnceLock<StubServer> = OnceLock::new();
/// OneDrive 的桩是不是接进了登录流程（`sync_demo` 里换端点 / 换打开方式时看它）。
static OD_ACTIVE: AtomicBool = AtomicBool::new(false);

impl StubServer {
    /// 起一个新的桩：只听 `127.0.0.1` 的随机端口。
    pub(crate) fn start() -> Result<Self, String> {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|err| format!("桩绑定端口失败：{err}"))?;
        let port = listener
            .local_addr()
            .map_err(|err| format!("桩读端口失败：{err}"))?
            .port();
        let state = Arc::new(Mutex::new(State::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = Arc::clone(&shared);
                std::thread::spawn(move || {
                    let _ = serve(stream, &state);
                });
            }
        });
        Ok(Self {
            base: format!("http://127.0.0.1:{port}"),
            state,
        })
    }

    /// 读写桩的状态。
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut lock(&self.state))
    }

    pub(crate) fn webdav_url(&self) -> String {
        format!("{}/dav", self.base)
    }

    pub(crate) fn s3_endpoint(&self) -> String {
        format!("{}/s3", self.base)
    }

    pub(crate) fn github_api(&self) -> String {
        format!("{}/gh", self.base)
    }

    /// 桩现有的第一个 Gist 的 ID。
    pub(crate) fn first_gist_id(&self) -> Option<String> {
        self.with(|state| state.gists.keys().next().cloned())
    }

    /// 清空云端（同步包、Gist）和请求记录，不动开关。
    pub(crate) fn reset_cloud(&self) {
        self.with(|state| {
            state.objects.clear();
            state.gists.clear();
            state.log.clear();
        });
    }

    /// 桩对应的存储设置。`gist` 是 Gist ID（GitHub 才用，空 = 第一次同步时新建）。
    pub(crate) fn storage_config(&self, kind: &str, gist: &str) -> Result<StorageConfig, String> {
        Ok(match kind {
            "webdav" => StorageConfig::Webdav(WebDavConfig {
                url: self.webdav_url(),
                directory: "zssh".to_owned(),
                username: DAV_USER.to_owned(),
                password: DAV_PASS.to_owned(),
                allow_insecure_http: true,
            }),
            "s3" => StorageConfig::S3(S3Config {
                endpoint: self.s3_endpoint(),
                region: "us-east-1".to_owned(),
                bucket: S3_BUCKET.to_owned(),
                access_key: S3_ACCESS.to_owned(),
                secret_key: S3_SECRET.to_owned(),
                prefix: "z-ssh".to_owned(),
                path_style: true,
            }),
            "github" => StorageConfig::Github(GithubConfig {
                api_base: self.github_api(),
                token: GH_TOKEN.to_owned(),
                gist_id: gist.to_owned(),
                allow_insecure_http: true,
            }),
            other => return Err(format!("桩没有这种存储：{other}")),
        })
    }

    /// OneDrive 的三个地址指向这个桩。
    pub(crate) fn onedrive_endpoints(&self) -> Endpoints {
        Endpoints {
            authorize: format!("{}/od/authorize", self.base),
            token: format!("{}/od/token", self.base),
            graph: format!("{}/od/v1.0", self.base),
        }
    }

    /// 不开真浏览器的「打开授权页」。
    pub(crate) fn onedrive_opener(&self) -> Box<dyn BrowserOpener> {
        Box::new(StubBrowser {
            server: self.clone(),
        })
    }
}

/// 驱动脚本用的那个桩（第一次用时起）。
pub(crate) fn global() -> Result<&'static StubServer, String> {
    if let Some(server) = GLOBAL.get() {
        return Ok(server);
    }
    let server = StubServer::start()?;
    let _ = GLOBAL.set(server);
    OD_ACTIVE.store(true, Ordering::SeqCst);
    GLOBAL.get().ok_or_else(|| "桩没有起来".to_owned())
}

/// 起桩（已经起过就什么也不做）。
pub(crate) fn start() -> Result<(), String> {
    global().map(|_| ())
}

/// 读写进程里那个桩的状态。
pub(crate) fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Result<R, String> {
    Ok(global_started()?.with(f))
}

/// 进程里那个桩（必须已经起过）。
fn global_started() -> Result<&'static StubServer, String> {
    GLOBAL
        .get()
        .ok_or_else(|| "桩还没有启动（先 act:stub_start）".to_owned())
}

/// 清空进程里那个桩的云端。
pub(crate) fn reset_cloud() -> Result<(), String> {
    global_started()?.reset_cloud();
    Ok(())
}

pub(crate) fn webdav_url() -> Result<String, String> {
    Ok(global_started()?.webdav_url())
}

pub(crate) fn s3_endpoint() -> Result<String, String> {
    Ok(global_started()?.s3_endpoint())
}

pub(crate) fn github_api() -> Result<String, String> {
    Ok(global_started()?.github_api())
}

/// 桩启动之后，OneDrive 的三个地址指向桩；没启动返回 `None`（用微软的）。
pub(crate) fn onedrive_endpoints() -> Option<Endpoints> {
    OD_ACTIVE
        .load(Ordering::SeqCst)
        .then(|| GLOBAL.get().map(StubServer::onedrive_endpoints))
        .flatten()
}

/// 桩启动之后，打开授权页的方式换成不开真浏览器的桩。
pub(crate) fn onedrive_opener() -> Option<Box<dyn BrowserOpener>> {
    OD_ACTIVE
        .load(Ordering::SeqCst)
        .then(|| GLOBAL.get().map(StubServer::onedrive_opener))
        .flatten()
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl State {
    pub(crate) fn set_onedrive_mode(&mut self, mode: OneDriveMode) {
        self.od_mode = mode;
    }

    /// 对同步包（`zssh-sync.json`）的某种请求有几次：断言「并发写之后重试了」用。
    pub(crate) fn package_requests(&self, method: &str) -> usize {
        self.log
            .iter()
            .filter(|req| req.method == method && req.path.ends_with("zssh-sync.json"))
            .count()
    }

    pub(crate) fn requests(&self) -> usize {
        self.log.len()
    }

    /// 对路径以 `suffix` 结尾的对象的某种请求有几次（云端备份：没变化时不该再传）。
    pub(crate) fn object_requests(&self, method: &str, suffix: &str) -> usize {
        self.log
            .iter()
            .filter(|req| req.method == method && req.path.ends_with(suffix))
            .count()
    }

    /// 桩里现有的 WebDAV / S3 对象的路径（排好序）。云端备份测试用来核对同步包和备份在不在、有没有互相覆盖。
    #[cfg(test)]
    pub(crate) fn object_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.objects.keys().cloned().collect();
        keys.sort();
        keys
    }

    pub(crate) fn opened_urls(&self) -> Vec<String> {
        self.od_opened.clone()
    }

    pub(crate) fn client_ids_seen(&self) -> Vec<String> {
        self.od_client_seen.clone()
    }

    /// 同步包的 PUT 用的条件：（`If-Match`, `If-None-Match`）。
    pub(crate) fn package_puts(&self) -> Vec<(Option<String>, Option<String>)> {
        self.log
            .iter()
            .filter(|req| req.method == "PUT" && req.path.ends_with("zssh-sync.json"))
            .map(|req| (req.if_match.clone(), req.if_none_match.clone()))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// OneDrive：「打开浏览器」
// ---------------------------------------------------------------------------

/// 不开真浏览器：把授权网址记下来，在后台线程里替「用户」点一下——`Approve` / `Deny` 就去
/// 访问授权页（桩会 302 到回环地址），再访问回环地址，让 core 的回调监听收到；`Hang` 什么也不做。
struct StubBrowser {
    server: StubServer,
}

impl BrowserOpener for StubBrowser {
    fn open(&self, url: &str) -> Result<(), String> {
        // 和真浏览器的白名单一样严：只允许本机桩的授权页，别的一律拒绝。
        if !url.starts_with(&format!("{}/od/authorize", self.server.base)) {
            return Err("桩只打开本机授权页".to_owned());
        }
        let mode = self.server.with(|state| {
            state.od_opened.push(url.to_owned());
            state.od_mode
        });
        if mode == OneDriveMode::Hang {
            return Ok(());
        }
        let url = url.to_owned();
        std::thread::spawn(move || {
            // 授权页 302 到 `redirect_uri?code=…&state=…`（或 `error=…`）；再访问那个回环地址。
            if let Some(location) =
                http_get(&url).and_then(|(_, headers, _)| headers.get("location").cloned())
            {
                let _ = http_get(&location);
            }
        });
        Ok(())
    }
}

/// 最小的 HTTP GET（只给本机桩用）：返回（状态码, 小写响应头, 响应体）。不跟随跳转。
fn http_get(url: &str) -> Option<(u16, HashMap<String, String>, String)> {
    let rest = url.strip_prefix("http://")?;
    // 地址后面可以直接跟 `?`（core 的回调地址是 `http://localhost:端口`，没有斜杠）。
    let (host, path) = match rest.find(['/', '?']) {
        Some(at) if rest.as_bytes()[at] == b'/' => (&rest[..at], rest[at..].to_owned()),
        Some(at) => (&rest[..at], format!("/{}", &rest[at..])),
        None => (rest, "/".to_owned()),
    };
    let mut stream = TcpStream::connect(host).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )
    .ok()?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).ok()?;
    let status = status_line.split_whitespace().nth(1)?.parse().ok()?;
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let mut body = String::new();
    let _ = reader.read_to_string(&mut body);
    Some((status, headers, body))
}

// ---------------------------------------------------------------------------
// HTTP 服务
// ---------------------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    query: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_owned(), value.into()));
        self
    }

    fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    fn json(status: u16, value: &serde_json::Value) -> Self {
        Self::new(status)
            .header("Content-Type", "application/json")
            .body(value.to_string())
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        302 => "Found",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        412 => "Precondition Failed",
        429 => "Too Many Requests",
        _ => "Status",
    }
}

fn serve(stream: TcpStream, state: &Mutex<State>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let (path, query) = target.split_once('?').map_or_else(
        || (target.clone(), String::new()),
        |(p, q)| (p.to_owned(), q.to_owned()),
    );
    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let request = Request {
        method,
        path,
        query,
        headers,
        body,
    };
    let delay = lock(state).delay_ms;
    if delay > 0 && !request.path.starts_with("/od/") {
        std::thread::sleep(Duration::from_millis(delay));
    }
    let response = {
        let mut state = lock(state);
        state.log.push(Req {
            method: request.method.clone(),
            path: request.path.clone(),
            if_match: request.headers.get("if-match").cloned(),
            if_none_match: request.headers.get("if-none-match").cloned(),
        });
        if !request.path.starts_with("/od/") && state.rate_limit_left > 0 {
            state.rate_limit_left -= 1;
            Response::new(429)
                .header("Retry-After", "120")
                .body("rate limited")
        } else {
            route(&mut state, &request)
        }
    };
    let mut stream = stream;
    write!(
        stream,
        "HTTP/1.1 {} {}\r\n",
        response.status,
        reason(response.status)
    )?;
    for (name, value) in &response.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(
        stream,
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        response.body.len()
    )?;
    stream.write_all(&response.body)?;
    stream.flush()
}

fn route(state: &mut State, request: &Request) -> Response {
    let path = request.path.as_str();
    if let Some(rest) = path.strip_prefix("/dav/") {
        return webdav(state, request, rest);
    }
    if let Some(rest) = path.strip_prefix("/s3/") {
        return s3(state, request, rest);
    }
    if let Some(rest) = path.strip_prefix("/gh/") {
        return github(state, request, rest);
    }
    if let Some(rest) = path.strip_prefix("/od/") {
        return onedrive(state, request, rest);
    }
    Response::new(404)
}

/// 同步包那个对象：读之后如果要「被别处写一次」就把 ETag 换掉。
fn touch_if_asked(state: &mut State, key: &str) {
    if key.ends_with("zssh-sync.json") && std::mem::take(&mut state.touch_after_next_read) {
        state.next_etag += 1;
        let next = state.next_etag;
        if let Some(object) = state.objects.get_mut(key) {
            object.1 = next;
        }
    }
}

/// 带 ETag 的键值存储：GET / PUT / DELETE，PUT 和 DELETE 认 `If-Match` / `If-None-Match`。
fn object_store(state: &mut State, request: &Request, key: &str) -> Response {
    let etag_of = |n: u64| format!("\"{n}\"");
    let if_match = request.headers.get("if-match").cloned();
    let if_none_match = request.headers.get("if-none-match").cloned();
    match request.method.as_str() {
        "GET" => match state.objects.get(key).cloned() {
            Some((bytes, n)) => {
                touch_if_asked(state, key);
                Response::new(200).header("ETag", etag_of(n)).body(bytes)
            }
            None => Response::new(404),
        },
        "PUT" => {
            let current = state.objects.get(key).map(|(_, n)| *n);
            let failed = match (&if_match, &if_none_match, current) {
                (_, Some(_), Some(_)) => true,
                (Some(wanted), _, current) => {
                    current.map(etag_of).as_deref() != Some(wanted.as_str())
                }
                _ => false,
            };
            if failed {
                return Response::new(412);
            }
            state.next_etag += 1;
            let n = state.next_etag;
            state
                .objects
                .insert(key.to_owned(), (request.body.clone(), n));
            Response::new(201).header("ETag", etag_of(n))
        }
        "DELETE" => {
            let current = state.objects.get(key).map(|(_, n)| *n);
            if let Some(wanted) = &if_match
                && current.map(etag_of).as_deref() != Some(wanted.as_str())
            {
                return Response::new(412);
            }
            if state.objects.remove(key).is_some() {
                Response::new(204)
            } else {
                Response::new(404)
            }
        }
        _ => Response::new(405),
    }
}

fn b64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn webdav(state: &mut State, request: &Request, rest: &str) -> Response {
    let expected = format!("Basic {}", b64(format!("{DAV_USER}:{DAV_PASS}").as_bytes()));
    if state.auth_revoked || request.headers.get("authorization") != Some(&expected) {
        return Response::new(401).header("WWW-Authenticate", "Basic realm=\"stub\"");
    }
    if request.method == "MKCOL" {
        return Response::new(201);
    }
    object_store(state, request, &format!("/dav/{rest}"))
}

fn s3(state: &mut State, request: &Request, rest: &str) -> Response {
    let xml = |status: u16, code: &str| {
        Response::new(status)
            .header("Content-Type", "application/xml")
            .body(format!(
                "<Error><Code>{code}</Code><Message>stub</Message></Error>"
            ))
    };
    let credential = format!("Credential={S3_ACCESS}/");
    if state.auth_revoked
        || !request
            .headers
            .get("authorization")
            .is_some_and(|value| value.contains(&credential))
    {
        return xml(403, "InvalidAccessKeyId");
    }
    let Some((bucket, key)) = rest.split_once('/') else {
        return xml(404, "NoSuchKey");
    };
    if bucket != S3_BUCKET {
        return xml(404, "NoSuchBucket");
    }
    object_store(state, request, &format!("/s3/{bucket}/{key}"))
}

fn github(state: &mut State, request: &Request, rest: &str) -> Response {
    let json = |status: u16, value: serde_json::Value| Response::json(status, &value);
    let expected = format!("Bearer {GH_TOKEN}");
    if state.auth_revoked || request.headers.get("authorization") != Some(&expected) {
        return json(401, serde_json::json!({"message": "Bad credentials"}));
    }
    let gist_json = |id: &str, gist: &Gist| {
        serde_json::json!({
            "id": id,
            "owner": {"login": "stub-user"},
            "history": [{"version": format!("v{}", gist.version)}],
            "files": {"zssh-sync.json": {
                "content": gist.content,
                "truncated": false,
                "size": gist.content.len(),
            }},
        })
    };
    if rest == "user" && request.method == "GET" {
        return Response::json(200, &serde_json::json!({"login": "stub-user"}))
            .header("X-OAuth-Scopes", "gist");
    }
    if rest == "gists" && request.method == "POST" {
        state.next_gist += 1;
        let id = format!("a1b2c3d4e5f6{:08x}", state.next_gist);
        let content = serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|value| {
                value["files"]["zssh-sync.json"]["content"]
                    .as_str()
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        let gist = Gist {
            content,
            version: 1,
        };
        let body = gist_json(&id, &gist);
        state.gists.insert(id, gist);
        return json(201, body);
    }
    let Some(id) = rest.strip_prefix("gists/") else {
        return json(404, serde_json::json!({"message": "Not Found"}));
    };
    if !state.gists.contains_key(id) {
        return json(404, serde_json::json!({"message": "Not Found"}));
    }
    match request.method.as_str() {
        "GET" => {
            let body = gist_json(id, &state.gists[id]);
            // 并发写：读完之后版本号被「另一台电脑」推进一格。
            if std::mem::take(&mut state.touch_after_next_read)
                && let Some(gist) = state.gists.get_mut(id)
            {
                gist.version += 1;
            }
            json(200, body)
        }
        "PATCH" => {
            let value = serde_json::from_slice::<serde_json::Value>(&request.body)
                .unwrap_or(serde_json::Value::Null);
            if let Some(gist) = state.gists.get_mut(id) {
                match &value["files"]["zssh-sync.json"] {
                    serde_json::Value::Null => gist.content.clear(),
                    file => {
                        gist.content = file["content"].as_str().unwrap_or_default().to_owned();
                    }
                }
                gist.version += 1;
            }
            json(200, gist_json(id, &state.gists[id]))
        }
        "DELETE" => {
            state.gists.remove(id);
            Response::new(204)
        }
        _ => Response::new(405),
    }
}

/// 把 `a=1&b=2` 拆成键值（`%3A` / `%2F` 解回去，别的百分号编码桩里用不到）。
fn form(text: &str) -> HashMap<String, String> {
    text.split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| {
            (
                k.to_owned(),
                v.replace("%3A", ":")
                    .replace("%3a", ":")
                    .replace("%2F", "/")
                    .replace("%2f", "/"),
            )
        })
        .collect()
}

fn onedrive(state: &mut State, request: &Request, rest: &str) -> Response {
    match (request.method.as_str(), rest) {
        // 授权页：按开关 302 回 `redirect_uri`。
        ("GET", "authorize") => {
            let query = form(&request.query);
            if let Some(client) = query.get("client_id") {
                state.od_client_seen.push(client.clone());
            }
            let redirect = query.get("redirect_uri").cloned().unwrap_or_default();
            let state_value = query.get("state").cloned().unwrap_or_default();
            let location = match state.od_mode {
                OneDriveMode::Deny => format!(
                    "{redirect}?error=access_denied&error_description=stub-denied&state={state_value}"
                ),
                _ => format!("{redirect}?code=stubcode&state={state_value}"),
            };
            Response::new(302).header("Location", location)
        }
        ("POST", "token") => {
            let params = form(&String::from_utf8_lossy(&request.body));
            let valid = params.get("grant_type").map(String::as_str) == Some("authorization_code")
                && params.get("code").map(String::as_str) == Some("stubcode")
                && params.get("code_verifier").is_some_and(|v| !v.is_empty());
            if !valid {
                return Response::json(
                    400,
                    &serde_json::json!({"error": "invalid_grant", "error_description": "stub"}),
                );
            }
            Response::json(
                200,
                &serde_json::json!({
                    "access_token": "stub-access-token",
                    "refresh_token": "stub-refresh-token",
                    "expires_in": 3600,
                }),
            )
        }
        ("GET", "v1.0/me") => {
            if request.headers.get("authorization").map(String::as_str)
                != Some("Bearer stub-access-token")
            {
                return Response::new(401);
            }
            Response::json(
                200,
                &serde_json::json!({
                    "displayName": "桩账号",
                    "userPrincipalName": "stub@example.invalid",
                }),
            )
        }
        _ => Response::new(404),
    }
}

// ---------------------------------------------------------------------------
// 「另一台电脑」
// ---------------------------------------------------------------------------

/// 另一台电脑的数据目录：和界面的数据目录同级。
pub(crate) fn peer_dir() -> PathBuf {
    let data = crate::settings::data_dir();
    data.parent()
        .map_or_else(|| data.join("peer"), |parent| parent.join("peer"))
}

fn draft(name: &str, id: Option<i64>) -> HostDraft {
    HostDraft {
        protocol: None,
        id,
        group_id: None,
        name: name.to_owned(),
        host: "192.0.2.10".to_owned(),
        port: 22,
        username: "ops".to_owned(),
        auth_method: AuthMethod::Password,
        identity_id: None,
        proxy_id: None,
        jump_host_id: None,
        proxy_mode: ProxyMode::Default,
        tags: Vec::new(),
        sort: 0,
        legacy_algorithms: false,
    }
}

/// 驱动脚本用：在进程里那个桩上，让「另一台电脑」（数据目录 [`peer_dir`]）做一件事再同步一次。
pub(crate) fn peer_run(
    handle: &tokio::runtime::Handle,
    kind: &str,
    op: &str,
) -> Result<String, String> {
    peer_run_at(
        global_started()?,
        handle,
        &DpapiSealer,
        &peer_dir(),
        kind,
        op,
    )
}

/// 在另一台电脑上做一件事再同步一次。`kind` 是存储（webdav / s3 / github），`op`：
/// - `hosts:<n>` 建 n 台主机（`peer-1`…）；
/// - `delete:<n>` 删掉 n 台主机（id 最小的先删）；
/// - `rename:<旧名>:<新名>` 改一台主机的名字；
/// - `sync` 只同步。
///
/// 另一台电脑自己的同步决定一律「继续」（它不是被验收的那台）。返回一句结果。
pub(crate) fn peer_run_at(
    server: &StubServer,
    handle: &tokio::runtime::Handle,
    sealer: &dyn SecretSealer,
    dir: &std::path::Path,
    kind: &str,
    op: &str,
) -> Result<String, String> {
    std::fs::create_dir_all(dir).map_err(|err| format!("建另一台电脑的目录失败：{err}"))?;
    let db = Database::open(&dir.join("zssh.db"))
        .map_err(|err| format!("打开另一台电脑的库失败：{err}"))?;
    let mut gist = if kind == "github" {
        server.first_gist_id().unwrap_or_default()
    } else {
        String::new()
    };
    // GitHub：另一台电脑第一次用时先新建一个 Gist（同步包只写这个 Gist）。
    if kind == "github" && gist.is_empty() {
        let StorageConfig::Github(github) = server.storage_config(kind, "")? else {
            return Err("桩没有 GitHub 设置".to_owned());
        };
        gist = handle
            .block_on(zssh_core::sync::github::create_gist(&github, None))
            .map_err(|err| format!("新建 Gist 失败：{err}"))?;
    }
    let config = server.storage_config(kind, &gist)?;
    service::save_config(&db, sealer, &config).map_err(|err| err.to_string())?;
    let (name, arg) = op.split_once(':').unwrap_or((op, ""));
    match name {
        "hosts" => {
            let count: usize = arg.parse().map_err(|_| "hosts:<个数>".to_owned())?;
            let existing = db.host_list().map_err(|err| err.to_string())?.len();
            for index in 1..=count {
                db.host_save(&draft(&format!("peer-{}", existing + index), None))
                    .map_err(|err| err.to_string())?;
            }
        }
        "delete" => {
            let count: usize = arg.parse().map_err(|_| "delete:<个数>".to_owned())?;
            let mut hosts = db.host_list().map_err(|err| err.to_string())?;
            hosts.sort_by_key(|host| host.id);
            for host in hosts.into_iter().take(count) {
                db.host_delete(host.id).map_err(|err| err.to_string())?;
            }
        }
        "rename" => {
            let (old, new) = arg.split_once(':').ok_or("rename:<旧名>:<新名>")?;
            let host = db
                .host_list()
                .map_err(|err| err.to_string())?
                .into_iter()
                .find(|host| host.name == old)
                .ok_or("另一台电脑上没有这台主机")?;
            db.host_save(&draft(new, Some(host.id)))
                .map_err(|err| err.to_string())?;
        }
        "sync" => {}
        _ => return Err(format!("不认识的操作：{name}")),
    }
    let device_id = db.sync_device_id().map_err(|err| err.to_string())?;
    let context = SyncContext {
        db: &db,
        sealer,
        device_id,
        data_dir: dir.to_path_buf(),
        known_hosts_path: Some(dir.join("known_hosts")),
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        onedrive_endpoints: Endpoints::microsoft(),
    };
    let report = handle
        .block_on(context.run_sync(MASTER_PASSWORD, Some(DeletionDecision::Continue)))
        .map_err(|err| err.to_string())?;
    Ok(format!("另一台电脑同步：{:?}", report.status))
}

/// 测试用的可逆「加密」（和 DPAPI 一样只管本机、不上传；绕开真的 DPAPI）。
#[cfg(test)]
pub(crate) struct XorSealer(pub u8);

#[cfg(test)]
impl SecretSealer for XorSealer {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, zssh_core::sync::error::SyncError> {
        let mut out = vec![b'X', self.0];
        out.extend(plaintext.iter().map(|byte| byte ^ self.0));
        Ok(out)
    }

    fn open(&self, blob: &[u8]) -> Result<Vec<u8>, zssh_core::sync::error::SyncError> {
        if blob.len() < 2 || blob[0] != b'X' || blob[1] != self.0 {
            return Err(zssh_core::sync::error::SyncError::InvalidData(
                "不是 XorSealer 的密文".to_owned(),
            ));
        }
        Ok(blob[2..].iter().map(|byte| byte ^ self.0).collect())
    }
}

/// 测试里造一台主机用的草稿。
#[cfg(test)]
pub(crate) fn draft_for_tests(name: &str) -> HostDraft {
    draft(name, None)
}

fn local_db() -> Result<Database, String> {
    let dir = crate::settings::data_dir();
    std::fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    Database::open(&dir.join("zssh.db")).map_err(|err| err.to_string())
}

/// 在界面这台电脑的库里改一台主机的名字（造「两边改了同一条记录」的冲突）。
pub(crate) fn local_rename(old: &str, new: &str) -> Result<(), String> {
    let db = local_db()?;
    let host = db
        .host_list()
        .map_err(|err| err.to_string())?
        .into_iter()
        .find(|host| host.name == old)
        .ok_or("本机没有这台主机")?;
    db.host_save(&draft(new, Some(host.id)))
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// 把本机的同步基线文件写成乱码（造「基线损坏」）。
pub(crate) fn corrupt_baseline() -> Result<(), String> {
    let path = crate::settings::data_dir().join("sync-baseline.bin");
    if !path.is_file() {
        return Err("本机还没有同步基线（先同步一次）".to_owned());
    }
    std::fs::write(&path, b"not a baseline").map_err(|err| format!("写基线失败：{err}"))
}

/// 本机现在有几台主机（断言用）。
pub(crate) fn local_host_count() -> Result<usize, String> {
    Ok(local_db()?
        .host_list()
        .map_err(|err| err.to_string())?
        .len())
}

/// 本机有没有这个名字的主机。
pub(crate) fn local_has_host(name: &str) -> Result<bool, String> {
    Ok(local_db()?
        .host_list()
        .map_err(|err| err.to_string())?
        .iter()
        .any(|host| host.name == name))
}

/// 在本机库里造 n 台主机（`local-1`…）。
pub(crate) fn local_hosts(count: usize) -> Result<(), String> {
    let db = local_db()?;
    let existing = db.host_list().map_err(|err| err.to_string())?.len();
    for index in 1..=count {
        db.host_save(&draft(&format!("local-{}", existing + index), None))
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}
