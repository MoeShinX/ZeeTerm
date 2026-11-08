//! 更新窗口在 Linux 上的安装流程：本机是哪种安装、后台安装任务、手动安装的说明文字、重启。
//!
//! 实际的选包、`pkexec`、tar.gz 原子替换都在 `zssh_update::linux`，这里只负责把它们接到更新窗口上：
//! 后台线程里跑 [`run`]（命令执行器、tar 更新都从参数传进来，测试里换成假实现，不会弹授权框也不改系统），
//! 结果 [`Outcome`] 经事件回到界面。这个文件在 Windows 上也编译（里面没有 Linux 才有的 API），
//! Windows 的更新流程不经过这里。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use zssh_update::linux::{
    self as lx, CommandRunner, DebResult, DebTools, Flavor, LinuxInstallRequest, ManualReason,
    SystemRunner, TarError, TarLayout,
};

use super::UpdateMsg;
use crate::i18n::t;

/// 更新窗口认的平台：决定用 `latest.json` 里的哪个条目、怎么安装。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Platform {
    Windows,
    /// Windows 免安装版（程序旁边有 `portable` 标记）：只认清单里的 `windows-x86_64-portable`（zip），原地替换程序文件。
    /// `writable` 是程序所在目录能不能写：不能写（Program Files、只读盘）就不下载、不替换，和 macOS 一样只提醒、引导到发布页。
    WindowsPortable {
        writable: bool,
    },
    Linux(Flavor),
    /// macOS：只提醒有新版本，引导到发布页；清单里没有（也不认）任何 macOS 的包，
    /// 所以永远不会选出安装包、不下载、不验签、不安装。
    MacOs,
}

impl Platform {
    /// 给 [`zssh_update::Manifest::select_for`] 的平台 key。macOS 没有可装的包，key 为空。
    pub(crate) fn keys(&self) -> &'static [&'static str] {
        match self {
            Self::Windows => zssh_update::PLATFORM_KEYS,
            Self::WindowsPortable { .. } => zssh_update::PORTABLE_PLATFORM_KEYS,
            Self::Linux(flavor) => flavor.platform_keys(),
            Self::MacOs => &[],
        }
    }

    /// 这个平台会不会下载并安装包。不会的（macOS、程序目录不可写的免安装版）只读清单里的版本号和说明，
    /// 不看清单里的任何包和网址，也用不着更新公钥。
    pub(crate) fn installs_packages(&self) -> bool {
        !matches!(
            self,
            Self::MacOs | Self::WindowsPortable { writable: false }
        )
    }
}

/// macOS「打开发布页」固定打开的地址：写死的常量，不取清单里的任何网址。
pub(crate) const MACOS_RELEASE_PAGE_URL: &str =
    "https://github.com/MoeShinX/ZeeTerm/releases/latest";

/// 「打开发布页」要打开的地址：macOS 是固定常量，其余平台从内嵌的更新源推出来。
pub(super) fn release_page_url_for(platform: &Platform) -> Option<String> {
    match platform {
        Platform::MacOs => Some(MACOS_RELEASE_PAGE_URL.to_owned()),
        Platform::Windows | Platform::WindowsPortable { .. } | Platform::Linux(_) => {
            release_page_url()
        }
    }
}

/// 本机的更新平台（进程里只判断一次；第一次要问 `dpkg`，所以别在界面线程上第一个调用它）。
pub(crate) fn host_platform() -> Platform {
    // macOS 必须在探测和调试钩子之前分出去：下面的探测在非 Linux 上会退成 Windows，
    // 钩子也能把平台改成 Windows，两者都可能让 macOS 走进 Windows 的下载 / 安装流程。
    if cfg!(target_os = "macos") {
        return Platform::MacOs;
    }
    static HOST: OnceLock<Platform> = OnceLock::new();
    HOST.get_or_init(|| resolve_platform(crate::dev_hooks::var("ZSSH_UPDATE_FLAVOR"), detect_host))
        .clone()
}

