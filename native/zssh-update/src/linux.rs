//! Linux 上的应用内更新：判断本机怎么装的、选哪种包、deb 经 `pkexec apt-get` 安装、
//! tar.gz 原子替换程序文件、重启命令。
//!
//! 整个模块不依赖 Linux 才有的 API，Windows 上也能编译；纯逻辑（选包、命令行、清单改写、
//! 重启命令）的测试在两边都跑，真正改文件 / 起进程的测试只在 unix 上跑。
//! 所有会起进程的地方都经 [`CommandRunner`]（或函数参数），测试里换成假实现，
//! 不会弹授权框，也不会碰系统。
//!
//! 安装布局（和 `native/installer/linux` 的打包一致）：
//! - deb：`/usr/lib/zeeterm/{zeeterm,zssh-mcp}`，`/usr/bin/zeeterm` 是指向 `../lib/zeeterm/zeeterm` 的相对软链接。
//! - tar.gz 用 `install.sh` 装：`<prefix>/lib/zeeterm/{zeeterm,zssh-mcp}`，`<prefix>/bin/zeeterm` 软链接，
//!   `<prefix>/share/zeeterm/install-manifest` 记着装了哪些文件。
//! - tar.gz 手动解压：平铺的目录，`zeeterm` 和 `zssh-mcp` 在一起。
//!
//! `zssh-mcp` 必须和主程序在同一个目录（`current_exe()` 在 Linux 上读 `/proc/self/exe`，会解析软链接），
//! 所以替换时两个文件一起换，并且作为一个整体回滚。

use std::ffi::{OsStr, OsString};
use std::io::{self, Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::install::{InstallError, check_file_hash};
use crate::{PLATFORM_LINUX_DEB, PLATFORM_LINUX_TAR};

/// 应用标识（`.desktop` 文件名、图标名）。
pub const APP_ID: &str = "app.zeeterm.ZeeTerm";
/// 主程序的文件名。
pub const MAIN_BIN: &str = "zeeterm";
/// 内部辅助程序（AI 工具桥）的文件名；必须和主程序在同一个目录。
pub const HELPER_BIN: &str = "zssh-mcp";
/// deb 装好之后用户命令的位置（软链接）；重启时从这里启动新版本。
pub const DEB_COMMAND: &str = "/usr/bin/zeeterm";
/// deb 的包名（`dpkg -S` 的结果里要出现它才算是 deb 装的）。
pub const DEB_PACKAGE_NAME: &str = "zeeterm";
/// `zeeterm --version` 输出的开头；校验新程序时核对 `ZeeTerm <版本>`。
pub const VERSION_BANNER: &str = "ZeeTerm";
/// 新程序 `--version` 的等待上限。
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// 选包
// ---------------------------------------------------------------------------

/// 本机是怎么装的，决定用 `latest.json` 里哪一个 Linux 条目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// 系统包管理器装的（`dpkg -S` 认得）。
    Deb,
    /// `install.sh` 装的，或者手动解压的 tar.gz。
    Tar,
}

impl Flavor {
    /// `latest.json` 里这种安装方式对应的平台 key。
    pub fn platform_key(self) -> &'static str {
        match self {
            Self::Deb => PLATFORM_LINUX_DEB,
            Self::Tar => PLATFORM_LINUX_TAR,
        }
    }

    /// [`crate::Manifest::select_for`] 用的 key 列表：只认自己这一种，不拿 tar.gz 去装 deb 的安装，反之亦然。
    pub fn platform_keys(self) -> &'static [&'static str] {
        match self {
            Self::Deb => &[PLATFORM_LINUX_DEB],
            Self::Tar => &[PLATFORM_LINUX_TAR],
        }
    }

    /// 平台 key 反查；Windows 的 key 和未知 key 返回 `None`。
    pub fn from_platform_key(key: &str) -> Option<Self> {
        match key {
            PLATFORM_LINUX_DEB => Some(Self::Deb),
            PLATFORM_LINUX_TAR => Some(Self::Tar),
            _ => None,
        }
    }

    /// 安装包的扩展名（含点），下载的文件必须是这个结尾。
    pub fn package_suffix(self) -> &'static str {
        match self {
            Self::Deb => ".deb",
            Self::Tar => ".tar.gz",
        }
    }

    fn accepts_file_name(self, path: &Path) -> bool {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.to_ascii_lowercase().ends_with(self.package_suffix()))
    }
}

/// 一次命令执行的结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandOutput {
    /// 退出码；被信号杀死时为 `None`。
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// 起外部命令的口子：产品里是 [`SystemRunner`]，测试里换成假实现。
pub trait CommandRunner {
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<CommandOutput>;
}

