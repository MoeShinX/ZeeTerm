//! 内置快照、运行时缓存的加载与更新。
//!
//! 加载顺序：缓存（比快照新、且解析成功）> 内置快照 > 空表。每一步失败都退到下一步，
//! 不 panic、不报错给用户（最差就是只靠规则兜底）。
//! 更新：缓存超过 7 天（或不存在 / 损坏）就拉一次完整 `api.json`，裁剪后原子写入缓存文件；
//! 任何失败都只是"这次没更新"，不影响正在用的数据。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ssh::BoxFuture;

use super::data::{trim_api_json, CatalogError, CatalogFile, FORMAT_VERSION};
use super::matcher::{Catalog, CatalogOrigin};

/// 安装包内置的裁剪快照（由 `scripts/models-dev-snapshot.ps1` 生成）。
const BUILTIN_SNAPSHOT: &str = include_str!("models-dev-snapshot.json");

/// 完整数据库的下载地址。
pub const MODELS_DEV_API_URL: &str = "https://models.dev/api.json";
/// 缓存文件名（放在调用方给的缓存目录下）。
pub const CACHE_FILE_NAME: &str = "models-dev-cache.json";
/// 缓存多久算过期（秒）。
pub const STALE_AFTER_SECS: u64 = 7 * 24 * 3600;
/// 响应体上限：完整 api.json 约 6 MB，留足余量，只防异常响应把内存吃光。
pub const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;
/// 裁剪后少于这个条数就认为数据有问题（错误页、被截断的响应），不覆盖现有缓存。
const MIN_REFRESH_ENTRIES: usize = 20;
const SOURCE_NAME: &str = "models.dev";

/// 可注入的 HTTP 下载。调用方负责代理、超时、证书等；失败返回面向日志的原因文本。
pub trait HttpFetch: Send + Sync {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, String>>;
}

/// 用 reqwest 客户端实现 [`HttpFetch`]（客户端由调用方按"软件网络"设置建好）。
pub struct ReqwestFetch {
    client: reqwest::Client,
}

impl ReqwestFetch {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl HttpFetch for ReqwestFetch {
    fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            let mut resp = self
                .client
                .get(url)
                .timeout(std::time::Duration::from_secs(60))
                .send()
                .await
                .map_err(|e| format!("请求失败：{e}"))?;
            if !resp.status().is_success() {
                return Err(format!("服务器返回 {}", resp.status()));
            }
            let mut body = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| format!("读取响应失败：{e}"))?
            {
                if body.len() + chunk.len() > MAX_BODY_BYTES {
                    return Err("响应体过大".to_string());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(body)
        })
    }
}

/// 现在的 Unix 秒。
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_file(text: &str) -> Result<CatalogFile, CatalogError> {
    let file: CatalogFile =
        serde_json::from_str(text).map_err(|e| CatalogError::Parse(e.to_string()))?;
    if file.version != FORMAT_VERSION {
        return Err(CatalogError::Invalid(format!(
            "不认识的格式版本 {}",
            file.version
        )));
    }
    if file.entries.is_empty() {
        return Err(CatalogError::Invalid("没有条目".to_string()));
    }
    Ok(file)
}

fn read_cache(cache_dir: &Path) -> Result<CatalogFile, CatalogError> {
    let text = std::fs::read_to_string(cache_dir.join(CACHE_FILE_NAME))
        .map_err(|e| CatalogError::Io(e.to_string()))?;
    let file = parse_file(&text)?;
    if file.fetched_at.is_none() {
        return Err(CatalogError::Invalid("缓存缺少拉取时间".to_string()));
    }
    Ok(file)
}

impl Catalog {
    /// 只加载内置快照；快照损坏（不该发生，单测会拦住）时返回空表。
    pub fn builtin() -> Catalog {
        Self::from_snapshot_text(BUILTIN_SNAPSHOT)
    }

    fn from_snapshot_text(text: &str) -> Catalog {
        match parse_file(text) {
            Ok(file) => Catalog::from_file(file, CatalogOrigin::Snapshot),
            Err(e) => {
                tracing::warn!("内置 models.dev 快照不可用：{e}");
                Catalog::empty()
            }
        }
    }

