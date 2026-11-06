use super::{
    CommandOutput, CommandRunner, DEB_COMMAND, DebResult, DebTools, Flavor, LinuxInstallRequest,
    ManualReason, TarError, TarLayout, desktop_keeping_exec, detect_flavor, install_deb,
    list_archive_safely, manual_deb_command, merge_manifest, plan_linux_install, restart_command,
    shell_quote, strip_deleted_suffix,
};
use crate::install::InstallError;
use crate::{PLATFORM_LINUX_DEB, PLATFORM_LINUX_TAR, parse_manifest, sha256_hex};
use std::cell::RefCell;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

type Reply = Box<dyn Fn(&Path, &[OsString]) -> io::Result<CommandOutput>>;

/// 记录每次调用、按预设回答的假命令执行器。
struct FakeRunner {
    calls: RefCell<Vec<(PathBuf, Vec<OsString>)>>,
    reply: Reply,
}

impl FakeRunner {
    fn new(reply: impl Fn(&Path, &[OsString]) -> io::Result<CommandOutput> + 'static) -> Self {
        Self {
            calls: RefCell::new(Vec::new()),
            reply: Box::new(reply),
        }
    }

    fn exits(code: i32) -> Self {
        Self::new(move |_, _| {
            Ok(CommandOutput {
                code: Some(code),
                ..CommandOutput::default()
            })
        })
    }