/// 真的起进程（标准输入接空，捕获输出）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<CommandOutput> {
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .output()?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// 判断本机是怎么装的：可执行文件在 `/usr/` 下，并且 `dpkg -S` 说它属于 `zeeterm` 包 → deb；其余 → tar.gz。
///
/// `exe` 是解析过软链接的绝对路径（`current_exe()`，去掉替换后 `/proc` 加的 ` (deleted)` 后缀）。
/// `dpkg` 不存在、执行失败、查不到都按 tar.gz 处理：tar.gz 的路径对"装不了"有手动安装的后备，更保守。
pub fn detect_flavor(exe: &Path, runner: &dyn CommandRunner) -> Flavor {
    if !exe.starts_with("/usr/") {
        return Flavor::Tar;
    }
    let args = [OsString::from("-S"), exe.as_os_str().to_owned()];
    match runner.run(Path::new("dpkg"), &args) {
        Ok(output) if output.code == Some(0) && dpkg_says_owned_by_zeeterm(&output.stdout, exe) => {
            Flavor::Deb
        }
        _ => Flavor::Tar,
    }
}

/// `dpkg -S` 的输出形如 `zeeterm: /usr/lib/zeeterm/zeeterm`（多个包是 `a, b: 路径`）。
/// 逐行看：路径要和 `exe` 完全一致，包名里要有 `zeeterm`；别的包占着这个路径不算。
fn dpkg_says_owned_by_zeeterm(stdout: &str, exe: &Path) -> bool {
    let exe = exe.to_string_lossy();
    stdout.lines().any(|line| {
        let Some((packages, path)) = line.split_once(": ") else {
            return false;
        };
        path.trim() == exe
            && packages
                .split(',')
                .any(|name| name.trim() == DEB_PACKAGE_NAME)
    })
}

/// `/proc/self/exe` 在程序文件被替换后会多出 ` (deleted)`；去掉它得到原来的路径。
pub fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    match path
        .to_str()
        .and_then(|text| text.strip_suffix(" (deleted)"))
    {
        Some(stripped) => PathBuf::from(stripped),
        None => path,
    }
}

// ---------------------------------------------------------------------------
// 安装计划（只做路径 / 哈希校验，不起进程）
// ---------------------------------------------------------------------------

/// 要安装的 Linux 包。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxInstallRequest {
    pub package: PathBuf,
    /// 更新缓存根目录；安装包必须在这里面。
    pub cache_root: PathBuf,
    pub version: String,
    /// 验签通过的那份字节的 SHA-256（十六进制）。
    pub sha256: String,
    pub flavor: Flavor,
}

/// 校验过的安装计划：包在缓存目录里、格式对、内容还是验签时的那份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxInstallPlan {
    /// 规范化后的绝对路径。
    pub package: PathBuf,
    pub version: String,
    pub sha256: String,
    pub flavor: Flavor,
}

/// 和 Windows 的 [`crate::install::plan_install`] 同样的防线：包必须存在、是文件、扩展名和要装的格式一致、
/// 在更新缓存目录里，并且内容的 SHA-256 等于验签时记下的那个（缓存目录可被同用户的其他进程写入）。
pub fn plan_linux_install(request: &LinuxInstallRequest) -> Result<LinuxInstallPlan, InstallError> {
    if request.version.trim().is_empty() {
        return Err(InstallError::EmptyVersion);
    }
    if !request.package.exists() {
        return Err(InstallError::MissingInstaller);
    }
    if !request.package.is_file() {
        return Err(InstallError::NotAFile);
    }
    if !request.flavor.accepts_file_name(&request.package) {
        return Err(InstallError::NotLinuxPackage);
    }
    let package = request
        .package
        .canonicalize()
        .map_err(|err| InstallError::Path(err.to_string()))?;
    let cache_root = request
        .cache_root
        .canonicalize()
        .map_err(|err| InstallError::Path(err.to_string()))?;
    if !package.starts_with(&cache_root) {
        return Err(InstallError::OutsideCache);
    }
    check_file_hash(&package, &request.sha256)?;
    Ok(LinuxInstallPlan {
        package,
        version: request.version.trim().to_owned(),
        sha256: request.sha256.trim().to_ascii_lowercase(),
        flavor: request.flavor,
    })
}

// ---------------------------------------------------------------------------
// deb：pkexec apt-get install
// ---------------------------------------------------------------------------

/// 为什么没能自动安装、改成让用户手动装。这不是错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManualReason {
    /// 本机没有 `pkexec`。
    NoPkexec,
    /// 用户取消了系统授权框，或者没有授权代理 / 没有授权。
    NotAuthorized,
    /// tar.gz：程序所在目录写不进去（系统目录、只读挂载……）。
    NotWritable,
}

