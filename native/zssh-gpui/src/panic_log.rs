//! panic 日志：出货版 panic 之后进程直接没了（`0xc0000409`、窗口过程里的 panic 会 abort），什么记录也不留，
//! 用户报「闪退」时没有任何线索。这里在启动时装一个 panic hook，把一条记录追加写到数据目录的
//! `logs/panic.log`，再交给原来的 hook（调试构建照旧在 stderr 打出来）。
//!
//! 只写四样东西：时间（UTC）、版本、线程名、panic 的位置和消息。不写调用栈（出货版去了符号，没用）、
//! 不写任何用户数据：位置只留 crate 内的相对路径（不带构建机 / 用户的绝对路径），消息里出现的用户目录换成
//! `~`、控制字符去掉、太长截断。文件超过 [`MAX_BYTES`] 就把旧的挪成 `panic.log.old`（只留这一份）再重新写，
//! 所以最多占两倍这个大小。整个过程不许再 panic，任何 IO 错误都吞掉。

use std::fs;
use std::io::Write as _;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};

/// 日志文件超过这个大小就轮转。
const MAX_BYTES: u64 = 256 * 1024;
/// 一条记录里消息最多留这么多个字符。
const MAX_MESSAGE_CHARS: usize = 500;
const FILE_NAME: &str = "panic.log";
const OLD_NAME: &str = "panic.log.old";

/// 装 panic hook。数据目录拿不到就什么也不装（不影响启动）。
pub(crate) fn install() {
    let Some(dir) = crate::settings::try_data_dir() else {
        return;
    };
    let logs = dir.join("logs");
    let home = home_dir_text();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let entry = entry_for(info, home.as_deref());
        append(&logs, &entry);
        previous(info);
    }));
}

fn home_dir_text() -> Option<String> {
    std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned())
}

fn entry_for(info: &PanicHookInfo<'_>, home: Option<&str>) -> String {
    let payload = info.payload();
    let message = payload
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(non-text payload)".to_owned());
    let location = info
        .location()
        .map(|place| (place.file().to_owned(), place.line(), place.column()));
    let thread = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_owned();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default();
    format_entry(
        env!("CARGO_PKG_VERSION"),
        secs,
        &thread,
        location
            .as_ref()
            .map(|(file, line, col)| (file.as_str(), *line, *col)),
        &message,
        home,
    )
}

/// 一条记录（一行头 + 一行消息 + 空行）。纯函数，方便测。
pub(crate) fn format_entry(
    version: &str,
    unix_secs: i64,
    thread: &str,
    location: Option<(&str, u32, u32)>,
    message: &str,
    home: Option<&str>,
) -> String {
    let place = location.map_or_else(
        || "<unknown location>".to_owned(),
        |(file, line, col)| format!("{}:{line}:{col}", short_path(file)),
    );
    format!(
        "[{}] v{version} thread '{}' panicked at {place}\n{}\n\n",
        utc_text(unix_secs),
        one_line(thread, 60),
        one_line(&scrub_home(message, home), MAX_MESSAGE_CHARS),
    )
}

/// `2026-11-07T12:34:56Z`。
fn utc_text(unix_secs: i64) -> String {
    let days = unix_secs.div_euclid(86_400);
    let rest = unix_secs.rem_euclid(86_400);
    let (year, month, day) = crate::datetime::civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// 源码位置只留 crate 内的相对路径：依赖里的 `.../registry/src/<索引>/<crate>/src/x.rs` 留 `<crate>/src/x.rs`，
/// 自己的 `.../native/...`、`.../crates/...` 留从那一级开始，相对路径原样留，其余绝对路径只留最后三段。绝对路径里可能有构建机或用户的名字。
fn short_path(file: &str) -> String {
    let path = file.replace('\\', "/");
    if let Some(index) = path.find("/registry/src/") {
        let after = &path[index + "/registry/src/".len()..];
        return after
            .split_once('/')
            .map_or(after, |(_, rest)| rest)
            .to_owned();
    }
    for marker in ["/native/", "/crates/"] {
        if let Some(index) = path.find(marker) {
            return path[index + 1..].to_owned();
        }
    }
    // 工作区成员的源码位置本来就是相对路径（`zssh-gpui/src/x.rs`），原样保留。
    let absolute = path.starts_with('/') || path.as_bytes().get(1) == Some(&b':');
    if !absolute {
        return path;
    }
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    parts[parts.len().saturating_sub(3)..].join("/")
}

/// 消息里出现的用户目录换成 `~`（不分大小写、斜杠方向都认）。
fn scrub_home(message: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| home.chars().count() >= 3) else {
        return message.to_owned();
    };
    let mut out = message.to_owned();
    for variant in [
        home.to_owned(),
        home.replace('\\', "/"),
        home.replace('/', "\\"),
    ] {
        out = replace_ignore_case(&out, &variant, "~");
    }
    out
}

fn replace_ignore_case(text: &str, needle: &str, with: &str) -> String {
    let (hay, needle) = (text.to_lowercase(), needle.to_lowercase());
    // 小写化改变了字节长度（极少见的 Unicode 情形）时位置对不上，原样返回，宁可不替换也不切错。
    if hay.len() != text.len() || needle.is_empty() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(found) = hay[from..].find(&needle) {
        let start = from + found;
        out.push_str(&text[from..start]);
        out.push_str(with);
        from = start + needle.len();
    }
    out.push_str(&text[from..]);
    out
}

