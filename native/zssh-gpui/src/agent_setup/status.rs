//! 三种智能体装没装、版本够不够、登录了没有（0.2.14 的 `ai_agent_discover` / `ai_agent_login` /
//! `ai_agent_guard` 加面板里的 `agentAvailabilityOf`）。
//!
//! 判定全部沿用 core：发现用 `discovery::discover`（设置里的路径 → tools 目录 → PATH → 常见位置，
//! 按 `--version` 探测、拆 npm `.cmd`）；Claude Code 的登录用 `claude auth status`，读不到再看
//! 环境变量 / 凭据文件 / `settings.json` 的 env 块；codex 依次问共用进程、`codex login status`、
//! `config.toml` 里的服务商配置。**只判断有没有，绝不读取或打印密钥的值**；读不出来 = 不确定
//! （`logged_in: None`），只提示、不拦着用。
//!
//! 结果放在进程级的 [`StatusStore`] 里：每个窗口的 AI 面板和设置页读同一份，不用每个面板各跑一遍
//! `--version`、`claude auth status`。设置页改了路径、下载完、点「重新检测」就刷新，面板下一个节拍
//! 就看到新结果。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use futures::future::BoxFuture;
use zssh_core::ai::agent::claude;
use zssh_core::ai::agent::codex;
use zssh_core::ai::agent::discovery::{self, AgentInstall};
use zssh_core::store::Database;

use super::AgentKind;
use crate::i18n::{Msg, t};

/// 登录状态。`logged_in` 为 `None` = 判断不了（读不出配置），或者这个智能体没有"登录"这个概念
/// （opencode 的登录在它自己的界面里做）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStatus {
    pub logged_in: Option<bool>,
    pub hint: Option<String>,
}

/// 发现的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallState {
    /// 还在检测。
    Checking,
    /// 哪儿都没找到。
    Missing,
    /// 设置里手动指定的路径不能用（用户明确指定的坏了要报出来，且不回退到自动发现）。
    Invalid(String),
    Found(AgentInstall),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub install: InstallState,
    /// 只有 codex / Claude Code 且版本够用时才查。
    pub login: Option<LoginStatus>,
    /// codex 的本机工具能不能确认关掉：非空 = 拒绝启动的完整原因。
    pub guard: Option<String>,
}

impl AgentStatus {
    pub fn checking() -> Self {
        Self {
            install: InstallState::Checking,
            login: None,
            guard: None,
        }
    }

    pub fn found(&self) -> Option<&AgentInstall> {
        match &self.install {
            InstallState::Found(install) => Some(install),
            _ => None,
        }
    }

    pub fn availability(&self) -> Availability {
        match &self.install {
            InstallState::Checking => Availability::Checking,
            InstallState::Missing | InstallState::Invalid(_) => Availability::Missing,
            InstallState::Found(install) => {
                if install.too_old {
                    Availability::TooOld
                } else if self.login.as_ref().and_then(|login| login.logged_in) == Some(false) {
                    Availability::NeedsLogin
                } else {
                    Availability::Ready
                }
            }
        }
    }

    /// 登录状态判断不了、又有话要说：面板只提示，不拦着用（R-2 第 3 节）。
    pub fn login_unknown_hint(&self) -> Option<&str> {
        let login = self.login.as_ref()?;
        if login.logged_in.is_none() {
            login.hint.as_deref().filter(|hint| !hint.is_empty())
        } else {
            None
        }
    }
}

/// 一个智能体现在能不能用（0.2.14 的 `AgentAvailability`，多一个"还在检测"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    Checking,
    Ready,
    Missing,
    /// 版本低于最低要求：面板顶部一条提示，仍然可以用；下拉里不让再选它。
    TooOld,
    NeedsLogin,
}

impl Availability {
    /// 输入框 / 发送 / 模型下拉是否禁用（0.2.14：没找到、还在检测、没登录；版本过低只提示）。
    pub fn blocks_input(self) -> bool {
        matches!(
            self,
            Availability::Checking | Availability::Missing | Availability::NeedsLogin
        )
    }

