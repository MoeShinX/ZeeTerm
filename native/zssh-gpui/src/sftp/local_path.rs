//! 本地栏的路径规则里「Linux / 其他 Unix 按 POSIX 处理」的那一半。
//!
//! Windows 的规则（盘符、`\\server\share`、`\\?\` 前缀、不分大小写、Windows 文件名规则）原样留在各自
//! 的函数里（`model.rs` 的 `windows_local_crumbs`、`move_to.rs` 的 `windows_path_key` 等）；这里只放
//! POSIX 的实现和两边共用的小工具，调用点按 `cfg!(windows)` 选。两套实现在两个系统上都会编译，
//! 所以任何一边改坏了另一边的编译都会立刻报出来。
//!
//! POSIX 本地路径的规则：
//! - 根是 `/`，没有「此电脑 / 盘符列表」这一层；`/` 的上一级还是 `/`。
//! - 区分大小写，分隔符只有 `/`；`\` 是普通的文件名字符；文件名里只有 `/` 和 NUL 不合法。
//! - 手敲的目标路径必须是绝对路径；`~` 和 `~/…` 展开成家目录（不支持 `~用户名`）。
//! - 远端那套 POSIX 函数（`zssh_core::sftp::path`）能复用的直接复用。

use std::path::{Path, PathBuf};

use zssh_core::pathnorm::simplify_verbatim;
use zssh_core::sftp::local::{check_child_name, long_path};
use zssh_core::sftp::path::{file_name, normalize};

use super::SftpMsg;
use super::model::Crumb;
use crate::error_text::error_text;
use crate::i18n::t;

/// 是不是路径分隔符：Windows 上 `/` 和 `\` 都是，POSIX 上只有 `/`。
pub(super) fn is_separator(c: char) -> bool {
    c == '/' || (cfg!(windows) && c == '\\')
}

// ---------------------------------------------------------------------------
// 面包屑和上一级
// ---------------------------------------------------------------------------

/// POSIX 面包屑：`/home/me` → `/`、home、me。第一项永远是根，所以点得回 `/`。
pub(super) fn posix_crumbs(path: &str) -> Vec<Crumb> {
    let mut out = vec![Crumb {
        name: "/".to_owned(),
        path: "/".to_owned(),
    }];
    let mut built = String::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        built.push('/');
        built.push_str(part);
        out.push(Crumb {
            name: part.to_owned(),
            path: built.clone(),
        });
    }
    out
}

/// POSIX 上一级：`/home/me` → `/home`；根的上一级还是根。
pub(super) fn posix_parent(path: &str) -> String {
    let crumbs = posix_crumbs(path);
    match crumbs.len() {
        0..=2 => "/".to_owned(),
        n => crumbs[n - 2].path.clone(),
    }
}

// ---------------------------------------------------------------------------
// 手敲的路径
// ---------------------------------------------------------------------------

/// 交给系统接口的路径：Windows 上加 `\\?\` 长路径前缀；Linux 上原样（核心的 `long_path` 是按
/// Windows 的盘符 / UNC 形状写的，会把 `//x` 这种合法的 POSIX 路径改成 `\\?\UNC\x`）。
pub(super) fn sys_path(path: &Path) -> PathBuf {
    if cfg!(windows) {
        long_path(path)
    } else {
        path.to_path_buf()
    }
}

/// 给界面看的路径：Windows 上去掉 `\\?\` 前缀；Linux 上原样（那个前缀在 Linux 上只是个普通名字）。
pub(super) fn ui_path(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        simplify_verbatim(path)
    } else {
        path
    }
}

/// 当前用户的家目录（交给标准库；取不到或为空时是 `None`）。
pub(super) fn home() -> Option<PathBuf> {
    std::env::home_dir().filter(|dir| !dir.as_os_str().is_empty())
}

/// `~` / `~/…` 展开成家目录；别的写法（含 `~用户名`）和取不到家目录时原样返回。
pub(super) fn expand_home(text: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return text.to_owned();
    };
    let home = home.display().to_string();
    if text == "~" {
        return home;
    }
    match text.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", home.trim_end_matches('/')),
        None => text.to_owned(),
    }
}

/// 手敲 / 选出来的本地路径：POSIX 上先展开 `~`；Windows 原样。
pub(super) fn expand_typed(text: &str) -> String {
    if cfg!(windows) {
        return text.to_owned();
    }
    expand_home(text, home().as_deref())
}

/// 「移动到…」目标的写法检查（POSIX）：展开 `~` 后必须是绝对路径，返回规范化（去掉重复的 `/`、
/// `.`、`..`、末尾的 `/`）的结果。
pub(super) fn posix_dest_shape(text: &str, home: Option<&Path>) -> Result<String, String> {
    if text.contains('\0') {
        return Err(t(SftpMsg::ConflictNameNullChar).to_owned());
    }
    let expanded = expand_home(text, home);
    if !expanded.starts_with('/') {
        return Err(t(SftpMsg::MoveDestNeedAbsoluteLocalPosix).to_owned());
    }
    Ok(normalize(&expanded))
}