    fn call_count(&self) -> usize {
        self.calls.borrow().len()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, program: &Path, args: &[OsString]) -> io::Result<CommandOutput> {
        self.calls
            .borrow_mut()
            .push((program.to_path_buf(), args.to_vec()));
        (self.reply)(program, args)
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zssh-update-linux-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

// ---- 选包 ----

#[test]
fn flavors_map_to_their_own_manifest_keys_and_back() {
    assert_eq!(Flavor::Deb.platform_key(), PLATFORM_LINUX_DEB);
    assert_eq!(Flavor::Tar.platform_key(), PLATFORM_LINUX_TAR);
    assert_eq!(Flavor::Deb.platform_keys(), [PLATFORM_LINUX_DEB]);
    assert_eq!(Flavor::Tar.platform_keys(), [PLATFORM_LINUX_TAR]);
    assert_eq!(
        Flavor::from_platform_key("linux-x86_64-deb"),
        Some(Flavor::Deb)
    );
    assert_eq!(
        Flavor::from_platform_key("linux-x86_64-tar"),
        Some(Flavor::Tar)
    );
    assert_eq!(Flavor::from_platform_key("windows-x86_64"), None);
    assert_eq!(Flavor::from_platform_key("windows-x86_64-nsis"), None);
}

fn manifest_with(platforms: &[(&str, &str)]) -> String {
    let entries: Vec<String> = platforms
        .iter()
        .map(|(key, url)| format!("\"{key}\":{{\"url\":\"{url}\",\"signature\":\"c2ln\"}}"))
        .collect();
    format!(
        "{{\"version\":\"2.0.0\",\"notes\":\"n\",\"pub_date\":\"2026-11-06T00:00:00Z\",\"platforms\":{{{}}}}}",
        entries.join(",")
    )
}

#[test]
fn a_deb_install_only_ever_picks_the_deb_entry_and_a_tar_install_the_tar_entry() {
    let manifest = parse_manifest(&manifest_with(&[
        ("windows-x86_64", "https://x.invalid/a.exe"),
        (PLATFORM_LINUX_DEB, "https://x.invalid/a.deb"),
        (PLATFORM_LINUX_TAR, "https://x.invalid/a.tar.gz"),
    ]))
    .expect("清单");
    let deb = manifest
        .select_for("1.0.0", Flavor::Deb.platform_keys())
        .expect("选包")
        .expect("有新版本");
    assert_eq!(deb.platform_key, PLATFORM_LINUX_DEB);
    assert_eq!(deb.url, "https://x.invalid/a.deb");
    let tar = manifest
        .select_for("1.0.0", Flavor::Tar.platform_keys())
        .expect("选包")
        .expect("有新版本");
    assert_eq!(tar.platform_key, PLATFORM_LINUX_TAR);
    // Windows 的选择和以前完全一样，不受 Linux 条目影响。
    let windows = manifest.select("1.0.0").expect("选包").expect("有新版本");
    assert_eq!(windows.platform_key, "windows-x86_64");
    // 已经是最新：什么都不选。
    assert!(
        manifest
            .select_for("2.0.0", Flavor::Tar.platform_keys())
            .expect("选包")
            .is_none()
    );
}

#[test]
fn a_manifest_without_a_matching_linux_entry_says_there_is_no_platform_package() {
    // 老版本发布的清单只有 Windows 条目。
    let only_windows = parse_manifest(&manifest_with(&[(
        "windows-x86_64",
        "https://x.invalid/a.exe",
    )]))
    .expect("清单");
    for flavor in [Flavor::Deb, Flavor::Tar] {
        let err = only_windows
            .select_for("1.0.0", flavor.platform_keys())
            .expect_err("没有 Linux 条目");
        assert!(matches!(err, crate::UpdateError::NoPlatform), "{err}");
    }
    // 只有 tar 条目时，deb 安装不去拿 tar（装法不同）。
    let only_tar = parse_manifest(&manifest_with(&[(
        PLATFORM_LINUX_TAR,
        "https://x.invalid/a.tar.gz",
    )]))
    .expect("清单");
    assert!(matches!(
        only_tar.select_for("1.0.0", Flavor::Deb.platform_keys()),
        Err(crate::UpdateError::NoPlatform)
    ));
}

fn owned_by(package: &str, path: &str) -> FakeRunner {
    let line = format!("{package}: {path}\n");
    FakeRunner::new(move |_, _| {
        Ok(CommandOutput {
            code: Some(0),
            stdout: line.clone(),
            ..CommandOutput::default()
        })
    })
}

#[test]
fn only_a_zeeterm_file_under_usr_that_dpkg_knows_counts_as_a_deb_install() {
    let exe = Path::new("/usr/lib/zeeterm/zeeterm");
    let runner = owned_by("zeeterm", "/usr/lib/zeeterm/zeeterm");
    assert_eq!(detect_flavor(exe, &runner), Flavor::Deb);
    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Path::new("dpkg"));
    assert_eq!(
        calls[0].1,
        [OsString::from("-S"), exe.as_os_str().to_owned()]
    );
}

#[test]
fn anything_dpkg_does_not_vouch_for_is_treated_as_a_tar_install() {
    let usr = Path::new("/usr/lib/zeeterm/zeeterm");
    // 不在 /usr 下：连 dpkg 都不问。
    for home in [
        "/home/u/.local/lib/zeeterm/zeeterm",
        "/opt/zeeterm/zeeterm",
        "/usrx/lib/zeeterm/zeeterm",
    ] {
        let runner = owned_by("zeeterm", home);
        assert_eq!(
            detect_flavor(Path::new(home), &runner),
            Flavor::Tar,
            "{home}"
        );
        assert_eq!(runner.call_count(), 0, "{home}");
    }
    // /usr 下但 dpkg 不认识（退出码 1）。
    assert_eq!(detect_flavor(usr, &FakeRunner::exits(1)), Flavor::Tar);
    // 没有 dpkg。
    let missing = FakeRunner::new(|_, _| Err(io::Error::from(io::ErrorKind::NotFound)));
    assert_eq!(detect_flavor(usr, &missing), Flavor::Tar);
    // 属于别的包。
    assert_eq!(
        detect_flavor(usr, &owned_by("something-else", "/usr/lib/zeeterm/zeeterm")),
        Flavor::Tar
    );
    // 路径对不上（dpkg 的模糊匹配给了别的文件）。
    assert_eq!(
        detect_flavor(usr, &owned_by("zeeterm", "/usr/lib/zeeterm/zeeterm-old")),
        Flavor::Tar
    );
    // 多个包共享时，其中有 zeeterm 就行。
    assert_eq!(
        detect_flavor(
            usr,
            &owned_by("zeeterm, zeeterm-extras", "/usr/lib/zeeterm/zeeterm")
        ),
        Flavor::Deb
    );
}

#[test]
fn the_deleted_suffix_proc_adds_after_replacement_is_removed() {
    assert_eq!(
        strip_deleted_suffix(PathBuf::from("/opt/x/zeeterm (deleted)")),
        PathBuf::from("/opt/x/zeeterm")
    );
    assert_eq!(
        strip_deleted_suffix(PathBuf::from("/opt/x/zeeterm")),
        PathBuf::from("/opt/x/zeeterm")
    );
}

// ---- 安装计划 ----

#[test]
fn the_plan_accepts_only_a_matching_package_inside_the_cache_with_the_verified_content() {
    let cache = temp_dir("plan");
    let deb = cache.join("ZeeTerm-2.0.0-linux-x86_64.deb");
    std::fs::write(&deb, b"deb bytes").expect("写包");
    let request = |package: &Path, flavor: Flavor, sha256: String| LinuxInstallRequest {
        package: package.to_path_buf(),
        cache_root: cache.clone(),
        version: "2.0.0".to_owned(),
        sha256,
        flavor,
    };
    let good = sha256_hex(b"deb bytes");

    let plan = plan_linux_install(&request(&deb, Flavor::Deb, good.to_uppercase())).expect("计划");
    assert_eq!(plan.sha256, good);
    assert_eq!(plan.flavor, Flavor::Deb);
    assert!(plan.package.ends_with("ZeeTerm-2.0.0-linux-x86_64.deb"));

    // 格式不对：deb 文件当 tar.gz 装。
    assert_eq!(
        plan_linux_install(&request(&deb, Flavor::Tar, good.clone())),
        Err(InstallError::NotLinuxPackage)
    );
    // 内容被换过。
    assert_eq!(
        plan_linux_install(&request(&deb, Flavor::Deb, sha256_hex(b"other"))),
        Err(InstallError::HashMismatch)
    );
    // 不存在 / 不在缓存里。
    assert_eq!(
        plan_linux_install(&request(
            &cache.join("missing.deb"),
            Flavor::Deb,
            good.clone()
        )),
        Err(InstallError::MissingInstaller)
    );
    let elsewhere = temp_dir("plan-elsewhere");
    let outside = elsewhere.join("ZeeTerm-2.0.0-linux-x86_64.deb");
    std::fs::write(&outside, b"deb bytes").expect("写包");
    assert_eq!(
        plan_linux_install(&request(&outside, Flavor::Deb, good)),
        Err(InstallError::OutsideCache)
    );
}

// ---- deb ----

fn tools() -> DebTools {
    DebTools {
        pkexec: Some(PathBuf::from("/usr/bin/pkexec")),
        apt_get: PathBuf::from("/usr/bin/apt-get"),
    }
}

#[test]
fn the_deb_is_installed_through_pkexec_apt_get_with_an_absolute_path() {
    let runner = FakeRunner::exits(0);
    let deb =
        Path::new("/home/u/.local/share/ZeeTerm/update-cache/ab/ZeeTerm-2.0.0-linux-x86_64.deb");
    assert_eq!(install_deb(&runner, &tools(), deb), DebResult::Installed);
    let calls = runner.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, Path::new("/usr/bin/pkexec"));
    assert_eq!(
        calls[0].1,
        [
            OsString::from("/usr/bin/apt-get"),
            OsString::from("install"),
            OsString::from("-y"),
            deb.as_os_str().to_owned()
        ]
    );
}

