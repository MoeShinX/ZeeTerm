//! 从 OpenSSH 配置文件（`~/.ssh/config`）导入主机（V2-U59）。
//!
//! 分三层，都不碰界面：
//! - 读和解析（[`parse_config`]）：按 `man ssh_config` 的语义把文件（连同 `Include`）整理成一份
//!   [`ParsedConfig`]。读文件只经过注入的 [`ConfigFs`]，**只读**：不写、不改、不移动任何文件，
//!   也不打开 `IdentityFile` 指向的私钥（只记路径）。
//! - 预览与计划（[`build_rows`]、[`plan_import`]）：纯函数。去重、跳板机（含多级）、凭据复用都在这里
//!   算好，界面每次勾选变化都可以重算一遍。
//! - 落库（[`Database::import_ssh_hosts`]）：一个事务，要么全部建好要么什么都不留。
//!
//! 提示（[`ConfigNotice`]、[`HostNote`]、[`PlanNote`]）都是带参数的枚举，不在这里拼界面文案，
//! 由界面按语言生成。

mod apply;
mod load;
mod plan;
mod resolve;
mod syntax;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

pub use apply::ImportOutcome;
pub use load::{ConfigFs, ReadError, RealFs};
pub use plan::{
    build_rows, default_selection, plan_import, ChainIssue, ImportContext, ImportPlan, ImportRow,
    JumpTarget, NewIdentity, PlanAuth, PlanNote, RowPlan,
};

/// 解析配置要用到的几个目录。
#[derive(Debug, Clone)]
pub struct ParseOptions {
    /// 相对路径（`Include`、`IdentityFile`）的基准目录，默认配置文件就是 `~/.ssh`。
    pub ssh_dir: PathBuf,
    /// 展开 `~` 用的用户目录。
    pub home: PathBuf,
}

/// 解析的结果：每个可导入的别名一项，加上被跳过的条目和全局提示。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedConfig {
    pub hosts: Vec<ConfigHost>,
    /// 解析出了别名但没法导入的条目（端口不合法等），连原因一起给用户看。
    pub skipped: Vec<SkippedEntry>,
    pub notices: Vec<ConfigNotice>,
}

/// 一个别名按「第一个取到的值生效」合并出来的设置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigHost {
    pub alias: String,
    /// 地址：`HostName`（`%h` 已换成别名），没写就是别名本身。
    pub address: String,
    pub port: u16,
    /// `User`；没写是 `None`（用什么默认值由导入时决定）。
    pub user: Option<String>,
    /// `IdentityFile` 的第一个，`~` 和相对路径已展开成完整路径（只是字符串，没有打开过）。
    pub identity_file: Option<String>,
    pub proxy_jump: ProxyJump,
    /// 没有导入的关键字（`ProxyCommand`、`LocalForward`……），按写法去重，保持出现顺序。
    pub unsupported: Vec<String>,
    pub notes: Vec<HostNote>,
}

/// `ProxyJump` 的取值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyJump {
    /// 没写。
    Unset,
    /// 写了 `none`。
    Disabled,
    /// 各级跳板的原文（`[user@]host[:port]` 或别名），从最外层到最靠近目标的一级。
    Hops(Vec<String>),
}

/// 某个别名的一条补充说明（解析阶段就能确定的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostNote {
    /// `HostName` 里除了 `%h` 还有别的 `%` 转义，原样保留了。
    PercentInHostName,
    /// `IdentityFile` 里有 `%` 变量，没有展开。
    PercentInIdentityFile,
    /// 写了好几个 `IdentityFile`，只用了第一个；数字是被忽略的个数。
    ExtraIdentityFiles(usize),
}

/// 解析出别名、但没法导入的条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedEntry {
    pub alias: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// `Port` 不是 1–65535 的整数；带原文。
    BadPort(String),
    /// `HostName` 不是合法的地址；带原文。
    BadAddress(String),
}

/// 和某个别名无关的全局提示。文件名都是相对 `ssh_dir` 的写法（在目录外就是完整路径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigNotice {
    /// 一个 `Match` 段被整段跳过了。
    MatchSkipped { file: String, line: usize },
    /// `Include` 绕回了自己，没有再展开。
    IncludeLoop { file: String },
    /// `Include` 嵌套超过 16 层，没有再展开。
    IncludeTooDeep { file: String },
    /// 被 `Include` 的文件读不了。
    IncludeUnreadable { file: String, error: String },
    /// 文件太大，没有读。
    FileTooLarge { file: String },
}

/// 读默认路径下的配置文件要用的目录：`~/.ssh/config` 和它的基准目录。
///
/// 家目录拿不到时返回 `None`（界面提示「没有找到」）。
pub fn default_config_location(home: Option<PathBuf>) -> Option<(PathBuf, ParseOptions)> {
    let home = home.filter(|home| !home.as_os_str().is_empty())?;
    let ssh_dir = home.join(".ssh");
    Some((ssh_dir.join("config"), ParseOptions { ssh_dir, home }))
}

/// 用户自己选了一个文件（或开发钩子指定了路径）：相对路径以这个文件所在的目录为基准，
/// 这样不会顺着 `Include` 去读真实的 `~/.ssh`。`home` 也跟着换成这个目录，只用来展开 `~`。
pub fn custom_config_location(path: &Path) -> ParseOptions {
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."));
    ParseOptions {
        ssh_dir: dir.clone(),
        home: dir,
    }
}

/// 读并解析 `root`。文件不存在 / 读不了 / 太大会返回 [`ReadError`]，其余问题都变成提示。
pub fn parse_config(
    fs: &dyn ConfigFs,
    root: &Path,
    options: &ParseOptions,
) -> Result<ParsedConfig, ReadError> {
    let loaded = load::load(fs, root, options)?;
    Ok(resolve::resolve(loaded, options))
}