    /// 按"缓存（更新时）> 内置快照 > 空表"加载。`cache_dir` 是 [`refresh_if_stale`] 用的同一个目录。
    pub fn load(cache_dir: &Path) -> Catalog {
        Self::load_with_snapshot(cache_dir, BUILTIN_SNAPSHOT)
    }

    fn load_with_snapshot(cache_dir: &Path, snapshot_text: &str) -> Catalog {
        let snapshot = Self::from_snapshot_text(snapshot_text);
        match read_cache(cache_dir) {
            Ok(file) => {
                let newer = match (file.timestamp(), snapshot.timestamp()) {
                    (Some(c), Some(s)) => c > s,
                    // 快照没有时间（空表）：缓存是唯一的数据。
                    (Some(_), None) => true,
                    _ => false,
                };
                if newer {
                    return Catalog::from_file(file, CatalogOrigin::Cache);
                }
            }
            Err(CatalogError::Io(_)) => {} // 没有缓存文件是常态。
            Err(e) => tracing::warn!("models.dev 缓存不可用，改用内置快照：{e}"),
        }
        snapshot
    }
}

/// 一次 [`refresh_if_stale`] 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// 设置里关了联网更新，什么都没做。
    Disabled,
    /// 缓存还新鲜，什么都没做。
    Fresh,
    /// 拉取并写入了新缓存，里面有这么多条目。
    Updated { entries: usize },
    /// 这次没更新成功（网络、解析或写盘问题）；原有缓存和快照不受影响。
    Failed(String),
}

/// 缓存过期就更新。`enabled` 为 false（设置里关掉联网更新）直接返回；`now` 是 Unix 秒。
pub async fn refresh_if_stale(
    cache_dir: &Path,
    http: &dyn HttpFetch,
    now: u64,
    enabled: bool,
) -> RefreshOutcome {
    if !enabled {
        return RefreshOutcome::Disabled;
    }
    if let Ok(file) = read_cache(cache_dir) {
        if let Some(at) = file.fetched_at {
            // 时钟被调回去（缓存时间在"未来"）按过期处理，免得永远不更新。
            if at <= now && now - at < STALE_AFTER_SECS {
                return RefreshOutcome::Fresh;
            }
        }
    }
    match fetch_and_store(cache_dir, http, now).await {
        Ok(entries) => RefreshOutcome::Updated { entries },
        Err(e) => {
            tracing::info!("models.dev 缓存更新失败：{e}");
            RefreshOutcome::Failed(e.to_string())
        }
    }
}

async fn fetch_and_store(
    cache_dir: &Path,
    http: &dyn HttpFetch,
    now: u64,
) -> Result<usize, CatalogError> {
    let body = http
        .get(MODELS_DEV_API_URL)
        .await
        .map_err(CatalogError::Io)?;
    if body.len() > MAX_BODY_BYTES {
        return Err(CatalogError::Invalid("响应体过大".to_string()));
    }
    let entries = trim_api_json(&body)?;
    if entries.len() < MIN_REFRESH_ENTRIES {
        return Err(CatalogError::Invalid(format!(
            "只有 {} 个可用条目，疑似不是完整数据",
            entries.len()
        )));
    }
    let count = entries.len();
    let file = CatalogFile {
        version: FORMAT_VERSION,
        source: SOURCE_NAME.to_string(),
        generated: None,
        fetched_at: Some(now),
        entries,
    };
    let json = file
        .to_json()
        .map_err(|e| CatalogError::Io(e.to_string()))?;
    write_atomic(&cache_dir.join(CACHE_FILE_NAME), json.as_bytes())?;
    Ok(count)
}