#[test]
fn a_missing_pkexec_or_a_declined_prompt_falls_back_to_manual_install_not_an_error() {
    let deb = Path::new("/c/ZeeTerm.deb");
    let no_tool = DebTools {
        pkexec: None,
        ..tools()
    };
    let runner = FakeRunner::exits(0);
    assert_eq!(
        install_deb(&runner, &no_tool, deb),
        DebResult::Manual(ManualReason::NoPkexec)
    );
    assert_eq!(runner.call_count(), 0, "没有 pkexec 时什么都不运行");
    // 126 = 关掉了授权框 / 没有授权；127 = 没法执行（没有授权代理）。
    for code in [126, 127] {
        assert_eq!(
            install_deb(&FakeRunner::exits(code), &tools(), deb),
            DebResult::Manual(ManualReason::NotAuthorized),
            "{code}"
        );
    }
    // 路径上找到的 pkexec 运行时消失了。
    let vanished = FakeRunner::new(|_, _| Err(io::Error::from(io::ErrorKind::NotFound)));
    assert_eq!(
        install_deb(&vanished, &tools(), deb),
        DebResult::Manual(ManualReason::NoPkexec)
    );
}

#[test]
fn an_apt_failure_is_an_error_with_the_tail_of_its_output() {
    let runner = FakeRunner::new(|_, _| {
        Ok(CommandOutput {
            code: Some(100),
            stdout: "reading package lists\n".to_owned(),
            stderr:
                "E: Unable to acquire the dpkg frontend lock\nE: Is another process using it?\n"
                    .to_owned(),
        })
    });
    match install_deb(&runner, &tools(), Path::new("/c/x.deb")) {
        DebResult::Failed(text) => {
            assert!(text.contains("dpkg frontend lock"), "{text}");
            assert!(
                !text.contains("reading package lists"),
                "有 stderr 时只显示 stderr"
            );
        }
        other => panic!("应是失败：{other:?}"),
    }
    // 没有任何输出时给退出码。
    match install_deb(&FakeRunner::exits(100), &tools(), Path::new("/c/x.deb")) {
        DebResult::Failed(text) => assert!(text.contains("100"), "{text}"),
        other => panic!("应是失败：{other:?}"),
    }
}

#[test]
fn the_manual_command_quotes_paths_for_the_shell() {
    let plain =
        Path::new("/home/u/.local/share/ZeeTerm/update-cache/ab12/ZeeTerm-2.0.0-linux-x86_64.deb");
    assert_eq!(
        manual_deb_command(plain),
        format!("sudo apt install {}", plain.display())
    );
    assert_eq!(shell_quote(Path::new("/tmp/a b/c.deb")), "'/tmp/a b/c.deb'");
    assert_eq!(
        shell_quote(Path::new("/tmp/it's.deb")),
        "'/tmp/it'\\''s.deb'"
    );
    assert_eq!(shell_quote(Path::new("/tmp/$x;y.deb")), "'/tmp/$x;y.deb'");
}

// ---- 重启 ----

#[test]
fn the_restart_waits_for_the_old_process_then_execs_the_new_program_with_the_same_arguments() {
    let (program, args) = restart_command(
        4242,
        Path::new(DEB_COMMAND),
        &[OsString::from("--flag"), OsString::from("a b")],
    );
    assert_eq!(program, Path::new("/bin/sh"));
    assert_eq!(args[0], "-c");
    let script = args[1].to_string_lossy();
    assert!(script.contains("kill -0 \"$1\""), "{script}");
    assert!(script.contains("exec \"$@\""), "{script}");
    // 参数原样排在 $0=名字、$1=pid 之后，不经 shell 重新解析（带空格的参数不会被拆开）。
    assert_eq!(
        args[2..],
        [
            OsString::from("zeeterm-restart"),
            OsString::from("4242"),
            OsString::from("/usr/bin/zeeterm"),
            OsString::from("--flag"),
            OsString::from("a b")
        ]
    );
}

// ---- 桌面文件和清单 ----