    /// 智能体下拉里这一项的备注；`None` = 可选。
    pub fn selector_note(self) -> Option<&'static str> {
        match self {
            Availability::Missing => Some(t(SetupStatusMsg::NotInstalled)),
            Availability::NeedsLogin => Some(t(SetupStatusMsg::NotLoggedIn)),
            Availability::TooOld => Some(t(SetupStatusMsg::TooOld)),
            Availability::Checking | Availability::Ready => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 提示文案（与 0.2.14 的 AIChatPanel / SettingsPage 逐字一致）
// ---------------------------------------------------------------------------

/// 本文件（智能体的安装 / 登录状态提示）自己的界面文案。
///
/// 只放应用自己的固定文案；版本号、安装路径、登录命令、核心库返回的原因和主机名一律原样
/// 显示，不进这张表。带参数的句子保留 `format!`，这里只提供固定片段。
#[derive(Clone, Copy, Debug)]
enum SetupStatusMsg {
    // ---- 可用性 ----
    NotInstalled,
    NotLoggedIn,
    TooOld,
    // ---- 没找到 ----
    MissingOpenCode,
    MissingCodex,
    MissingClaude,
    // ---- 没登录 ----
    LoginRequiredCodexHead,
    LoginRequiredClaudeHead,
    LoginCommandCodex,
    LoginCommandClaude,
    LoginCommandCodexDone,
    LoginCommandClaudeDone,
    // ---- 登录状态 ----
    LoggedIn,
    LoginUnknown,
    UnknownLoginHead,
    UnknownLoginPrefix,
    UnknownLoginSuffix,
    FollowupSeparator,
    // ---- 正在检测 ----
    CheckingPrefix,
    CheckingSuffix,
    // ---- 版本过低 ----
    TooOldOpenCode,
    TooOldCodexPrefix,
    TooOldCodexSuffix,
    TooOldClaudePrefix,
    TooOldClaudeSuffix,
    // ---- 来源 ----
    SourceManual,
    SourceDownloaded,
    SourceFound,
    SourceKnownLocation,
    SourceDataDir,
    SourcePath,
    // ---- 跳过的旧版本 ----
    SkippedOlderMiddle,
    SkippedOlderSuffix,
}

impl Msg for SetupStatusMsg {
    fn zh(&self) -> &'static str {
        match *self {
            Self::NotInstalled => "未安装",
            Self::NotLoggedIn => "未登录",
            Self::TooOld => "版本过低",
            Self::MissingOpenCode => "没有找到 opencode。去设置页指定路径，或者一键下载。",
            Self::MissingCodex => {
                "没有找到 codex。安装方法：npm i -g @openai/codex（也可以在设置页指定路径）。"
            }
            Self::MissingClaude => {
                "没有找到 Claude Code。安装方法：npm i -g @anthropic-ai/claude-code（也可以在设置页指定路径）。"
            }
            Self::LoginRequiredCodexHead => "codex 还没有登录。",
            Self::LoginRequiredClaudeHead => "Claude Code 还没有登录。",
            Self::LoginCommandCodex => "请先在终端里运行 `codex login`",
            Self::LoginCommandClaude => "请先在终端里运行 `claude` 完成登录",
            Self::LoginCommandCodexDone => "请先在终端里运行 `codex login` 完成登录",
            Self::LoginCommandClaudeDone => "请先在终端里运行 `claude` 完成登录",
            Self::LoggedIn => "已登录",
            Self::LoginUnknown => "不确定",
            Self::UnknownLoginHead => "登录状态不确定",
            Self::UnknownLoginPrefix => "没能确认登录状态（",
            Self::UnknownLoginSuffix => {
                "）。已经登录过就能直接用；不行就在终端里跑一次 `codex login`"
            }
            Self::FollowupSeparator => "：",
            Self::CheckingPrefix => "正在检测 ",
            Self::CheckingSuffix => "，请稍后再发送。",
            Self::TooOldOpenCode => "版本过低，建议升级（低于 1.19 在共享数据目录上有已知问题）。",
            Self::TooOldCodexPrefix => "版本过低，建议升级（codex 需要 ",
            Self::TooOldCodexSuffix => " 及以上，可 `codex update`）。",
            Self::TooOldClaudePrefix => "版本过低，建议升级（Claude Code 需要 ",
            Self::TooOldClaudeSuffix => " 及以上，可 `claude update`）。",
            Self::SourceManual => "手动指定",
            Self::SourceDownloaded => "已下载",
            Self::SourceFound => "已找到",
            Self::SourceKnownLocation => "常见安装位置",
            Self::SourceDataDir => "数据目录",
            Self::SourcePath => "PATH",
            Self::SkippedOlderMiddle => " 上还有一个旧版本 ",
            Self::SkippedOlderSuffix => "（低于最低要求，没有用它）",
        }
    }

    fn en(&self) -> &'static str {
        match *self {
            Self::NotInstalled => "Not installed",
            Self::NotLoggedIn => "Not signed in",
            Self::TooOld => "Version too old",
            Self::MissingOpenCode => {
                "opencode was not found. Specify its path in Settings, or use the one-click download."
            }
            Self::MissingCodex => {
                "codex was not found. Install it with npm i -g @openai/codex (you can also specify its path in Settings)."
            }
            Self::MissingClaude => {
                "Claude Code was not found. Install it with npm i -g @anthropic-ai/claude-code (you can also specify its path in Settings)."
            }
            Self::LoginRequiredCodexHead => "codex is not signed in yet.",
            Self::LoginRequiredClaudeHead => "Claude Code is not signed in yet.",
            Self::LoginCommandCodex => "run `codex login` in a terminal first",
            Self::LoginCommandClaude => "run `claude` in a terminal to sign in first",
            Self::LoginCommandCodexDone => "run `codex login` in a terminal to finish signing in",
            Self::LoginCommandClaudeDone => "run `claude` in a terminal to finish signing in",
            Self::LoggedIn => "Signed in",
            Self::LoginUnknown => "Unknown",
            Self::UnknownLoginHead => "Login status unknown",
            Self::UnknownLoginPrefix => "Could not confirm the login status (",
            Self::UnknownLoginSuffix => {
                "). If it is already signed in you can use it directly; otherwise run `codex login` once in a terminal"
            }
            Self::FollowupSeparator => ": ",
            Self::CheckingPrefix => "Still checking ",
            Self::CheckingSuffix => "; please try sending again in a moment.",
            Self::TooOldOpenCode => {
                "Version too old; consider upgrading (versions below 1.19 have known issues with the shared data directory)."
            }
            Self::TooOldCodexPrefix => "Version too old; consider upgrading (codex requires ",
            Self::TooOldCodexSuffix => " or later; try `codex update`).",
            Self::TooOldClaudePrefix => {
                "Version too old; consider upgrading (Claude Code requires "
            }
            Self::TooOldClaudeSuffix => " or later; try `claude update`).",
            Self::SourceManual => "Manual",
            Self::SourceDownloaded => "Downloaded",
            Self::SourceFound => "Found",
            Self::SourceKnownLocation => "Common install location",
            Self::SourceDataDir => "Data directory",
            Self::SourcePath => "PATH",
            Self::SkippedOlderMiddle => " also has an older version ",
            Self::SkippedOlderSuffix => " (below the minimum requirement, not used)",
        }
    }
}

/// 没找到时面板里的整段说明。
pub fn missing_text(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::OpenCode => t(SetupStatusMsg::MissingOpenCode),
        AgentKind::Codex => t(SetupStatusMsg::MissingCodex),
        AgentKind::Claude => t(SetupStatusMsg::MissingClaude),
        AgentKind::Builtin => super::builtin::missing_text(),
    }
}

