//! 崩溃后提示反馈（V2-U62）：读 `panic.log`（`panic_log` 写的），判断「用户有没有看过」，拼出反馈地址。
//!
//! 「看过」靠数据目录 `logs/panic.seen` 记：内容是用户已经处理过的最后一条记录的时间戳（记录头一行方括号里那段，
//! 如 `2026-11-07T12:34:56Z`，UTC、定宽，可以直接按字符串比大小）。启动时把 `panic.log.old` 和 `panic.log`
//! 里比它新的记录找出来，有就提示一次；用户点「反馈问题」或「不用了」时把标记更新到最新一条，
//! 同一批记录不会再提示。
//!
//! - 标记文件不存在（第一次装上带这个功能的版本、老用户升级上来）：不弹提示，直接把标记写成现有最新一条的时间戳，
//!   免得对很久以前的旧记录弹窗；一条记录都没有就写 1970 年的起点，之后的任何一次崩溃都算新的。
//!   标记里的内容读不懂时同样当作不存在重新播种。
//! - 标记文件读不了、日志读不了：当作没有新记录，不弹、不崩。
//! - 只读 `panic.log`，不改它的写入格式；`panic.seen` 是这里自己的文件，只在播种和用户处理过之后写。
//!
//! ZeeTerm 自己不发任何网络请求：这里只产出一个预填好的地址，由主窗口交给浏览器，用户在网页上看过、改过再自己提交。

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::crash_scrub::{self, Names};
use crate::feedback;
use crate::panic_log;

/// 标记文件名（在 `logs/` 下）。
pub(crate) const MARKER_NAME: &str = "panic.seen";
/// 没有任何记录时播种用的时间戳：比任何真实记录都早。
const EPOCH: &str = "1970-01-01T00:00:00Z";
/// 一次最多带进反馈页的记录条数（最近的几条）。
pub(crate) const MAX_REPORT_RECORDS: usize = 3;
/// 一条记录最多读这么多行正文（`panic_log` 写的消息只有一行，多出来的不是我们写的）。
const MAX_BODY_LINES: usize = 20;

/// `panic.log` 里的一条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    /// 头一行方括号里的时间戳。
    pub timestamp: String,
    /// 头一行原文（时间、版本、线程、源码位置）。
    pub header: String,
    /// 头一行之后、空行之前的正文（消息）。
    pub body: Vec<String>,
}

/// 启动时发现的、还没处理过的崩溃记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pending {
    /// 比标记新的记录，从旧到新。
    pub records: Vec<Record>,
    /// 其中最新一条的时间戳：用户处理之后写进标记。
    pub latest: String,
}

/// 标记文件当前的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Marker {
    Missing,
    /// 文件在，内容不是时间戳。
    Invalid,
    /// 读不了（权限、被占用……）：不确定，什么都不做。
    Unreadable,
    Seen(String),
}

/// 启动时对「要不要提示」的判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    Nothing,
    /// 不提示，把标记写成这个时间戳。
    Seed(String),
    Prompt(Pending),
}

/// `YYYY-MM-DDTHH:MM:SSZ`：定宽、各位置是数字、月日时分秒在范围内。
pub(crate) fn is_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 20 {
        return false;
    }
    let digit_at = |from: usize, to: usize| bytes[from..to].iter().all(u8::is_ascii_digit);
    let value = |from: usize, to: usize| text[from..to].parse::<u32>().unwrap_or(u32::MAX);
    digit_at(0, 4)
        && bytes[4] == b'-'
        && digit_at(5, 7)
        && bytes[7] == b'-'
        && digit_at(8, 10)
        && bytes[10] == b'T'
        && digit_at(11, 13)
        && bytes[13] == b':'
        && digit_at(14, 16)
        && bytes[16] == b':'
        && digit_at(17, 19)
        && bytes[19] == b'Z'
        && (1..=12).contains(&value(5, 7))
        && (1..=31).contains(&value(8, 10))
        && value(11, 13) < 24
        && value(14, 16) < 60
        && value(17, 19) < 61
}