#[test]
fn an_updated_desktop_file_keeps_the_exec_lines_the_installer_wrote() {
    let template = "[Desktop Entry]\nName=ZeeTerm\nComment=new text\nTryExec=/usr/bin/zeeterm\nExec=/usr/bin/zeeterm\nIcon=x\n";
    let installed = "[Desktop Entry]\nName=ZeeTerm\nComment=old\nTryExec=/home/u/.local/bin/zeeterm\nExec=\"/home/u/my apps/bin/zeeterm\"\nIcon=x\n";
    let merged = desktop_keeping_exec(template, installed).expect("合并");
    assert_eq!(
        merged,
        "[Desktop Entry]\nName=ZeeTerm\nComment=new text\nTryExec=/home/u/.local/bin/zeeterm\nExec=\"/home/u/my apps/bin/zeeterm\"\nIcon=x\n"
    );
    // 更早的包没有 TryExec：不凭空加一个。
    let without_try = "[Desktop Entry]\nExec=/home/u/.local/bin/zeeterm\n";
    let merged = desktop_keeping_exec(template, without_try).expect("合并");
    assert!(!merged.contains("TryExec"), "{merged}");
    assert!(merged.contains("Exec=/home/u/.local/bin/zeeterm\n"));
    // 已装的没有 Exec：不动它。
    assert_eq!(
        desktop_keeping_exec(template, "[Desktop Entry]\nName=x\n"),
        None
    );
}

#[test]
fn the_manifest_keeps_old_lines_adds_new_files_once_and_ends_with_itself() {
    let manifest = Path::new("/p/share/zeeterm/install-manifest");
    let old = "/p/lib/zeeterm/zeeterm\n/p/lib/zeeterm/zssh-mcp\n/p/share/icons/a.png\n/p/share/zeeterm/install-manifest\n";
    let merged = merge_manifest(
        old,
        &[
            Path::new("/p/share/icons/a.png"),
            Path::new("/p/share/icons/b.png"),
            manifest,
        ],
        manifest,
    );
    assert_eq!(
        merged,
        "/p/lib/zeeterm/zeeterm\n/p/lib/zeeterm/zssh-mcp\n/p/share/icons/a.png\n/p/share/icons/b.png\n/p/share/zeeterm/install-manifest\n"
    );
}

// ---- 压缩包条目检查（假 tar） ----

fn listing(entries: &str) -> FakeRunner {
    let text = entries.to_owned();
    FakeRunner::new(move |_, _| {
        Ok(CommandOutput {
            code: Some(0),
            stdout: text.clone(),
            ..CommandOutput::default()
        })
    })
}

#[test]
fn archive_entries_must_share_one_top_directory_and_stay_inside_it() {
    let archive = Path::new("/c/a.tar.gz");
    let ok = listing(
        "ZeeTerm-2.0.0-linux-x86_64/\nZeeTerm-2.0.0-linux-x86_64/zeeterm\nZeeTerm-2.0.0-linux-x86_64/icons/hicolor/16x16/apps/a.png\n",
    );
    assert_eq!(list_archive_safely(&ok, archive), Ok(()));
    for (bad, why) in [
        ("/etc/passwd\n", "绝对路径"),
        ("top/../../etc/passwd\n", "上跳"),
        ("../top/zeeterm\n", "开头上跳"),
        ("./top/zeeterm\n", "开头 ./"),
        ("top/zeeterm\nother/zssh-mcp\n", "两个顶层目录"),
    ] {
        assert!(
            matches!(
                list_archive_safely(&listing(bad), archive),
                Err(TarError::UnsafeEntry(_))
            ),
            "{why}：{bad}"
        );
    }
    assert!(matches!(
        list_archive_safely(&listing(""), archive),
        Err(TarError::MissingFile(_))
    ));
    // tar 自己失败（不是 gzip、文件坏了……）。
    let broken = FakeRunner::new(|_, _| {
        Ok(CommandOutput {
            code: Some(2),
            stderr: "tar: This does not look like a tar archive\n".to_owned(),
            ..CommandOutput::default()
        })
    });
    assert!(matches!(
        list_archive_safely(&broken, archive),
        Err(TarError::ExtractFailed(text)) if text.contains("does not look like")
    ));
}

// ---- 布局识别 ----

#[test]
fn only_a_lib_zeeterm_directory_with_an_install_manifest_is_the_installer_layout() {
    let root = temp_dir("layout");
    // install.sh 的布局。
    let prefix = root.join("prefix with space");
    let lib = prefix.join("lib/zeeterm");
    std::fs::create_dir_all(&lib).expect("lib");
    std::fs::create_dir_all(prefix.join("share/zeeterm")).expect("share");
    std::fs::write(prefix.join("share/zeeterm/install-manifest"), "x\n").expect("清单");
    let layout = TarLayout::detect(&lib.join("zeeterm"));
    assert_eq!(layout.prefix.as_deref(), Some(prefix.as_path()));
    assert_eq!(layout.dir, lib);
    // 没有清单：当平铺目录（比如有人自己建了 lib/zeeterm）。
    let bare = root.join("bare/lib/zeeterm");
    std::fs::create_dir_all(&bare).expect("bare");
    assert_eq!(TarLayout::detect(&bare.join("zeeterm")).prefix, None);
    // 手动解压的平铺目录。
    let flat = root.join("ZeeTerm-1.0.0-linux-x86_64");
    std::fs::create_dir_all(&flat).expect("flat");
    let layout = TarLayout::detect(&flat.join("zeeterm"));
    assert_eq!(layout.prefix, None);
    assert_eq!(layout.dir, flat);
    // 没有软链接时重启路径就是程序本身。
    assert_eq!(layout.restart_command(), flat.join("zeeterm"));
}

