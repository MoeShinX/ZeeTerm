//! 命令行参数：目前只有 `--version` / `-V`（Linux 打包后用来在干净的系统里确认程序能跑起来）。
//!
//! 在 `main` 的最开头处理，早于窗口、数据目录、钥匙串的任何初始化，所以没有显示环境、没有家目录的
//! 容器里也能运行。只在 Unix 上编译：Windows 的行为保持原样（带不带参数都是打开程序）。

use std::ffi::OsString;

use crate::product;

/// 第一个参数是 `--version` 或 `-V` 时，返回要打印的那一行；否则返回 `None`（正常启动）。
pub fn version_line(args: impl IntoIterator<Item = OsString>) -> Option<String> {
    let first = args.into_iter().nth(1)?;
    (first == "--version" || first == "-V").then(|| version_text(env!("CARGO_PKG_VERSION")))
}

fn version_text(version: &str) -> String {
    format!("{} {version}", product::NAME)
}

/// 有没有能开窗口的显示环境（Wayland 或 X11）。两个变量都没有（或是空串）时 GPUI 会退到无头模式：
/// 进程照常运行，但永远不出窗口，从 SSH 会话、cron、systemd 服务里启动的人只会看到"卡住了"。
pub fn has_display(wayland: Option<OsString>, x11: Option<OsString>) -> bool {
    [wayland, x11]
        .into_iter()
        .flatten()
        .any(|value| !value.is_empty())
}

/// 没有显示环境时的提示（设置还没读，按系统语言二选一）。
#[cfg(target_os = "linux")]
pub fn no_display_message() -> &'static str {
    if zssh_platform_windows::locale::system_is_chinese() {
        "ZeeTerm 需要图形桌面才能运行：没有找到 WAYLAND_DISPLAY 或 DISPLAY。请在桌面会话里启动它（例如从应用菜单打开）。" // i18n-allow: 设置还没读、按系统语言二选一，启动前的致命提示
    } else {
        "ZeeTerm needs a graphical desktop: neither WAYLAND_DISPLAY nor DISPLAY is set. Start it from a desktop session (for example from the application menu)."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn version_flag_prints_the_product_and_version() {
        let expected = format!("ZeeTerm {}", env!("CARGO_PKG_VERSION"));
        assert_eq!(
            version_line(args(&["zeeterm", "--version"])).as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(
            version_line(args(&["zeeterm", "-V"])).as_deref(),
            Some(expected.as_str())
        );
    }

    #[test]
    fn other_arguments_start_the_app_normally() {
        assert_eq!(version_line(args(&["zeeterm"])), None);
        assert_eq!(version_line(args(&["zeeterm", "--help"])), None);
        assert_eq!(version_line(args(&["zeeterm", "x", "--version"])), None);
        assert_eq!(version_line(args(&[])), None);
    }

    #[test]
    fn a_display_is_wayland_or_x11_and_empty_values_do_not_count() {
        let some = |text: &str| Some(OsString::from(text));
        assert!(has_display(some("wayland-0"), None));
        assert!(has_display(None, some(":0")));
        assert!(has_display(some("wayland-0"), some(":0")));
        assert!(!has_display(None, None));
        assert!(!has_display(some(""), some("")));
    }

    #[test]
    fn the_version_line_is_a_single_line_with_a_semver() {
        let line = version_text("1.2.3");
        assert_eq!(line, "ZeeTerm 1.2.3");
        let real = version_line(args(&["zeeterm", "--version"])).unwrap();
        assert!(!real.contains('\n'));
        let version = real.strip_prefix("ZeeTerm ").unwrap();
        assert!(version.split('.').count() >= 3, "{version}");
    }
}