/// deb 安装的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebResult {
    Installed,
    Manual(ManualReason),
    /// `apt-get` 跑了但失败；带它输出的末尾几行（系统原文，不翻译）。
    Failed(String),
}

/// 起 `pkexec` 需要的程序位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebTools {
    /// 没有就是 `None`（走手动安装的后备）。
    pub pkexec: Option<PathBuf>,
    pub apt_get: PathBuf,
}

impl DebTools {
    /// 在 `PATH`（再加上 `/usr/bin`、`/bin`）里找 `pkexec` 和 `apt-get`。
    /// `apt-get` 优先用绝对路径：授权框里显示的就是"以管理员身份运行 /usr/bin/apt-get"。
    pub fn discover(path_var: Option<&OsStr>) -> Self {
        let mut dirs: Vec<PathBuf> = path_var
            .map(|value| std::env::split_paths(value).collect())
            .unwrap_or_default();
        for fallback in ["/usr/bin", "/bin"] {
            dirs.push(PathBuf::from(fallback));
        }
        let find = |name: &str| {
            dirs.iter()
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(name))
                .find(|candidate| is_executable_file(candidate))
        };
        Self {
            pkexec: find("pkexec"),
            apt_get: find("apt-get").unwrap_or_else(|| PathBuf::from("apt-get")),
        }
    }
}

#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// `pkexec apt-get install -y <deb>`。
///
/// pkexec 的退出码：126 = 用户关掉了授权框或没有授权，127 = 没法执行（没有授权代理、命令不存在……）；
/// 这两种都不算错误，交给调用方显示手动安装的说明。0 = 装好了；其他非 0 是 apt 自己失败了，带上它的输出。
pub fn install_deb(runner: &dyn CommandRunner, tools: &DebTools, deb: &Path) -> DebResult {
    let Some(pkexec) = &tools.pkexec else {
        return DebResult::Manual(ManualReason::NoPkexec);
    };
    let args = [
        tools.apt_get.as_os_str().to_owned(),
        OsString::from("install"),
        OsString::from("-y"),
        deb.as_os_str().to_owned(),
    ];
    match runner.run(pkexec, &args) {
        Ok(output) => match output.code {
            Some(0) => DebResult::Installed,
            Some(126 | 127) => DebResult::Manual(ManualReason::NotAuthorized),
            code => {
                let tail = tail_text(if output.stderr.trim().is_empty() {
                    &output.stdout
                } else {
                    &output.stderr
                });
                DebResult::Failed(if tail.is_empty() {
                    match code {
                        Some(code) => format!("exit code {code}"),
                        None => "terminated by a signal".to_owned(),
                    }
                } else {
                    tail
                })
            }
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            DebResult::Manual(ManualReason::NoPkexec)
        }
        Err(err) => DebResult::Failed(err.to_string()),
    }
}

/// 命令输出的末尾几行（最多 8 行、800 字符），界面里显示用。
fn tail_text(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(8);
    let joined = lines[start..].join("\n");
    if joined.chars().count() <= 800 {
        return joined;
    }
    let skip = joined.chars().count() - 800;
    joined.chars().skip(skip).collect()
}

/// 手动装 deb 时给用户看的命令：`sudo apt install <路径>`（路径按 shell 规则加引号）。
pub fn manual_deb_command(deb: &Path) -> String {
    format!("sudo apt install {}", shell_quote(deb))
}

/// 路径按 POSIX shell 规则加单引号；只含安全字符时原样返回。
pub fn shell_quote(path: &Path) -> String {
    let text = path.to_string_lossy();
    let safe = !text.is_empty()
        && text.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(ch, '/' | '.' | '_' | '-' | '+' | ',' | ':' | '@' | '%')
        });
    if safe {
        return text.into_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// tar.gz：解压、校验、原子替换
// ---------------------------------------------------------------------------

/// 本机的 tar.gz 安装布局（由正在运行的程序文件推出来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarLayout {
    /// 正在运行的程序（解析过软链接）；替换的就是它。
    pub exe: PathBuf,
    /// 程序所在目录；`zssh-mcp` 在这里，临时文件也放这里（保证同一个文件系统，`rename` 才是原子的）。
    pub dir: PathBuf,
    /// `install.sh` 装的布局才有：安装前缀。图标、桌面文件、清单都相对它。
    pub prefix: Option<PathBuf>,
}

