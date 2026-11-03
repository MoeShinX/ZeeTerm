//! 调试版的"假智能体世界"：窗口级自动化（截图、场景）用它代替真实的 opencode / codex / Claude Code，
//! 这样验收三种驱动的切换、缺失 / 版本过低 / 未登录提示、模型与强度、一键下载的确认框时，
//! **不拉任何真实进程、不读用户真实的智能体配置**。
//!
//! 只在 `ZSSH_AGENT_FAKE=<json 文件>` 且是 debug 构建时生效（走 `dev_hooks`，release 恒为 `None`，
//! 见 `dev_hooks.rs` 的统一规则）。JSON 例子：
//!
//! ```json
//! {
//!   "opencode": {
//!     "install": {"path": "C:\\Tools\\opencode\\opencode.exe", "version": "opencode 1.19.3",
//!                 "source": "path"},
//!     "catalog": {"models": [{"id": "demo/fast", "name": "Fast"}], "default": null}
//!   },
//!   "codex": {"install": null},
//!   "claude": {"install": {"path": "...", "version": "2.1.300"},
//!              "login": {"logged_in": false, "hint": "请先在终端里运行 `claude` 完成登录"}},
//!   "download": {"version": "v1.20.0", "size": 52428800}
//! }
//! ```
//!
//! 假对话只回一句带智能体名和模型的文字（`tool …` 开头的消息再带一张只读工具卡片），足够验证"同一套
//! 原生状态机在三种驱动上都走通"。真实驱动的对话由 `ZSSH_AGENT_E2E=1` 的测试单独验证。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;
use tokio::sync::mpsc;
use zssh_core::ai::agent::discovery::{AgentInstall, DiscoverySource, SkippedOlder};
use zssh_core::ai::agent::download::ReleaseInfo;
use zssh_core::ai::agent::{
    AgentConversation, AgentError, AgentEvent, AgentModel, AgentModelCatalog,
};

use super::AgentKind;
pub use super::fake_tools::FakeBridge;
use super::fake_tools::{self, BridgeClient};
use super::status::{LoginStatus, Probe};

#[derive(Debug, Clone, Deserialize)]
struct FakeInstall {
    path: String,
    version: String,
    #[serde(default = "default_source")]
    source: String,
    #[serde(default)]
    too_old: bool,
    #[serde(default = "yes")]
    auto: bool,
    #[serde(default)]
    skipped_older: Option<FakeSkipped>,
}

#[derive(Debug, Clone, Deserialize)]
struct FakeSkipped {
    version: String,
    #[serde(default = "default_source")]
    source: String,
}