/// 路径栏里敲的路径整理成绝对路径（POSIX）：相对的接在 `cwd` 后面，`~` 展开，`.` / `..` 按文字整理
/// （和 shell 的 `cd` 一样，不解符号链接）。空串给 `None`。
pub(super) fn posix_resolve_typed(cwd: &str, typed: &str, home: Option<&Path>) -> Option<String> {
    let trimmed = typed.trim();
    if trimmed.is_empty() {
        return None;
    }
    let expanded = expand_home(trimmed, home);
    let joined = if expanded.starts_with('/') {
        expanded
    } else {
        format!("{}/{expanded}", cwd.trim_end_matches('/'))
    };
    Some(normalize(&joined))
}

// ---------------------------------------------------------------------------
// 名字
// ---------------------------------------------------------------------------

/// 一个本地文件 / 文件夹的名字能不能用（POSIX）：非空、不是 `.` / `..`、没有 `/` 和 NUL。
/// 前后空格去掉（对话框和输入框都会 trim），返回去掉空格后的名字。
pub(super) fn posix_child_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(t(SftpMsg::NameEmpty).to_owned());
    }
    if trimmed == "." || trimmed == ".." {
        return Err(format!("{}{trimmed}", t(SftpMsg::NameCannotBePrefix)));
    }
    if trimmed.contains('/') {
        return Err(format!("{}{trimmed}", t(SftpMsg::NamePathSeparatorPrefix)));
    }
    if trimmed.contains('\0') {
        return Err(t(SftpMsg::ConflictNameNullChar).to_owned());
    }
    Ok(trimmed.to_owned())
}

/// 落在本机的新名字（另存为、冲突改名、粘贴改名）：Windows 过 Windows 文件名规则，POSIX 过 POSIX 规则。
pub(super) fn local_child_name(name: &str) -> Result<String, String> {
    if cfg!(windows) {
        check_child_name(name).map_err(|err| error_text(&err))
    } else {
        posix_child_name(name)
    }
}

/// 本地路径最后一段（上传时用它当远端文件名）。Windows 的 `\` 先当成 `/`；POSIX 上 `\` 是名字的一部分。
pub(super) fn local_file_name(path: &str) -> String {
    if cfg!(windows) {
        file_name(&path.replace('\\', "/"))
    } else {
        file_name(path)
    }
}

/// 路径最后一段，末尾的分隔符忽略（远端 `/`；Windows 本地再加 `\`）。
pub(super) fn last_segment(path: &str) -> String {
    path.rsplit(is_separator)
        .find(|part| !part.is_empty())
        .unwrap_or(path)
        .to_owned()
}

// ---------------------------------------------------------------------------
// 文案（按平台给例子）
// ---------------------------------------------------------------------------