/// 没登录时面板里的两行（说明 + 命令）。opencode 没有登录概念，不会走到这里。
pub fn login_required_lines(kind: AgentKind) -> (&'static str, &'static str) {
    match kind {
        AgentKind::Codex => (
            t(SetupStatusMsg::LoginRequiredCodexHead),
            t(SetupStatusMsg::LoginCommandCodex),
        ),
        _ => (
            t(SetupStatusMsg::LoginRequiredClaudeHead),
            t(SetupStatusMsg::LoginCommandClaude),
        ),
    }
}

/// 发送前的拦截原因（能发返回 `None`）：还在检测、没找到、没登录。版本过低只提示、不拦着。
pub fn blocked_reason(kind: AgentKind, status: &AgentStatus) -> Option<String> {
    match status.availability() {
        Availability::Ready | Availability::TooOld => None,
        Availability::Checking => Some(format!(
            "{}{}{}",
            t(SetupStatusMsg::CheckingPrefix),
            kind.label(),
            t(SetupStatusMsg::CheckingSuffix)
        )),
        Availability::Missing => Some(missing_text(kind).to_owned()),
        Availability::NeedsLogin => {
            let (head, command) = login_required_lines(kind);
            Some(format!("{head}{command}"))
        }
    }
}

/// 设置页里登录状态的说法。
pub fn login_text(logged_in: Option<bool>) -> &'static str {
    match logged_in {
        Some(true) => t(SetupStatusMsg::LoggedIn),
        Some(false) => t(SetupStatusMsg::NotLoggedIn),
        None => t(SetupStatusMsg::LoginUnknown),
    }
}

/// 设置页里没登录 / 不确定时的补充句（没有 hint 时给默认的命令提示）。
pub fn login_followup(kind: AgentKind, login: &LoginStatus) -> Option<String> {
    if login.logged_in == Some(true) {
        return None;
    }
    let head = if login.logged_in == Some(false) {
        t(SetupStatusMsg::NotLoggedIn)
    } else {
        t(SetupStatusMsg::UnknownLoginHead)
    };
    let tail = match login.hint.as_deref().filter(|hint| !hint.is_empty()) {
        Some(hint) => hint.to_owned(),
        None if kind == AgentKind::Codex => t(SetupStatusMsg::LoginCommandCodex).to_owned(),
        None => t(SetupStatusMsg::LoginCommandClaude).to_owned(),
    };
    Some(format!(
        "{head}{}{tail}",
        t(SetupStatusMsg::FollowupSeparator)
    ))
}

/// 版本过低时设置页里的补充（最低版本取自发现规则，不另写一份）。
pub fn too_old_note(kind: AgentKind) -> String {
    let minimum = kind
        .spec()
        .and_then(|spec| spec.minimum_version)
        .map(|(a, b, c)| format!("{a}.{b}.{c}"))
        .unwrap_or_default();
    match kind {
        AgentKind::OpenCode => t(SetupStatusMsg::TooOldOpenCode).to_owned(),
        AgentKind::Codex => format!(
            "{}{minimum}{}",
            t(SetupStatusMsg::TooOldCodexPrefix),
            t(SetupStatusMsg::TooOldCodexSuffix)
        ),
        AgentKind::Claude => format!(
            "{}{minimum}{}",
            t(SetupStatusMsg::TooOldClaudePrefix),
            t(SetupStatusMsg::TooOldClaudeSuffix)
        ),
        // 内置助手没有版本这回事。
        AgentKind::Builtin => String::new(),
    }
}

/// 来源在设置页里的说法：`tools_dir` 对 opencode 才是"已下载"（一键下载装在那），
/// 别的智能体落在那儿只可能是用户自己放的。
pub fn source_label(kind: AgentKind, source: discovery::DiscoverySource) -> &'static str {
    use discovery::DiscoverySource::*;
    match source {
        Setting => t(SetupStatusMsg::SourceManual),
        ToolsDir => {
            if kind == AgentKind::OpenCode {
                t(SetupStatusMsg::SourceDownloaded)
            } else {
                t(SetupStatusMsg::SourceFound)
            }
        }
        KnownLocation => t(SetupStatusMsg::SourceKnownLocation),
        Path => t(SetupStatusMsg::SourcePath),
    }
}

/// 来源的简短说法（旧版本提示里用）。
pub fn plain_source_label(source: discovery::DiscoverySource) -> &'static str {
    use discovery::DiscoverySource::*;
    match source {
        Setting => t(SetupStatusMsg::SourceManual),
        ToolsDir => t(SetupStatusMsg::SourceDataDir),
        KnownLocation => t(SetupStatusMsg::SourceKnownLocation),
        Path => t(SetupStatusMsg::SourcePath),
    }
}

/// "还有一个旧版本被跳过了"的提示；没有可跳过的不提示。
pub fn skipped_older_note(install: &AgentInstall) -> Option<String> {
    let older = install.skipped_older.as_ref()?;
    Some(format!(
        "{}{}{version}{}",
        plain_source_label(older.source),
        t(SetupStatusMsg::SkippedOlderMiddle),
        t(SetupStatusMsg::SkippedOlderSuffix),
        version = older.version.trim()
    ))
}

// ---------------------------------------------------------------------------
// 探测
// ---------------------------------------------------------------------------

/// 一次检测要问的三件事。真实实现调 core；调试版的假世界（`fake.rs`）和单测各有自己的实现。
pub trait Probe: Send + Sync {
    fn discover(
        &self,
        kind: AgentKind,
        data_dir: PathBuf,
    ) -> BoxFuture<'static, Result<Option<AgentInstall>, String>>;