/// 先写同目录的临时文件再改名：读的一方要么看到旧文件要么看到新文件，崩溃也不会留下半截。
fn write_atomic(path: &Path, data: &[u8]) -> Result<(), CatalogError> {
    let io = |e: std::io::Error| CatalogError::Io(e.to_string());
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io)?;
    let mut tmp: PathBuf = path.to_path_buf();
    tmp.set_extension(format!("tmp{}", std::process::id()));
    let result = (|| {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(io(e));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::agent::builtin::catalog::data::{date_to_unix, CatalogEntry};
    use crate::ai::agent::builtin::catalog::ReasoningSupport;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn entry(provider: &str, id: &str, ctx: u32) -> CatalogEntry {
        CatalogEntry {
            provider: provider.to_string(),
            id: id.to_string(),
            name: id.to_string(),
            canonical: None,
            family: None,
            context: ctx,
            input: None,
            output: 4096,
            reasoning: ReasoningSupport::None,
            tool_call: true,
            image: false,
            cost: None,
            deprecated: false,
        }
    }

    fn file(generated: Option<&str>, fetched: Option<u64>, ctx: u32) -> String {
        let f = CatalogFile {
            version: FORMAT_VERSION,
            source: SOURCE_NAME.to_string(),
            generated: generated.map(str::to_string),
            fetched_at: fetched,
            entries: vec![entry("openai", "gpt-test", ctx)],
        };
        f.to_json().expect("serialize")
    }

    /// 一份够大的假 api.json（超过最小条数）。
    fn fake_api(n: usize, ctx: u32) -> Vec<u8> {
        let mut models = String::new();
        for i in 0..n {
            if i > 0 {
                models.push(',');
            }
            models.push_str(&format!(
                r#""m{i}":{{"id":"m{i}","name":"M {i}","reasoning":false,"tool_call":true,"modalities":{{"input":["text"],"output":["text"]}},"limit":{{"context":{ctx},"output":4096}}}}"#
            ));
        }
        format!(r#"{{"openai":{{"id":"openai","models":{{{models}}}}},"someone-else":{{"models":{{}}}}}}"#)
            .into_bytes()
    }

    struct Stub {
        body: Result<Vec<u8>, String>,
        calls: AtomicUsize,
    }
    impl Stub {
        fn ok(body: Vec<u8>) -> Self {
            Stub {
                body: Ok(body),
                calls: AtomicUsize::new(0),
            }
        }
        fn err(msg: &str) -> Self {
            Stub {
                body: Err(msg.to_string()),
                calls: AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }
    impl HttpFetch for Stub {
        fn get<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(url, MODELS_DEV_API_URL);
            let r = self.body.clone();
            Box::pin(async move { r })
        }
    }

    fn write_cache(dir: &Path, content: &str) {
        std::fs::write(dir.join(CACHE_FILE_NAME), content).expect("write cache");
    }

    const DAY: u64 = 86_400;

    #[test]
    fn builtin_snapshot_loads_and_is_small() {
        assert!(
            BUILTIN_SNAPSHOT.len() < 1024 * 1024,
            "快照超过 1MB：{}",
            BUILTIN_SNAPSHOT.len()
        );
        let c = Catalog::builtin();
        assert_eq!(c.origin(), CatalogOrigin::Snapshot);
        assert!(c.len() > 200, "条数太少：{}", c.len());
        assert!(c.timestamp().is_some());
        for provider in [
            "openai",
            "anthropic",
            "google",
            "deepseek",
            "alibaba",
            "moonshotai",
            "zhipuai",
            "minimax",
            "xai",
            "mistral",
            "openrouter",
        ] {
            assert!(
                c.entries().iter().any(|e| e.provider == provider),
                "快照里没有 {provider}"
            );
        }
        // 键唯一。
        let mut keys: Vec<_> = c.entries().iter().map(|e| e.key()).collect();
        keys.sort();
        keys.dedup();
        assert_eq!(keys.len(), c.len());
    }

    #[test]
    fn snapshot_roundtrips_through_its_own_format() {
        let c = Catalog::builtin();
        let parsed = parse_file(BUILTIN_SNAPSHOT).expect("parse");
        let again = parse_file(&parsed.to_json().expect("ser")).expect("reparse");
        assert_eq!(parsed, again);
        assert_eq!(parsed.entries.len(), c.len());
    }

    #[test]
    fn load_prefers_cache_only_when_newer_than_snapshot() {
        let dir = tempfile::tempdir().expect("tmp");
        let snap = file(Some("2026-01-10"), None, 111_000);
        let snap_ts = date_to_unix("2026-01-10").expect("date");

        // 缓存比快照新 → 用缓存。
        write_cache(dir.path(), &file(None, Some(snap_ts + DAY), 222_000));
        let c = Catalog::load_with_snapshot(dir.path(), &snap);
        assert_eq!(c.origin(), CatalogOrigin::Cache);
        assert_eq!(c.entries()[0].context, 222_000);

        // 缓存比快照旧（装了新版程序）→ 用快照。
        write_cache(dir.path(), &file(None, Some(snap_ts - DAY), 222_000));
        let c = Catalog::load_with_snapshot(dir.path(), &snap);
        assert_eq!(c.origin(), CatalogOrigin::Snapshot);
        assert_eq!(c.entries()[0].context, 111_000);
    }

    #[test]
    fn load_falls_back_on_missing_or_broken_cache() {
        let dir = tempfile::tempdir().expect("tmp");
        let snap = file(Some("2026-01-10"), None, 111_000);
        // 没有缓存文件。
        assert_eq!(
            Catalog::load_with_snapshot(dir.path(), &snap).origin(),
            CatalogOrigin::Snapshot
        );
        // 缓存是垃圾 / 空条目 / 版本不认识 / 缺拉取时间。
        for bad in [
            "not json at all".to_string(),
            "{\"version\":1,\"source\":\"x\",\"fetched_at\":99999999999,\"entries\":[]}"
                .to_string(),
            "{\"version\":99,\"source\":\"x\",\"fetched_at\":99999999999,\"entries\":[]}"
                .to_string(),
            file(Some("2030-01-01"), None, 5), // 有条目但没有 fetched_at
        ] {
            write_cache(dir.path(), &bad);
            let c = Catalog::load_with_snapshot(dir.path(), &snap);
            assert_eq!(c.origin(), CatalogOrigin::Snapshot, "坏缓存：{bad}");
            assert_eq!(c.entries()[0].context, 111_000);
        }
    }

    #[test]
    fn load_falls_back_to_empty_when_snapshot_is_broken() {
        let dir = tempfile::tempdir().expect("tmp");
        let c = Catalog::load_with_snapshot(dir.path(), "{ broken");
        assert_eq!(c.origin(), CatalogOrigin::Empty);
        assert!(c.is_empty());
        assert!(c.find("gpt-5.5").is_none());
        // 快照坏了但缓存好：缓存顶上。
        write_cache(dir.path(), &file(None, Some(1_800_000_000), 333_000));
        let c = Catalog::load_with_snapshot(dir.path(), "{ broken");
        assert_eq!(c.origin(), CatalogOrigin::Cache);
    }

    #[tokio::test]
    async fn refresh_is_noop_when_disabled_or_fresh() {
        let dir = tempfile::tempdir().expect("tmp");
        let stub = Stub::ok(fake_api(30, 1000));
        assert_eq!(
            refresh_if_stale(dir.path(), &stub, 1_800_000_000, false).await,
            RefreshOutcome::Disabled
        );
        assert_eq!(stub.calls(), 0);
        assert!(!dir.path().join(CACHE_FILE_NAME).exists());

        // 6 天前拉的 → 新鲜，不联网。
        let now = 1_800_000_000;
        write_cache(dir.path(), &file(None, Some(now - 6 * DAY), 5));
        assert_eq!(
            refresh_if_stale(dir.path(), &stub, now, true).await,
            RefreshOutcome::Fresh
        );
        assert_eq!(stub.calls(), 0);
    }

    #[tokio::test]
    async fn refresh_fetches_when_missing_or_stale_and_writes_trimmed_cache() {
        let dir = tempfile::tempdir().expect("tmp");
        let now = 1_800_000_000;
        let stub = Stub::ok(fake_api(30, 64_000));

        // 没有缓存。
        let r = refresh_if_stale(dir.path(), &stub, now, true).await;
        assert_eq!(r, RefreshOutcome::Updated { entries: 30 });
        assert_eq!(stub.calls(), 1);
        let cached = read_cache(dir.path()).expect("cache readable");
        assert_eq!(cached.fetched_at, Some(now));
        assert_eq!(cached.entries.len(), 30);
        assert!(cached.entries.iter().all(|e| e.context == 64_000));
        // 没有残留临时文件。
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("ls")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(leftovers, vec![CACHE_FILE_NAME.to_string()]);

        // 恰好 7 天 → 过期，再拉一次。
        let r = refresh_if_stale(dir.path(), &stub, now + STALE_AFTER_SECS, true).await;
        assert_eq!(r, RefreshOutcome::Updated { entries: 30 });
        assert_eq!(stub.calls(), 2);

        // 时钟倒退（缓存时间在未来）也算过期。
        let r = refresh_if_stale(dir.path(), &stub, now - 10 * DAY, true).await;
        assert!(matches!(r, RefreshOutcome::Updated { .. }));
        assert_eq!(stub.calls(), 3);
    }

    #[tokio::test]
    async fn refresh_replaces_corrupt_cache() {
        let dir = tempfile::tempdir().expect("tmp");
        write_cache(dir.path(), "garbage");
        let stub = Stub::ok(fake_api(25, 1000));
        let r = refresh_if_stale(dir.path(), &stub, 1_800_000_000, true).await;
        assert_eq!(r, RefreshOutcome::Updated { entries: 25 });
        assert!(read_cache(dir.path()).is_ok());
    }

    #[tokio::test]
    async fn refresh_failures_leave_existing_cache_untouched() {
        let dir = tempfile::tempdir().expect("tmp");
        let now = 1_800_000_000;
        let old = file(None, Some(now - 30 * DAY), 777);
        write_cache(dir.path(), &old);

        for stub in [
            Stub::err("连接超时"),
            Stub::ok(b"<html>502 Bad Gateway</html>".to_vec()),
            Stub::ok(fake_api(3, 1000)),        // 条数太少
            Stub::ok(b"{\"openai\":".to_vec()), // 被截断
        ] {
            let r = refresh_if_stale(dir.path(), &stub, now, true).await;
            assert!(matches!(r, RefreshOutcome::Failed(_)), "{r:?}");
            assert_eq!(stub.calls(), 1);
            let text = std::fs::read_to_string(dir.path().join(CACHE_FILE_NAME)).expect("read");
            assert_eq!(text, old);
        }
    }

    #[tokio::test]
    async fn refresh_creates_missing_cache_dir_and_reports_unwritable_dir() {
        let dir = tempfile::tempdir().expect("tmp");
        let nested = dir.path().join("a").join("b");
        let stub = Stub::ok(fake_api(21, 1000));
        let r = refresh_if_stale(&nested, &stub, 1_800_000_000, true).await;
        assert!(matches!(r, RefreshOutcome::Updated { .. }), "{r:?}");

        // 目录位置被一个普通文件占了 → 写失败要报 Failed，而不是 panic。
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "x").expect("write");
        let r = refresh_if_stale(&blocker.join("sub"), &stub, 1_800_000_000, true).await;
        assert!(matches!(r, RefreshOutcome::Failed(_)), "{r:?}");
    }

    /// 本机回环上的最小 HTTP 服务器，验证 [`ReqwestFetch`] 真的能走通（不联网）。
    #[tokio::test]
    async fn reqwest_fetch_reads_body_and_reports_status() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            for status in ["200 OK", "404 Not Found"] {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                let body = "hello";
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client");
        let fetch = ReqwestFetch::new(client);
        let ok = fetch.get(&format!("http://{addr}/")).await;
        assert_eq!(ok, Ok(b"hello".to_vec()));
        let bad = fetch.get(&format!("http://{addr}/")).await;
        assert!(bad.is_err());
    }
}