/// 「下载到…」输入框的占位。
pub(super) fn download_dir_placeholder() -> &'static str {
    t(if cfg!(windows) {
        SftpMsg::OpsDownloadDirPlaceholder
    } else {
        SftpMsg::OpsDownloadDirPlaceholderPosix
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn paths(crumbs: &[Crumb]) -> Vec<&str> {
        crumbs.iter().map(|c| c.path.as_str()).collect()
    }

    #[test]
    fn posix_crumbs_start_at_the_root() {
        let crumbs = posix_crumbs("/home/me/下载 目录");
        assert_eq!(
            paths(&crumbs),
            ["/", "/home", "/home/me", "/home/me/下载 目录"]
        );
        assert_eq!(crumbs[0].name, "/");
        assert_eq!(crumbs[3].name, "下载 目录");
        // 根、重复的斜杠、末尾的斜杠。
        assert_eq!(paths(&posix_crumbs("/")), ["/"]);
        assert_eq!(paths(&posix_crumbs("//a///b/")), ["/", "/a", "/a/b"]);
        // 反斜杠是普通的名字字符，不是分隔符。
        let odd = posix_crumbs(r"/tmp/a\b/c");
        assert_eq!(paths(&odd), ["/", "/tmp", r"/tmp/a\b", r"/tmp/a\b/c"]);
        assert_eq!(odd[2].name, r"a\b");
    }

    #[test]
    fn posix_parent_stops_at_the_root() {
        assert_eq!(posix_parent("/home/me/下载"), "/home/me");
        assert_eq!(posix_parent("/home"), "/");
        assert_eq!(posix_parent("/"), "/");
        assert_eq!(posix_parent(""), "/");
        assert_eq!(posix_parent("/a b/c d/"), "/a b");
        assert_eq!(posix_parent(r"/tmp/a\b"), "/tmp");
    }

    #[test]
    fn tilde_expands_only_to_the_home_directory() {
        let home = Some(Path::new("/home/me"));
        assert_eq!(expand_home("~", home), "/home/me");
        assert_eq!(expand_home("~/下载 目录", home), "/home/me/下载 目录");
        assert_eq!(expand_home("~other/x", home), "~other/x");
        assert_eq!(expand_home("a/~/b", home), "a/~/b");
        assert_eq!(expand_home("/srv/data", home), "/srv/data");
        assert_eq!(expand_home("~/x", None), "~/x");
        // 家目录自己带斜杠结尾（根用户在某些环境里是 `/`）不产生双斜杠。
        assert_eq!(expand_home("~/x", Some(Path::new("/"))), "/x");
    }

    #[test]
    fn posix_destinations_must_be_absolute() {
        let home = Some(Path::new("/home/me"));
        assert_eq!(
            posix_dest_shape("/srv//data/", home).as_deref(),
            Ok("/srv/data")
        );
        assert_eq!(posix_dest_shape("/", home).as_deref(), Ok("/"));
        assert_eq!(
            posix_dest_shape("~/Downloads", home).as_deref(),
            Ok("/home/me/Downloads")
        );
        assert_eq!(
            posix_dest_shape("/a/b/../c", home).as_deref(),
            Ok("/a/c"),
            "`..` 按文字整理"
        );
        // 反斜杠不是分隔符；`D:\x` 在 Linux 上是个相对路径，要拒绝。
        assert_eq!(
            posix_dest_shape(r"D:\x", home),
            Err(dest_need_absolute_posix())
        );
        assert_eq!(
            posix_dest_shape("relative/dir", home),
            Err(dest_need_absolute_posix())
        );
        assert_eq!(
            posix_dest_shape("~other", home),
            Err(dest_need_absolute_posix())
        );
        assert_eq!(
            posix_dest_shape("~/x", None),
            Err(dest_need_absolute_posix())
        );
        assert!(posix_dest_shape("/a\0b", home).is_err());
        // 反斜杠、冒号、问号都是合法的文件名字符。
        assert_eq!(
            posix_dest_shape(r"/srv/a\b:c?d", home).as_deref(),
            Ok(r"/srv/a\b:c?d")
        );
    }

    fn dest_need_absolute_posix() -> String {
        t(SftpMsg::MoveDestNeedAbsoluteLocalPosix).to_owned()
    }

    #[test]
    fn typed_paths_resolve_like_a_shell_cd() {
        let home = Some(Path::new("/home/me"));
        let resolve = |cwd, typed| posix_resolve_typed(cwd, typed, home);
        assert_eq!(resolve("/srv", "/etc/").as_deref(), Some("/etc"));
        assert_eq!(resolve("/srv", "data/x").as_deref(), Some("/srv/data/x"));
        assert_eq!(resolve("/srv/app", "..").as_deref(), Some("/srv"));
        assert_eq!(resolve("/", "..").as_deref(), Some("/"));
        assert_eq!(resolve("/", "tmp").as_deref(), Some("/tmp"));
        assert_eq!(
            resolve("/srv", "~/下载 目录").as_deref(),
            Some("/home/me/下载 目录")
        );
        assert_eq!(resolve("/srv", " ~ ").as_deref(), Some("/home/me"));
        assert_eq!(resolve("/srv", r"a\b").as_deref(), Some(r"/srv/a\b"));
        assert_eq!(resolve("/srv", "   "), None);
        assert_eq!(resolve("/srv", ""), None);
    }

    #[test]
    fn posix_names_only_exclude_slash_nul_and_dots() {
        assert_eq!(
            posix_child_name("  报告 (1).txt ").as_deref(),
            Ok("报告 (1).txt")
        );
        // Windows 才有的限制在 Linux 上都是合法名字。
        for ok in [
            r"a\b", "a:b", "a*b", "a?b", "a\"b", "a<b>", "a|b", "CON", "aux.txt", "tail.",
            ".bashrc", "x y",
        ] {
            assert_eq!(posix_child_name(ok).as_deref(), Ok(ok), "{ok:?}");
        }
        for bad in ["", "  ", ".", "..", "a/b", "/", "a\0b"] {
            assert!(posix_child_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn last_segment_follows_the_platform_separators() {
        assert_eq!(last_segment("/a/b/c.txt"), "c.txt");
        assert_eq!(last_segment("/a/b/"), "b");
        assert_eq!(last_segment("/"), "/");
        #[cfg(unix)]
        assert_eq!(last_segment(r"/a/b\c.txt"), r"b\c.txt");
        #[cfg(windows)]
        assert_eq!(last_segment(r"C:\w\x y.txt"), "x y.txt");
    }

    #[cfg(unix)]
    #[test]
    fn uploads_keep_backslashes_in_posix_names() {
        assert_eq!(local_file_name(r"/tmp/a\b.txt"), r"a\b.txt");
        assert_eq!(local_file_name("/tmp/下载 目录/报告.txt"), "报告.txt");
        assert_eq!(local_file_name("/tmp/dir/"), "dir");
        assert!(local_child_name(r"a\b:c").is_ok());
        assert!(local_child_name("a/b").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn uploads_split_windows_paths_on_backslashes() {
        assert_eq!(local_file_name(r"C:\w\a b.txt"), "a b.txt");
        assert!(local_child_name(r"a\b").is_err());
        assert!(local_child_name("a:b").is_err());
        assert!(local_child_name("CON").is_err());
    }
}