/// 压成一行、去掉控制字符、最多 `max` 个字符（超出加 `…`）。
fn one_line(text: &str, max: usize) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut cut: String = flat.chars().take(max).collect();
    cut.push('…');
    cut
}

/// 追加一条记录；文件已经超过上限先轮转。失败一律吞掉。
pub(crate) fn append(logs: &Path, entry: &str) {
    if fs::create_dir_all(logs).is_err() {
        return;
    }
    let path: PathBuf = logs.join(FILE_NAME);
    rotate_if_large(logs, MAX_BYTES);
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(entry.as_bytes());
    }
}

/// `panic.log` 超过 `max` 字节就挪成 `panic.log.old`（覆盖旧的那份）。
fn rotate_if_large(logs: &Path, max: u64) {
    let path = logs.join(FILE_NAME);
    let large = fs::metadata(&path).is_ok_and(|meta| meta.len() >= max);
    if large {
        let old = logs.join(OLD_NAME);
        let _ = fs::remove_file(&old);
        let _ = fs::rename(&path, &old);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FILE_NAME, MAX_MESSAGE_CHARS, OLD_NAME, append, format_entry, one_line, rotate_if_large,
        scrub_home, short_path, utc_text,
    };

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!(
            "zssh-panic-log-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn the_time_is_utc_iso() {
        assert_eq!(utc_text(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_text(1_794_000_000), "2026-11-06T21:20:00Z");
        assert_eq!(utc_text(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn locations_never_carry_a_home_or_build_machine_path() {
        assert_eq!(
            short_path(
                r"C:\Users\someone\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\gpui-pre-0.3.10\src\app\entity_map.rs"
            ),
            "gpui-pre-0.3.10/src/app/entity_map.rs"
        );
        assert_eq!(
            short_path(r"D:\a\ZeeTerm\ZeeTerm\native\zssh-gpui\src\file_window.rs"),
            "native/zssh-gpui/src/file_window.rs"
        );
        assert_eq!(
            short_path("/home/me/build/crates/zssh-core/src/lib.rs"),
            "crates/zssh-core/src/lib.rs"
        );
        assert_eq!(short_path("/opt/x/y/z/w.rs"), "y/z/w.rs");
        assert_eq!(short_path("w.rs"), "w.rs");
        assert_eq!(
            short_path(r"zssh-gpui\src\main_window\dev_acts_dock.rs"),
            "zssh-gpui/src/main_window/dev_acts_dock.rs"
        );
    }

    #[test]
    fn the_home_directory_in_a_message_becomes_a_tilde() {
        let home = r"C:\Users\someone";
        assert_eq!(
            scrub_home(r"cannot open C:\Users\someone\Documents\a.txt", Some(home)),
            r"cannot open ~\Documents\a.txt"
        );
        assert_eq!(
            scrub_home("cannot open c:/users/SOMEONE/a.txt", Some(home)),
            "cannot open ~/a.txt"
        );
        // 没有用户目录 / 太短的不替换。
        assert_eq!(scrub_home("abc", None), "abc");
        assert_eq!(scrub_home("a/b", Some("/")), "a/b");
    }

    #[test]
    fn a_message_is_one_short_clean_line() {
        assert_eq!(one_line("a\nb\r\n\tc\u{7}d", 50), "a b c d");
        let long = "x".repeat(MAX_MESSAGE_CHARS + 50);
        let cut = one_line(&long, MAX_MESSAGE_CHARS);
        assert_eq!(cut.chars().count(), MAX_MESSAGE_CHARS + 1);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn an_entry_has_time_version_thread_place_and_message_only() {
        let text = format_entry(
            "1.0.1",
            0,
            "main",
            Some((r"D:\a\ZeeTerm\native\zssh-gpui\src\file_window.rs", 592, 9)),
            "cannot read MainWindow while it is already being updated",
            None,
        );
        assert_eq!(
            text,
            "[1970-01-01T00:00:00Z] v1.0.1 thread 'main' panicked at native/zssh-gpui/src/file_window.rs:592:9\ncannot read MainWindow while it is already being updated\n\n"
        );
        let unknown = format_entry("1.0.1", 0, "t", None, "boom", None);
        assert!(unknown.contains("panicked at <unknown location>"));
    }

    #[test]
    fn appending_creates_the_folder_and_keeps_adding() {
        let dir = temp_dir("append").join("logs");
        append(&dir, "one\n");
        append(&dir, "two\n");
        let text = std::fs::read_to_string(dir.join(FILE_NAME)).expect("读日志");
        assert_eq!(text, "one\ntwo\n");
        let _ = std::fs::remove_dir_all(dir.parent().expect("上一级"));
    }

    #[test]
    fn a_big_log_is_rotated_to_one_old_file() {
        let dir = temp_dir("rotate");
        std::fs::write(dir.join(FILE_NAME), vec![b'a'; 100]).expect("写");
        // 没到上限：不动。
        rotate_if_large(&dir, 101);
        assert!(!dir.join(OLD_NAME).exists());
        // 到了上限：挪成 .old，原文件没了（下一条重新写）。
        rotate_if_large(&dir, 100);
        assert!(!dir.join(FILE_NAME).exists());
        assert_eq!(std::fs::read(dir.join(OLD_NAME)).expect("读").len(), 100);
        // 再轮转一次：旧的 .old 被覆盖，不会越攒越多。
        std::fs::write(dir.join(FILE_NAME), vec![b'b'; 120]).expect("写");
        rotate_if_large(&dir, 100);
        assert_eq!(
            std::fs::read(dir.join(OLD_NAME)).expect("读"),
            vec![b'b'; 120]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
