//! codex 的启动参数与启动前探测（AI-3 第 2 / 3 节）。
//!
//! 启动命令行由三块拼成（顺序见 [`build_args`]）：
//!
//! 1. `app-server --stdio`（stdio 上的 JSON-RPC）；
//! 2. `--disable <特性>`：本机能力清单（[`super::policy::DENIED_FEATURES`]）按
//!    `codex features list` **过滤后**再传——实测未知特性名会让 codex 报错退出
//!    （0.144.3），过滤让不同版本都能用；
//! 3. `-c` 配置覆盖：用户自己配置里的每个 MCP 服务补 `enabled=false`（`-c` 是逐键
//!    合并不是整表替换，0.162.1 实测），再加上我们的 `zssh` 服务。
//!
//! 探测本身（`codex features list` / `codex mcp list --json`）在这里实现；枚举不出
//! 用户 MCP 时的兜底是扫 `config.toml` 的 `[mcp_servers.*]` 段头（**只读**，不改）。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::config::McpServerSpec;
use super::{policy, StartOptions, StartupProbes};

/// 最低支持的 codex 版本（AI-3 第 1 节，依据见报告）：
/// 0.144.0 的导出 schema 覆盖我们用到的全部方法与字段，且 `--disable` /
/// `features list` / `mcp list --json` / `app-server --stdio` 都在 0.144.0 上实测可用。
pub const MIN_VERSION: (u64, u64, u64) = (0, 144, 0);

/// 探测命令的超时（`features list` 秒级返回）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// 界面上显示的版本文本。
pub fn format_version(version: (u64, u64, u64)) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

/// 从 `initialize` 的 userAgent（`zssh/0.162.1 (Windows ...)`）里取版本号。
pub fn parse_agent_version(user_agent: &str) -> Option<(u64, u64, u64)> {
    super::super::discovery::parse_version(user_agent)
}

/// 版本是不是低于最低要求。解析不出（None）时**不**判旧：宁可放过，别把能用的版本拦下来。
pub fn version_too_old(version: Option<(u64, u64, u64)>, minimum: (u64, u64, u64)) -> bool {
    version.is_some_and(|found| found < minimum)
}

/// 探测/规划失败（失败关闭，AI-3 验收 0.1）时给用户的统一文案：
/// **探测不出可信的禁用清单就不启动 codex**，绝不退回成"一个都不禁"。
pub fn startup_refusal(reason: &str) -> String {
    format!("无法确认 codex 的本机工具已关闭，已停止启动：{reason}")
}

/// 过滤禁用清单 + 失败关闭检查（纯函数，单测覆盖）。
///
/// - 可用清单为空（`features list` 探测失败 / 超时 / 输出为空）：`Err`——
///   没有清单就没法证明本机能力被关了，不能带着 shell / 浏览器 / 电脑操控启动；
/// - 探测成功但一个**核心项**都对不上（见 [`policy::core_features`]）：`Err`——
///   特性体系像是不一样了，清单不再可信；
/// - 正常：返回要传的 `--disable` 清单（只含这个版本真的有的名字；未知名字会让
///   codex 直接报错退出，0.144.3 实测）。
pub fn plan_disabled_features(available: &[String]) -> Result<Vec<String>, String> {
    if available.is_empty() {
        return Err("`codex features list` 探测失败或输出为空".to_string());
    }
    if !policy::core_features()
        .iter()
        .any(|core| available.iter().any(|have| have == core))
    {
        return Err(format!(
            "特性清单里一个核心项都对不上（应该是 {} 之一，实际没有）——codex 的特性可能改名了",
            policy::core_features().join(" / ")
        ));
    }
    Ok(denied_feature_names(available))
}

/// 实际要传的禁用清单：`DENIED_FEATURES` ∩ 可用清单。
pub fn denied_feature_names(available: &[String]) -> Vec<String> {
    policy::denied_features()
        .iter()
        .filter(|name| available.iter().any(|have| have == *name))
        .map(|name| (*name).to_string())
        .collect()
}

/// 要逐个 `enabled=false` 的 MCP 服务名：用户配置里的全部，除了我们自己的。
pub fn servers_to_disable(configured: &[String], ours: &str) -> Vec<String> {
    let mut out: Vec<String> = configured
        .iter()
        .filter(|name| name.as_str() != ours && !name.is_empty())
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 组装 app-server 的启动参数。
pub fn build_args(
    disabled_features: &[String],
    disabled_mcp_servers: &[String],
    mcp: &McpServerSpec,
) -> Result<Vec<OsString>, String> {
    let mut args: Vec<OsString> = vec!["app-server".into(), "--stdio".into()];
    for feature in disabled_features {
        args.push("--disable".into());
        args.push(feature.into());
    }
    for server in disabled_mcp_servers {
        args.push("-c".into());
        args.push(format!("mcp_servers.{}.enabled=false", toml_key(server)).into());
    }
    args.push("-c".into());
    args.push(policy::zssh_mcp_toml(mcp).into());
    Ok(args)
}

fn toml_key(key: &str) -> String {
    if !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        key.to_string()
    } else {
        format!("'{}'", key.replace('\'', ""))
    }
}

/// 解析 `codex features list` 的输出（每行第一列是特性名）。
pub fn parse_features_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let name = line.split_whitespace().next().unwrap_or("");
        if name.is_empty() || name.starts_with('#') {
            continue;
        }
        out.push(name.to_string());
    }
    out
}