/// 记录头一行 `[时间戳] v… thread '…' panicked at …` 里的时间戳；不是记录头返回 `None`。
fn header_timestamp(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let (stamp, after) = rest.split_once(']')?;
    (is_timestamp(stamp) && after.starts_with(' ')).then_some(stamp)
}

/// 从日志文字里解析记录。一条记录是「头一行 + 正文若干行 + 空行」；
/// 认不出的行（半截行、乱码、没有头的正文）一律跳过，不影响别的记录。
pub(crate) fn parse_records(text: &str) -> Vec<Record> {
    let mut records = Vec::new();
    let mut current: Option<Record> = None;
    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        if let Some(stamp) = header_timestamp(line) {
            records.extend(current.take());
            current = Some(Record {
                timestamp: stamp.to_owned(),
                header: line.to_owned(),
                body: Vec::new(),
            });
        } else if line.trim().is_empty() {
            records.extend(current.take());
        } else if let Some(record) = current.as_mut()
            && record.body.len() < MAX_BODY_LINES
        {
            record.body.push(line.to_owned());
        }
    }
    records.extend(current);
    records
}

/// 标记的内容：第一行去掉空白后是时间戳就算数。
pub(crate) fn parse_marker(text: &str) -> Marker {
    let first = text
        .trim_start_matches('\u{feff}')
        .lines()
        .next()
        .unwrap_or("")
        .trim();
    if is_timestamp(first) {
        Marker::Seen(first.to_owned())
    } else {
        Marker::Invalid
    }
}

/// 纯逻辑：标记和已有的记录 → 做什么。`records` 按文件里的顺序给（先 `.old` 后当前）。
pub(crate) fn decide(marker: Marker, records: Vec<Record>) -> Decision {
    match marker {
        Marker::Unreadable => Decision::Nothing,
        Marker::Missing | Marker::Invalid => {
            let latest = records
                .iter()
                .map(|record| record.timestamp.as_str())
                .max()
                .unwrap_or(EPOCH);
            Decision::Seed(latest.to_owned())
        }
        Marker::Seen(seen) => {
            let mut fresh: Vec<Record> = records
                .into_iter()
                .filter(|record| record.timestamp > seen)
                .collect();
            // 稳定排序：同一秒的记录保持文件里的先后。
            fresh.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
            match fresh.last().map(|record| record.timestamp.clone()) {
                Some(latest) => Decision::Prompt(Pending {
                    records: fresh,
                    latest,
                }),
                None => Decision::Nothing,
            }
        }
    }
}

fn read_marker(logs: &Path) -> Marker {
    match fs::read(logs.join(MARKER_NAME)) {
        Ok(bytes) => parse_marker(&String::from_utf8_lossy(&bytes)),
        Err(err) if err.kind() == ErrorKind::NotFound => Marker::Missing,
        Err(_) => Marker::Unreadable,
    }
}

