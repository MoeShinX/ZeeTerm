//! 本地栏一开始落在哪个目录（「设置 → 文件传输 → 文件面板 → 本地默认目录」）。
//!
//! 选择规则是纯函数（[`resolve`]），文件系统和已知文件夹都从参数传进来，测试用临时目录模拟，
//! 不碰用户真实的下载文件夹。真正读盘的是后台线程里的 `RealFs::start_dir`，界面线程不做 `is_dir`。

use std::path::{Path, PathBuf};

use crate::settings::SftpLocalDir;

/// 视图创建时带上的起始目录设置（设置页的两个字段 + 这台主机上次用过的目录）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct StartPlan {
    pub(crate) choice: SftpLocalDir,
    /// 「自定义」选的目录；空 = 还没选。
    pub(crate) custom: String,
    /// 这台主机上次本地栏所在的目录（按主机记在本机，不进同步）。
    pub(crate) last: Option<String>,
}

/// 选定的起始目录，以及有没有因为「自定义」目录不可用而退回去。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chosen {
    pub(crate) dir: PathBuf,
    /// 用户选了「自定义」、也填了目录，但那个目录现在不存在 / 不是目录。
    pub(crate) custom_missing: bool,
}

/// 按设置挑起始目录。
///
/// - 下载文件夹：系统的「下载」；取不到退到用户主目录；再取不到才用 `fallback`（临时区的专用目录）。
/// - 上次用过的：记录的目录还在就用，不在了退到下载文件夹。
/// - 自定义：目录还在就用，不在退到下载文件夹并标出 `custom_missing`（界面提示一次）。
pub(crate) fn resolve(
    plan: &StartPlan,
    downloads: Option<&Path>,
    home: Option<&Path>,
    fallback: &Path,
    is_dir: &dyn Fn(&Path) -> bool,
) -> Chosen {
    let usable = |dir: &str| -> Option<PathBuf> {
        let dir = dir.trim();
        (!dir.is_empty() && is_dir(Path::new(dir))).then(|| PathBuf::from(dir))
    };
    let base = || -> PathBuf {
        [downloads, home]
            .into_iter()
            .flatten()
            .find(|dir| is_dir(dir))
            .map(Path::to_path_buf)
            .unwrap_or_else(|| fallback.to_path_buf())
    };
    match plan.choice {
        SftpLocalDir::Downloads => Chosen {
            dir: base(),
            custom_missing: false,
        },
        SftpLocalDir::Last => Chosen {
            dir: plan.last.as_deref().and_then(usable).unwrap_or_else(base),
            custom_missing: false,
        },
        SftpLocalDir::Custom => match usable(&plan.custom) {
            Some(dir) => Chosen {
                dir,
                custom_missing: false,
            },
            None => Chosen {
                dir: base(),
                custom_missing: !plan.custom.trim().is_empty(),
            },
        },
    }
}

/// Windows 的「下载」已知文件夹；取不到（很老的系统、被重定向到不存在的位置）返回 `None`。
#[cfg(windows)]
fn known_downloads() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    // SAFETY: 返回的字符串由 shell 分配，复制出来之后立刻用 `CoTaskMemFree` 释放。
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None).ok()?;
        let text = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        text.filter(|text| !text.is_empty()).map(PathBuf::from)
    }
}

#[cfg(not(windows))]
fn known_downloads() -> Option<PathBuf> {
    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|dir| !dir.as_os_str().is_empty())
}

/// 没有「下载」文件夹、没有主目录时的最后一站，也是自动化默认落点：`%TEMP%\zssh-sftp-dl`。
pub(super) fn temp_fallback() -> PathBuf {
    std::env::temp_dir().join("zssh-sftp-dl")
}

/// 调试构建里的保险：自动化（设了 `ZSSH_DATA_DIR`）没有明确指定下载文件夹时，「下载文件夹」
/// 指向临时区的专用目录，免得脚本和测试把文件写进开发者真正的下载文件夹。
/// `ZSSH_KNOWN_DOWNLOADS=<目录>` 可以明确指定；发布版读不到环境变量，这里恒为 `None`。
#[cfg(debug_assertions)]
fn debug_downloads_override() -> Option<PathBuf> {
    if let Some(dir) = super::engine::env_value("ZSSH_KNOWN_DOWNLOADS") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("ZSSH_DATA_DIR")
        .filter(|dir| !dir.is_empty())
        .map(|_| temp_fallback())
}

#[cfg(not(debug_assertions))]
fn debug_downloads_override() -> Option<PathBuf> {
    None
}

/// 没有指定目录时的默认落点：「下载」文件夹，取不到退到主目录，再退到临时区。只算路径，不读盘、不建目录。
pub(super) fn default_downloads() -> PathBuf {
    debug_downloads_override()
        .or_else(known_downloads)
        .or_else(home_dir)
        .unwrap_or_else(temp_fallback)
}

