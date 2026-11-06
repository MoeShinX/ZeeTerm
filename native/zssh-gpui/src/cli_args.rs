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
    fn the_version_line_is_a_single_line_with_a_semver() {
        let line = version_text("1.2.3");
        assert_eq!(line, "ZeeTerm 1.2.3");
        let real = version_line(args(&["zeeterm", "--version"])).unwrap();
        assert!(!real.contains('\n'));
        let version = real.strip_prefix("ZeeTerm ").unwrap();
        assert!(version.split('.').count() >= 3, "{version}");
    }
}
