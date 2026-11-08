//! 更新窗口在 Windows 免安装版上的安装流程：校验计划、后台原地替换程序文件、重启。
//!
//! 实际的解压、替换、回滚都在 `zssh_update::portable`，这里只把它接到更新窗口上：后台线程里跑 [`run`]
//! （替换动作从参数传进来，测试里换成假实现，不会碰真正在用的程序），结果 [`Outcome`] 经事件回到界面，
//! 和 Linux 的 tar.gz 更新共用同一套"安装中 / 已安装、重启生效"的屏。这个文件在别的平台上也编译。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use zssh_update::portable::{
    self as core, PortableError, PortableInstallRequest, PortableLayout, plan_portable_install,
};

use super::UpdateMsg;
use super::linux::Outcome;
use crate::i18n::t;

/// 一次安装要用的全部信息（在界面线程上收集好，交给后台线程）。
#[derive(Debug, Clone)]
pub(super) struct Job {
    pub package: PathBuf,
    pub cache_root: PathBuf,
    pub version: String,
    pub sha256: String,
    /// 正在运行的程序和它所在的目录。
    pub layout: PortableLayout,
}

/// 替换动作的类型（产品里是 `apply_portable_update_system`，测试里换成假实现）。
pub(super) type ApplyFn<'a> = dyn Fn(&Path, &PortableLayout) -> Result<(), PortableError> + 'a;

/// 产品用的安装：真的解压、真的改名替换。
pub(super) fn run_system(job: &Job) -> Outcome {
    run(job, &core::apply_portable_update_system)
}

/// 校验计划（包还在缓存里、内容还是验签时的那份）后原地替换。
pub(super) fn run(job: &Job, apply: &ApplyFn<'_>) -> Outcome {
    let plan = match plan_portable_install(&PortableInstallRequest {
        package: job.package.clone(),
        cache_root: job.cache_root.clone(),
        version: job.version.clone(),
        sha256: job.sha256.clone(),
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
    match apply(&plan.package, &job.layout) {
        // 新程序已经在原来的位置上了，重启就是再启动这个路径。
        Ok(()) => Outcome::Installed {
            restart: job.layout.exe.clone(),
        },
        // 检查更新时目录还能写、现在写不进去了（被改成只读、盘被拔掉……）：不是故障，告诉用户去手动下载。
        Err(err) if err.is_manual() => Outcome::Failed(format!(
            "{}\n{}",
            crate::update_text::portable_error(&err),
            t(UpdateMsg::PortableManualMessage)
        )),
        Err(err) => Outcome::Failed(format!(
            "{}{}",
            t(UpdateMsg::PortableInstallFailedPrefix),
            crate::update_text::portable_error(&err)
        )),
    }
}

/// 重启：直接起新程序，参数里带上 `--zeeterm-wait-pid=<本进程号>`，新进程在抢单实例互斥体之前先等本进程退出。
/// 成功返回后，调用方要让界面退出。
pub(super) fn restart(program: &Path, args: &[String]) -> Result<(), String> {
    let argv: Vec<OsString> = core::restart_args(std::process::id(), args);
    core::spawn_detached(program, &argv)
        .map_err(|err| format!("{}{err}", t(UpdateMsg::RestartFailedPrefix)))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn cache_with_zip(name: &str) -> (PathBuf, PathBuf, String) {
        let root = std::env::temp_dir().join(format!(
            "zeeterm-update-portable-glue-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建缓存目录");
        let package = root.join("ZeeTerm-2.0.0-windows-x64-portable.zip");
        std::fs::write(&package, b"zip bytes").expect("写包");
        (root, package, zssh_update::sha256_hex(b"zip bytes"))
    }

    fn job(root: &Path, package: &Path, sha256: &str) -> Job {
        Job {
            package: package.to_path_buf(),
            cache_root: root.to_path_buf(),
            version: "2.0.0".to_owned(),
            sha256: sha256.to_owned(),
            layout: PortableLayout::new(Path::new(r"D:\Tools\ZeeTerm\ZeeTerm.exe")),
        }
    }

    #[test]
    fn a_verified_package_is_applied_and_the_restart_target_is_the_running_path() {
        let (root, package, sha) = cache_with_zip("ok");
        let seen = RefCell::new(None);
        let outcome = run(&job(&root, &package, &sha), &|archive, layout| {
            *seen.borrow_mut() = Some((archive.to_path_buf(), layout.clone()));
            Ok(())
        });
        assert_eq!(
            outcome,
            Outcome::Installed {
                restart: PathBuf::from(r"D:\Tools\ZeeTerm\ZeeTerm.exe")
            }
        );
        let (archive, layout) = seen.into_inner().expect("调用了替换");
        assert_eq!(archive, package.canonicalize().expect("规范化"));
        assert_eq!(layout.dir, Path::new(r"D:\Tools\ZeeTerm"));
    }

    #[test]
    fn a_package_that_changed_after_verification_is_never_applied() {
        let (root, package, sha) = cache_with_zip("changed");
        std::fs::write(&package, b"swapped").expect("换包");
        let outcome = run(&job(&root, &package, &sha), &|_, _| {
            panic!("包不对就不能去替换程序文件")
        });
        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    }

    #[test]
    fn failures_are_shown_and_an_unwritable_folder_points_at_the_release_page() {
        let (root, package, sha) = cache_with_zip("fail");
        let failed = run(&job(&root, &package, &sha), &|_, _| {
            Err(PortableError::MissingFile("ZeeTerm.exe".into()))
        });
        match failed {
            Outcome::Failed(text) => assert!(text.contains("ZeeTerm.exe"), "{text}"),
            other => panic!("应是失败：{other:?}"),
        }
        let manual = run(&job(&root, &package, &sha), &|_, _| {
            Err(PortableError::NotWritable("access is denied".into()))
        });
        match manual {
            Outcome::Failed(text) => {
                assert!(text.contains("access is denied"), "{text}");
                assert!(text.contains(t(UpdateMsg::PortableManualMessage)), "{text}");
            }
            other => panic!("应是失败：{other:?}"),
        }
    }
}