/// 真实环境下选起始目录（后台线程里调：会读盘）。
pub(crate) fn resolve_real(plan: &StartPlan) -> Chosen {
    let downloads = debug_downloads_override().or_else(known_downloads);
    let fallback = temp_fallback();
    let chosen = resolve(
        plan,
        downloads.as_deref(),
        home_dir().as_deref(),
        &fallback,
        &|dir| dir.is_dir(),
    );
    if chosen.dir == fallback {
        let _ = std::fs::create_dir_all(&fallback);
    }
    chosen
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn dirs(list: &[&str]) -> HashSet<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    fn pick(plan: &StartPlan, existing: &HashSet<PathBuf>) -> Chosen {
        resolve(
            plan,
            Some(Path::new("T:/dl")),
            Some(Path::new("T:/home")),
            Path::new("T:/tmp-fallback"),
            &|dir| existing.contains(dir),
        )
    }

    fn plan(choice: SftpLocalDir, custom: &str, last: Option<&str>) -> StartPlan {
        StartPlan {
            choice,
            custom: custom.to_owned(),
            last: last.map(str::to_owned),
        }
    }

    #[test]
    fn downloads_is_the_default_source() {
        let existing = dirs(&["T:/dl", "T:/home"]);
        let chosen = pick(&StartPlan::default(), &existing);
        assert_eq!(chosen.dir, PathBuf::from("T:/dl"));
        assert!(!chosen.custom_missing);
    }

    #[test]
    fn a_missing_downloads_folder_falls_back_to_home_then_to_the_temp_dir() {
        let only_home = dirs(&["T:/home"]);
        assert_eq!(
            pick(&StartPlan::default(), &only_home).dir,
            PathBuf::from("T:/home")
        );
        let nothing = dirs(&[]);
        assert_eq!(
            pick(&StartPlan::default(), &nothing).dir,
            PathBuf::from("T:/tmp-fallback")
        );
        // 取不到已知文件夹和主目录（None）也走最后一站。
        let chosen = resolve(
            &StartPlan::default(),
            None,
            None,
            Path::new("T:/tmp-fallback"),
            &|_| false,
        );
        assert_eq!(chosen.dir, PathBuf::from("T:/tmp-fallback"));
    }

    #[test]
    fn last_used_is_kept_when_it_still_exists_and_falls_back_to_downloads_when_gone() {
        let existing = dirs(&["T:/dl", "T:/proj"]);
        let kept = pick(&plan(SftpLocalDir::Last, "", Some("T:/proj")), &existing);
        assert_eq!(kept.dir, PathBuf::from("T:/proj"));
        let gone = pick(&plan(SftpLocalDir::Last, "", Some("T:/deleted")), &existing);
        assert_eq!(gone.dir, PathBuf::from("T:/dl"));
        assert!(!gone.custom_missing, "上次用过的目录没了不需要提示");
        let never = pick(&plan(SftpLocalDir::Last, "", None), &existing);
        assert_eq!(never.dir, PathBuf::from("T:/dl"));
    }

    #[test]
    fn a_custom_dir_is_used_when_present_and_flagged_when_missing() {
        let existing = dirs(&["T:/dl", "T:/mine"]);
        let ok = pick(&plan(SftpLocalDir::Custom, "T:/mine", None), &existing);
        assert_eq!(ok.dir, PathBuf::from("T:/mine"));
        assert!(!ok.custom_missing);
        let missing = pick(&plan(SftpLocalDir::Custom, "T:/nope", None), &existing);
        assert_eq!(missing.dir, PathBuf::from("T:/dl"));
        assert!(missing.custom_missing, "选了自定义但目录不在：要提示一次");
        // 还没选目录（空串）：退回下载文件夹，但不当成「目录不存在」。
        let unset = pick(&plan(SftpLocalDir::Custom, "  ", None), &existing);
        assert_eq!(unset.dir, PathBuf::from("T:/dl"));
        assert!(!unset.custom_missing);
    }

    #[test]
    fn the_real_resolver_never_creates_the_directories_the_user_picked() {
        // 用临时目录当「自定义」：存在就用，不存在就退回，且不会替用户建目录。
        let base = std::env::temp_dir().join(format!("zssh-localstart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let there = base.join("here");
        std::fs::create_dir_all(&there).unwrap();
        let ok = resolve_real(&plan(
            SftpLocalDir::Custom,
            &there.display().to_string(),
            None,
        ));
        assert_eq!(ok.dir, there);
        let gone = base.join("gone");
        let missing = resolve_real(&plan(
            SftpLocalDir::Custom,
            &gone.display().to_string(),
            None,
        ));
        assert!(missing.custom_missing);
        assert!(!gone.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}