/// 调试钩子 `ZSSH_UPDATE_FLAVOR`（`deb` / `tar` / `windows`）优先，其余按本机探测；认不得的值当没设。
fn resolve_platform(hook: Option<String>, detect: impl FnOnce() -> Platform) -> Platform {
    match hook.as_deref().map(str::trim) {
        Some("deb") => Platform::Linux(Flavor::Deb),
        Some("tar") => Platform::Linux(Flavor::Tar),
        Some("windows") => Platform::Windows,
        _ => detect(),
    }
}

/// 探测本机安装方式的线程没跑完（panic）时用的退路：按编译平台给，别在 Linux 上退成 Windows 的包。
///
/// Linux 上退成 tar.gz：它是三种里最保守的——不需要 `pkexec` / `dpkg` / root，只在程序所在目录能写时才自动替换，
/// 写不了就走「手动安装」的说明，不会去动系统包管理器；而 deb 要拉起 root 授权框、直接 `apt-get install`。
pub(crate) fn fallback_platform() -> Platform {
    if cfg!(target_os = "linux") {
        Platform::Linux(Flavor::Tar)
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if crate::portable::is_portable() {
        // 免安装版：目录能不能写没探测成，按"不能写"处理——只提醒、不替换，最保守。
        Platform::WindowsPortable { writable: false }
    } else {
        Platform::Windows
    }
}

#[cfg(target_os = "linux")]
fn detect_host() -> Platform {
    match std::env::current_exe() {
        Ok(exe) => Platform::Linux(lx::detect_flavor(
            &lx::strip_deleted_suffix(exe),
            &SystemRunner,
        )),
        // 读不到自己的路径：按 tar.gz 处理，装不了时还有手动安装的后备。
        Err(_) => Platform::Linux(Flavor::Tar),
    }
}

#[cfg(not(target_os = "linux"))]
fn detect_host() -> Platform {
    detect_windows(
        crate::portable::layout().map(|layout| zssh_update::portable::dir_is_writable(&layout.dir)),
    )
}

/// Windows 上的平台：`portable_writable` 有值就是免安装版（程序旁边有 `portable` 标记），值是程序所在目录能不能写；
/// 没有值是安装版。
#[cfg(any(not(target_os = "linux"), test))]
fn detect_windows(portable_writable: Option<bool>) -> Platform {
    match portable_writable {
        Some(writable) => Platform::WindowsPortable { writable },
        None => Platform::Windows,
    }
}

/// 本进程里已经装好、还没重启的版本。只放内存里，不落盘：重启之后新进程自己的版本号就是它，
/// 没有什么要记；而真正已经换在磁盘上的文件，下次启动本来就是新版本。
///
/// 为什么需要：deb / tar.gz 装好后磁盘上的程序已经是新版本，但正在跑的还是旧的，`current_version()` 不变。
/// 用户点了「稍后重启」再检查更新，会又看到同一个版本，可以再下载、再走一遍授权安装；自动检查也会重复提醒。
#[derive(Debug, Default)]
pub(crate) struct AwaitingRestart {
    installed: Mutex<Option<(String, PathBuf)>>,
}

impl AwaitingRestart {
    pub(crate) const fn new() -> Self {
        Self {
            installed: Mutex::new(None),
        }
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Option<(String, PathBuf)>> {
        self.installed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 记下刚装好的版本和重启时要启动的程序（只留最近一次）。
    pub(crate) fn remember(&self, version: &str, restart: PathBuf) {
        *self.slot() = Some((normalize_version(version).to_owned(), restart));
    }

    /// 这个版本已经装好、只差重启吗？是的话返回重启要启动的程序。
    pub(crate) fn restart_for(&self, version: &str) -> Option<PathBuf> {
        self.slot()
            .as_ref()
            .filter(|(installed, _)| installed == normalize_version(version))
            .map(|(_, restart)| restart.clone())
    }
}

/// 清单里的版本可能带 `v` 前缀、两端有空白。
fn normalize_version(version: &str) -> &str {
    version.trim().trim_start_matches('v')
}

static AWAITING_RESTART: AwaitingRestart = AwaitingRestart::new();

/// 记下本进程里刚装好的版本。
pub(crate) fn remember_installed(version: &str, restart: PathBuf) {
    AWAITING_RESTART.remember(version, restart);
}

/// 清单公告的这个版本，在本进程里是不是已经装好、只差重启？
pub(crate) fn awaiting_restart(version: &str) -> Option<PathBuf> {
    AWAITING_RESTART.restart_for(version)
}

/// 正在运行的程序文件（解析过软链接）。替换之后 `/proc/self/exe` 会带 ` (deleted)`，去掉它。
pub(super) fn running_exe() -> PathBuf {
    std::env::current_exe()
        .map(lx::strip_deleted_suffix)
        .unwrap_or_default()
}

/// 一次安装要用的全部信息（在界面线程上收集好，交给后台线程）。
#[derive(Debug, Clone)]
pub(super) struct Job {
    pub flavor: Flavor,
    pub package: PathBuf,
    pub cache_root: PathBuf,
    pub version: String,
    pub sha256: String,
    /// 正在运行的程序文件；tar.gz 的替换以它所在的目录为准。
    pub exe: PathBuf,
}

/// 安装的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    /// 装好了；`restart` 是重启时要启动的程序。
    Installed { restart: PathBuf },
    /// 不能自动装，包已下载并校验，改成让用户手动装。不是错误。
    Manual(ManualReason),
    /// 失败，已经是可以直接显示的文字。
    Failed(String),
}

/// tar.gz 更新的入口（产品里是 `apply_tar_update_system`，测试里换成假实现）。
pub(super) type TarUpdate<'a> = dyn Fn(&Path, &TarLayout, &str) -> Result<(), TarError> + 'a;

/// 产品用的安装：真的跑 `pkexec` / `tar`。
pub(super) fn run_system(job: &Job) -> Outcome {
    let tools = DebTools::discover(std::env::var_os("PATH").as_deref());
    run(job, &SystemRunner, &tools, &|archive, layout, version| {
        lx::apply_tar_update_system(archive, layout, version)
    })
}

/// 校验计划（包还在缓存里、内容还是验签时的那份）后按安装方式装。
pub(super) fn run(
    job: &Job,
    runner: &dyn CommandRunner,
    tools: &DebTools,
    tar_update: &TarUpdate<'_>,
) -> Outcome {
    let plan = match lx::plan_linux_install(&LinuxInstallRequest {
        package: job.package.clone(),
        cache_root: job.cache_root.clone(),
        version: job.version.clone(),
        sha256: job.sha256.clone(),
        flavor: job.flavor,
    }) {
        Ok(plan) => plan,
        Err(err) => {
            return Outcome::Failed(format!(
                "{}{}",
                t(UpdateMsg::PrepareInstallFailedPrefix),
                crate::update_text::install_error(&err)
            ));
        }
    };
    match job.flavor {
        // 传进去的是验签时算出的哈希（plan.sha256 就是请求里那个值，不是这里重读磁盘算的）：root 侧复制后对复制品核对它。
        Flavor::Deb => match lx::install_deb(runner, tools, &plan.package, &plan.sha256) {
            DebResult::Installed => Outcome::Installed {
                restart: PathBuf::from(lx::DEB_COMMAND),
            },
            DebResult::Manual(reason) => Outcome::Manual(reason),
            DebResult::HashChanged => Outcome::Failed(t(UpdateMsg::DebHashChanged).to_owned()),
            DebResult::WrongPackage => Outcome::Failed(t(UpdateMsg::DebWrongPackage).to_owned()),
            DebResult::NotRegularFile => {
                Outcome::Failed(t(UpdateMsg::DebNotRegularFile).to_owned())
            }
            DebResult::TooLarge => Outcome::Failed(t(UpdateMsg::DebTooLarge).to_owned()),
            DebResult::Failed(output) => {
                Outcome::Failed(format!("{}{output}", t(UpdateMsg::DebInstallFailedPrefix)))
            }
        },
        Flavor::Tar => {
            let layout = TarLayout::detect(&job.exe);
            match tar_update(&plan.package, &layout, &plan.version) {
                Ok(()) => Outcome::Installed {
                    restart: layout.restart_command(),
                },
                Err(err) if err.is_manual() => Outcome::Manual(ManualReason::NotWritable),
                Err(err) => Outcome::Failed(format!(
                    "{}{}",
                    t(UpdateMsg::LinuxInstallFailedPrefix),
                    crate::update_text::tar_error(&err)
                )),
            }
        }
    }
}

/// 手动安装的说明，每项一行：为什么没法自动装、怎么手动装、要运行的命令 / 包的位置。
pub(super) fn manual_lines(flavor: Flavor, reason: ManualReason, package: &Path) -> Vec<String> {
    let why = match reason {
        ManualReason::NoPkexec => UpdateMsg::ManualReasonNoPkexec,
        ManualReason::NotAuthorized => UpdateMsg::ManualReasonNotAuthorized,
        ManualReason::NotWritable => UpdateMsg::ManualReasonNotWritable,
    };
    match flavor {
        Flavor::Deb => vec![
            t(why).to_owned(),
            t(UpdateMsg::ManualDebHowTo).to_owned(),
            lx::manual_deb_command(package),
        ],
        Flavor::Tar => vec![
            t(why).to_owned(),
            t(UpdateMsg::ManualTarHowTo).to_owned(),
            lx::shell_quote(package),
        ],
    }
}

/// 重启：交给一个脱离当前进程的小 shell，等本进程退出（释放单实例锁）后再启动新程序。
/// 成功返回后，调用方要让界面退出。
pub(super) fn restart(program: &Path, args: &[String]) -> Result<(), String> {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    let (shell, argv) = lx::restart_command(std::process::id(), program, &args);
    lx::spawn_detached(&shell, &argv)
        .map_err(|err| format!("{}{err}", t(UpdateMsg::RestartFailedPrefix)))
}

/// 发布页地址（从内嵌的更新源推出来）；"没有 Linux 条目"的提示里用它。
pub(super) fn release_page_url() -> Option<String> {
    let config = zssh_update::embedded_config().ok()?;
    config
        .endpoints
        .iter()
        .find_map(|endpoint| zssh_update::releases_page_url(endpoint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Msg as _;
    use std::cell::RefCell;
    use zssh_update::linux::CommandOutput;

    struct Scripted {
        calls: RefCell<Vec<Vec<OsString>>>,
        code: Option<i32>,
        stderr: &'static str,
    }

    impl CommandRunner for Scripted {
        fn run(&self, _program: &Path, args: &[OsString]) -> std::io::Result<CommandOutput> {
            self.calls.borrow_mut().push(args.to_vec());
            Ok(CommandOutput {
                code: self.code,
                stdout: String::new(),
                stderr: self.stderr.to_owned(),
            })
        }
    }

    fn scripted(code: i32, stderr: &'static str) -> Scripted {
        Scripted {
            calls: RefCell::new(Vec::new()),
            code: Some(code),
            stderr,
        }
    }

    fn tools() -> DebTools {
        DebTools {
            pkexec: Some(PathBuf::from("/usr/bin/pkexec")),
            apt_get: PathBuf::from("/usr/bin/apt-get"),
        }
    }

    /// 缓存目录里放一个包，返回 (目录守卫, 缓存根, 包路径, 内容哈希)。
    /// 目录名随机唯一，守卫要拿到用例结束，drop 时整棵删掉。
    fn cached_package(name: &str, file: &str) -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let guard = tempfile::Builder::new()
            .prefix(&format!("zeeterm-update-linux-glue-{name}-"))
            .tempdir()
            .expect("建缓存目录");
        let root = guard.path().to_path_buf();
        let package = root.join(file);
        std::fs::write(&package, b"package bytes").expect("写包");
        (
            guard,
            root,
            package,
            zssh_update::sha256_hex(b"package bytes"),
        )
    }

    fn job(flavor: Flavor, root: &Path, package: &Path, sha256: &str) -> Job {
        Job {
            flavor,
            package: package.to_path_buf(),
            cache_root: root.to_path_buf(),
            version: "2.0.0".to_owned(),
            sha256: sha256.to_owned(),
            exe: PathBuf::from("/opt/zeeterm/lib/zeeterm/zeeterm"),
        }
    }

    fn never_called(_: &Path, _: &TarLayout, _: &str) -> Result<(), TarError> {
        panic!("tar 更新不该被调用");
    }

    #[test]
    fn the_platform_hook_wins_and_unknown_values_fall_back_to_detection() {
        let detected = || Platform::Linux(Flavor::Tar);
        assert_eq!(
            resolve_platform(Some("deb".into()), detected),
            Platform::Linux(Flavor::Deb)
        );
        assert_eq!(
            resolve_platform(Some(" tar ".into()), || Platform::Windows),
            Platform::Linux(Flavor::Tar)
        );
        assert_eq!(
            resolve_platform(Some("windows".into()), detected),
            Platform::Windows
        );
        assert_eq!(
            resolve_platform(Some("nonsense".into()), detected),
            detected()
        );
        assert_eq!(resolve_platform(None, detected), detected());
    }

    #[test]
    fn each_platform_only_looks_at_its_own_manifest_keys() {
        assert_eq!(Platform::Windows.keys(), zssh_update::PLATFORM_KEYS);
        assert_eq!(
            Platform::Linux(Flavor::Deb).keys(),
            [zssh_update::PLATFORM_LINUX_DEB]
        );
        assert_eq!(
            Platform::Linux(Flavor::Tar).keys(),
            [zssh_update::PLATFORM_LINUX_TAR]
        );
    }

    #[test]
    fn the_portable_version_only_looks_at_its_own_zip_and_only_installs_when_it_can_write() {
        let writable = Platform::WindowsPortable { writable: true };
        let read_only = Platform::WindowsPortable { writable: false };
        // 免安装版只认 zip 条目，绝不去选安装包；安装版也不会选 zip。
        assert_eq!(writable.keys(), [zssh_update::PLATFORM_WINDOWS_PORTABLE]);
        assert_eq!(read_only.keys(), writable.keys());
        assert!(
            !Platform::Windows
                .keys()
                .contains(&zssh_update::PLATFORM_WINDOWS_PORTABLE)
        );
        // 目录不可写：和 macOS 一样只提醒；可写才下载安装。
        assert!(writable.installs_packages());
        assert!(!read_only.installs_packages());
        assert!(!Platform::MacOs.installs_packages());
        assert!(Platform::Windows.installs_packages());
        assert!(Platform::Linux(Flavor::Tar).installs_packages());
        // 发布页地址和安装版一样从内嵌更新源推出来。
        assert_eq!(release_page_url_for(&writable), release_page_url());
        assert_eq!(release_page_url_for(&read_only), release_page_url());
    }

    #[test]
    fn windows_is_portable_exactly_when_the_marker_layout_is_found() {
        assert_eq!(detect_windows(None), Platform::Windows);
        assert_eq!(
            detect_windows(Some(true)),
            Platform::WindowsPortable { writable: true }
        );
        assert_eq!(
            detect_windows(Some(false)),
            Platform::WindowsPortable { writable: false }
        );
        // 测试程序旁边没有 portable 标记：本机探测的结果是安装版 / 其他平台，不会是免安装版。
        assert!(!matches!(host_platform(), Platform::WindowsPortable { .. }));
    }

    #[test]
    fn a_failed_platform_probe_falls_back_to_the_build_platform_not_to_windows() {
        let fallback = fallback_platform();
        if cfg!(target_os = "linux") {
            // 最保守的一种：不拉起 root 授权框，写不了就改手动安装。
            assert_eq!(fallback, Platform::Linux(Flavor::Tar));
            assert_eq!(fallback.keys(), [zssh_update::PLATFORM_LINUX_TAR]);
        } else if cfg!(target_os = "macos") {
            // macOS 只提醒、不安装：退回去也绝不能变成 Windows（那会去下 Windows 安装包）。
            assert_eq!(fallback, Platform::MacOs);
            assert!(fallback.keys().is_empty());
        } else {
            assert_eq!(fallback, Platform::Windows);
        }
    }

    #[test]
    fn an_installed_version_is_remembered_in_memory_until_the_restart() {
        let awaiting = AwaitingRestart::new();
        assert_eq!(awaiting.restart_for("2.0.0"), None, "什么都没装过");
        awaiting.remember("2.0.0", PathBuf::from("/usr/bin/zeeterm"));
        assert_eq!(
            awaiting.restart_for("2.0.0"),
            Some(PathBuf::from("/usr/bin/zeeterm"))
        );
        // 清单里的写法带 v、带空白也认。
        assert!(awaiting.restart_for(" v2.0.0 ").is_some());
        // 别的版本（更新的、更旧的）照常当新版本处理。
        assert_eq!(awaiting.restart_for("2.0.1"), None);
        assert_eq!(awaiting.restart_for("1.9.0"), None);
        // 又装了更新的：只记最近一次。
        awaiting.remember("v2.0.1", PathBuf::from("/usr/local/lib/zeeterm/zeeterm"));
        assert_eq!(awaiting.restart_for("2.0.0"), None);
        assert_eq!(
            awaiting.restart_for("2.0.1"),
            Some(PathBuf::from("/usr/local/lib/zeeterm/zeeterm"))
        );
    }

    #[test]
    fn a_deb_install_asks_pkexec_once_and_restarts_through_the_usr_bin_command() {
        let (_root_guard, root, package, sha) =
            cached_package("deb-ok", "ZeeTerm-2.0.0-linux-x86_64.deb");
        let runner = scripted(0, "");
        let outcome = run(
            &job(Flavor::Deb, &root, &package, &sha),
            &runner,
            &tools(),
            &never_called,
        );
        assert_eq!(
            outcome,
            Outcome::Installed {
                restart: PathBuf::from("/usr/bin/zeeterm")
            }
        );
        let calls = runner.calls.borrow();
        assert_eq!(calls.len(), 1);
        // 位置参数：缓存里（规范化后）的 deb、验签时的哈希（就是 job.sha256，不是重新读磁盘算的）、固定的 apt-get。
        assert_eq!(
            calls[0][4..],
            [
                OsString::from(package.canonicalize().expect("规范化")),
                OsString::from(sha.as_str()),
                OsString::from("/usr/bin/apt-get"),
                OsString::from("536870912"),
            ]
        );
    }

    #[test]
    fn a_failed_root_side_check_is_a_clear_error_not_a_missing_authorization() {
        let (_root_guard, root, package, sha) =
            cached_package("deb-root-check", "ZeeTerm-2.0.0-linux-x86_64.deb");
        for (code, wording) in [
            (83, "验证之后被改动"),
            (84, "不是 ZeeTerm 的安装包"),
            (85, "不是普通文件"),
            (86, "太大"),
        ] {
            match run(
                &job(Flavor::Deb, &root, &package, &sha),
                &scripted(code, ""),
                &tools(),
                &never_called,
            ) {
                Outcome::Failed(text) => {
                    assert!(text.contains(wording), "{code}：{text}");
                    assert!(text.contains("没有安装任何东西"), "{text}");
                }
                other => panic!("{code} 应是明确的错误：{other:?}"),
            }
        }
        // 英文也有（i18n 两份）。
        assert!(
            UpdateMsg::DebHashChanged
                .en()
                .contains("modified after it was verified")
        );
        assert!(
            UpdateMsg::DebWrongPackage
                .en()
                .contains("not a ZeeTerm package")
        );
        assert!(
            UpdateMsg::DebNotRegularFile
                .en()
                .contains("not a regular file")
        );
        assert!(UpdateMsg::DebTooLarge.en().contains("too large"));
    }

    #[test]
    fn a_declined_or_unavailable_authorization_is_not_an_error() {
        let (_root_guard, root, package, sha) =
            cached_package("deb-manual", "ZeeTerm-2.0.0-linux-x86_64.deb");
        let declined = run(
            &job(Flavor::Deb, &root, &package, &sha),
            &scripted(126, ""),
            &tools(),
            &never_called,
        );
        assert_eq!(declined, Outcome::Manual(ManualReason::NotAuthorized));
        let no_pkexec = DebTools {
            pkexec: None,
            ..tools()
        };
        let runner = scripted(0, "");
        let outcome = run(
            &job(Flavor::Deb, &root, &package, &sha),
            &runner,
            &no_pkexec,
            &never_called,
        );
        assert_eq!(outcome, Outcome::Manual(ManualReason::NoPkexec));
        assert!(runner.calls.borrow().is_empty());
    }

    #[test]
    fn an_apt_failure_shows_its_output_and_a_changed_package_is_never_installed() {
        let (_root_guard, root, package, sha) =
            cached_package("deb-fail", "ZeeTerm-2.0.0-linux-x86_64.deb");
        match run(
            &job(Flavor::Deb, &root, &package, &sha),
            &scripted(100, "E: dpkg was interrupted\n"),
            &tools(),
            &never_called,
        ) {
            Outcome::Failed(text) => assert!(text.contains("dpkg was interrupted"), "{text}"),
            other => panic!("应是失败：{other:?}"),
        }
        // 验签之后包被换掉：计划这一步就拒绝，pkexec 一次都不会被叫起来。
        std::fs::write(&package, b"swapped").expect("换包");
        let runner = scripted(0, "");
        let outcome = run(
            &job(Flavor::Deb, &root, &package, &sha),
            &runner,
            &tools(),
            &never_called,
        );
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
        assert!(runner.calls.borrow().is_empty(), "包不对就不能起 pkexec");
    }

    #[test]
    fn a_tar_update_reports_success_manual_fallback_or_failure() {
        let (_root_guard, root, package, sha) =
            cached_package("tar", "ZeeTerm-2.0.0-linux-x86_64.tar.gz");
        let tar_job = job(Flavor::Tar, &root, &package, &sha);
        let runner = scripted(0, "");

        let seen = RefCell::new(None);
        let ok = run(&tar_job, &runner, &tools(), &|archive, layout, version| {
            *seen.borrow_mut() = Some((
                archive.to_path_buf(),
                layout.dir.clone(),
                version.to_owned(),
            ));
            Ok(())
        });
        let (archive, dir, version) = seen.into_inner().expect("调用了 tar 更新");
        assert_eq!(archive, package.canonicalize().expect("规范化"));
        assert_eq!(dir, Path::new("/opt/zeeterm/lib/zeeterm"));
        assert_eq!(version, "2.0.0");
        assert_eq!(
            ok,
            Outcome::Installed {
                restart: PathBuf::from("/opt/zeeterm/lib/zeeterm/zeeterm")
            }
        );

        let manual = run(&tar_job, &runner, &tools(), &|_, _, _| {
            Err(TarError::NotWritable("read-only".into()))
        });
        assert_eq!(manual, Outcome::Manual(ManualReason::NotWritable));

        match run(&tar_job, &runner, &tools(), &|_, _, _| {
            Err(TarError::MissingFile("zssh-mcp".into()))
        }) {
            Outcome::Failed(text) => assert!(text.contains("zssh-mcp"), "{text}"),
            other => panic!("应是失败：{other:?}"),
        }
        assert!(runner.calls.borrow().is_empty(), "tar.gz 路径不碰 pkexec");
    }

    #[test]
    fn the_manual_instructions_name_the_reason_and_the_exact_command_or_file() {
        let deb = Path::new("/cache/a b/ZeeTerm-2.0.0-linux-x86_64.deb");
        let lines = manual_lines(Flavor::Deb, ManualReason::NoPkexec, deb);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("pkexec"), "{lines:?}");
        assert_eq!(
            lines[2],
            "sudo apt install '/cache/a b/ZeeTerm-2.0.0-linux-x86_64.deb'"
        );
        let tar = Path::new("/cache/ZeeTerm-2.0.0-linux-x86_64.tar.gz");
        let lines = manual_lines(Flavor::Tar, ManualReason::NotWritable, tar);
        assert!(lines[1].contains("install.sh"), "{lines:?}");
        assert_eq!(lines[2], "/cache/ZeeTerm-2.0.0-linux-x86_64.tar.gz");
    }
}
