//! 更新窗口在 Linux 上的安装流程：本机是哪种安装、后台安装任务、手动安装的说明文字、重启。
//!
//! 实际的选包、`pkexec`、tar.gz 原子替换都在 `zssh_update::linux`，这里只负责把它们接到更新窗口上：
//! 后台线程里跑 [`run`]（命令执行器、tar 更新都从参数传进来，测试里换成假实现，不会弹授权框也不改系统），
//! 结果 [`Outcome`] 经事件回到界面。这个文件在 Windows 上也编译（里面没有 Linux 才有的 API），
//! Windows 的更新流程不经过这里。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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
    Linux(Flavor),
}

impl Platform {
    /// 给 [`zssh_update::Manifest::select_for`] 的平台 key。
    pub(crate) fn keys(&self) -> &'static [&'static str] {
        match self {
            Self::Windows => zssh_update::PLATFORM_KEYS,
            Self::Linux(flavor) => flavor.platform_keys(),
        }
    }
}

/// 本机的更新平台（进程里只判断一次；第一次要问 `dpkg`，所以别在界面线程上第一个调用它）。
pub(crate) fn host_platform() -> Platform {
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
    Platform::Windows
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
        Flavor::Deb => match lx::install_deb(runner, tools, &plan.package) {
            DebResult::Installed => Outcome::Installed {
                restart: PathBuf::from(lx::DEB_COMMAND),
            },
            DebResult::Manual(reason) => Outcome::Manual(reason),
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

    /// 缓存目录里放一个包，返回 (缓存根, 包路径, 内容哈希)。
    fn cached_package(name: &str, file: &str) -> (PathBuf, PathBuf, String) {
        let root = std::env::temp_dir().join(format!(
            "zeeterm-update-linux-glue-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建缓存目录");
        let package = root.join(file);
        std::fs::write(&package, b"package bytes").expect("写包");
        (root, package, zssh_update::sha256_hex(b"package bytes"))
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
    fn a_deb_install_asks_pkexec_once_and_restarts_through_the_usr_bin_command() {
        let (root, package, sha) = cached_package("deb-ok", "ZeeTerm-2.0.0-linux-x86_64.deb");
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
        // apt 拿到的是规范化后的缓存里的那个文件。
        assert_eq!(
            calls[0].last().map(PathBuf::from),
            Some(package.canonicalize().expect("规范化"))
        );
    }

    #[test]
    fn a_declined_or_unavailable_authorization_is_not_an_error() {
        let (root, package, sha) = cached_package("deb-manual", "ZeeTerm-2.0.0-linux-x86_64.deb");
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
        let (root, package, sha) = cached_package("deb-fail", "ZeeTerm-2.0.0-linux-x86_64.deb");
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
        let (root, package, sha) = cached_package("tar", "ZeeTerm-2.0.0-linux-x86_64.tar.gz");
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