fn default_source() -> String {
    "path".to_owned()
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
struct FakeLogin {
    logged_in: Option<bool>,
    #[serde(default)]
    hint: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct FakeModel {
    id: String,
    name: String,
    #[serde(default)]
    unavailable: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct FakeCatalog {
    #[serde(default)]
    models: Vec<FakeModel>,
    #[serde(default)]
    default: Option<String>,
    #[serde(default)]
    default_label: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FakeAgent {
    /// `null` / 缺省 = 没装。
    #[serde(default)]
    install: Option<FakeInstall>,
    /// 手动指定的路径不能用。
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    login: Option<FakeLogin>,
    #[serde(default)]
    guard: Option<String>,
    #[serde(default)]
    catalog: Option<FakeCatalog>,
    /// 起对话时报的错（比如 codex 的本机工具没法确认关掉）。
    #[serde(default)]
    start_error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct FakeDownload {
    version: String,
    size: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Doc {
    #[serde(default)]
    opencode: FakeAgent,
    #[serde(default)]
    codex: FakeAgent,
    #[serde(default)]
    claude: FakeAgent,
    #[serde(default)]
    download: Option<FakeDownload>,
}

#[derive(Debug)]
pub struct FakeWorld {
    doc: Doc,
    /// 假下载完成后 opencode 变成这个安装（"下载并重新检测"之后设置页看到的）。
    downloaded: Mutex<Option<FakeInstall>>,
    next_session: AtomicU64,
}

static WORLD: OnceLock<Option<FakeWorld>> = OnceLock::new();

/// 当前生效的假世界；没设 `ZSSH_AGENT_FAKE`（或不是 debug 构建）是 `None`。
pub fn world() -> Option<&'static FakeWorld> {
    WORLD
        .get_or_init(|| {
            let path = crate::dev_hooks::var("ZSSH_AGENT_FAKE")?;
            match load(&PathBuf::from(path)) {
                Ok(world) => Some(world),
                Err(err) => {
                    tracing::warn!(%err, "ZSSH_AGENT_FAKE 读不了，按没有假世界处理");
                    None
                }
            }
        })
        .as_ref()
}

fn load(path: &std::path::Path) -> Result<FakeWorld, String> {
    let text = std::fs::read_to_string(path).map_err(|err| format!("读文件失败：{err}"))?;
    let doc: Doc = serde_json::from_str(&text).map_err(|err| format!("JSON 不对：{err}"))?;
    Ok(FakeWorld::new(doc))
}

fn source_of(raw: &str) -> DiscoverySource {
    match raw {
        "setting" => DiscoverySource::Setting,
        "tools_dir" => DiscoverySource::ToolsDir,
        "known_location" => DiscoverySource::KnownLocation,
        _ => DiscoverySource::Path,
    }
}

fn install_of(fake: &FakeInstall) -> AgentInstall {
    AgentInstall {
        path: fake.path.clone(),
        version: fake.version.clone(),
        source: source_of(&fake.source),
        needs_shell: false,
        auto: fake.auto,
        too_old: fake.too_old,
        skipped_older: fake.skipped_older.as_ref().map(|older| SkippedOlder {
            version: older.version.clone(),
            source: source_of(&older.source),
        }),
    }
}

impl FakeWorld {
    fn new(doc: Doc) -> Self {
        Self {
            doc,
            downloaded: Mutex::new(None),
            next_session: AtomicU64::new(1),
        }
    }

    fn agent(&self, kind: AgentKind) -> &FakeAgent {
        match kind {
            AgentKind::OpenCode => &self.doc.opencode,
            AgentKind::Codex => &self.doc.codex,
            AgentKind::Claude => &self.doc.claude,
            // 假世界只管三个外部智能体；内置助手走自己的假接口（`builtin_fake`），这里当"什么都没装"。
            AgentKind::Builtin => {
                static NONE: std::sync::OnceLock<FakeAgent> = std::sync::OnceLock::new();
                NONE.get_or_init(FakeAgent::default)
            }
        }
    }

    pub fn probe(&'static self) -> FakeProbe {
        FakeProbe { world: self }
    }

    /// 假世界里这个智能体"装"的安装（opencode 下载过就是下载的那份）。
    fn install(&self, kind: AgentKind) -> Option<AgentInstall> {
        if kind == AgentKind::OpenCode
            && let Some(downloaded) = self
                .downloaded
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .as_ref()
        {
            return Some(install_of(downloaded));
        }
        self.agent(kind).install.as_ref().map(install_of)
    }

    pub fn catalog(&self, kind: AgentKind) -> Result<AgentModelCatalog, String> {
        let Some(fake) = &self.agent(kind).catalog else {
            return Ok(AgentModelCatalog::default());
        };
        Ok(AgentModelCatalog {
            models: fake
                .models
                .iter()
                .map(|model| AgentModel {
                    id: model.id.clone(),
                    name: model.name.clone(),
                    unavailable: model.unavailable,
                })
                .collect(),
            default: fake.default.clone(),
            default_label: fake.default_label.clone(),
        })
    }

    /// 起对话；`start_error` 设了就报错（面板上显示"启动失败"和重试）。
    pub fn start(
        &self,
        kind: AgentKind,
        events: mpsc::UnboundedSender<AgentEvent>,
        resume: Option<&str>,
        bridge: Option<FakeBridge>,
    ) -> Result<(Arc<dyn AgentConversation>, bool), String> {
        if let Some(reason) = &self.agent(kind).start_error {
            return Err(reason.clone());
        }
        let id = self.next_session.fetch_add(1, Ordering::SeqCst);
        let conversation = FakeConversation {
            kind,
            session_id: format!("fake-{}-{id}", kind.id()),
            catalog: self.catalog(kind).unwrap_or_default(),
            events,
            stopped: Arc::new(AtomicBool::new(false)),
            busy: Arc::new(AtomicBool::new(false)),
            bridge,
        };
        // 续接：假世界里"之前的会话"只认以 fake- 开头的标识。
        let resumed = resume.is_some_and(|id| id.starts_with("fake-"));
        Ok((Arc::new(conversation), resumed))
    }

    /// 一键下载的确认框信息；没配 `download` 就像真的一样报错。
    pub fn release_info(&self) -> Result<ReleaseInfo, String> {
        let Some(fake) = &self.doc.download else {
            return Err("访问 GitHub 失败：假世界里没有配置下载".to_owned());
        };
        Ok(ReleaseInfo {
            version: fake.version.clone(),
            asset_name: "opencode-windows-x64.zip".to_owned(),
            size: fake.size,
            sha256: "0".repeat(64),
            download_url: "https://github.com/example/opencode/releases/download/fake/opencode-windows-x64.zip"
                .to_owned(),
            page_url: "https://github.com/example/opencode/releases/tag/fake".to_owned(),
        })
    }

    /// 假装下载：分几步走完进度，期间能取消；成功后 opencode 变成 `download.version` 那一份。
    pub async fn download(
        &self,
        on_progress: impl Fn(u64, u64),
        cancel: &AtomicBool,
    ) -> Result<AgentInstall, String> {
        let info = self.release_info()?;
        let steps = 10u64;
        for step in 0..=steps {
            if cancel.load(Ordering::SeqCst) {
                return Err("用户取消".to_owned());
            }
            on_progress(info.size / steps * step, info.size);
            tokio::time::sleep(Duration::from_millis(120)).await;
        }
        let installed = FakeInstall {
            path: "C:\\Tools\\opencode\\opencode.exe".to_owned(),
            // 真实 `opencode --version` 只打印版本号本身。
            version: info.version.trim_start_matches('v').to_owned(),
            source: "tools_dir".to_owned(),
            too_old: false,
            auto: true,
            skipped_older: None,
        };
        let install = install_of(&installed);
        *self.downloaded.lock().unwrap_or_else(|p| p.into_inner()) = Some(installed);
        Ok(install)
    }
}

pub struct FakeProbe {
    world: &'static FakeWorld,
}

impl Probe for FakeProbe {
    fn discover(
        &self,
        kind: AgentKind,
        _data_dir: PathBuf,
    ) -> BoxFuture<'static, Result<Option<AgentInstall>, String>> {
        let world = self.world;
        Box::pin(async move {
            if let Some(reason) = &world.agent(kind).error {
                return Err(reason.clone());
            }
            Ok(world.install(kind))
        })
    }

    fn login(
        &self,
        kind: AgentKind,
        _install: AgentInstall,
    ) -> BoxFuture<'static, Option<LoginStatus>> {
        let login = self.world.agent(kind).login.clone();
        Box::pin(async move {
            login.map(|login| LoginStatus {
                logged_in: login.logged_in,
                hint: login.hint,
            })
        })
    }

    fn guard(&self, _install: AgentInstall) -> BoxFuture<'static, Option<String>> {
        let guard = self.world.agent(AgentKind::Codex).guard.clone();
        Box::pin(async move { guard })
    }
}

/// 假对话：收到消息先等一小会儿，回一句带智能体名和模型的文字，然后结束回合。
pub struct FakeConversation {
    kind: AgentKind,
    session_id: String,
    catalog: AgentModelCatalog,
    events: mpsc::UnboundedSender<AgentEvent>,
    stopped: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    /// 连工具桥用的信息；有了它，`call [...]` 消息才会真的去调工具（V2-G10b）。
    bridge: Option<FakeBridge>,
}

/// 按顺序发 `call [...]` 里的工具调用：每个都真的走工具桥（审批、执行位置、终端、文件工具、记忆）。
async fn run_scripted_calls(
    bridge: FakeBridge,
    calls: Vec<fake_tools::FakeCall>,
    events: mpsc::UnboundedSender<AgentEvent>,
    stopped: Arc<AtomicBool>,
) {
    let mut client = match BridgeClient::connect(&bridge).await {
        Ok(client) => client,
        Err(message) => {
            let _ = events.send(AgentEvent::Error { message });
            return;
        }
    };
    for (index, call) in calls.into_iter().enumerate() {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let call_id = format!("fake-call-{}", index + 1);
        let tool = format!("{}{}", fake_tools::TOOL_PREFIX, call.tool);
        let _ = events.send(AgentEvent::ToolStart {
            call_id: call_id.clone(),
            tool: tool.clone(),
            input: call.input.clone(),
        });
        tokio::time::sleep(fake_tools::CALL_GAP).await;
        let (output, error) = match client.call(&call.tool, call.input).await {
            Ok(outcome) => (outcome.text, !outcome.ok),
            Err(message) => (message, true),
        };
        let _ = events.send(AgentEvent::ToolEnd {
            call_id,
            tool,
            output,
            error,
        });
    }
}

impl AgentConversation for FakeConversation {
    fn session_id(&self) -> &str {
        &self.session_id
    }

    fn models<'a>(&'a self) -> BoxFuture<'a, Result<AgentModelCatalog, AgentError>> {
        let catalog = self.catalog.clone();
        Box::pin(async move { Ok(catalog) })
    }

    fn send<'a>(
        &'a self,
        text: &'a str,
        model: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), AgentError>> {
        let text = text.to_owned();
        let model = model.unwrap_or("默认").to_owned();
        Box::pin(async move {
            if self.busy.swap(true, Ordering::SeqCst) {
                return Err(AgentError::Busy);
            }
            self.stopped.store(false, Ordering::SeqCst);
            let events = self.events.clone();
            let stopped = Arc::clone(&self.stopped);
            let busy = Arc::clone(&self.busy);
            let label = self.kind.label();
            let bridge = self.bridge.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                if !stopped.load(Ordering::SeqCst) {
                    match (fake_tools::parse_calls(&text), bridge) {
                        (Some(Ok(calls)), Some(bridge)) => {
                            run_scripted_calls(bridge, calls, events.clone(), Arc::clone(&stopped))
                                .await;
                        }
                        (Some(Err(message)), _) => {
                            let _ = events.send(AgentEvent::Error { message });
                        }
                        _ => {}
                    }
                }
                // `usage <已用>/<窗口>[/est] <正文>`：回复前先报一次上下文用量（`est` = 窗口是估计的），
                // 明细按比例造；用来截标题栏「上下文」指示器的各种状态。
                if let Some(usage) = fake_usage(&text, &model) {
                    let _ = events.send(AgentEvent::Usage { usage });
                }
                // `think <秒>`：先沉默这么久（窗口里能看到「思考中」）。`retry <秒>`：先报一次重试，再沉默这么久。
                let quiet = |prefix: &str| {
                    text.strip_prefix(prefix)
                        .and_then(|rest| rest.split_whitespace().next())
                        .and_then(|secs| secs.parse::<u64>().ok())
                };
                if let Some(secs) = quiet("retry ") {
                    let _ = events.send(AgentEvent::Retrying {
                        attempt: 2,
                        reason: "服务繁忙".to_owned(),
                        next_in_secs: Some(30),
                    });
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                } else if let Some(secs) = quiet("think ") {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                }
                if !stopped.load(Ordering::SeqCst) {
                    if text.starts_with("tool ") {
                        let call_id = "fake-call-1".to_owned();
                        let _ = events.send(AgentEvent::ToolStart {
                            call_id: call_id.clone(),
                            tool: "list_dir".to_owned(),
                            input: serde_json::json!({"path": "/srv/app"}),
                        });
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        let _ = events.send(AgentEvent::ToolEnd {
                            call_id,
                            tool: "list_dir".to_owned(),
                            output: "app.conf\nlogs".to_owned(),
                            error: false,
                        });
                    }
                    // 脚本化的工具调用消息很长，回复写短一点；普通消息照旧回显。
                    let reply = if text.starts_with("call ") {
                        format!("[{label} 假驱动] 已按脚本发起工具调用（模型：{model}）")
                    } else {
                        format!("[{label} 假驱动] 收到：{text}（模型：{model}）")
                    };
                    let _ = events.send(AgentEvent::TextDelta { text: reply });
                }
                busy.store(false, Ordering::SeqCst);
                let _ = events.send(AgentEvent::TurnEnd);
            });
            Ok(())
        })
    }

    fn stop<'a>(&'a self) -> BoxFuture<'a, Result<(), AgentError>> {
        self.stopped.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        self.stopped.store(true, Ordering::SeqCst);
        Box::pin(async {})
    }
}