// ---- 真的解压、替换（需要 unix：shell 脚本当程序、系统 tar） ----

#[cfg(unix)]
mod on_disk {
    use super::*;
    use crate::linux::{
        APP_ID, IoStep, TarContext, apply_tar_update, apply_tar_update_system,
        probe_version_system, rename_system,
    };
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    const OLD_VERSION: &str = "1.0.0";
    const NEW_VERSION: &str = "2.0.0";

    fn script(version: &str) -> String {
        format!("#!/bin/sh\necho 'ZeeTerm {version}'\n")
    }

    fn write_exec(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("父目录")).expect("建目录");
        std::fs::write(path, text).expect("写文件");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("权限");
    }

    /// 在 `root/ZeeTerm-<版本>-linux-x86_64/` 里摆出一个 tar.gz 包的内容（和打包脚本同样的文件）。
    fn package_tree(root: &Path, version: &str) -> PathBuf {
        let top = root.join(format!("ZeeTerm-{version}-linux-x86_64"));
        write_exec(&top.join("zeeterm"), &script(version));
        write_exec(
            &top.join("zssh-mcp"),
            &format!("#!/bin/sh\necho 'mcp {version}'\n"),
        );
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../installer/linux");
        for name in ["install.sh", "uninstall.sh"] {
            write_exec(
                &top.join(name),
                &std::fs::read_to_string(repo.join("tar").join(name)).expect("读安装脚本"),
            );
        }
        let desktop = std::fs::read_to_string(repo.join(format!("{APP_ID}.desktop")))
            .expect("读桌面文件")
            .replace(
                "Name=ZeeTerm",
                &format!("Name=ZeeTerm\nX-Test-Version={version}"),
            );
        std::fs::write(top.join(format!("{APP_ID}.desktop")), desktop).expect("写桌面文件");
        std::fs::write(
            top.join(format!("{APP_ID}.metainfo.xml")),
            format!("<c>{version}</c>"),
        )
        .expect("metainfo");
        for size in ["16x16", "256x256"] {
            let icon = top.join(format!("icons/hicolor/{size}/apps/{APP_ID}.png"));
            std::fs::create_dir_all(icon.parent().expect("父目录")).expect("建目录");
            std::fs::write(icon, format!("png {size} {version}")).expect("图标");
        }
        top
    }

    fn make_tarball(root: &Path, top: &Path) -> PathBuf {
        let out = root.join(format!(
            "{}.tar.gz",
            top.file_name().expect("名字").to_string_lossy()
        ));
        let status = Command::new("tar")
            .arg("-czf")
            .arg(&out)
            .arg("-C")
            .arg(top.parent().expect("父目录"))
            .arg(top.file_name().expect("名字"))
            .status()
            .expect("需要系统 tar");
        assert!(status.success());
        out
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
    }

    /// 目录里有没有残留的更新临时文件 / 临时目录。
    fn leftovers(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in std::fs::read_dir(&current).expect("读目录").flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.contains(".zt-new.")
                    || name.contains(".zt-old.")
                    || name.starts_with(".zeeterm-update")
                {
                    found.push(entry.path().to_string_lossy().into_owned());
                }
                if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
        found
    }

    fn fake_probe(version: &'static str) -> impl Fn(&Path) -> io::Result<String> {
        move |_| Ok(format!("ZeeTerm {version}\n"))
    }

    fn apply(
        archive: &Path,
        layout: &TarLayout,
        probe: &dyn Fn(&Path) -> io::Result<String>,
        rename: &dyn Fn(&Path, &Path) -> io::Result<()>,
    ) -> Result<(), TarError> {
        apply_tar_update(
            archive,
            layout,
            NEW_VERSION,
            &TarContext {
                runner: &crate::linux::SystemRunner,
                probe_version: probe,
                rename,
            },
        )
    }

    /// 手动解压的平铺布局：旧版本的 zeeterm、zssh-mcp 并排放着。
    fn flat_install(root: &Path) -> TarLayout {
        let dir = root.join("flat");
        write_exec(&dir.join("zeeterm"), &script(OLD_VERSION));
        write_exec(&dir.join("zssh-mcp"), "#!/bin/sh\necho 'mcp 1.0.0'\n");
        std::fs::write(dir.join("install.sh"), "old installer").expect("旧脚本");
        TarLayout::detect(&dir.join("zeeterm"))
    }

    #[test]
    fn a_flat_unpacked_directory_gets_both_programs_replaced_and_nothing_left_behind() {
        let root = temp_dir("flat-ok");
        let layout = flat_install(&root);
        let archive = make_tarball(&root, &package_tree(&root.join("new"), NEW_VERSION));

        apply(&archive, &layout, &fake_probe(NEW_VERSION), &rename_system).expect("更新");

        assert_eq!(read(&layout.dir.join("zeeterm")), script(NEW_VERSION));
        assert_eq!(
            read(&layout.dir.join("zssh-mcp")),
            "#!/bin/sh\necho 'mcp 2.0.0'\n"
        );
        for name in ["zeeterm", "zssh-mcp"] {
            let mode = std::fs::metadata(layout.dir.join(name))
                .expect("元数据")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "{name} 要可执行");
        }
        // 平铺布局只换程序，别的东西不碰。
        assert_eq!(read(&layout.dir.join("install.sh")), "old installer");
        assert!(
            leftovers(&layout.dir).is_empty(),
            "{:?}",
            leftovers(&layout.dir)
        );
        // 新程序真的能跑，报告的就是新版本（用真实的 probe 再确认一遍）。
        assert_eq!(
            probe_version_system(&layout.dir.join("zeeterm"))
                .expect("运行")
                .trim(),
            "ZeeTerm 2.0.0"
        );
    }

    #[test]
    fn staging_directories_left_by_a_killed_update_are_cleaned_up_after_a_day() {
        let root = temp_dir("stale");
        let layout = flat_install(&root);
        let old = layout.dir.join(".zeeterm-update-1-1");
        let recent = layout.dir.join(".zeeterm-update-2-2");
        let unrelated = layout.dir.join(".other-hidden-dir");
        for dir in [&old, &recent, &unrelated] {
            std::fs::create_dir_all(dir.join("inner")).expect("建目录");
        }
        let two_days = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 86_400);
        for dir in [&old, &unrelated] {
            std::fs::File::open(dir)
                .and_then(|file| file.set_modified(two_days))
                .expect("改修改时间");
        }
        let archive = make_tarball(&root, &package_tree(&root.join("new"), NEW_VERSION));

        apply(&archive, &layout, &fake_probe(NEW_VERSION), &rename_system).expect("更新");

        assert!(!old.exists(), "超过一天的临时目录要清掉");
        assert!(recent.exists(), "刚建的可能是另一个正在更新的进程，不动");
        assert!(unrelated.exists(), "不是我们的目录不动");
    }

    #[test]
    fn a_running_program_can_be_replaced_because_the_swap_is_a_rename() {
        let root = temp_dir("busy");
        let layout = flat_install(&root);
        // 旧程序正在运行（sleep 的脚本，exec 之后文件被内核占着）。
        write_exec(&layout.dir.join("zeeterm"), "#!/bin/sh\nexec sleep 30\n");
        let mut running = Command::new(layout.dir.join("zeeterm"))
            .spawn()
            .expect("启动旧程序");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let archive = make_tarball(&root, &package_tree(&root.join("new"), NEW_VERSION));

        let result = apply(&archive, &layout, &fake_probe(NEW_VERSION), &rename_system);
        let _ = running.kill();
        let _ = running.wait();
        result.expect("正在运行的程序也能换（rename 不会 Text file busy）");
        assert_eq!(read(&layout.dir.join("zeeterm")), script(NEW_VERSION));
    }

    #[test]
    fn a_wrong_version_or_an_unrunnable_binary_leaves_the_old_install_untouched() {
        let root = temp_dir("flat-bad");
        let layout = flat_install(&root);
        let archive = make_tarball(&root, &package_tree(&root.join("new"), NEW_VERSION));

        // 新程序报告的版本和公告的不一致（包是别的版本）。
        let err =
            apply(&archive, &layout, &fake_probe("9.9.9"), &rename_system).expect_err("版本不符");
        assert!(matches!(err, TarError::WrongVersion { .. }), "{err}");
        // 新程序跑不起来（缺库、架构不对……）。
        let broken = |_: &Path| -> io::Result<String> { Err(io::Error::other("no such lib")) };
        let err = apply(&archive, &layout, &broken, &rename_system).expect_err("跑不起来");
        assert!(matches!(err, TarError::ProbeFailed(_)), "{err}");

        assert_eq!(read(&layout.dir.join("zeeterm")), script(OLD_VERSION));
        assert_eq!(
            read(&layout.dir.join("zssh-mcp")),
            "#!/bin/sh\necho 'mcp 1.0.0'\n"
        );
        assert!(
            leftovers(&layout.dir).is_empty(),
            "{:?}",
            leftovers(&layout.dir)
        );
    }

    #[test]
    fn a_package_without_the_helper_or_with_links_is_refused_before_anything_changes() {
        let root = temp_dir("flat-refuse");
        let layout = flat_install(&root);

        let missing = package_tree(&root.join("missing"), NEW_VERSION);
        std::fs::remove_file(missing.join("zssh-mcp")).expect("删");
        let err = apply(
            &make_tarball(&root, &missing),
            &layout,
            &fake_probe(NEW_VERSION),
            &rename_system,
        )
        .expect_err("缺 zssh-mcp");
        assert!(
            matches!(&err, TarError::MissingFile(name) if name == "zssh-mcp"),
            "{err}"
        );

        let linked = package_tree(&root.join("linked"), NEW_VERSION);
        std::fs::remove_file(linked.join("zssh-mcp")).expect("删");
        std::os::unix::fs::symlink("/bin/sh", linked.join("zssh-mcp")).expect("软链接");
        let err = apply(
            &make_tarball(&root, &linked),
            &layout,
            &fake_probe(NEW_VERSION),
            &rename_system,
        )
        .expect_err("软链接");
        assert!(matches!(err, TarError::UnsafeEntry(_)), "{err}");

        // 不是 gzip。
        let junk = root.join("junk.tar.gz");
        std::fs::write(&junk, b"not an archive").expect("写");
        let err =
            apply(&junk, &layout, &fake_probe(NEW_VERSION), &rename_system).expect_err("坏包");
        assert!(matches!(err, TarError::ExtractFailed(_)), "{err}");

        assert_eq!(read(&layout.dir.join("zeeterm")), script(OLD_VERSION));
        assert!(
            leftovers(&layout.dir).is_empty(),
            "{:?}",
            leftovers(&layout.dir)
        );
    }

    #[test]
    fn a_directory_that_cannot_be_written_means_manual_install_and_changes_nothing() {
        let root = temp_dir("not-writable");
        // 程序所在目录建不出临时目录（用"父路径是文件"模拟：以 root 跑测试时权限位挡不住）。
        let blocker = root.join("blocker");
        std::fs::write(&blocker, b"file").expect("写");
        let layout = TarLayout {
            exe: blocker.join("zeeterm"),
            dir: blocker,
            prefix: None,
        };
        let archive = make_tarball(&root, &package_tree(&root.join("new"), NEW_VERSION));
        let err =
            apply(&archive, &layout, &fake_probe(NEW_VERSION), &rename_system).expect_err("不可写");
        assert!(err.is_manual(), "{err}");
    }

    #[test]
    fn a_failure_halfway_through_the_swap_restores_every_file_already_replaced() {
        let root = temp_dir("rollback");
        let layout = flat_install(&root);
        let archive = make_tarball(&root, &package_tree(&root.join("new"), NEW_VERSION));
        // zssh-mcp 先换上，换主程序时失败：zssh-mcp 必须退回旧的。
        let calls = std::cell::Cell::new(0);
        let failing = |from: &Path, to: &Path| -> io::Result<()> {
            calls.set(calls.get() + 1);
            if calls.get() == 2 {
                return Err(io::Error::other("disk on fire"));
            }
            std::fs::rename(from, to)
        };
        let err =
            apply(&archive, &layout, &fake_probe(NEW_VERSION), &failing).expect_err("中途失败");
        assert!(matches!(err, TarError::Io(IoStep::Replace, _)), "{err}");
        assert_eq!(calls.get(), 2);
        assert_eq!(read(&layout.dir.join("zeeterm")), script(OLD_VERSION));
        assert_eq!(
            read(&layout.dir.join("zssh-mcp")),
            "#!/bin/sh\necho 'mcp 1.0.0'\n"
        );
        assert!(
            leftovers(&layout.dir).is_empty(),
            "{:?}",
            leftovers(&layout.dir)
        );
    }

    /// 用仓库里真正的 install.sh 装一份旧版本，返回（前缀, 布局）。
    fn installer_install(root: &Path, version: &str) -> (PathBuf, TarLayout) {
        let top = package_tree(&root.join("pkg-old"), version);
        let prefix = root.join("prefix with space");
        let output = Command::new("sh")
            .arg(top.join("install.sh"))
            .arg("--prefix")
            .arg(&prefix)
            .output()
            .expect("运行 install.sh");
        assert!(
            output.status.success(),
            "install.sh 失败：{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let exe = prefix.join("lib/zeeterm/zeeterm");
        (prefix, TarLayout::detect(&exe))
    }

    #[test]
    fn the_installer_layout_gets_programs_icons_desktop_file_and_manifest_updated_together() {
        let root = temp_dir("prefix-ok");
        let (prefix, layout) = installer_install(&root, OLD_VERSION);
        assert_eq!(layout.prefix.as_deref(), Some(prefix.as_path()));
        let desktop_path = prefix.join(format!("share/applications/{APP_ID}.desktop"));
        let old_desktop = read(&desktop_path);
        let exec_line = old_desktop
            .lines()
            .find(|l| l.starts_with("Exec="))
            .expect("Exec")
            .to_owned();
        let try_line = old_desktop
            .lines()
            .find(|l| l.starts_with("TryExec="))
            .expect("TryExec")
            .to_owned();
        assert!(exec_line.contains("prefix with space"), "{exec_line}");
        let link = prefix.join("bin/zeeterm");
        assert_eq!(layout.restart_command(), link, "重启走 bin/zeeterm 软链接");

        // 新包多一个尺寸的图标。
        let new_top = package_tree(&root.join("pkg-new"), NEW_VERSION);
        let extra = new_top.join(format!("icons/hicolor/512x512/apps/{APP_ID}.png"));
        std::fs::create_dir_all(extra.parent().expect("父目录")).expect("建目录");
        std::fs::write(&extra, "png 512 new").expect("图标");
        let archive = make_tarball(&root, &new_top);

        apply(&archive, &layout, &fake_probe(NEW_VERSION), &rename_system).expect("更新");

        // 程序：通过软链接和直接路径都是新版本；旧文件不在。
        assert_eq!(
            read(&prefix.join("lib/zeeterm/zeeterm")),
            script(NEW_VERSION)
        );
        assert_eq!(
            read(&prefix.join("lib/zeeterm/zssh-mcp")),
            "#!/bin/sh\necho 'mcp 2.0.0'\n"
        );
        assert_eq!(read(&link), script(NEW_VERSION));
        assert_eq!(
            std::fs::read_link(&link).expect("软链接"),
            Path::new("../lib/zeeterm/zeeterm")
        );
        // 随包文件：内容是新的，新增尺寸也装上了。
        assert_eq!(
            read(&prefix.join(format!("share/icons/hicolor/16x16/apps/{APP_ID}.png"))),
            "png 16x16 2.0.0"
        );
        assert_eq!(
            read(&prefix.join(format!("share/icons/hicolor/512x512/apps/{APP_ID}.png"))),
            "png 512 new"
        );
        assert_eq!(
            read(&prefix.join(format!("share/metainfo/{APP_ID}.metainfo.xml"))),
            "<c>2.0.0</c>"
        );
        // 桌面文件：内容取新的，Exec / TryExec 沿用安装时写的。
        let new_desktop = read(&desktop_path);
        assert!(
            new_desktop.contains("X-Test-Version=2.0.0"),
            "{new_desktop}"
        );
        assert!(new_desktop.lines().any(|l| l == exec_line), "{new_desktop}");
        assert!(new_desktop.lines().any(|l| l == try_line), "{new_desktop}");
        assert!(leftovers(&prefix).is_empty(), "{:?}", leftovers(&prefix));

        // 清单更新后，卸载仍然能删干净（包括新增的图标）。
        let manifest = read(&prefix.join("share/zeeterm/install-manifest"));
        assert!(manifest.contains("512x512"), "{manifest}");
        assert!(
            manifest.trim_end().ends_with("install-manifest"),
            "清单自己在最后"
        );
        let uninstall = Command::new("sh")
            .arg(prefix.join("share/zeeterm/uninstall.sh"))
            .output()
            .expect("运行 uninstall.sh");
        assert!(
            uninstall.status.success(),
            "{}",
            String::from_utf8_lossy(&uninstall.stderr)
        );
        // update-desktop-database / gtk-update-icon-cache 生成的缓存是共用的，不算我们的文件。
        let rest: Vec<String> = walk_non_dirs(&prefix)
            .into_iter()
            .filter(|path| !path.ends_with("mimeinfo.cache") && !path.ends_with("icon-theme.cache"))
            .collect();
        assert!(rest.is_empty(), "卸载后还剩：{rest:?}");
    }

    fn walk_non_dirs(root: &Path) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).expect("读目录").flatten() {
                let kind = entry.file_type().expect("类型");
                if kind.is_dir() {
                    pending.push(entry.path());
                } else {
                    found.push(entry.path().to_string_lossy().into_owned());
                }
            }
        }
        found
    }

    #[test]
    fn a_failure_in_the_installer_layout_rolls_back_programs_and_shared_files_together() {
        let root = temp_dir("prefix-rollback");
        let (prefix, layout) = installer_install(&root, OLD_VERSION);
        let manifest_before = read(&prefix.join("share/zeeterm/install-manifest"));
        let archive = make_tarball(&root, &package_tree(&root.join("pkg-new"), NEW_VERSION));

        // 图标、metainfo、桌面文件、卸载脚本、清单都换上了，最后换主程序时失败。
        let total = std::cell::Cell::new(0);
        let failing = |from: &Path, to: &Path| -> io::Result<()> {
            total.set(total.get() + 1);
            if to.file_name().is_some_and(|name| name == "zeeterm") {
                return Err(io::Error::other("no space left"));
            }
            std::fs::rename(from, to)
        };
        let err = apply(&archive, &layout, &fake_probe(NEW_VERSION), &failing).expect_err("失败");
        assert!(matches!(err, TarError::Io(IoStep::Replace, _)), "{err}");
        assert!(total.get() > 3, "失败前已经换过好几个文件：{}", total.get());

        assert_eq!(
            read(&prefix.join("lib/zeeterm/zeeterm")),
            script(OLD_VERSION)
        );
        assert_eq!(
            read(&prefix.join("lib/zeeterm/zssh-mcp")),
            "#!/bin/sh\necho 'mcp 1.0.0'\n"
        );
        assert_eq!(
            read(&prefix.join(format!("share/icons/hicolor/16x16/apps/{APP_ID}.png"))),
            "png 16x16 1.0.0"
        );
        assert!(
            !read(&prefix.join(format!("share/applications/{APP_ID}.desktop"))).contains("2.0.0")
        );
        assert_eq!(
            read(&prefix.join("share/zeeterm/install-manifest")),
            manifest_before
        );
        assert!(leftovers(&prefix).is_empty(), "{:?}", leftovers(&prefix));
    }

    #[test]
    fn the_system_entry_point_runs_the_real_tar_and_the_real_version_probe() {
        let root = temp_dir("system");
        let layout = flat_install(&root);
        let archive = make_tarball(&root, &package_tree(&root.join("new"), NEW_VERSION));
        apply_tar_update_system(&archive, &layout, NEW_VERSION).expect("更新");
        assert_eq!(read(&layout.dir.join("zeeterm")), script(NEW_VERSION));
        // 同一个包再来一次（版本相同）也行，覆盖安装。
        apply_tar_update_system(&archive, &layout, NEW_VERSION).expect("再来一次");
        // 版本不对时真实的 probe 也会拒绝。
        assert!(matches!(
            apply_tar_update_system(&archive, &layout, "3.0.0"),
            Err(TarError::WrongVersion { .. })
        ));
        assert!(
            leftovers(&layout.dir).is_empty(),
            "{:?}",
            leftovers(&layout.dir)
        );
    }
}