/// 读一份日志；不存在、读不了都当空。
fn read_log(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// 先 `.old` 后当前：轮转发生在两次启动之间时，较早的记录在 `.old` 里。
fn read_all_records(logs: &Path) -> Vec<Record> {
    let mut records = parse_records(&read_log(&logs.join(panic_log::OLD_NAME)));
    records.extend(parse_records(&read_log(&logs.join(panic_log::FILE_NAME))));
    records
}

/// 启动时调用（在后台线程里）：读标记和日志，需要播种就写标记，有新记录就返回要提示的内容。
pub(crate) fn scan(logs: &Path) -> Option<Pending> {
    match decide(read_marker(logs), read_all_records(logs)) {
        Decision::Nothing => None,
        Decision::Seed(stamp) => {
            mark_seen(logs, &stamp);
            None
        }
        Decision::Prompt(pending) => Some(pending),
    }
}

/// 把标记写成 `stamp`（用户处理过这一批记录了）。写不进去只记一条日志：下次启动会再提示一次，不影响别的。
pub(crate) fn mark_seen(logs: &Path, stamp: &str) {
    if !is_timestamp(stamp) {
        return;
    }
    let written = fs::create_dir_all(logs).and_then(|()| fs::write(logs.join(MARKER_NAME), stamp));
    if let Err(err) = written {
        tracing::warn!("没能写崩溃记录的已读标记：{err}");
    }
}

/// 一条记录清洗后的文字：头一行只做用户目录 / 邮箱 / IP，正文再加上主机库和本机的名字。
pub(crate) fn record_text(record: &Record, names: &Names) -> String {
    let mut lines = vec![crash_scrub::scrub_header(&record.header, names)];
    lines.extend(
        record
            .body
            .iter()
            .map(|line| crash_scrub::scrub(line, names)),
    );
    lines.join("\n")
}

/// 把要反馈的记录拼成预填好的「报告问题」地址：最近 [`MAX_REPORT_RECORDS`] 条（从旧到新）清洗后放进「日志」框；
/// 整个地址放不下就从最旧的一条开始丢，只剩一条还放不下就截断它的末尾（加 `…`）。
pub(crate) fn report_url(
    version: &str,
    os: &str,
    install: Option<&str>,
    records: &[Record],
    names: &Names,
) -> String {
    let start = records.len().saturating_sub(MAX_REPORT_RECORDS);
    let mut texts: Vec<String> = records[start..]
        .iter()
        .map(|record| record_text(record, names))
        .collect();
    while texts.len() > 1 && !feedback::bug_url_fits(version, os, install, &texts.join("\n\n")) {
        texts.remove(0);
    }
    feedback::bug_url(version, os, install, Some(&texts.join("\n\n")))
}

/// 日志文件夹：`<数据目录>/logs`。
pub(crate) fn logs_dir(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(stamp: &str, message: &str) -> String {
        format!(
            "[{stamp}] v1.1.2 thread 'main' panicked at native/zeeterm-gpui/src/x.rs:12:3\n{message}\n\n"
        )
    }

    fn record(stamp: &str, message: &str) -> Record {
        Record {
            timestamp: stamp.to_owned(),
            header: format!(
                "[{stamp}] v1.1.2 thread 'main' panicked at native/zeeterm-gpui/src/x.rs:12:3"
            ),
            body: vec![message.to_owned()],
        }
    }

    fn temp_logs(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = tempfile::Builder::new()
            .prefix(&format!("zeeterm-crash-{tag}-"))
            .tempdir()
            .expect("建临时目录");
        let logs = root.path().join("logs");
        (root, logs)
    }

    const T1: &str = "2026-11-07T12:34:56Z";
    const T2: &str = "2026-11-08T01:02:03Z";
    const T3: &str = "2026-11-09T23:59:59Z";

    #[test]
    fn timestamps_are_fixed_width_utc() {
        for good in [T1, "1970-01-01T00:00:00Z", "2026-12-31T23:59:59Z"] {
            assert!(is_timestamp(good), "{good}");
        }
        for bad in [
            "",
            "2026-11-07T12:34:56",
            "2026-11-07 12:34:56Z",
            "2026-13-07T12:34:56Z",
            "2026-11-32T12:34:56Z",
            "2026-11-07T24:34:56Z",
            "2026-11-07T12:60:56Z",
            "2026-1-07T12:34:56Z",
            "26-11-07T12:34:56ZZZ",
            "abcd-ef-ghTij:kl:mnZ",
            "2026-11-07T12:34:56Z ",
        ] {
            assert!(!is_timestamp(bad), "{bad}");
        }
    }

    #[test]
    fn records_are_parsed_from_the_panic_log_format() {
        let text = format!("{}{}", entry(T1, "first"), entry(T2, "second"));
        let records = parse_records(&text);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].timestamp, T1);
        assert_eq!(records[0].body, vec!["first"]);
        assert_eq!(records[1].timestamp, T2);
        assert!(
            records[1]
                .header
                .starts_with("[2026-11-08T01:02:03Z] v1.1.2 thread 'main'")
        );
        // CRLF 也认。
        let crlf = text.replace('\n', "\r\n");
        assert_eq!(parse_records(&crlf), records);
        assert!(parse_records("").is_empty());
    }

    #[test]
    fn broken_lines_are_skipped_without_losing_good_records() {
        let text = format!(
            "garbage with no header\n\n[not a timestamp] v1 thread 'x'\nstray body\n\n{}[2026-99-99T00:00:00Z] v1 bad month\nlost\n\n[2026-11-08T01:02:03Z]no space\n\n{}\u{0}\u{1}binary\n[2026-11-09",
            entry(T1, "kept one"),
            entry(T3, "kept two"),
        );
        let records = parse_records(&text);
        let stamps: Vec<&str> = records
            .iter()
            .map(|record| record.timestamp.as_str())
            .collect();
        assert_eq!(stamps, vec![T1, T3], "{records:?}");
        // 坏头后面的正文不会并进前一条。
        assert_eq!(records[0].body, vec!["kept one"]);
        // 一条记录没有空行收尾（被截断）也算。
        let cut = parse_records(&format!(
            "[{T2}] v1 thread 't' panicked at x.rs:1:1\nhalf a mess"
        ));
        assert_eq!(cut.len(), 1);
        assert_eq!(cut[0].body, vec!["half a mess"]);
    }

    #[test]
    fn marker_text_must_start_with_a_timestamp() {
        assert_eq!(parse_marker(T1), Marker::Seen(T1.to_owned()));
        assert_eq!(
            parse_marker(&format!("  {T1}\r\nignored")),
            Marker::Seen(T1.to_owned())
        );
        assert_eq!(
            parse_marker(&format!("\u{feff}{T1}")),
            Marker::Seen(T1.to_owned())
        );
        for bad in ["", "yesterday", "2026-11-07", "[2026-11-07T12:34:56Z]"] {
            assert_eq!(parse_marker(bad), Marker::Invalid, "{bad:?}");
        }
    }

    #[test]
    fn a_missing_marker_seeds_to_the_newest_record_without_prompting() {
        let records = vec![record(T1, "a"), record(T3, "c"), record(T2, "b")];
        assert_eq!(
            decide(Marker::Missing, records.clone()),
            Decision::Seed(T3.to_owned())
        );
        assert_eq!(
            decide(Marker::Invalid, records),
            Decision::Seed(T3.to_owned())
        );
        // 没有任何记录：用起点播种，之后的第一次崩溃就算新的。
        assert_eq!(
            decide(Marker::Missing, Vec::new()),
            Decision::Seed(EPOCH.to_owned())
        );
    }

    #[test]
    fn records_newer_than_the_marker_prompt_and_older_ones_do_not() {
        let records = vec![record(T1, "a"), record(T2, "b"), record(T3, "c")];
        match decide(Marker::Seen(T1.to_owned()), records.clone()) {
            Decision::Prompt(pending) => {
                assert_eq!(pending.latest, T3);
                let stamps: Vec<&str> = pending
                    .records
                    .iter()
                    .map(|record| record.timestamp.as_str())
                    .collect();
                assert_eq!(stamps, vec![T2, T3]);
            }
            other => panic!("应该提示：{other:?}"),
        }
        // 标记等于最新一条、或比所有记录都新：没有新记录。
        assert_eq!(
            decide(Marker::Seen(T3.to_owned()), records.clone()),
            Decision::Nothing
        );
        assert_eq!(
            decide(
                Marker::Seen("2030-01-01T00:00:00Z".to_owned()),
                records.clone()
            ),
            Decision::Nothing
        );
        assert_eq!(
            decide(Marker::Seen(T1.to_owned()), Vec::new()),
            Decision::Nothing
        );
        // 读不了标记：什么都不做，也不播种。
        assert_eq!(decide(Marker::Unreadable, records), Decision::Nothing);
    }

    #[test]
    fn scanning_a_fresh_install_writes_the_marker_and_stays_quiet() {
        let (_root, logs) = temp_logs("fresh");
        fs::create_dir_all(&logs).expect("建目录");
        fs::write(logs.join(panic_log::FILE_NAME), entry(T1, "old crash")).expect("写");
        assert_eq!(scan(&logs), None);
        assert_eq!(
            fs::read_to_string(logs.join(MARKER_NAME)).expect("读标记"),
            T1
        );
        // 之后再来一条新的：提示。
        panic_log::append(&logs, &entry(T2, "new crash"));
        let pending = scan(&logs).expect("应该有新记录");
        assert_eq!(pending.latest, T2);
        assert_eq!(pending.records.len(), 1);
        // 处理完之后同一批不再提示。
        mark_seen(&logs, &pending.latest);
        assert_eq!(scan(&logs), None);
        assert_eq!(
            fs::read_to_string(logs.join(MARKER_NAME)).expect("读标记"),
            T2
        );
    }

    #[test]
    fn scanning_with_no_log_at_all_seeds_the_epoch_and_the_first_crash_prompts() {
        let (_root, logs) = temp_logs("nolog");
        assert_eq!(scan(&logs), None);
        assert_eq!(
            fs::read_to_string(logs.join(MARKER_NAME)).expect("读标记"),
            EPOCH
        );
        panic_log::append(&logs, &entry(T1, "first ever"));
        let pending = scan(&logs).expect("第一次崩溃要提示");
        assert_eq!(pending.latest, T1);
    }

    #[test]
    fn records_in_the_rotated_old_file_are_read_before_the_current_one() {
        let (_root, logs) = temp_logs("rotated");
        fs::create_dir_all(&logs).expect("建目录");
        fs::write(logs.join(MARKER_NAME), T1).expect("写标记");
        // 轮转刚好发生在两次启动之间：T2 在 .old 里，T3 在新的 panic.log 里。
        fs::write(
            logs.join(panic_log::OLD_NAME),
            format!("{}{}", entry(T1, "seen"), entry(T2, "in old")),
        )
        .expect("写");
        fs::write(logs.join(panic_log::FILE_NAME), entry(T3, "in current")).expect("写");
        let pending = scan(&logs).expect("应该有新记录");
        let stamps: Vec<&str> = pending
            .records
            .iter()
            .map(|record| record.timestamp.as_str())
            .collect();
        assert_eq!(stamps, vec![T2, T3]);
        assert_eq!(pending.latest, T3);
        // 只有 .old 也认。
        fs::remove_file(logs.join(panic_log::FILE_NAME)).expect("删");
        assert_eq!(scan(&logs).expect("仍有新记录").latest, T2);
    }

    #[test]
    fn a_garbled_marker_is_reseeded_and_a_garbled_log_does_not_prompt() {
        let (_root, logs) = temp_logs("garbled");
        fs::create_dir_all(&logs).expect("建目录");
        fs::write(logs.join(MARKER_NAME), "not a time").expect("写");
        fs::write(logs.join(panic_log::FILE_NAME), entry(T2, "x")).expect("写");
        assert_eq!(scan(&logs), None, "标记坏了：重新播种，不弹");
        assert_eq!(
            fs::read_to_string(logs.join(MARKER_NAME)).expect("读标记"),
            T2
        );
        // 日志全是乱码：没有记录，没有提示。
        fs::write(
            logs.join(panic_log::FILE_NAME),
            [0xff_u8, 0xfe, 0x00, b'\n', 0x80],
        )
        .expect("写");
        assert_eq!(scan(&logs), None);
    }

    #[test]
    fn an_unreadable_marker_means_do_nothing() {
        let (_root, logs) = temp_logs("unreadable");
        fs::create_dir_all(&logs).expect("建目录");
        // 标记的位置上是个目录：读它失败（不是「不存在」）。
        fs::create_dir_all(logs.join(MARKER_NAME)).expect("建目录");
        fs::write(logs.join(panic_log::FILE_NAME), entry(T2, "x")).expect("写");
        assert_eq!(read_marker(&logs), Marker::Unreadable);
        assert_eq!(scan(&logs), None);
        assert!(logs.join(MARKER_NAME).is_dir(), "不动它");
    }

    #[test]
    fn marking_ignores_values_that_are_not_timestamps() {
        let (_root, logs) = temp_logs("badmark");
        mark_seen(&logs, "whatever");
        assert!(!logs.join(MARKER_NAME).exists());
        mark_seen(&logs, T1);
        assert_eq!(
            fs::read_to_string(logs.join(MARKER_NAME)).expect("读标记"),
            T1
        );
    }

    #[test]
    fn the_report_carries_the_newest_three_records_scrubbed() {
        let names = Names {
            hosts: vec!["web-1".into()],
            users: vec!["deploy".into()],
            ..Names::default()
        };
        let records = vec![
            record(T1, "oldest 1"),
            record("2026-11-07T13:00:00Z", "kept A for deploy"),
            record("2026-11-07T14:00:00Z", "kept B web-1 at 192.0.2.10"),
            record(T3, "kept C mail a@example.com"),
        ];
        let url = report_url(
            "v1.1.2",
            "Windows 22631",
            Some(feedback::INSTALL_INSTALLER),
            &records,
            &names,
        );
        assert!(url.contains("template=bug_report.yml"));
        assert!(url.contains("&version=v1.1.2&os=Windows%2022631&install="));
        let logs = url.split("&logs=").nth(1).expect("有 logs 参数");
        let decoded = decode(logs);
        assert!(!decoded.contains("oldest 1"), "只带最近 3 条：{decoded}");
        assert!(decoded.contains("kept A for <user>"), "{decoded}");
        assert!(decoded.contains("kept B <host> at <ip>"), "{decoded}");
        assert!(decoded.contains("kept C mail <email>"), "{decoded}");
        for private in ["deploy", "web-1", "192.0.2.10", "a@example.com"] {
            assert!(!decoded.contains(private), "{private} 泄漏了：{decoded}");
        }
        // 头一行（版本、线程、源码位置）原样保留。
        assert!(decoded.contains("thread 'main' panicked at native/zeeterm-gpui/src/x.rs:12:3"));
        // 记录之间空一行，从旧到新。
        let a = decoded.find("kept A").expect("A");
        let c = decoded.find("kept C").expect("C");
        assert!(a < c);
        assert!(decoded.contains("\n\n[2026-11-07T14:00:00Z]"), "{decoded}");
    }

    #[test]
    fn a_report_that_is_too_long_drops_the_oldest_records_first() {
        let names = Names::default();
        let big = "x".repeat(2400);
        let records = vec![
            record("2026-11-07T10:00:00Z", &format!("first {big}")),
            record("2026-11-07T11:00:00Z", &format!("second {big}")),
            record("2026-11-07T12:00:00Z", &format!("third {big}")),
        ];
        let url = report_url("v1.1.2", "Windows 22631", None, &records, &names);
        assert!(
            url.chars().count() <= feedback::MAX_URL_CHARS,
            "{}",
            url.chars().count()
        );
        let decoded = decode(url.split("&logs=").nth(1).expect("logs"));
        assert!(!decoded.contains("first "), "最旧的先丢");
        assert!(
            decoded.contains("second ") && decoded.contains("third "),
            "{decoded}"
        );
        assert!(!decoded.ends_with('…'), "丢整条就够放下，不必截断");
    }

    #[test]
    fn a_single_record_that_is_still_too_long_is_cut_with_an_ellipsis() {
        let names = Names::default();
        let records = vec![record(T2, &"很长的消息".repeat(3000))];
        let url = report_url(
            "v1.1.2",
            "Windows 22631",
            Some(feedback::INSTALL_PORTABLE),
            &records,
            &names,
        );
        assert!(
            url.chars().count() <= feedback::MAX_URL_CHARS,
            "{}",
            url.chars().count()
        );
        let decoded = decode(url.split("&logs=").nth(1).expect("logs"));
        assert!(decoded.ends_with('…'));
        // 头一行还在。
        assert!(
            decoded.starts_with(&format!("[{T2}] v1.1.2 thread 'main'")),
            "{decoded}"
        );
    }

    #[test]
    fn the_logs_folder_is_under_the_data_dir() {
        assert_eq!(logs_dir(Path::new("data")), Path::new("data").join("logs"));
    }

    /// 测试用的百分号解码（只需要处理本模块编出来的东西）。
    fn decode(encoded: &str) -> String {
        let bytes = encoded.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).expect("十六进制");
                out.push(u8::from_str_radix(hex, 16).expect("十六进制"));
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("UTF-8")
    }
}