impl TarLayout {
    /// 程序在 `<prefix>/lib/zeeterm/` 并且 `<prefix>/share/zeeterm/install-manifest` 在 → `install.sh` 的布局；
    /// 否则当作手动解压的平铺目录。
    pub fn detect(exe: &Path) -> Self {
        let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let prefix = (|| {
            if dir.file_name()? != "zeeterm" {
                return None;
            }
            let lib = dir.parent()?;
            if lib.file_name()? != "lib" {
                return None;
            }
            let prefix = lib.parent()?;
            prefix
                .join("share/zeeterm/install-manifest")
                .is_file()
                .then(|| prefix.to_path_buf())
        })();
        Self {
            exe: exe.to_path_buf(),
            dir,
            prefix,
        }
    }

    /// 重启时启动哪个路径：`install.sh` 的 `<prefix>/bin/zeeterm` 软链接在就用它（和桌面文件里的 Exec 一致），
    /// 否则就是程序文件本身。
    pub fn restart_command(&self) -> PathBuf {
        if let Some(prefix) = &self.prefix {
            let link = prefix.join("bin").join(MAIN_BIN);
            if std::fs::symlink_metadata(&link).is_ok() {
                return link;
            }
        }
        self.exe.clone()
    }
}

/// 替换过程中出错的那一步（界面按步骤出词；具体的系统错误作为参数原样带上）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoStep {
    /// 在程序目录里准备文件（临时副本）。
    Prepare,
    /// 备份旧文件。
    Backup,
    /// 用新文件换掉旧文件。
    Replace,
    /// 读 / 写安装清单、桌面文件。
    Metadata,
}

impl std::fmt::Display for IoStep {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Prepare => "准备新文件",
            Self::Backup => "备份旧文件",
            Self::Replace => "替换程序文件",
            Self::Metadata => "更新安装清单",
        })
    }
}

/// tar.gz 更新失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TarError {
    /// 程序所在目录写不进去：不是故障，改成手动安装。
    NotWritable(String),
    /// `tar` 没能解压（带它的输出末尾）。
    ExtractFailed(String),
    /// 压缩包里有绝对路径、`..`、软链接或设备文件。
    UnsafeEntry(String),
    /// 包里缺必要的文件。
    MissingFile(String),
    /// 必要的文件不是普通文件（或是空的）。
    NotRegular(String),
    /// 新程序的 `--version` 不是公告的版本。
    WrongVersion {
        found: String,
        expected: String,
    },
    /// 新程序没能运行（缺库、架构不对、超时……）。
    ProbeFailed(String),
    Io(IoStep, String),
}

impl TarError {
    /// 是不是"改成手动安装"而不是报错。
    pub fn is_manual(&self) -> bool {
        matches!(self, Self::NotWritable(_))
    }
}

impl std::fmt::Display for TarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotWritable(err) => write!(f, "程序所在目录不可写：{err}"),
            Self::ExtractFailed(err) => write!(f, "解压失败：{err}"),
            Self::UnsafeEntry(name) => write!(f, "压缩包里有不安全的条目：{name}"),
            Self::MissingFile(name) => write!(f, "压缩包里没有 {name}"),
            Self::NotRegular(name) => write!(f, "{name} 不是普通文件，或者是空的"),
            Self::WrongVersion { found, expected } => {
                write!(f, "新程序报告的版本是“{found}”，期望“{expected}”")
            }
            Self::ProbeFailed(err) => write!(f, "新程序没能运行：{err}"),
            Self::Io(IoStep::Replace, err) => {
                write!(f, "替换程序文件失败（已恢复成旧版本）：{err}")
            }
            Self::Io(step, err) => write!(f, "{step}失败：{err}"),
        }
    }
}

impl std::error::Error for TarError {}

/// tar.gz 更新用到的外部能力；测试里换成假实现。
pub struct TarContext<'a> {
    /// 跑 `tar`。
    pub runner: &'a dyn CommandRunner,
    /// 运行新程序的 `--version`，返回它打印的文字。
    pub probe_version: &'a dyn Fn(&Path) -> io::Result<String>,
    /// 把新文件换到位的 `rename`（测试用它模拟中途失败）。
    pub rename: &'a dyn Fn(&Path, &Path) -> io::Result<()>,
}