/// 假对话的 `usage <已用>/<窗口>[/est] ...` 消息 → 一条用量事件；别的消息返回 `None`。
fn fake_usage(text: &str, model: &str) -> Option<zssh_core::ai::agent::usage::ContextUsage> {
    let spec = text.strip_prefix("usage ")?.split_whitespace().next()?;
    let mut parts = spec.split('/');
    let used: u64 = parts.next()?.parse().ok()?;
    let window: u64 = parts.next()?.parse().ok()?;
    let estimated = parts.next() == Some("est");
    let input = used / 50;
    let cache_read = used * 9 / 10;
    Some(zssh_core::ai::agent::usage::ContextUsage {
        used,
        window: Some(window),
        window_estimated: estimated,
        input: Some(input),
        output: Some(800),
        cache_read: Some(cache_read),
        cache_write: Some(used.saturating_sub(input + cache_read)),
        model: Some(model.to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_usage_messages_become_usage_events() {
        let usage = fake_usage("usage 12000/200000/est 你好", "opus/high").expect("有用量");
        assert_eq!(
            (usage.used, usage.window, usage.window_estimated),
            (12_000, Some(200_000), true)
        );
        assert_eq!(usage.cache_read, Some(10_800));
        assert_eq!(usage.model.as_deref(), Some("opus/high"));
        assert!(fake_usage("usage 5/10 hi", "m").is_some_and(|u| !u.window_estimated));
        assert_eq!(fake_usage("hello", "m"), None);
        assert_eq!(fake_usage("usage abc/10", "m"), None);
    }

    fn doc() -> Doc {
        serde_json::from_str(
            r#"{
              "opencode": {"install": {"path": "C:\\工具 箱\\opencode.exe", "version": "opencode 1.19.3"},
                           "catalog": {"models": [{"id": "demo/fast", "name": "Fast"}]}},
              "codex": {"install": null},
              "claude": {"install": {"path": "claude.exe", "version": "2.1.300"},
                         "login": {"logged_in": false, "hint": "x"}},
              "download": {"version": "v1.20.0", "size": 1000}
            }"#,
        )
        .expect("样例 JSON")
    }

    #[test]
    fn the_sample_document_describes_all_three_agents() {
        let world = FakeWorld::new(doc());
        let opencode = world.install(AgentKind::OpenCode).expect("装了");
        assert_eq!(opencode.path, "C:\\工具 箱\\opencode.exe");
        assert_eq!(opencode.source, DiscoverySource::Path);
        assert!(world.install(AgentKind::Codex).is_none());
        assert_eq!(
            world
                .catalog(AgentKind::OpenCode)
                .expect("目录")
                .models
                .len(),
            1
        );
        assert!(
            world
                .catalog(AgentKind::Claude)
                .expect("没配目录也不报错")
                .models
                .is_empty()
        );
    }

    #[test]
    fn a_missing_file_or_bad_json_is_reported_not_panicked() {
        assert!(load(std::path::Path::new("no-such-fake-world.json")).is_err());
        let dir = std::env::temp_dir().join(format!("zssh-g10a-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("目录");
        let file = dir.join("bad.json");
        std::fs::write(&file, "{ not json").expect("写");
        assert!(load(&file).expect_err("坏 JSON").contains("JSON 不对"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_fake_download_installs_into_the_fake_tools_dir_and_can_be_cancelled() {
        let world = FakeWorld::new(doc());
        let cancel = AtomicBool::new(true);
        assert_eq!(
            world.download(|_, _| {}, &cancel).await.expect_err("取消"),
            "用户取消"
        );
        assert_eq!(
            world.install(AgentKind::OpenCode).expect("装了").version,
            "opencode 1.19.3",
            "取消不改变已装的"
        );
        let cancel = AtomicBool::new(false);
        let seen = Mutex::new(Vec::new());
        let installed = world
            .download(
                |received, total| seen.lock().expect("锁").push((received, total)),
                &cancel,
            )
            .await
            .expect("下载");
        assert_eq!(installed.version, "1.20.0");
        assert_eq!(installed.source, DiscoverySource::ToolsDir);
        assert_eq!(
            world.install(AgentKind::OpenCode).expect("装了").version,
            "1.20.0"
        );
        assert_eq!(seen.lock().expect("锁").last().copied(), Some((1000, 1000)));
    }

    #[tokio::test]
    async fn a_fake_conversation_echoes_the_model_and_resumes_only_fake_sessions() {
        let world = FakeWorld::new(doc());
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (conversation, resumed) = world
            .start(AgentKind::Codex, tx, Some("fake-codex-9"), None)
            .expect("起对话");
        assert!(resumed);
        conversation.send("hi", Some("gpt/high")).await.expect("发");
        let mut text = String::new();
        loop {
            match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
                Ok(Some(AgentEvent::TextDelta { text: delta })) => text.push_str(&delta),
                Ok(Some(AgentEvent::TurnEnd)) => break,
                Ok(Some(_)) => {}
                _ => panic!("回合没有结束"),
            }
        }
        assert_eq!(text, "[codex 假驱动] 收到：hi（模型：gpt/high）");
        let (tx, _rx) = mpsc::unbounded_channel();
        let (_c, resumed) = world
            .start(AgentKind::Claude, tx, Some("real-id"), None)
            .expect("起");
        assert!(!resumed, "不是假世界的会话标识接不上");
    }
}