    fn login(
        &self,
        kind: AgentKind,
        install: AgentInstall,
    ) -> BoxFuture<'static, Option<LoginStatus>>;

    fn guard(&self, install: AgentInstall) -> BoxFuture<'static, Option<String>>;
}

/// 真实探测。
pub struct RealProbe;

impl Probe for RealProbe {
    fn discover(
        &self,
        kind: AgentKind,
        data_dir: PathBuf,
    ) -> BoxFuture<'static, Result<Option<AgentInstall>, String>> {
        Box::pin(async move {
            // 手动路径读不出来（库打不开 / 值坏了）按自动发现，不让一条设置读失败拦住整个检测。
            let Some(spec) = kind.spec() else {
                return Ok(None);
            };
            let setting = read_saved_path(kind, &data_dir);
            discovery::discover(spec, setting.as_deref(), &data_dir).await
        })
    }

    fn login(
        &self,
        kind: AgentKind,
        install: AgentInstall,
    ) -> BoxFuture<'static, Option<LoginStatus>> {
        Box::pin(async move { real_login(kind, &install).await })
    }

    fn guard(&self, install: AgentInstall) -> BoxFuture<'static, Option<String>> {
        Box::pin(async move {
            codex::startup::guard_check(std::path::Path::new(&install.path))
                .await
                .err()
        })
    }
}

/// 读设置库里这个智能体的手动路径。
pub fn read_saved_path(kind: AgentKind, data_dir: &std::path::Path) -> Option<String> {
    let db = Database::open(&data_dir.join("zssh.db")).ok()?;
    kind.saved_path(&db).ok().flatten()
}

/// 登录判断（Tauri 版 `ai_agent_login` 的原生实现）。
async fn real_login(kind: AgentKind, install: &AgentInstall) -> Option<LoginStatus> {
    match kind {
        // opencode 的登录在它自己的界面里做，没有登录状态可查。
        AgentKind::OpenCode => None,
        AgentKind::Claude => Some(claude_login(install).await),
        AgentKind::Codex => codex_login(install).await,
        // 内置助手靠 API Key，没有"登录"。
        AgentKind::Builtin => None,
    }
}

/// Claude Code：`claude auth status` → 兜底（环境变量 / 凭据文件 / settings.json 的 env 块）。
async fn claude_login(install: &AgentInstall) -> LoginStatus {
    if let Some(login) = claude::login::login_status_cli(&install.path).await {
        return LoginStatus {
            logged_in: login.logged_in,
            hint: login.hint,
        };
    }
    // 兜底读不出来时 `logged_in: None`（不确定）：提示用户，但不拦着他用（R-2 第 3 节）。
    if let Some(login) = claude::login::login_status_fallback() {
        return LoginStatus {
            logged_in: login.logged_in,
            hint: login.hint,
        };
    }
    LoginStatus {
        logged_in: Some(false),
        hint: Some(t(SetupStatusMsg::LoginCommandClaudeDone).to_owned()),
    }
}

/// codex：共用进程活着就问 `account/read`（最权威）→ `codex login status` →
/// 用户 `config.toml` 的服务商配置兜底。
async fn codex_login(install: &AgentInstall) -> Option<LoginStatus> {
    // ① 共用进程还活着：直接问它。问不出来（报错）就当没有这条信息，不当成没登录。
    if let Some(answer) = crate::agent_host::live_codex_logged_in().await {
        return match answer {
            Ok(true) => Some(LoginStatus {
                logged_in: Some(true),
                hint: None,
            }),
            Ok(false) => Some(LoginStatus {
                logged_in: Some(false),
                hint: Some(t(SetupStatusMsg::LoginCommandCodexDone).to_owned()),
            }),
            Err(_) => None,
        };
    }
    // ② `codex login status`。
    if let Some(login) = codex::startup::login_status_cli(std::path::Path::new(&install.path)).await
        && login.logged_in
    {
        return Some(LoginStatus {
            logged_in: Some(true),
            hint: None,
        });
    }
    // ③ 用户配置里的自建服务商兜底。配置文件存在但读不出来时报"不确定"：读不到不等于没登录。
    match codex::startup::read_config_toml(None) {
        codex::startup::ConfigRead::Read(text) => {
            if let Some(by_config) = codex::startup::login_status_from_config_text(&text, &|key| {
                std::env::var_os(key).is_some()
            }) {
                return Some(LoginStatus {
                    logged_in: Some(by_config.logged_in),
                    hint: by_config.hint,
                });
            }
        }
        codex::startup::ConfigRead::Unreadable(reason) => {
            return Some(LoginStatus {
                logged_in: None,
                hint: Some(format!(
                    "{}{reason}{}",
                    t(SetupStatusMsg::UnknownLoginPrefix),
                    t(SetupStatusMsg::UnknownLoginSuffix)
                )),
            });
        }
        codex::startup::ConfigRead::Missing => {}
    }
    Some(LoginStatus {
        logged_in: Some(false),
        hint: Some(t(SetupStatusMsg::LoginCommandCodexDone).to_owned()),
    })
}