/// 扫 `config.toml` 拿用户自己的 MCP 服务名（只读）。
///
/// 按 TOML 的写法逐行跟踪"当前在哪张表里"，把落在 `mcp_servers` 下的第一层键都收进来：
/// - 段头 `[mcp_servers.<名字>]` / `[mcp_servers."名字"]` / `[mcp_servers.a.env]`，点号两边和括号里
///   可以有空白（`[ mcp_servers . a ]`），键名可以带引号；
/// - 顶级点号键 `mcp_servers.<名字>.command = "x"`、`mcp_servers.<名字> = {…}`；
/// - 先开 `[mcp_servers]` 再在里面写 `<名字> = {…}` / `<名字>.command = "x"`；
/// - `mcp_servers = { 名字 = {...}, ... }` 顶级内联表的第一层键（可以跨行）。
///
/// 为什么要写这么全：这份名单决定哪些用户服务会被 `enabled=false` 关掉，漏掉一个合法写法，
/// 那个服务就会被 codex 照常加载，启动核对发现后只能把进程杀掉——用户的配置没改，
/// 以后每次都启动失败。读不懂的行一律忽略（按"不确定"处理，不崩、不猜）；
/// 多行字符串 / 数组 / 内联表的内部不是配置结构，不扫。
pub fn parse_config_toml_mcp_names(text: &str) -> Vec<String> {
    /// 一个还没写完的跨行值。
    enum Pending {
        /// `mcp_servers = {` 本体：攒起来最后取第一层键。
        McpRoot(String),
        /// 别的值（数组、别的内联表）：只数括号，内部不扫。
        Other,
    }

    let mut out = Vec::new();
    // 当前表头路径；顶层为空。
    let mut table: Vec<String> = Vec::new();
    let mut pending: Option<(Pending, i32)> = None;
    // 在三引号多行字符串里时，记着要等哪种结束符。
    let mut in_multiline: Option<&str> = None;
    for raw in text.lines() {
        if let Some(delim) = in_multiline {
            if raw.contains(delim) {
                in_multiline = None;
            }
            continue;
        }
        let line = strip_toml_comment(raw).trim();
        if let Some((kind, depth)) = pending.take() {
            let depth = depth + net_depth(line);
            let kind = match kind {
                Pending::McpRoot(mut buf) => {
                    buf.push(' ');
                    buf.push_str(line);
                    Pending::McpRoot(buf)
                }
                Pending::Other => Pending::Other,
            };
            if depth > 0 {
                pending = Some((kind, depth));
            } else if let Pending::McpRoot(buf) = kind {
                out.extend(inline_table_keys(&buf));
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            // `[[数组表]]` 也会改变当前表，但里面不会有我们要的 MCP 服务。
            let is_array = header.starts_with('[');
            let header = header.strip_prefix('[').unwrap_or(header);
            let Some(inner) = header_inner(header) else {
                // 段头没闭合：读不懂，当前表不变。
                continue;
            };
            let Some(path) = parse_key_path(inner) else {
                continue;
            };
            if !is_array && path.len() >= 2 && path[0] == "mcp_servers" {
                out.push(path[1].clone());
            }
            table = path;
            continue;
        }
        let Some((key_part, value)) = split_key_value(line) else {
            continue;
        };
        let Some(key) = parse_key_path(key_part) else {
            continue;
        };
        let mut full = table.clone();
        full.extend(key);
        let value = value.trim();
        let is_root = full.len() == 1 && full[0] == "mcp_servers";
        if full.first().map(String::as_str) == Some("mcp_servers") {
            if full.len() >= 2 {
                out.push(full[1].clone());
            } else if value.starts_with('{') && net_depth(value) == 0 {
                out.extend(inline_table_keys(value));
            }
        }
        let depth = net_depth(value);
        if depth > 0 {
            pending = Some(if is_root && value.starts_with('{') {
                (Pending::McpRoot(value.to_string()), depth)
            } else {
                (Pending::Other, depth)
            });
        } else {
            for delim in ["\"\"\"", "'''"] {
                if value.matches(delim).count() % 2 == 1 {
                    in_multiline = Some(delim);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// 引号状态的一步推进（TOML 的基本字符串里 `\` 转义，字面字符串里没有转义）。
fn quote_step(quote: &mut Option<char>, escaped: &mut bool, ch: char) -> bool {
    match *quote {
        Some(q) => {
            if *escaped {
                *escaped = false;
            } else if ch == '\\' && q == '"' {
                *escaped = true;
            } else if ch == q {
                *quote = None;
            }
            true
        }
        None => {
            if ch == '"' || ch == '\'' {
                *quote = Some(ch);
                true
            } else {
                false
            }
        }
    }
}

/// 去掉行尾注释：`#` 在引号里不算（`[mcp_servers."a#b"]`）。
fn strip_toml_comment(line: &str) -> &str {
    let (mut quote, mut escaped) = (None, false);
    for (at, ch) in line.char_indices() {
        if !quote_step(&mut quote, &mut escaped, ch) && ch == '#' {
            return &line[..at];
        }
    }
    line
}

/// 引号外 `{` `[` 加一、`}` `]` 减一（值是否还没写完）。
fn net_depth(text: &str) -> i32 {
    let (mut quote, mut escaped, mut depth) = (None, false, 0);
    for ch in text.chars() {
        if quote_step(&mut quote, &mut escaped, ch) {
            continue;
        }
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// 段头方括号里的内容：到引号外的第一个 `]` 为止；没闭合返回 `None`。
fn header_inner(text: &str) -> Option<&str> {
    let (mut quote, mut escaped) = (None, false);
    for (at, ch) in text.char_indices() {
        if !quote_step(&mut quote, &mut escaped, ch) && ch == ']' {
            return Some(&text[..at]);
        }
    }
    None
}

/// `键 = 值` 在引号外的第一个 `=` 处切开；没有 `=` 返回 `None`。
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let (mut quote, mut escaped) = (None, false);
    for (at, ch) in line.char_indices() {
        if !quote_step(&mut quote, &mut escaped, ch) && ch == '=' {
            return Some((&line[..at], &line[at + 1..]));
        }
    }
    None
}

/// 点号分隔的键路径（`a."b c".d`）：每段可以是裸键（字母数字 `_` `-`）或带引号的键，
/// 点号两边允许空白。读不懂（裸键里有别的字符、引号没闭合、空段）返回 `None`。
fn parse_key_path(text: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut chars = text.trim().chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut part = String::new();
        match chars.peek().copied() {
            Some(q @ ('"' | '\'')) => {
                chars.next();
                loop {
                    match chars.next()? {
                        '\\' if q == '"' => part.push(chars.next()?),
                        c if c == q => break,
                        c => part.push(c),
                    }
                }
            }
            _ => {
                while let Some(&c) = chars.peek() {
                    if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                        part.push(c);
                        chars.next();
                    } else {
                        break;
                    }
                }
                if part.is_empty() {
                    return None;
                }
            }
        }
        parts.push(part);
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.next() {
            None => return Some(parts),
            Some('.') => {}
            Some(_) => return None,
        }
    }
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(at) => &line[..at],
        None => line,
    }
}

/// 顶级内联表 `{ a = {...}, b = {...} }` 里第一层的键。
fn inline_table_keys(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    let mut in_key = true;
    for ch in text.chars() {
        match ch {
            '{' => {
                if depth == 0 {
                    in_key = true;
                    current.clear();
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
            }
            ',' if depth == 1 => {
                if in_key && !current.trim().is_empty() {
                    out.push(clean_key(&current));
                }
                current.clear();
                in_key = true;
            }
            '=' if depth == 1 && in_key => {
                out.push(clean_key(&current));
                current.clear();
                in_key = false;
            }
            _ => {
                if depth == 1 && in_key {
                    current.push(ch);
                }
            }
        }
    }
    out.into_iter().filter(|k| !k.is_empty()).collect()
}

fn clean_key(raw: &str) -> String {
    raw.trim().trim_matches('"').trim_matches('\'').to_string()
}

// ---------------------------------------------------------------------------
// 真实现：跑 codex CLI 做探测
// ---------------------------------------------------------------------------

/// 跑真实 codex CLI 的探测实现。
pub struct CommandProbes {
    /// 已解析的真实可执行文件（`.cmd` 拆过包装之后的）。
    pub install_path: PathBuf,
    /// 覆盖的配置目录（联网测试用独立副本时设置；`None` = 用户自己的
    /// `$CODEX_HOME` / `<家目录>/.codex`，扫 config.toml 看的就是它这份）。
    pub codex_home: Option<PathBuf>,
}

impl CommandProbes {
    /// 用用户默认的配置目录。
    pub fn new(install_path: impl Into<PathBuf>) -> Self {
        Self {
            install_path: install_path.into(),
            codex_home: None,
        }
    }

    /// 覆盖配置目录（测试用独立副本；不碰用户真实配置）。
    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.codex_home = home;
        self
    }
}

impl StartupProbes for CommandProbes {
    fn available_features(&self) -> crate::ssh::BoxFuture<'_, Vec<String>> {
        Box::pin(async move {
            let exe = self.install_path.clone();
            match run_cli(&exe, &["features", "list"]).await {
                Some(text) => parse_features_list(&text),
                None => Vec::new(),
            }
        })
    }

    fn configured_mcp_servers(&self) -> crate::ssh::BoxFuture<'_, Result<Vec<String>, String>> {
        // 只读扫用户的 `config.toml`（`[mcp_servers.*]` 段）。
        //
        // 不用 `codex mcp list --json`：那份列表**混着插件提供的服务**，JSON 里
        // 又分不出来源，给插件服务补 `enabled=false` 会造出没有 transport 的条目，
        // codex 直接启动失败（实测报 `invalid transport in mcp_servers.code-review`）。
        // 插件来源的服务由 `--disable plugins` 统一关掉（见 policy 模块）。
        //
        // **读不出来要返回 `Err`（拒绝启动），不是空表**：空表等于"用户没配 MCP"，
        // 本该禁用的服务就会照常加载（R-2 第 3 节）。
        let home = self.codex_home.clone();
        Box::pin(async move { read_config_toml_mcp_names(home.as_deref()) })
    }
}

/// 跑一次 codex CLI，拿 stdout（失败/超时给 None）。
async fn run_cli(exe: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = super::super::discovery::build_command(exe);
    cmd.args(args);
    super::super::discovery::hide_console(&mut cmd);
    let output = super::super::short_probe::run(cmd, PROBE_TIMEOUT)
        .await
        .ok()?;
    if !output.status.success() {
        tracing::debug!(?args, "codex 探测命令失败（用兜底）");
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 用户配置文件读出来是什么情况（R-2 第 3 节）。
///
/// **"读不出来"和"没有这个文件"必须分开**：前者是"不知道用户配了什么"，
/// 不能当成"用户没配"（那就可能把本该禁用的 MCP 服务放进去）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigRead {
    /// 文件不存在（用户没配过）。
    Missing,
    /// 读到内容（有损解码 + 吃掉 BOM，编码不对也能读到 ASCII 结构）。
    Read(String),
    /// 文件在，但读不出来（权限、被占用、是个目录……）。`reason` 不带文件内容。
    Unreadable(String),
}

/// 用户配置文件的位置：`$CODEX_HOME/config.toml`，没设就是
/// `<家目录>/.codex/config.toml`（`home` 是测试用的显式覆盖）。
fn config_toml_path_in(home: Option<&Path>) -> Option<std::path::PathBuf> {
    match home {
        Some(home) => Some(home.join("config.toml")),
        None => config_toml_path(),
    }
}

/// 只读读用户的 `config.toml`（`home` = `None` 用真实环境）。
pub fn read_config_toml(home: Option<&Path>) -> ConfigRead {
    let Some(path) = config_toml_path_in(home) else {
        return ConfigRead::Missing;
    };
    match std::fs::metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return ConfigRead::Unreadable(format!("{} 不是普通文件", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return ConfigRead::Missing,
        Err(err) => return ConfigRead::Unreadable(format!("{}：{err}", path.display())),
    }
    match crate::ai::agent::text::read_text_lossy(&path) {
        Some(text) => ConfigRead::Read(text),
        None => ConfigRead::Unreadable(format!("读不了 {}", path.display())),
    }
}

/// 扫用户配置里的 MCP 服务名；**读不出来报 `Err`**（调用方据此拒绝启动 codex）。
pub fn read_config_toml_mcp_names(home: Option<&Path>) -> Result<Vec<String>, String> {
    match read_config_toml(home) {
        ConfigRead::Missing => Ok(Vec::new()),
        ConfigRead::Read(text) => Ok(parse_config_toml_mcp_names(&text)),
        ConfigRead::Unreadable(reason) => Err(format!(
            "{reason}：没法确认用户配了哪些 MCP 服务，所以不启动 codex（R-2 第 3 节）"
        )),
    }
}

/// codex 配置文件的路径（只读）。
pub fn config_toml_path() -> Option<PathBuf> {
    config_toml_path_from(std::env::var_os("CODEX_HOME"), std::env::home_dir())
}

/// [`config_toml_path`] 的纯函数版（环境和家目录由调用方给，方便单测）。
fn config_toml_path_from(
    codex_home: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(codex_home) = codex_home.filter(|dir| !dir.is_empty()) {
        return Some(PathBuf::from(codex_home).join("config.toml"));
    }
    let home = home.filter(|home| !home.as_os_str().is_empty())?;
    Some(home.join(".codex").join("config.toml"))
}

/// 建 `StartOptions` 时用的默认探测（真命令；用户默认配置目录）。
pub fn command_probes(options_path: &str) -> std::sync::Arc<dyn StartupProbes> {
    std::sync::Arc::new(CommandProbes::new(options_path))
}

/// 同上，但覆盖配置目录（联网测试用独立 `CODEX_HOME` 副本）。
pub fn command_probes_with_home(
    options_path: &str,
    codex_home: Option<PathBuf>,
) -> std::sync::Arc<dyn StartupProbes> {
    std::sync::Arc::new(CommandProbes::new(options_path).with_home(codex_home))
}

// ---------------------------------------------------------------------------
// 登录判断（AI-3 第 5 节）
// ---------------------------------------------------------------------------

/// codex 的登录状态。**只判断"有没有登录"，绝不读取或打印密钥的值**。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CodexLogin {
    pub logged_in: bool,
    /// 不可用时给用户的可操作提示（例如"先跑 `codex login`"）。
    pub hint: Option<String>,
}

/// 用 `codex login status` 判断（输出里出现 "logged in" 且不是 "not logged in"）。
///
/// 输出文本可能带用户名（邮箱），**只做关键词判断，不把原文带出去**。
pub async fn login_status_cli(exe: &Path) -> Option<CodexLogin> {
    let (success, text) = run_cli_capture(exe, &["login", "status"]).await?;
    let lower = text.to_ascii_lowercase();
    let logged_in = success && lower.contains("logged in") && !lower.contains("not logged in");
    Some(if logged_in {
        CodexLogin {
            logged_in: true,
            hint: None,
        }
    } else {
        CodexLogin {
            logged_in: false,
            hint: Some("请先在终端里运行 `codex login` 完成登录".to_string()),
        }
    })
}

/// 用用户配置兜底：`model_provider` 指向自建服务商时，看服务商段和 `env_key`。
///
/// - `model_provider` 是 `openai`（默认）：这条兜底判断不了，返回 `None`；
/// - 指向别的服务商但没有 `[model_providers.<名字>]` 段：不可用 + 提示；
/// - 段里有 `env_key` 且该环境变量没设置：不可用 + 可操作提示；
/// - 其余：可用。
pub fn login_status_from_config_text(
    text: &str,
    env_set: &dyn Fn(&str) -> bool,
) -> Option<CodexLogin> {
    let provider = top_level_string(text, "model_provider").unwrap_or_else(|| "openai".to_string());
    if provider == "openai" {
        return None;
    }
    let Some((start, end)) = provider_section_range(text, &provider) else {
        return Some(CodexLogin {
            logged_in: false,
            hint: Some(format!(
                "config.toml 里 model_provider = \"{provider}\"，但没有对应的 [model_providers.{provider}] 段"
            )),
        });
    };
    let section = &text[start..end];
    if let Some(env_key) = section_string(section, "env_key") {
        if !env_key.is_empty() && !env_set(&env_key) {
            return Some(CodexLogin {
                logged_in: false,
                hint: Some(format!(
                    "服务商 {provider} 需要环境变量 {env_key}，但它没有设置"
                )),
            });
        }
    }
    Some(CodexLogin {
        logged_in: true,
        hint: None,
    })
}

/// 用真实的用户配置做兜底判断。
pub fn login_status_from_config() -> Option<CodexLogin> {
    match read_config_toml(None) {
        ConfigRead::Read(text) => {
            login_status_from_config_text(&text, &|key| std::env::var_os(key).is_some())
        }
        // 没配过 / 读不出来：这条兜底判断不了（读不出来的情况由调用方给"不确定"）。
        ConfigRead::Missing | ConfigRead::Unreadable(_) => None,
    }
}

/// 顶层（第一个 `[section]` 之前）的 `key = "value"`。
fn top_level_string(text: &str, key: &str) -> Option<String> {
    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.starts_with('[') {
            return None;
        }
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(value) = rest.trim_start().strip_prefix('=') {
                return Some(
                    value
                        .trim()
                        .trim_matches('"')
                        .trim_matches('\'')
                        .to_string(),
                );
            }
        }
    }
    None
}

/// `[model_providers.<名字>]` 段的字节范围（到下一个段头为止）。
fn provider_section_range(text: &str, provider: &str) -> Option<(usize, usize)> {
    let header_plain = format!("[model_providers.{provider}]");
    let header_quoted = format!("[model_providers.\"{provider}\"]");
    let header_quoted_single = format!("[model_providers.'{provider}']");
    let mut start = None;
    let mut offset = 0;
    for raw in text.split_inclusive('\n') {
        let line = strip_comment(raw).trim();
        match start {
            None => {
                if line == header_plain || line == header_quoted || line == header_quoted_single {
                    start = Some(offset + raw.len());
                }
            }
            Some(at) => {
                if line.starts_with('[') {
                    return Some((at, offset));
                }
            }
        }
        offset += raw.len();
    }
    start.map(|start| (start, text.len()))
}

/// 段里（已经切好的文本）的 `key = "value"`。
fn section_string(section: &str, key: &str) -> Option<String> {
    for raw in section.lines() {
        let line = strip_comment(raw).trim();
        if let Some(rest) = line.strip_prefix(key) {
            if let Some(value) = rest.trim_start().strip_prefix('=') {
                let value = value.trim().trim_matches('"').trim_matches('\'');
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// 跑一次 CLI 并把 stdout + stderr 合起来（登录判断要能看错误文本；失败也给文本）。
async fn run_cli_capture(exe: &Path, args: &[&str]) -> Option<(bool, String)> {
    let mut cmd = super::super::discovery::build_command(exe);
    cmd.args(args);
    super::super::discovery::hide_console(&mut cmd);
    let output = super::super::short_probe::run(cmd, PROBE_TIMEOUT)
        .await
        .ok()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Some((output.status.success(), text))
}

/// 启动计划：命令行参数 + 我们明确禁用掉的 MCP 服务名（UI-1 0.3 的启动核对要用）。
pub struct StartupPlan {
    pub args: Vec<OsString>,
    /// 被 `enabled=false` 关掉的服务名（`mcpServerStatus/list` 里只允许这些名字
    /// 和 `zssh` 出现；别的名字说明隔离没生效）。
    pub disabled_servers: Vec<String>,
}

/// 从 `StartOptions` 里跑探测并组参数（`CodexRuntime::start` 用的入口，测试可注入）。
pub async fn startup_plan(options: &StartOptions) -> Result<StartupPlan, String> {
    let available = options.probes.available_features().await;
    let features = plan_disabled_features(&available).map_err(|reason| startup_refusal(&reason))?;
    let configured = options
        .probes
        .configured_mcp_servers()
        .await
        .map_err(|reason| startup_refusal(&reason))?;
    let servers = servers_to_disable(&configured, &options.mcp.name);
    let args = build_args(&features, &servers, &options.mcp)?;
    Ok(StartupPlan {
        args,
        disabled_servers: servers,
    })
}

/// 只做"本机工具关不关得掉"的核对（设置页 / 面板显示用；启动路径在
/// [`startup_plan`] 里同样检查一遍）。通过返回 `Ok(())`，拒绝返回带统一前缀的原因。
pub async fn guard_check(exe: &Path) -> Result<(), String> {
    let available = match run_cli(exe, &["features", "list"]).await {
        Some(text) => parse_features_list(&text),
        None => Vec::new(),
    };
    plan_disabled_features(&available)
        .map(|_| ())
        .map_err(|reason| startup_refusal(&reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_features_list_output() {
        let text = "plugins                              stable             true\n\
                    shell_tool                           stable             true\n\
                    code_mode_host                       stable             true\n";
        let names = parse_features_list(text);
        assert_eq!(names, vec!["plugins", "shell_tool", "code_mode_host"]);
    }

    #[test]
    fn config_toml_scan_finds_section_headers_and_inline_tables() {
        let text = "\
model = \"gpt-x\"\n\
[mcp_servers.my_mcp_server]\n\
command = \"my-mcp-server.exe\"\n\
[mcp_servers.\"my mcp server\"]\n\
command = \"x\"\n\
[mcp_servers.my_mcp_server.env]\n\
SECRET = \"hidden\"\n\
[other]\n\
x = 1\n";
        let names = parse_config_toml_mcp_names(text);
        assert_eq!(names, vec!["my mcp server", "my_mcp_server"]);

        let inline = "mcp_servers = { a = { command = \"x\" }, b = { url = \"y\" } }\n";
        assert_eq!(parse_config_toml_mcp_names(inline), vec!["a", "b"]);

        let commented = "# [mcp_servers.fake]\nmcp_servers = {} # none\n";
        assert!(parse_config_toml_mcp_names(commented).is_empty());
    }

    /// 2026-11-11 审查：只认 `[mcp_servers.名字]` 段头和顶级内联表，别的合法写法漏掉名字，
    /// 那个服务没被 `enabled=false` 关掉，启动核对发现后杀进程——配置不改就永远启动不了。
    #[test]
    fn config_toml_scan_understands_the_other_legal_toml_spellings() {
        let names = |text: &str| parse_config_toml_mcp_names(text);
        // 括号 / 点号两边有空白，键名带引号
        assert_eq!(names("[ mcp_servers . a ]\n"), vec!["a"]);
        assert_eq!(names("[mcp_servers.'b c'.env]\n"), vec!["b c"]);
        assert_eq!(names("[\"mcp_servers\".\"d\"]\n"), vec!["d"]);
        // 引号里有 # 不是注释
        assert_eq!(names("[mcp_servers.\"a#b\"] # 注释\n"), vec!["a#b"]);
        // 顶级点号键
        assert_eq!(
            names("mcp_servers.x.command = \"c\"\nmcp_servers.y = { url = \"u\" }\n"),
            vec!["x", "y"]
        );
        // 先开 [mcp_servers] 再在里面写
        let nested =
            "[mcp_servers]\nfoo = { command = \"c\" }\nbar.command = \"c\"\n\n[other]\nbaz = 1\n";
        assert_eq!(names(nested), vec!["bar", "foo"]);
        // 跨行的内联表：内部的键不是服务名
        let multi = "[mcp_servers]\nfoo = {\n  command = \"c\",\n  args = [\n    \"a\",\n  ],\n}\nafter = { url = \"u\" }\n";
        assert_eq!(names(multi), vec!["after", "foo"]);
        let root = "mcp_servers = {\n  p = { command = \"c\" },\n  q = { url = \"u\" },\n}\n";
        assert_eq!(names(root), vec!["p", "q"]);
        // 多行数组 / 多行字符串里长得像段头的内容不算
        let strings = "args = [\n  \"[mcp_servers.fake]\",\n]\nnote = \"\"\"\n[mcp_servers.fake2]\n\"\"\"\n[mcp_servers.real]\n";
        assert_eq!(names(strings), vec!["real"]);
        // 其他表里同名的键不算
        assert!(names("[profiles.p]\nmcp_servers = 1\n[tools]\nmcp_servers.z = 1\n").is_empty());
        assert!(names("[tools]\nfoo = { a = 1 }\n").is_empty());
        // 读不懂的行直接忽略，不崩
        assert!(names("[mcp_servers\n= = =\n[[x]]\n\"unterminated\n").is_empty());
    }

    /// R-2 第 3 节：用户的 `config.toml` 不存在 / 空 / 格式坏了 / 编码不对 / 带 BOM
    /// 都不能崩，而且**"读不出来"要和"没有这个文件"分开**——读不出来时报错（拒绝
    /// 启动 codex），不能返回空表当成"用户没配 MCP"。
    #[test]
    fn config_toml_read_survives_broken_files_and_fails_closed() {
        let root = crate::test_util::TestDir::new("codex-config");

        // ① 文件不存在：Missing，扫出来是空表（确实没配）。
        let missing = root.join("没有配置");
        std::fs::create_dir_all(&missing).unwrap();
        assert_eq!(read_config_toml(Some(&missing)), ConfigRead::Missing);
        assert!(read_config_toml_mcp_names(Some(&missing))
            .unwrap()
            .is_empty());

        // ② 空文件：读得到（空内容），扫出来是空表，不崩。
        let empty = root.join("空配置");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("config.toml"), b"").unwrap();
        assert_eq!(
            read_config_toml(Some(&empty)),
            ConfigRead::Read(String::new())
        );
        assert!(read_config_toml_mcp_names(Some(&empty)).unwrap().is_empty());

        // ③ 编码不是 UTF-8（GBK 的"中"混在注释里）：段头还是 ASCII，名字要扫出来
        //    （该禁用的服务不能因为编码问题漏掉）。
        let gbk = root.join("编码不对");
        std::fs::create_dir_all(&gbk).unwrap();
        let mut bytes = b"[mcp_servers.my_mcp_server]\n# ".to_vec();
        bytes.extend_from_slice(&[0xD6, 0xD0]);
        bytes.push(b'\n');
        std::fs::write(gbk.join("config.toml"), &bytes).unwrap();
        assert_eq!(
            read_config_toml_mcp_names(Some(&gbk)).unwrap(),
            vec!["my_mcp_server"]
        );

        // ④ 带 BOM：段头前面的 BOM 要吃掉，否则名字会带上 BOM。
        let bom = root.join("带 BOM");
        std::fs::create_dir_all(&bom).unwrap();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"[mcp_servers.my_mcp_server]\n");
        std::fs::write(bom.join("config.toml"), &bytes).unwrap();
        assert_eq!(
            read_config_toml_mcp_names(Some(&bom)).unwrap(),
            vec!["my_mcp_server"]
        );

        // ⑤ 格式坏了（段头没闭合）：扫不出名字——但这不是"放过去"的借口：启动路径
        //    在放行回合前还会用 `mcpServerStatus/list` 核对（见 mod.rs 的用例）。
        let broken = root.join("格式坏了");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("config.toml"), b"[mcp_servers\ncommand = 'x'\n").unwrap();
        assert!(
            read_config_toml_mcp_names(Some(&broken))
                .unwrap()
                .is_empty(),
            "坏格式扫不出名字（这是「启动后核对」这道防线的前提）"
        );

        // ⑥ config.toml 是个目录（读不出来）：Unreadable + `Err`，拒绝启动。
        let as_dir = root.join("config.toml 是目录");
        std::fs::create_dir_all(as_dir.join("config.toml")).unwrap();
        assert!(matches!(
            read_config_toml(Some(&as_dir)),
            ConfigRead::Unreadable(_)
        ));
        let err = read_config_toml_mcp_names(Some(&as_dir)).expect_err("读不出来要报错");
        assert!(err.contains("不启动 codex"), "{err}");
    }

    #[test]
    fn version_comparison_and_minimum_floor() {
        assert_eq!(
            parse_agent_version("zssh/0.162.1 (Windows 10; x86_64)"),
            Some((0, 162, 1))
        );
        assert!(version_too_old(Some((0, 143, 9)), MIN_VERSION));
        assert!(!version_too_old(Some((0, 144, 0)), MIN_VERSION));
        assert!(!version_too_old(None, MIN_VERSION), "解析不出不判旧");
        assert_eq!(format_version(MIN_VERSION), "0.144.0");
    }

    #[test]
    fn plan_intersects_the_deny_list_with_the_available_features() {
        let available: Vec<String> = vec![
            "plugins".into(),
            "shell_tool".into(),
            "tui_app_server".into(),
        ];
        let planned = plan_disabled_features(&available).expect("有核心项，应通过");
        assert!(planned.contains(&"plugins".to_string()));
        assert!(planned.contains(&"shell_tool".to_string()));
        assert!(
            !planned.contains(&"view_image".to_string()),
            "不存在的名字不能传（实测会报错）"
        );
    }

    #[test]
    fn plan_refuses_when_the_probe_fails_or_the_features_are_renamed() {
        // AI-3 验收 0.1：探测失败 / 输出为空 → 拒绝启动，不能退回成"一个都不禁"。
        for empty in [vec![], Vec::new()] {
            let err = plan_disabled_features(&empty).expect_err("空清单要拒绝");
            assert!(err.contains("探测失败"), "原因要说清：{err}");
        }
        // 探测成功但核心项一个都对不上（比如全改名了）→ 同样拒绝。
        let renamed: Vec<String> = vec!["tui_app_server".into(), "sqlite".into(), "steer".into()];
        let err = plan_disabled_features(&renamed).expect_err("核心项缺失要拒绝");
        assert!(err.contains("核心项"), "原因要说清：{err}");
        assert!(err.contains("shell_tool"), "要把核心项列出来：{err}");

        // 只要命中一个核心项就放行（老版本没有 unified_exec_tty 这类也要能用）。
        let only_shell: Vec<String> = vec!["shell_tool".into(), "sqlite".into()];
        assert!(plan_disabled_features(&only_shell).is_ok());

        // 拒绝文案带统一前缀（面板 / 设置页直接显示）。
        let message = startup_refusal("原因");
        assert!(message.starts_with("无法确认 codex 的本机工具已关闭，已停止启动："));
    }

    #[test]
    fn servers_to_disable_keeps_user_servers_and_drops_ours() {
        let names = vec![
            "my_mcp_server".to_string(),
            "zssh".to_string(),
            "other_server".to_string(),
            "my_mcp_server".to_string(),
        ];
        assert_eq!(
            servers_to_disable(&names, "zssh"),
            vec!["my_mcp_server", "other_server"]
        );
    }

    #[test]
    fn build_args_pins_the_isolation_recipe() {
        let spec = McpServerSpec {
            name: "zssh".to_string(),
            command: vec!["C:\\data\\zssh-mcp.exe".to_string()],
            environment: vec![("ZSSH_BRIDGE_PORT".to_string(), "1".to_string())],
        };
        let args = build_args(
            &["plugins".to_string()],
            &["my_mcp_server".to_string()],
            &spec,
        )
        .unwrap();
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(shown[0], "app-server");
        assert_eq!(shown[1], "--stdio");
        let plugins_at = shown.iter().position(|a| a == "plugins").unwrap();
        assert_eq!(shown[plugins_at - 1], "--disable");
        let node_at = shown
            .iter()
            .position(|a| a == "mcp_servers.my_mcp_server.enabled=false")
            .unwrap();
        assert_eq!(shown[node_at - 1], "-c");
        let zssh = shown.last().unwrap();
        assert!(zssh.starts_with("mcp_servers.zssh={command='C:\\data\\zssh-mcp.exe'"));
    }

    #[test]
    fn build_args_expose_only_the_zssh_server_whose_tools_include_the_file_tools() {
        let spec = McpServerSpec {
            name: "zssh".to_string(),
            command: vec!["zssh-mcp.exe".to_string()],
            environment: vec![],
        };
        let args = build_args(
            &["shell_tool".to_string()],
            &["my_mcp_server".to_string()],
            &spec,
        )
        .unwrap();
        let shown: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // 启用的 MCP 服务只有 zssh 一个；其余都是 enabled=false。
        let enabled: Vec<&String> = shown
            .iter()
            .filter(|a| a.starts_with("mcp_servers.") && !a.ends_with(".enabled=false"))
            .collect();
        assert_eq!(enabled.len(), 1, "{shown:?}");
        assert!(enabled[0].starts_with("mcp_servers.zssh="));
        // zssh 的工具调用自动放行（确认由工具桥负责），文件工具跟着这个服务一起放行；
        // 本机命令仍然靠 --disable shell_tool 关着。
        assert!(enabled[0].contains("default_tools_approval_mode='approve'"));
        assert!(shown
            .windows(2)
            .any(|w| w[0] == "--disable" && w[1] == "shell_tool"));
        // 给模型的提示里写明了新工具。
        let prompt = crate::ai::agent::config::system_prompt("cap");
        for tool in ["list_dir", "read_file", "write_file", "stat"] {
            assert!(prompt.contains(tool), "提示里要有 {tool}");
        }
    }

    #[tokio::test]
    async fn startup_plan_uses_injected_probes() {
        let plan_workspace = crate::test_util::TestDir::new("codex-plan");
        struct Fake {
            features: Vec<String>,
            servers: Vec<String>,
        }
        impl StartupProbes for Fake {
            fn available_features(&self) -> crate::ssh::BoxFuture<'_, Vec<String>> {
                Box::pin(async move { self.features.clone() })
            }
            fn configured_mcp_servers(
                &self,
            ) -> crate::ssh::BoxFuture<'_, Result<Vec<String>, String>> {
                Box::pin(async move { Ok(self.servers.clone()) })
            }
        }
        let options = StartOptions {
            install: super::super::super::discovery::AgentInstall {
                path: "codex.exe".into(),
                version: "0.162.1".into(),
                source: super::super::super::discovery::DiscoverySource::Path,
                needs_shell: false,
                auto: true,
                too_old: false,
                skipped_older: None,
            },
            workspace: plan_workspace.to_path_buf(),
            mcp: McpServerSpec {
                name: "zssh".to_string(),
                command: vec!["C:\\data\\zssh-mcp.exe".to_string()],
                environment: vec![],
            },
            probes: std::sync::Arc::new(Fake {
                features: vec!["plugins".into(), "shell_tool".into()],
                servers: vec!["my_mcp_server".into(), "zssh".into()],
            }),
            codex_home: None,
            startup_timeout: Duration::from_secs(1),
        };
        let plan = startup_plan(&options).await.unwrap();
        let shown: Vec<String> = plan
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(shown.contains(&"plugins".to_string()));
        assert!(shown.contains(&"shell_tool".to_string()));
        assert!(shown.contains(&"mcp_servers.my_mcp_server.enabled=false".to_string()));
        assert!(!shown
            .iter()
            .any(|a| a.contains("mcp_servers.zssh.enabled=false")));
        assert_eq!(
            plan.disabled_servers,
            vec!["my_mcp_server".to_string()],
            "禁用名单要跟着返回（启动核对用）"
        );
    }

    #[test]
    fn config_login_check_covers_provider_combinations() {
        // openai（默认）：这条兜底判断不了。
        assert!(login_status_from_config_text("model = \"gpt-x\"\n", &|_| true).is_none());
        assert!(
            login_status_from_config_text("model_provider = \"openai\"\n", &|_| true).is_none()
        );

        // 自建服务商 + 段存在 + env_key 已设置 → 可用。
        let good = "\
model_provider = \"my-gw\"\n\
[model_providers.my-gw]\n\
name = \"My Gateway\"\n\
base_url = \"https://example.com/v1\"\n\
env_key = \"MY_GW_KEY\"\n";
        let status = login_status_from_config_text(good, &|key| key == "MY_GW_KEY").unwrap();
        assert!(status.logged_in);
        assert!(status.hint.is_none());

        // env_key 没设置 → 不可用 + 可操作提示（只出现变量名，不出现值）。
        let status = login_status_from_config_text(good, &|_| false).unwrap();
        assert!(!status.logged_in);
        let hint = status.hint.unwrap();
        assert!(hint.contains("MY_GW_KEY"));

        // 没有对应段 → 不可用 + 提示。
        let missing = "model_provider = \"ghost\"\n";
        let status = login_status_from_config_text(missing, &|_| true).unwrap();
        assert!(!status.logged_in);
        assert!(status.hint.unwrap().contains("model_providers.ghost"));

        // 段里没有 env_key（例如用登录态的服务商）→ 可用。
        let no_env = "\
model_provider = \"my-gw\"\n\
[model_providers.my-gw]\n\
base_url = \"https://example.com/v1\"\n";
        assert!(
            login_status_from_config_text(no_env, &|_| false)
                .unwrap()
                .logged_in
        );
    }

    /// `config.toml` 的位置：`CODEX_HOME` 优先，没设就是 `<家目录>/.codex/config.toml`。
    #[test]
    fn config_toml_path_follows_codex_home_then_the_home_directory() {
        let home = PathBuf::from("home-base").join("张 三");
        assert_eq!(
            config_toml_path_from(Some("codex 数据".into()), Some(home.clone())),
            Some(PathBuf::from("codex 数据").join("config.toml"))
        );
        for unset in [None, Some(std::ffi::OsString::new())] {
            assert_eq!(
                config_toml_path_from(unset, Some(home.clone())),
                Some(home.join(".codex").join("config.toml"))
            );
        }
        assert_eq!(config_toml_path_from(None, None), None);
    }
}