/// 产品用的 `rename`。
pub fn rename_system(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// 把 tar.gz 里的新版本换到 `layout` 指的位置。成功后旧文件不在、新文件到位；
/// 任何一步失败，已经换过的都回滚，旧版本原样留着。
///
/// 步骤：在程序目录里建临时目录（建不了 = 目录不可写 = 手动安装）→ 查压缩包条目 → 解压 →
/// 检查只有普通文件和目录、必要文件在 → 运行新程序的 `--version`，必须是 `ZeeTerm <version>` →
/// 先在各目标旁边准备好临时副本、备份旧文件 → 依次 `rename` 换上（`install.sh` 布局的图标 / 桌面文件 / 清单在前，
/// `zssh-mcp` 倒数第二，主程序最后）→ 清理。
pub fn apply_tar_update(
    archive: &Path,
    layout: &TarLayout,
    version: &str,
    ctx: &TarContext<'_>,
) -> Result<(), TarError> {
    let stage = unique_name(&layout.dir, STAGE_PREFIX);
    std::fs::create_dir(&stage).map_err(|err| TarError::NotWritable(err.to_string()))?;
    remove_stale_stages(&layout.dir, &stage);
    let _cleanup = RemoveDirOnDrop(stage.clone());

    list_archive_safely(ctx.runner, archive)?;
    extract_archive(ctx.runner, archive, &stage)?;
    check_extracted_tree(&stage)?;

    let new_main = stage.join(MAIN_BIN);
    let new_helper = stage.join(HELPER_BIN);
    for (path, name) in [(&new_main, MAIN_BIN), (&new_helper, HELPER_BIN)] {
        require_regular_file(path, name)?;
        make_executable(path).map_err(|err| TarError::Io(IoStep::Prepare, err.to_string()))?;
    }
    let reported =
        (ctx.probe_version)(&new_main).map_err(|err| TarError::ProbeFailed(err.to_string()))?;
    let expected = format!("{VERSION_BANNER} {}", version.trim());
    if reported.trim() != expected {
        return Err(TarError::WrongVersion {
            found: reported.trim().to_owned(),
            expected,
        });
    }

    let mut entries = Vec::new();
    if let Some(prefix) = &layout.prefix {
        entries.extend(share_entries(&stage, prefix)?);
    }
    entries.push(Entry {
        source: Source::File(new_helper),
        dest: layout.dir.join(HELPER_BIN),
        mode: 0o755,
    });
    entries.push(Entry {
        source: Source::File(new_main),
        dest: layout.exe.clone(),
        mode: 0o755,
    });
    apply_entries(&entries, ctx.rename)
}

/// 产品用的入口：真的跑 `tar`、真的运行新程序。
pub fn apply_tar_update_system(
    archive: &Path,
    layout: &TarLayout,
    version: &str,
) -> Result<(), TarError> {
    apply_tar_update(
        archive,
        layout,
        version,
        &TarContext {
            runner: &SystemRunner,
            probe_version: &probe_version_system,
            rename: &rename_system,
        },
    )
}

/// 临时解压目录的名字前缀（放在程序所在目录里）。
const STAGE_PREFIX: &str = ".zeeterm-update";

/// 上次更新中途被杀（断电、崩溃）留下的临时目录：超过一天的清掉。当前这次的目录和更新鲜的不动。
fn remove_stale_stages(dir: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_stage = entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{STAGE_PREFIX}-"));
        let stale = entry
            .metadata()
            .ok()
            .filter(std::fs::Metadata::is_dir)
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(24 * 60 * 60));
        if is_stage && stale && path != current {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// 在 `dir` 下起一个不会撞车的名字（进程号 + 纳秒）。
fn unique_name(dir: &Path, prefix: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    dir.join(format!("{prefix}-{}-{nanos}", std::process::id()))
}

struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 压缩包里所有条目都在同一个顶层目录下，没有绝对路径和 `..`。
fn list_archive_safely(runner: &dyn CommandRunner, archive: &Path) -> Result<(), TarError> {
    let args = [OsString::from("-tzf"), archive.as_os_str().to_owned()];
    let output = runner
        .run(Path::new("tar"), &args)
        .map_err(|err| TarError::ExtractFailed(err.to_string()))?;
    if output.code != Some(0) {
        return Err(TarError::ExtractFailed(tail_text(&output.stderr)));
    }
    let mut top: Option<String> = None;
    for line in output.stdout.lines().filter(|line| !line.trim().is_empty()) {
        // 每一段都必须是普通名字：绝对路径、`..`、开头的 `./` 都不要（打包脚本不会产生它们）。
        let entry = Path::new(line);
        let mut components = entry.components();
        let first = match components.next() {
            Some(Component::Normal(first)) => first.to_string_lossy().into_owned(),
            _ => return Err(TarError::UnsafeEntry(line.to_owned())),
        };
        if components.any(|component| !matches!(component, Component::Normal(_))) {
            return Err(TarError::UnsafeEntry(line.to_owned()));
        }
        match &top {
            None => top = Some(first),
            Some(expected) if *expected == first => {}
            Some(_) => return Err(TarError::UnsafeEntry(line.to_owned())),
        }
    }
    if top.is_none() {
        return Err(TarError::MissingFile(MAIN_BIN.to_owned()));
    }
    Ok(())
}

fn extract_archive(
    runner: &dyn CommandRunner,
    archive: &Path,
    dest: &Path,
) -> Result<(), TarError> {
    let args = [
        OsString::from("-xzf"),
        archive.as_os_str().to_owned(),
        OsString::from("-C"),
        dest.as_os_str().to_owned(),
        OsString::from("--strip-components=1"),
        OsString::from("--no-same-owner"),
    ];
    let output = runner
        .run(Path::new("tar"), &args)
        .map_err(|err| TarError::ExtractFailed(err.to_string()))?;
    if output.code == Some(0) {
        Ok(())
    } else {
        Err(TarError::ExtractFailed(tail_text(&output.stderr)))
    }
}

/// 解压出来的东西只能是目录和普通文件：软链接、设备文件一律拒绝。
fn check_extracted_tree(root: &Path) -> Result<(), TarError> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|err| TarError::Io(IoStep::Prepare, err.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|err| TarError::Io(IoStep::Prepare, err.to_string()))?;
            let file_type = entry
                .file_type()
                .map_err(|err| TarError::Io(IoStep::Prepare, err.to_string()))?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if !file_type.is_file() {
                return Err(TarError::UnsafeEntry(
                    entry.path().to_string_lossy().into_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn require_regular_file(path: &Path, name: &str) -> Result<(), TarError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(TarError::MissingFile(name.to_owned()));
        }
        Err(err) => return Err(TarError::Io(IoStep::Prepare, err.to_string())),
    };
    if !meta.is_file() || meta.len() == 0 {
        return Err(TarError::NotRegular(name.to_owned()));
    }
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// 运行 `<exe> --version`：清空环境（它在任何初始化之前就打印版本退出）、最多等 10 秒。
/// 刚写好的文件偶尔会撞上 `ETXTBSY`（别的线程正好 fork 时还握着写句柄），短暂重试几次。
pub fn probe_version_system(exe: &Path) -> io::Result<String> {
    /// Linux 的 ETXTBSY。
    const TEXT_FILE_BUSY: i32 = 26;
    let mut attempt = 0;
    loop {
        let spawned = Command::new(exe)
            .arg("--version")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(child) => return wait_for_version(child),
            Err(err) if err.raw_os_error() == Some(TEXT_FILE_BUSY) && attempt < 20 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err),
        }
    }
}

fn wait_for_version(mut child: std::process::Child) -> io::Result<String> {
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "--version timed out",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut text = String::new();
    if let Some(stdout) = child.stdout.take() {
        let _ = stdout.take(4096).read_to_string(&mut text);
    }
    if status.success() {
        Ok(text)
    } else {
        Err(io::Error::other(format!(
            "--version exited with {}",
            status
                .code()
                .map_or_else(|| "a signal".to_owned(), |code| code.to_string())
        )))
    }
}

// ---- 要换的文件清单 ----

#[derive(Debug, Clone)]
enum Source {
    File(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone)]
struct Entry {
    source: Source,
    dest: PathBuf,
    mode: u32,
}

/// `install.sh` 布局里除了程序以外随包的文件：图标、metainfo、桌面文件（保留已装那份的 Exec / TryExec）、
/// 卸载脚本，以及更新后的安装清单（旧清单 + 新增的文件）。
fn share_entries(stage: &Path, prefix: &Path) -> Result<Vec<Entry>, TarError> {
    let mut entries = Vec::new();
    let png_name = format!("{APP_ID}.png");

    let icon_root = stage.join("icons/hicolor");
    if let Ok(sizes) = std::fs::read_dir(&icon_root) {
        let mut sizes: Vec<PathBuf> = sizes.filter_map(Result::ok).map(|e| e.path()).collect();
        sizes.sort();
        for size_dir in sizes {
            let source = size_dir.join("apps").join(&png_name);
            let (Some(size), true) = (size_dir.file_name(), source.is_file()) else {
                continue;
            };
            entries.push(Entry {
                source: Source::File(source),
                dest: prefix
                    .join("share/icons/hicolor")
                    .join(size)
                    .join("apps")
                    .join(&png_name),
                mode: 0o644,
            });
        }
    }

    let metainfo_name = format!("{APP_ID}.metainfo.xml");
    let metainfo = stage.join(&metainfo_name);
    if metainfo.is_file() {
        entries.push(Entry {
            source: Source::File(metainfo),
            dest: prefix.join("share/metainfo").join(&metainfo_name),
            mode: 0o644,
        });
    }

    // 桌面文件：安装时 Exec / TryExec 被改写成了装好的绝对路径，更新时沿用已装那份的这两行，
    // 其余内容（名称、多语言说明、分类……）取新版本的。已装的桌面文件不在（用户删了）就不碰。
    let desktop_name = format!("{APP_ID}.desktop");
    let new_desktop = stage.join(&desktop_name);
    let installed_desktop = prefix.join("share/applications").join(&desktop_name);
    if new_desktop.is_file()
        && let (Ok(template), Ok(installed)) = (
            std::fs::read_to_string(&new_desktop),
            std::fs::read_to_string(&installed_desktop),
        )
        && let Some(merged) = desktop_keeping_exec(&template, &installed)
    {
        entries.push(Entry {
            source: Source::Bytes(merged.into_bytes()),
            dest: installed_desktop,
            mode: 0o644,
        });
    }

    let uninstall = stage.join("uninstall.sh");
    if uninstall.is_file() {
        entries.push(Entry {
            source: Source::File(uninstall),
            dest: prefix.join("share/zeeterm/uninstall.sh"),
            mode: 0o755,
        });
    }

    // 清单：旧的原样保留，新增的文件追加进去，卸载才删得干净。
    let manifest = prefix.join("share/zeeterm/install-manifest");
    let old = std::fs::read_to_string(&manifest)
        .map_err(|err| TarError::Io(IoStep::Metadata, err.to_string()))?;
    let added: Vec<&Path> = entries.iter().map(|entry| entry.dest.as_path()).collect();
    entries.push(Entry {
        source: Source::Bytes(merge_manifest(&old, &added, &manifest).into_bytes()),
        dest: manifest,
        mode: 0o644,
    });
    Ok(entries)
}

/// 新桌面文件模板 + 已装桌面文件里的 `Exec=` / `TryExec=` 两行。已装的没有 `Exec=` 就不合并（返回 `None`）。
fn desktop_keeping_exec(template: &str, installed: &str) -> Option<String> {
    let pick = |key: &str| {
        installed
            .lines()
            .find(|line| line.starts_with(key))
            .map(str::to_owned)
    };
    let exec = pick("Exec=")?;
    let try_exec = pick("TryExec=");
    let mut out = String::new();
    for line in template.lines() {
        if line.starts_with("Exec=") {
            out.push_str(&exec);
        } else if line.starts_with("TryExec=") {
            // 已装的没有 TryExec（更早的包）就不加：没法可靠地从 Exec 的转义里还原路径。
            match &try_exec {
                Some(line) => out.push_str(line),
                None => continue,
            }
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    Some(out)
}

/// 旧清单的每一行原样保留，把 `added` 里没出现过的路径加在清单自己那一行之前；清单自己始终在最后一行。
fn merge_manifest(old: &str, added: &[&Path], manifest: &Path) -> String {
    let manifest_line = manifest.to_string_lossy();
    let mut lines: Vec<String> = old
        .lines()
        .filter(|line| !line.is_empty() && *line != manifest_line)
        .map(str::to_owned)
        .collect();
    for path in added {
        let text = path.to_string_lossy();
        if *text != manifest_line && !lines.iter().any(|line| *line == text) {
            lines.push(text.into_owned());
        }
    }
    lines.push(manifest_line.into_owned());
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// 一个已经换到位的文件，回滚要用。
struct Applied {
    dest: PathBuf,
    /// 换之前的旧文件的备份；没有说明 `dest` 原来不存在，回滚时删掉。
    backup: Option<PathBuf>,
}

/// 事务式替换：先在每个目标旁边准备好新文件的临时副本、备份旧文件（这两步出错时目标一个都没动），
/// 再依次 `rename` 换上；中途 `rename` 失败就按相反顺序恢复。
fn apply_entries(
    entries: &[Entry],
    rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
) -> Result<(), TarError> {
    let tag = format!("{}-{}", std::process::id(), unique_suffix());
    let mut temps: Vec<(PathBuf, PathBuf)> = Vec::new(); // (临时副本, 目标)
    let mut backups: Vec<Option<PathBuf>> = Vec::new();
    let mut cleanup_files: Vec<PathBuf> = Vec::new();

    let result = (|| -> Result<(), TarError> {
        // 1. 临时副本：和目标在同一个目录，所以后面的 rename 一定在同一个文件系统里。
        for entry in entries {
            let name = file_name_of(&entry.dest)?;
            let parent = entry.dest.parent().ok_or_else(|| {
                TarError::Io(
                    IoStep::Prepare,
                    format!("{}: no parent", entry.dest.display()),
                )
            })?;
            std::fs::create_dir_all(parent)
                .map_err(|err| TarError::Io(IoStep::Prepare, err.to_string()))?;
            let temp = parent.join(format!(".{name}.zt-new.{tag}"));
            cleanup_files.push(temp.clone());
            write_temp(&entry.source, &temp, entry.mode)
                .map_err(|err| TarError::Io(IoStep::Prepare, err.to_string()))?;
            temps.push((temp, entry.dest.clone()));
        }
        // 2. 备份旧文件：硬链接最省（同一个 inode，不占空间），不支持就复制。目标必须是普通文件或不存在。
        for entry in entries {
            match std::fs::symlink_metadata(&entry.dest) {
                Ok(meta) if meta.is_file() => {
                    let name = file_name_of(&entry.dest)?;
                    let backup = entry.dest.with_file_name(format!(".{name}.zt-old.{tag}"));
                    cleanup_files.push(backup.clone());
                    if std::fs::hard_link(&entry.dest, &backup).is_err() {
                        std::fs::copy(&entry.dest, &backup)
                            .map_err(|err| TarError::Io(IoStep::Backup, err.to_string()))?;
                    }
                    backups.push(Some(backup));
                }
                Ok(_) => {
                    return Err(TarError::Io(
                        IoStep::Backup,
                        format!("{} is not a regular file", entry.dest.display()),
                    ));
                }
                Err(err) if err.kind() == io::ErrorKind::NotFound => backups.push(None),
                Err(err) => return Err(TarError::Io(IoStep::Backup, err.to_string())),
            }
        }
        Ok(())
    })();
    if let Err(err) = result {
        remove_files(&cleanup_files);
        return Err(err);
    }

    // 3. 换上。
    let mut applied: Vec<Applied> = Vec::new();
    for ((temp, dest), backup) in temps.iter().zip(backups.iter()) {
        match rename(temp, dest) {
            Ok(()) => applied.push(Applied {
                dest: dest.clone(),
                backup: backup.clone(),
            }),
            Err(err) => {
                // 回滚：已经换上的恢复成旧文件（原来没有的删掉）。
                for done in applied.iter().rev() {
                    match &done.backup {
                        Some(backup) => {
                            let _ = std::fs::rename(backup, &done.dest);
                        }
                        None => {
                            let _ = std::fs::remove_file(&done.dest);
                        }
                    }
                }
                remove_files(&cleanup_files);
                return Err(TarError::Io(IoStep::Replace, err.to_string()));
            }
        }
    }
    remove_files(&cleanup_files);
    Ok(())
}

fn unique_suffix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos())
}

fn file_name_of(path: &Path) -> Result<String, TarError> {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| TarError::Io(IoStep::Prepare, format!("{}: no file name", path.display())))
}

/// 清理临时副本和备份（已经 rename 走的、不存在的都忽略）。
fn remove_files(files: &[PathBuf]) {
    for file in files {
        let _ = std::fs::remove_file(file);
    }
}

fn write_temp(source: &Source, temp: &Path, mode: u32) -> io::Result<()> {
    let _ = std::fs::remove_file(temp);
    match source {
        Source::File(path) => {
            std::fs::copy(path, temp)?;
            set_mode(temp, mode)?;
            std::fs::File::open(temp)?.sync_all()
        }
        Source::Bytes(bytes) => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(temp)?;
            file.write_all(bytes)?;
            set_mode(temp, mode)?;
            file.sync_all()
        }
    }
}