/// 把一次检测的几个答案拼成状态。版本过低的不查登录和本机工具核对（0.2.14 同）。
pub async fn check_agent(probe: &dyn Probe, kind: AgentKind, data_dir: PathBuf) -> AgentStatus {
    match probe.discover(kind, data_dir).await {
        Err(reason) => AgentStatus {
            install: InstallState::Invalid(reason),
            login: None,
            guard: None,
        },
        Ok(None) => AgentStatus {
            install: InstallState::Missing,
            login: None,
            guard: None,
        },
        Ok(Some(install)) => {
            let usable = !install.too_old;
            let login = if usable && kind != AgentKind::OpenCode {
                probe.login(kind, install.clone()).await
            } else {
                None
            };
            let guard = if usable && kind == AgentKind::Codex {
                probe.guard(install.clone()).await
            } else {
                None
            };
            AgentStatus {
                install: InstallState::Found(install),
                login,
                guard,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 进程级状态表
// ---------------------------------------------------------------------------

/// 三种智能体的最新状态加一个版本号（任何一项变了就加一，界面据此决定要不要重画）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub version: u64,
    pub agents: [AgentStatus; 3],
    /// 内置助手：有没有配好 Key 的接口（由 `builtin::status` 填，不在 [`StatusStore`] 里）。
    pub builtin: AgentStatus,
}

impl Snapshot {
    pub fn get(&self, kind: AgentKind) -> &AgentStatus {
        match kind {
            AgentKind::Builtin => &self.builtin,
            external => &self.agents[external.index()],
        }
    }
}

struct Inner {
    version: u64,
    agents: [AgentStatus; 3],
    /// 每个智能体最近一次检测的编号：迟到的旧结果按它丢掉。
    generations: [u64; 3],
    /// 有没有开始过检测（面板第一次出现时才触发）。
    started: bool,
}

pub struct StatusStore {
    inner: Mutex<Inner>,
    probe: Arc<dyn Probe>,
    /// 状态变了之后的回调（清掉旧安装的模型目录缓存）。
    on_changed: fn(AgentKind),
}

impl StatusStore {
    pub fn new(probe: Arc<dyn Probe>, on_changed: fn(AgentKind)) -> Self {
        Self {
            inner: Mutex::new(Inner {
                version: 0,
                agents: std::array::from_fn(|_| AgentStatus::checking()),
                generations: [0; 3],
                started: false,
            }),
            probe,
            on_changed,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn version(&self) -> u64 {
        self.lock().version
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.lock();
        Snapshot {
            version: inner.version,
            agents: inner.agents.clone(),
            builtin: AgentStatus::checking(),
        }
    }

    /// 还没检测过就把三个都检测一遍；已经检测过（或正在检测）什么也不做。
    pub fn ensure_checked(self: &Arc<Self>, data_dir: &std::path::Path) {
        let first = {
            let mut inner = self.lock();
            !std::mem::replace(&mut inner.started, true)
        };
        if first {
            self.refresh_all(data_dir);
        }
    }

    pub fn refresh_all(self: &Arc<Self>, data_dir: &std::path::Path) {
        for kind in AgentKind::ALL {
            self.refresh(kind, data_dir);
        }
    }

    /// 重新检测一个智能体：先标成"检测中"，结果回来按编号落地（旧结果作废）。
    pub fn refresh(self: &Arc<Self>, kind: AgentKind, data_dir: &std::path::Path) {
        let generation = {
            let mut inner = self.lock();
            inner.started = true;
            inner.generations[kind.index()] += 1;
            inner.agents[kind.index()] = AgentStatus::checking();
            inner.version += 1;
            inner.generations[kind.index()]
        };
        (self.on_changed)(kind);
        let store = Arc::clone(self);
        let data_dir = data_dir.to_path_buf();
        let task = async move {
            let status = check_agent(store.probe.as_ref(), kind, data_dir).await;
            store.apply(kind, generation, status);
        };
        match crate::ssh_backend::runtime() {
            Ok(runtime) => {
                runtime.spawn(task);
            }
            Err(err) => {
                self.apply(
                    kind,
                    generation,
                    AgentStatus {
                        install: InstallState::Invalid(err),
                        login: None,
                        guard: None,
                    },
                );
            }
        }
    }

    /// 在已有的运行时上跑一次检测并等结果（单测用）。
    #[cfg(test)]
    pub async fn check_now(&self, kind: AgentKind, data_dir: PathBuf) {
        let generation = {
            let mut inner = self.lock();
            inner.generations[kind.index()] += 1;
            inner.generations[kind.index()]
        };
        let status = check_agent(self.probe.as_ref(), kind, data_dir).await;
        self.apply(kind, generation, status);
    }

    fn apply(&self, kind: AgentKind, generation: u64, status: AgentStatus) {
        {
            let mut inner = self.lock();
            if inner.generations[kind.index()] != generation {
                return;
            }
            inner.agents[kind.index()] = status;
            inner.version += 1;
        }
        (self.on_changed)(kind);
    }
}

static STORE: OnceLock<Arc<StatusStore>> = OnceLock::new();

/// 进程级的那一份。调试版里设了 `ZSSH_AGENT_FAKE` 就用假世界（不拉任何真实进程）。
pub fn store() -> &'static Arc<StatusStore> {
    STORE.get_or_init(|| {
        let probe: Arc<dyn Probe> = match super::fake::world() {
            Some(world) => Arc::new(world.probe()),
            None => Arc::new(RealProbe),
        };
        Arc::new(StatusStore::new(probe, super::catalog::forget))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use discovery::DiscoverySource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn install(version: &str, too_old: bool) -> AgentInstall {
        AgentInstall {
            path: "C:\\Tools\\x\\agent.exe".to_owned(),
            version: version.to_owned(),
            source: DiscoverySource::Path,
            needs_shell: false,
            auto: true,
            too_old,
            skipped_older: None,
        }
    }

    struct Scripted {
        discover: Result<Option<AgentInstall>, String>,
        login: Option<LoginStatus>,
        guard: Option<String>,
        logins: AtomicUsize,
        guards: AtomicUsize,
    }

    impl Scripted {
        fn new(
            discover: Result<Option<AgentInstall>, String>,
            login: Option<LoginStatus>,
            guard: Option<String>,
        ) -> Self {
            Self {
                discover,
                login,
                guard,
                logins: AtomicUsize::new(0),
                guards: AtomicUsize::new(0),
            }
        }
    }

    impl Probe for Scripted {
        fn discover(
            &self,
            _kind: AgentKind,
            _data_dir: PathBuf,
        ) -> BoxFuture<'static, Result<Option<AgentInstall>, String>> {
            let answer = self.discover.clone();
            Box::pin(async move { answer })
        }
        fn login(
            &self,
            _kind: AgentKind,
            _install: AgentInstall,
        ) -> BoxFuture<'static, Option<LoginStatus>> {
            self.logins.fetch_add(1, Ordering::SeqCst);
            let answer = self.login.clone();
            Box::pin(async move { answer })
        }
        fn guard(&self, _install: AgentInstall) -> BoxFuture<'static, Option<String>> {
            self.guards.fetch_add(1, Ordering::SeqCst);
            let answer = self.guard.clone();
            Box::pin(async move { answer })
        }
    }

    fn login(logged_in: Option<bool>, hint: Option<&str>) -> LoginStatus {
        LoginStatus {
            logged_in,
            hint: hint.map(str::to_owned),
        }
    }

    #[tokio::test]
    async fn every_state_the_card_names_gets_its_own_availability() {
        let dir = PathBuf::new();
        let cases: Vec<(Scripted, AgentKind, Availability)> = vec![
            (
                Scripted::new(Ok(None), None, None),
                AgentKind::Codex,
                Availability::Missing,
            ),
            (
                Scripted::new(Err("手动指定的路径不存在：codex".into()), None, None),
                AgentKind::Codex,
                Availability::Missing,
            ),
            (
                Scripted::new(Ok(Some(install("opencode 1.17.3", true))), None, None),
                AgentKind::OpenCode,
                Availability::TooOld,
            ),
            (
                Scripted::new(
                    Ok(Some(install("2.1.300 (Claude Code)", false))),
                    Some(login(
                        Some(false),
                        Some("请先在终端里运行 `claude` 完成登录"),
                    )),
                    None,
                ),
                AgentKind::Claude,
                Availability::NeedsLogin,
            ),
            (
                Scripted::new(
                    Ok(Some(install("codex-cli 0.150.0", false))),
                    Some(login(None, Some("没能确认登录状态（读不了）"))),
                    None,
                ),
                AgentKind::Codex,
                Availability::Ready,
            ),
            (
                // 版本串解析不出来：core 把它当不旧，面板照常能用。
                Scripted::new(Ok(Some(install("some build", false))), None, None),
                AgentKind::OpenCode,
                Availability::Ready,
            ),
        ];
        for (probe, kind, want) in cases {
            let status = check_agent(&probe, kind, dir.clone()).await;
            assert_eq!(status.availability(), want, "{kind:?} {status:?}");
        }
    }

    #[tokio::test]
    async fn login_and_guard_are_asked_only_when_they_apply() {
        // opencode 没有登录概念；版本过低的不查登录也不查本机工具核对。
        let probe = Scripted::new(Ok(Some(install("opencode 1.19.1", false))), None, None);
        check_agent(&probe, AgentKind::OpenCode, PathBuf::new()).await;
        assert_eq!(probe.logins.load(Ordering::SeqCst), 0);
        assert_eq!(probe.guards.load(Ordering::SeqCst), 0);

        let probe = Scripted::new(Ok(Some(install("codex-cli 0.100.0", true))), None, None);
        let status = check_agent(&probe, AgentKind::Codex, PathBuf::new()).await;
        assert_eq!(probe.logins.load(Ordering::SeqCst), 0);
        assert_eq!(probe.guards.load(Ordering::SeqCst), 0);
        assert_eq!(status.availability(), Availability::TooOld);

        // 只有 codex 查本机工具核对；Claude 只查登录。
        let probe = Scripted::new(
            Ok(Some(install("codex-cli 0.150.0", false))),
            Some(login(Some(true), None)),
            Some("codex 的本机工具没法确认关掉".into()),
        );
        let status = check_agent(&probe, AgentKind::Codex, PathBuf::new()).await;
        assert_eq!(probe.guards.load(Ordering::SeqCst), 1);
        assert_eq!(
            status.guard.as_deref(),
            Some("codex 的本机工具没法确认关掉")
        );
        let probe = Scripted::new(
            Ok(Some(install("2.1.300", false))),
            Some(login(Some(true), None)),
            Some("不该用到".into()),
        );
        let status = check_agent(&probe, AgentKind::Claude, PathBuf::new()).await;
        assert_eq!(probe.guards.load(Ordering::SeqCst), 0);
        assert_eq!(status.guard, None);
        assert_eq!(status.availability(), Availability::Ready);
    }

    #[test]
    fn unknown_login_only_hints_and_never_blocks() {
        let mut status = AgentStatus {
            install: InstallState::Found(install("2.1.300", false)),
            login: Some(login(
                None,
                Some("没能确认登录状态（settings.json 解析不了）"),
            )),
            guard: None,
        };
        assert_eq!(status.availability(), Availability::Ready);
        assert!(!status.availability().blocks_input());
        assert_eq!(
            status.login_unknown_hint(),
            Some("没能确认登录状态（settings.json 解析不了）")
        );
        // 确定没登录才拦；确定登录了没有提示。
        status.login = Some(login(Some(false), Some("x")));
        assert!(status.availability().blocks_input());
        assert_eq!(status.login_unknown_hint(), None);
        status.login = Some(login(Some(true), None));
        assert_eq!(status.login_unknown_hint(), None);
    }

    #[test]
    fn too_old_only_warns_but_is_not_selectable() {
        assert!(!Availability::TooOld.blocks_input());
        assert_eq!(Availability::TooOld.selector_note(), Some("版本过低"));
        assert_eq!(Availability::Missing.selector_note(), Some("未安装"));
        assert_eq!(Availability::NeedsLogin.selector_note(), Some("未登录"));
        assert_eq!(Availability::Ready.selector_note(), None);
        assert_eq!(Availability::Checking.selector_note(), None);
        assert!(Availability::Checking.blocks_input());
    }

    #[test]
    fn texts_match_0_2_14() {
        assert_eq!(
            missing_text(AgentKind::OpenCode),
            "没有找到 opencode。去设置页指定路径，或者一键下载。"
        );
        assert!(missing_text(AgentKind::Codex).contains("npm i -g @openai/codex"));
        assert!(missing_text(AgentKind::Claude).contains("npm i -g @anthropic-ai/claude-code"));
        assert_eq!(
            login_required_lines(AgentKind::Codex),
            ("codex 还没有登录。", "请先在终端里运行 `codex login`")
        );
        assert_eq!(
            login_required_lines(AgentKind::Claude).1,
            "请先在终端里运行 `claude` 完成登录"
        );
        assert_eq!(login_text(Some(true)), "已登录");
        assert_eq!(login_text(None), "不确定");
        assert!(too_old_note(AgentKind::Codex).contains("0.144.0"));
        assert!(too_old_note(AgentKind::Claude).contains("2.0.41"));
        assert!(too_old_note(AgentKind::OpenCode).contains("1.19"));
        // 来源：只有 opencode 的数据目录叫"已下载"。
        assert_eq!(
            source_label(AgentKind::OpenCode, DiscoverySource::ToolsDir),
            "已下载"
        );
        assert_eq!(
            source_label(AgentKind::Codex, DiscoverySource::ToolsDir),
            "已找到"
        );
        assert_eq!(
            source_label(AgentKind::Claude, DiscoverySource::Setting),
            "手动指定"
        );
    }

    #[test]
    fn login_followup_falls_back_to_the_command_hint() {
        assert_eq!(
            login_followup(AgentKind::Codex, &login(Some(true), None)),
            None
        );
        assert_eq!(
            login_followup(AgentKind::Codex, &login(Some(false), None)).as_deref(),
            Some("未登录：请先在终端里运行 `codex login`")
        );
        assert_eq!(
            login_followup(AgentKind::Claude, &login(None, Some("没能确认登录状态"))).as_deref(),
            Some("登录状态不确定：没能确认登录状态")
        );
    }

    #[test]
    fn an_older_copy_that_was_skipped_is_explained() {
        let mut found = install("opencode 1.19.1", false);
        found.skipped_older = Some(discovery::SkippedOlder {
            version: " 1.17.11 ".to_owned(),
            source: DiscoverySource::Path,
        });
        assert_eq!(
            skipped_older_note(&found).as_deref(),
            Some("PATH 上还有一个旧版本 1.17.11（低于最低要求，没有用它）")
        );
        assert_eq!(skipped_older_note(&install("opencode 1.19.1", false)), None);
    }

    #[tokio::test]
    async fn a_late_result_for_an_old_refresh_is_dropped() {
        fn noop(_: AgentKind) {}
        let store = StatusStore::new(
            Arc::new(Scripted::new(
                Ok(Some(install("2.1.300", false))),
                None,
                None,
            )),
            noop,
        );
        // 新的一次检测先落地，编号更小的旧结果后到：不能盖掉新的。
        store.check_now(AgentKind::Claude, PathBuf::new()).await;
        let after_new = store.snapshot();
        assert!(after_new.get(AgentKind::Claude).found().is_some());
        store.apply(
            AgentKind::Claude,
            0,
            AgentStatus {
                install: InstallState::Missing,
                login: None,
                guard: None,
            },
        );
        assert_eq!(store.snapshot(), after_new);
    }

    /// 真实发现（不用假探测）：路径带空格、带中文、版本串各种写法。手动路径存在设置库里，
    /// 和设置页「检测并保存」之后的状态一样；`.cmd` 垫片代替真实的 npm 安装。
    #[tokio::test]
    async fn real_discovery_handles_spaces_chinese_unknown_and_old_versions() {
        // 路径里带空格和中文是这个用例要测的部分，所以放在随机唯一的父目录里。
        let _root_guard = tempfile::Builder::new()
            .prefix("zssh-g10a-")
            .tempdir()
            .expect("临时目录");
        let root = _root_guard.path().join("发现 测试");
        let tool_dir = root.join("我的 工具").join("bin 目录");
        std::fs::create_dir_all(&tool_dir).expect("建目录");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&data_dir).expect("建数据目录");
        let db = Database::open(&data_dir.join("zssh.db")).expect("设置库");

        let shim = |name: &str, output: &str| {
            // Windows：`.cmd` 垫片。Linux：同名去掉 `.cmd` 的可执行 shell 脚本。
            #[cfg(windows)]
            let path = {
                let path = tool_dir.join(name);
                std::fs::write(&path, format!("@echo off\r\necho {output}\r\n")).expect("写垫片");
                path
            };
            #[cfg(unix)]
            let path = {
                use std::os::unix::fs::PermissionsExt;
                let path = tool_dir.join(name.trim_end_matches(".cmd"));
                std::fs::write(&path, format!("#!/bin/sh\necho '{output}'\n")).expect("写垫片");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("设可执行位");
                path
            };
            path.to_string_lossy().into_owned()
        };
        let cases = [
            (
                AgentKind::OpenCode,
                "opencode.cmd",
                "opencode 1.19.9",
                Availability::Ready,
            ),
            (
                AgentKind::OpenCode,
                "opencode-old.cmd",
                "opencode 1.17.3",
                Availability::TooOld,
            ),
            // 版本串解析不出来：不当成太旧，能用，只是版本显示原样。
            (
                AgentKind::Claude,
                "claude.cmd",
                "some custom build",
                Availability::Ready,
            ),
            (
                AgentKind::Codex,
                "codex-old.cmd",
                "codex-cli 0.100.0",
                Availability::TooOld,
            ),
        ];
        for (kind, file, output, want) in cases {
            let path = shim(file, output);
            db.setting_set(
                kind.path_key(),
                &serde_json::to_string(&path).expect("json"),
            )
            .expect("写设置");
            let found = RealProbe
                .discover(kind, data_dir.clone())
                .await
                .unwrap_or_else(|err| panic!("{kind:?} {file}：{err}"))
                .unwrap_or_else(|| panic!("{kind:?} {file}：没发现"));
            assert_eq!(found.version.trim(), output, "{kind:?}");
            assert_eq!(found.source, DiscoverySource::Setting);
            assert!(!found.auto);
            // 版本够用与否用不着登录 / 本机工具探测，只看发现这一步。
            let status = AgentStatus {
                install: InstallState::Found(found),
                login: None,
                guard: None,
            };
            assert_eq!(status.availability(), want, "{kind:?} {file}");
        }

        // 手动指定的路径不存在：报错，不回退到自动发现。
        db.setting_set(
            AgentKind::Codex.path_key(),
            &serde_json::to_string(&tool_dir.join("没有这个.cmd").to_string_lossy()).expect("json"),
        )
        .expect("写设置");
        let err = RealProbe
            .discover(AgentKind::Codex, data_dir.clone())
            .await
            .expect_err("手动路径坏了要报错");
        assert!(err.contains("手动指定的路径不存在"), "{err}");
        let status = check_agent(&DiscoverOnly, AgentKind::Codex, data_dir.clone()).await;
        assert_eq!(status.availability(), Availability::Missing);
        assert!(matches!(status.install, InstallState::Invalid(_)));
        drop(db);
    }

    /// 真实发现，登录 / 本机工具探测换成空实现：这条用例只关心发现，不能去碰用户真实的智能体配置。
    struct DiscoverOnly;
    impl Probe for DiscoverOnly {
        fn discover(
            &self,
            kind: AgentKind,
            data_dir: PathBuf,
        ) -> BoxFuture<'static, Result<Option<AgentInstall>, String>> {
            RealProbe.discover(kind, data_dir)
        }
        fn login(
            &self,
            _kind: AgentKind,
            _install: AgentInstall,
        ) -> BoxFuture<'static, Option<LoginStatus>> {
            Box::pin(async { None })
        }
        fn guard(&self, _install: AgentInstall) -> BoxFuture<'static, Option<String>> {
            Box::pin(async { None })
        }
    }

    #[test]
    fn blocked_reasons_name_what_to_do() {
        let ready = AgentStatus {
            install: InstallState::Found(install("opencode 1.19.1", false)),
            login: None,
            guard: None,
        };
        assert_eq!(blocked_reason(AgentKind::OpenCode, &ready), None);
        let old = AgentStatus {
            install: InstallState::Found(install("opencode 1.17.3", true)),
            login: None,
            guard: None,
        };
        assert_eq!(
            blocked_reason(AgentKind::OpenCode, &old),
            None,
            "版本过低只提示"
        );
        let checking = AgentStatus::checking();
        assert!(
            blocked_reason(AgentKind::Codex, &checking)
                .expect("检测中不发")
                .contains("正在检测 codex")
        );
        let missing = AgentStatus {
            install: InstallState::Missing,
            login: None,
            guard: None,
        };
        assert_eq!(
            blocked_reason(AgentKind::Claude, &missing).as_deref(),
            Some(missing_text(AgentKind::Claude))
        );
        let logged_out = AgentStatus {
            install: InstallState::Found(install("2.1.300", false)),
            login: Some(login(Some(false), None)),
            guard: None,
        };
        assert_eq!(
            blocked_reason(AgentKind::Claude, &logged_out).as_deref(),
            Some("Claude Code 还没有登录。请先在终端里运行 `claude` 完成登录")
        );
    }

    #[test]
    fn every_status_message_has_both_languages() {
        for key in [
            SetupStatusMsg::NotInstalled,
            SetupStatusMsg::NotLoggedIn,
            SetupStatusMsg::TooOld,
            SetupStatusMsg::MissingOpenCode,
            SetupStatusMsg::MissingCodex,
            SetupStatusMsg::MissingClaude,
            SetupStatusMsg::LoginRequiredCodexHead,
            SetupStatusMsg::LoginRequiredClaudeHead,
            SetupStatusMsg::LoginCommandCodex,
            SetupStatusMsg::LoginCommandClaude,
            SetupStatusMsg::LoginCommandCodexDone,
            SetupStatusMsg::LoginCommandClaudeDone,
            SetupStatusMsg::LoggedIn,
            SetupStatusMsg::LoginUnknown,
            SetupStatusMsg::UnknownLoginHead,
            SetupStatusMsg::UnknownLoginPrefix,
            SetupStatusMsg::UnknownLoginSuffix,
            SetupStatusMsg::FollowupSeparator,
            SetupStatusMsg::CheckingPrefix,
            SetupStatusMsg::CheckingSuffix,
            SetupStatusMsg::TooOldOpenCode,
            SetupStatusMsg::TooOldCodexPrefix,
            SetupStatusMsg::TooOldCodexSuffix,
            SetupStatusMsg::TooOldClaudePrefix,
            SetupStatusMsg::TooOldClaudeSuffix,
            SetupStatusMsg::SourceManual,
            SetupStatusMsg::SourceDownloaded,
            SetupStatusMsg::SourceFound,
            SetupStatusMsg::SourceKnownLocation,
            SetupStatusMsg::SourceDataDir,
            SetupStatusMsg::SourcePath,
            SetupStatusMsg::SkippedOlderMiddle,
            SetupStatusMsg::SkippedOlderSuffix,
        ] {
            assert!(!key.zh().is_empty(), "{key:?} 缺中文");
            assert!(!key.en().is_empty(), "{key:?} 缺英文");
        }
    }
}