// ---------------------------------------------------------------------------
// 重启
// ---------------------------------------------------------------------------

/// 重启命令：`/bin/sh` 等当前进程退出（单实例锁释放）后 `exec` 新程序。
///
/// 新进程不能先于旧进程起来：ZeeTerm 是单实例的，后起的会把请求转给先起的然后退出，等于什么都没发生。
/// 等待最多 15 秒，之后照样启动（旧进程卡死时，新进程至少会把旧窗口叫到前台）。
pub fn restart_command(
    wait_pid: u32,
    program: &Path,
    args: &[OsString],
) -> (PathBuf, Vec<OsString>) {
    let script = "n=0; while kill -0 \"$1\" 2>/dev/null && [ \"$n\" -lt 150 ]; do n=$((n+1)); sleep 0.1; done; shift; exec \"$@\"";
    let mut argv = vec![
        OsString::from("-c"),
        OsString::from(script),
        OsString::from("zeeterm-restart"),
        OsString::from(wait_pid.to_string()),
        program.as_os_str().to_owned(),
    ];
    argv.extend(args.iter().cloned());
    (PathBuf::from("/bin/sh"), argv)
}

/// 起一个脱离当前进程组的子进程，不等它。标准输入输出全接空。
pub fn spawn_detached(program: &Path, args: &[OsString]) -> io::Result<()> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn().map(|_| ())
}

#[cfg(test)]
mod tests;
