//! 崩溃记录发出去之前的清洗（V2-U62）。
//!
//! `panic.log` 写入时已经去掉了调用栈、把用户目录换成 `~`，但消息里仍可能带着服务器地址、主机名、用户名
//! （比如一条连接报错的原文）。这里在「反馈」把记录预填进 GitHub 页面之前再清一遍：
//!
//! - 邮箱 → `<email>`；公网和内网的 IPv4、IPv6 地址 → `<ip>`；
//! - 主机库里每台主机的地址和名字 → `<host>`、用户名 → `<user>`（不分大小写，不到 3 个字符的跳过，免得把无关的字也换掉）；
//! - 本机用户名 → `<user>`、计算机名 → `<computer>`；
//! - 用户目录再换一遍 `~`（当前用户的目录原样认，别的用户的 `C:\Users\<名字>`、`/home/<名字>` 按形状认）。
//!
//! 每一步只处理还没被换过的文字：换进去的 `<host>`、`<email>` 不会被后面的步骤再匹配（否则主机名叫 `email`
//! 就会把 `<email>` 改成 `<<host>>`）。地址、主机名这类按「出现过就换」处理，不要求前后是词边界：宁可多换一点，
//! 也不漏掉嵌在别的文字里的用户名。记录头（时间、版本、线程、源码位置）是程序自己写的，只做用户目录、邮箱、IP 三项，
//! 不按主机库换，免得主机叫 `src` 之类时把源码路径改得面目全非。
//!
//! 不引入正则库：这几类都是固定形状，手写扫描器更容易写反例测试。

use std::collections::HashSet;
use std::net::Ipv6Addr;
use std::str::FromStr as _;

/// 清洗要用的名字清单。字段都是原样的文字，匹配时不分大小写。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Names {
    /// 主机的地址和名字 → `<host>`。
    pub hosts: Vec<String>,
    /// 用户名（主机库里的、本机的）→ `<user>`。
    pub users: Vec<String>,
    /// 本机的计算机名 → `<computer>`。
    pub computers: Vec<String>,
    /// 当前用户的目录 → `~`。
    pub home: Option<String>,
}

impl Names {
    /// 主机库里的每台主机，再加上这台电脑的用户名、计算机名、用户目录。
    pub(crate) fn collect(hosts: &[zeeterm_core::model::Host]) -> Self {
        let mut names = Self::default();
        for host in hosts {
            names.hosts.push(host.host.clone());
            names.hosts.push(host.name.clone());
            names.users.push(host.username.clone());
        }
        names.add_local(
            |key| std::env::var(key).ok(),
            std::env::home_dir().map(|dir| dir.to_string_lossy().into_owned()),
            local_hostname_file(),
        );
        names
    }

    /// 本机的名字：环境变量（`USERNAME` / `USER`，`COMPUTERNAME` / `HOSTNAME`）、用户目录，
    /// 以及 Unix 上 `/etc/hostname` 的内容（`HOSTNAME` 常常没导出）。
    pub(crate) fn add_local(
        &mut self,
        env: impl Fn(&str) -> Option<String>,
        home: Option<String>,
        hostname_file: Option<String>,
    ) {
        for key in ["USERNAME", "USER", "LOGNAME"] {
            self.users.extend(env(key));
        }
        for key in ["COMPUTERNAME", "HOSTNAME"] {
            self.computers.extend(env(key));
        }
        self.computers.extend(hostname_file);
        self.home = home;
    }

    /// 按长度从长到短排好的（名字, 替换成什么）。长的在前：`web-1.example.com` 先于 `web-1` 被整段换掉；
    /// 同一个名字（不分大小写）只留一份，先来的优先（主机 > 用户 > 计算机）。
    fn needles(&self) -> Vec<(Vec<char>, &'static str)> {
        let mut seen = HashSet::new();
        let mut list: Vec<(Vec<char>, &'static str)> = Vec::new();
        for (names, replacement) in [
            (&self.hosts, "<host>"),
            (&self.users, "<user>"),
            (&self.computers, "<computer>"),
        ] {
            for name in names {
                let name = name.trim();
                if name.chars().count() < 3 {
                    continue;
                }
                let key: String = name.chars().flat_map(char::to_lowercase).collect();
                if seen.insert(key) {
                    list.push((name.chars().collect(), replacement));
                }
            }
        }
        list.sort_by_key(|(chars, _)| std::cmp::Reverse(chars.len()));
        list
    }
}

#[cfg(unix)]
fn local_hostname_file() -> Option<String> {
    let text = std::fs::read_to_string("/etc/hostname").ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(not(unix))]
fn local_hostname_file() -> Option<String> {
    None
}

/// 清洗一段正文（记录的消息部分）。
pub(crate) fn scrub(text: &str, names: &Names) -> String {
    scrub_with(text, names, true)
}

/// 清洗记录头：只做用户目录、邮箱、IP，不按主机库换（见模块说明）。
pub(crate) fn scrub_header(text: &str, names: &Names) -> String {
    scrub_with(text, names, false)
}

/// 已处理的文字分成两种：还要继续扫的，和已经换成占位符、后面的步骤不许再碰的。
enum Piece {
    Raw(String),
    Done(&'static str),
}

/// 一处命中：`[start, end)` 的字符区间换成占位符。
type Hit = (usize, usize, &'static str);

fn scrub_with(text: &str, names: &Names, with_names: bool) -> String {
    let text = match names.home.as_deref() {
        Some(home) => crate::panic_log::scrub_home(text, Some(home)),
        None => text.to_owned(),
    };
    let text = user_dirs(&text);
    let mut pieces = vec![Piece::Raw(text)];
    pieces = apply(pieces, &find_emails);
    if with_names {
        let needles = names.needles();
        pieces = apply(pieces, &|chars| find_names(chars, &needles));
    }
    pieces = apply(pieces, &find_ipv6);
    pieces = apply(pieces, &find_ipv4);
    pieces
        .into_iter()
        .map(|piece| match piece {
            Piece::Raw(text) => text,
            Piece::Done(placeholder) => placeholder.to_owned(),
        })
        .collect()
}

/// 对每一段还没处理的文字跑一遍查找，把命中的区间切出来换成占位符。
fn apply(pieces: Vec<Piece>, find: &dyn Fn(&[char]) -> Vec<Hit>) -> Vec<Piece> {
    let mut out = Vec::new();
    for piece in pieces {
        let Piece::Raw(text) = piece else {
            out.push(piece);
            continue;
        };
        let chars: Vec<char> = text.chars().collect();
        let hits = find(&chars);
        if hits.is_empty() {
            out.push(Piece::Raw(text));
            continue;
        }
        let mut from = 0;
        for (start, end, placeholder) in hits {
            if start > from {
                out.push(Piece::Raw(chars[from..start].iter().collect()));
            }
            out.push(Piece::Done(placeholder));
            from = end;
        }
        if from < chars.len() {
            out.push(Piece::Raw(chars[from..].iter().collect()));
        }
    }
    out
}

/// 字母、数字、下划线：紧挨着它们的一串不算一个独立的地址。
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ---- 用户目录 ----

/// `X:\Users\<名字>`、`/home/<名字>`、`/Users/<名字>` 整段换成 `~`。当前用户的目录前面已经按原值换过了，
/// 这里管的是别的用户（比如消息里带着另一个账户的路径）。
fn user_dirs(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if let Some(end) = windows_user_dir(&chars, i).or_else(|| unix_user_dir(&chars, i)) {
            out.push('~');
            i = end;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

fn matches_word(chars: &[char], at: usize, word: &str) -> bool {
    let len = word.chars().count();
    chars.len() >= at + len
        && chars[at..at + len]
            .iter()
            .zip(word.chars())
            .all(|(have, want)| same_char(*have, want))
}

fn same_char(a: char, b: char) -> bool {
    a == b || a.to_lowercase().eq(b.to_lowercase())
}

/// `C:\Users\<名字>`：盘符、冒号、斜杠、`Users`、斜杠、名字。Windows 的用户名可以带空格和中文，
/// 不能含 `\ / : * ? " < > |`，名字到这些字符或行尾为止。
fn windows_user_dir(chars: &[char], at: usize) -> Option<usize> {
    if at > 0 && is_word(chars[at - 1]) {
        return None;
    }
    let drive = *chars.get(at)?;
    if !drive.is_ascii_alphabetic() || chars.get(at + 1) != Some(&':') {
        return None;
    }
    if !chars.get(at + 2).copied().is_some_and(is_sep) || !matches_word(chars, at + 3, "users") {
        return None;
    }
    if !chars.get(at + 8).copied().is_some_and(is_sep) {
        return None;
    }
    let start = at + 9;
    let mut end = start;
    while end < chars.len() && !is_windows_name_stop(chars[end]) {
        end += 1;
    }
    while end > start && chars[end - 1] == ' ' {
        end -= 1;
    }
    (end > start).then_some(end)
}

fn is_windows_name_stop(c: char) -> bool {
    c.is_control() || matches!(c, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
}

/// `/home/<名字>`、`/Users/<名字>`：前面不能紧挨着路径字符（`/usr/home/x`、`C:/Users/x` 不算），名字到空白或分隔符为止。
fn unix_user_dir(chars: &[char], at: usize) -> Option<usize> {
    if chars.get(at) != Some(&'/') {
        return None;
    }
    if at > 0 && (is_word(chars[at - 1]) || is_sep(chars[at - 1]) || chars[at - 1] == '.') {
        return None;
    }
    // `C:/home/x` 是 Windows 盘符下的普通目录，不是 Unix 的家目录；`PATH:/home/x` 这种冒号分隔的列表才算。
    let after_drive_letter = at >= 2
        && chars[at - 1] == ':'
        && chars[at - 2].is_ascii_alphabetic()
        && (at < 3 || !is_word(chars[at - 3]));
    if after_drive_letter {
        return None;
    }
    let word_len = if matches_word(chars, at + 1, "home") {
        4
    } else if matches_word(chars, at + 1, "users") {
        5
    } else {
        return None;
    };
    if chars.get(at + 1 + word_len) != Some(&'/') {
        return None;
    }
    let start = at + 2 + word_len;
    let mut end = start;
    while end < chars.len()
        && !chars[end].is_whitespace()
        && !is_windows_name_stop(chars[end])
        && chars[end] != '\''
    {
        end += 1;
    }
    (end > start).then_some(end)
}

// ---- 邮箱 ----

fn is_email_local(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn is_email_domain(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-')
}

/// `名字@域名.顶级域`。域名至少两段、最后一段是两个以上的字母：`root@192.0.2.10`、`user@localhost`
/// 是 SSH 的 用户@主机，不是邮箱，留给后面的步骤处理。
fn find_emails(chars: &[char]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut floor = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '@' {
            i += 1;
            continue;
        }
        let mut start = i;
        while start > floor && is_email_local(chars[start - 1]) {
            start -= 1;
        }
        let mut end = i + 1;
        while end < chars.len() && is_email_domain(chars[end]) {
            end += 1;
        }
        while end > i + 1 && matches!(chars[end - 1], '.' | '-') {
            end -= 1;
        }
        if start < i && valid_domain(&chars[i + 1..end]) {
            hits.push((start, end, "<email>"));
            floor = end;
            i = end;
        } else {
            i += 1;
        }
    }
    hits
}

fn valid_domain(domain: &[char]) -> bool {
    let text: String = domain.iter().collect();
    let labels: Vec<&str> = text.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let label_ok =
        |label: &&str| !label.is_empty() && !label.starts_with('-') && !label.ends_with('-');
    if !labels.iter().all(label_ok) {
        return false;
    }
    let tld = labels[labels.len() - 1];
    tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic())
}

// ---- 主机库 / 本机的名字 ----

fn find_names(chars: &[char], needles: &[(Vec<char>, &'static str)]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let mut i = 0;
    'scan: while i < chars.len() {
        for (needle, placeholder) in needles {
            let end = i + needle.len();
            if end <= chars.len()
                && chars[i..end]
                    .iter()
                    .zip(needle)
                    .all(|(have, want)| same_char(*have, *want))
            {
                hits.push((i, end, *placeholder));
                i = end;
                continue 'scan;
            }
        }
        i += 1;
    }
    hits
}

// ---- IP 地址 ----

/// IPv6：由十六进制数字、冒号、点组成的一串，整串能被标准库解析成 IPv6 地址，并且前后不紧挨着字母数字
/// （`std::fmt` 里的 `d::f` 不算）。至少两个冒号、至少一个数字：`::`、`a::b` 这种更像 Rust 路径的不算。
/// 带 `%eth0` 这样的区域标识时一并换掉。
fn find_ipv6(chars: &[char]) -> Vec<Hit> {
    let member = |c: char| c.is_ascii_hexdigit() || c == ':' || c == '.';
    let mut hits = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !member(chars[i]) {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < chars.len() && member(chars[i]) {
            i += 1;
        }
        let run_end = i;
        let mut start = run_start;
        // 单个冒号开头是前面的分隔符（`host:2001:db8::1`），`::` 开头才是地址的一部分。
        if chars[start] == ':' && chars.get(start + 1) != Some(&':') {
            start += 1;
        }
        if start >= run_end || (start > 0 && is_word(chars[start - 1])) {
            continue;
        }
        let mut end = run_end;
        while end > start {
            let candidate: String = chars[start..end].iter().collect();
            if looks_like_ipv6(&candidate) {
                if end < chars.len() && is_word(chars[end]) {
                    break;
                }
                hits.push((start, zone_end(chars, end), "<ip>"));
                break;
            }
            // 句末的点、冒号不属于地址：去掉一个再试。
            if matches!(chars[end - 1], '.' | ':') {
                end -= 1;
            } else {
                break;
            }
        }
    }
    hits
}

fn looks_like_ipv6(text: &str) -> bool {
    text.matches(':').count() >= 2
        && text.chars().any(|c| c.is_ascii_digit())
        && Ipv6Addr::from_str(text).is_ok()
}

/// 地址后面紧跟 `%区域标识`（`fe80::1%eth0`）时，一起算进去。
fn zone_end(chars: &[char], end: usize) -> usize {
    if chars.get(end) != Some(&'%') {
        return end;
    }
    let mut zone = end + 1;
    while zone < chars.len()
        && (chars[zone].is_ascii_alphanumeric() || matches!(chars[zone], '_' | '.' | '-'))
    {
        zone += 1;
    }
    if zone > end + 1 { zone } else { end }
}

/// IPv4：四段、每段 1 到 3 位数字且不超过 255，前后不紧挨着字母数字（`v1.2.3.4` 不算，五段的版本号不算）。
/// 后面跟 `:22` 这样的端口没问题。
fn find_ipv4(chars: &[char]) -> Vec<Hit> {
    let member = |c: char| c.is_ascii_digit() || c == '.';
    let mut hits = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if !member(chars[i]) {
            i += 1;
            continue;
        }
        let run_start = i;
        while i < chars.len() && member(chars[i]) {
            i += 1;
        }
        let mut start = run_start;
        let mut end = i;
        while start < end && chars[start] == '.' {
            start += 1;
        }
        while end > start && chars[end - 1] == '.' {
            end -= 1;
        }
        if start >= end
            || (start > 0 && is_word(chars[start - 1]))
            || (end < chars.len() && is_word(chars[end]))
        {
            continue;
        }
        let text: String = chars[start..end].iter().collect();
        if is_ipv4(&text) {
            hits.push((start, end, "<ip>"));
        }
    }
    hits
}

fn is_ipv4(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|part| {
            (1..=3).contains(&part.len())
                && part.chars().all(|c| c.is_ascii_digit())
                && part.parse::<u16>().is_ok_and(|value| value <= 255)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Names {
        Names::default()
    }

    fn scrubbed(text: &str) -> String {
        scrub(text, &names())
    }

    #[test]
    fn ipv4_addresses_public_and_private_become_ip() {
        assert_eq!(
            scrubbed("connect to 203.0.113.9 failed"),
            "connect to <ip> failed"
        );
        assert_eq!(scrubbed("192.0.2.10:22 refused"), "<ip>:22 refused");
        assert_eq!(
            scrubbed("via 10.0.0.5, 172.16.3.4 and 192.168.1.1."),
            "via <ip>, <ip> and <ip>."
        );
        assert_eq!(scrubbed("(127.0.0.1)"), "(<ip>)");
        assert_eq!(
            scrubbed("ip=0.0.0.0\nnext 255.255.255.255"),
            "ip=<ip>\nnext <ip>"
        );
    }

    #[test]
    fn things_that_only_look_like_ipv4_are_left_alone() {
        for text in [
            "v1.2.3",
            "version 1.1.2 build 20261123",
            "1.2.3.4.5",
            "999.1.1.1",
            "1.2.3",
            "file1.2.3.4",
            "1.2.3.4abc",
            "256.0.0.1",
            "1..2.3",
        ] {
            assert_eq!(scrubbed(text), text, "{text}");
        }
    }

    #[test]
    fn ipv6_addresses_become_ip() {
        assert_eq!(
            scrubbed("connect [2001:db8::1]:22 failed"),
            "connect [<ip>]:22 failed"
        );
        assert_eq!(
            scrubbed("peer 2001:0db8:85a3:0000:0000:8a2e:0370:7334."),
            "peer <ip>."
        );
        assert_eq!(scrubbed("addr=::1 ok"), "addr=<ip> ok");
        assert_eq!(scrubbed("fe80::1%eth0 up"), "<ip> up");
        assert_eq!(scrubbed("mapped ::ffff:192.0.2.1 here"), "mapped <ip> here");
        assert_eq!(scrubbed("host:2001:db8::7"), "host:<ip>");
        assert_eq!(scrubbed("range 2001:db8::/32"), "range <ip>/32");
    }

    #[test]
    fn things_that_only_look_like_ipv6_are_left_alone() {
        for text in [
            "std::fmt::Display",
            "at 12:34:56 today",
            "[2026-11-07T12:34:56Z] v1.1.2",
            "aa:bb:cc:dd:ee:ff",
            "a::b",
            "::",
            "io::Error",
            "key:value:other",
            "dead:beef",
        ] {
            assert_eq!(scrubbed(text), text, "{text}");
        }
    }

    #[test]
    fn emails_become_email_and_ssh_targets_are_not_emails() {
        assert_eq!(scrubbed("mail alice@example.com now"), "mail <email> now");
        assert_eq!(scrubbed("a.b+tag@mail.example.co.uk."), "<email>.");
        assert_eq!(
            scrubbed("x bob@example.com, y carol@example.org"),
            "x <email>, y <email>"
        );
        for text in [
            "a@b",
            "@handle",
            "user@localhost",
            "me@ example.com",
            "x@1.2",
            "user@-bad.com",
        ] {
            assert_eq!(scrubbed(text), text, "{text}");
        }
        // SSH 的 用户@地址：地址当 IP 处理，用户名留给主机库的名单。
        assert_eq!(scrubbed("root@192.0.2.10"), "root@<ip>");
    }

    #[test]
    fn host_names_addresses_and_users_from_the_library_are_replaced() {
        let names = Names {
            hosts: vec![
                "web-1.example.net".into(),
                "Web-1".into(),
                "db.internal".into(),
            ],
            users: vec!["deploy".into()],
            ..Names::default()
        };
        assert_eq!(
            scrub("cannot reach web-1.example.net as deploy", &names),
            "cannot reach <host> as <user>"
        );
        // 不分大小写，名字嵌在别的文字里也换。
        assert_eq!(
            scrub("WEB-1 / db.INTERNAL:3306 (DEPLOY)", &names),
            "<host> / <host>:3306 (<user>)"
        );
        assert_eq!(scrub("user=deploy123", &names), "user=<user>123");
        // 反例：没出现的、库里没有的原样。
        assert_eq!(
            scrub("plain message about web-2", &names),
            "plain message about web-2"
        );
    }

    #[test]
    fn names_shorter_than_three_characters_are_skipped() {
        let names = Names {
            hosts: vec!["ab".into(), "a".into(), "  ".into(), String::new()],
            users: vec!["me".into()],
            ..Names::default()
        };
        assert_eq!(scrub("ab a me and  here", &names), "ab a me and  here");
        let three = Names {
            users: vec!["bob".into()],
            ..Names::default()
        };
        assert_eq!(scrub("Bob and bobby", &three), "<user> and <user>by");
    }

    #[test]
    fn chinese_and_spaced_user_names_work() {
        let names = Names {
            users: vec!["张 三".into()],
            hosts: vec!["测试机".into()],
            ..Names::default()
        };
        assert_eq!(
            scrub("用户 张 三 登录 测试机 失败", &names),
            "用户 <user> 登录 <host> 失败"
        );
    }

    #[test]
    fn local_names_become_user_and_computer() {
        let mut names = Names::default();
        names.add_local(
            |key| match key {
                "USERNAME" => Some("Alice Smith".to_owned()),
                "COMPUTERNAME" => Some("DESKTOP-ABC123".to_owned()),
                _ => None,
            },
            None,
            Some("build-box".to_owned()),
        );
        assert_eq!(
            scrub("alice smith@desktop-abc123 on build-box", &names),
            "<user>@<computer> on <computer>"
        );
        // 读不到任何名字时什么也不换。
        let mut empty = Names::default();
        empty.add_local(|_| None, None, None);
        assert_eq!(scrub("alice@desktop", &empty), "alice@desktop");
    }

    #[test]
    fn the_same_name_in_two_lists_is_replaced_once_with_host_first() {
        let names = Names {
            hosts: vec!["shared".into()],
            users: vec!["SHARED".into()],
            ..Names::default()
        };
        assert_eq!(scrub("shared", &names), "<host>");
    }

    #[test]
    fn placeholders_are_never_rescanned_by_later_steps() {
        // 主机名叫 email / host：换进去的 <email>、<host> 不能再被匹配。
        let names = Names {
            hosts: vec!["email".into(), "host".into()],
            users: vec!["user".into()],
            ..Names::default()
        };
        assert_eq!(
            scrub("x@example.com email host user", &names),
            "<email> <host> <host> <user>"
        );
    }

    #[test]
    fn the_longest_name_wins() {
        let names = Names {
            hosts: vec!["web".into(), "web-1.example.net".into()],
            ..Names::default()
        };
        assert_eq!(
            scrub("web-1.example.net and web", &names),
            "<host> and <host>"
        );
    }

    #[test]
    fn the_home_directory_becomes_a_tilde_even_with_spaces_and_chinese() {
        let names = Names {
            home: Some(r"C:\Users\张 三".into()),
            ..Names::default()
        };
        assert_eq!(
            scrub(r"cannot open C:\Users\张 三\Documents\a.txt", &names),
            r"cannot open ~\Documents\a.txt"
        );
        assert_eq!(scrub("c:/users/张 三/a.txt", &names), "~/a.txt");
    }

    #[test]
    fn other_users_directories_are_recognised_by_shape() {
        assert_eq!(scrubbed(r"C:\Users\Alice\AppData\x"), r"~\AppData\x");
        assert_eq!(scrubbed(r"d:\users\Bob Jones\a.txt"), r"~\a.txt");
        assert_eq!(scrubbed("open C:/Users/Carol/x failed"), "open ~/x failed");
        assert_eq!(scrubbed(r"in C:\Users\张 三: denied"), "in ~: denied");
        assert_eq!(scrubbed("/home/dave/.cache/x"), "~/.cache/x");
        assert_eq!(scrubbed("(/Users/erin/Library)"), "(~/Library)");
        assert_eq!(
            scrubbed("PATH=/usr/bin:/home/alice/bin"),
            "PATH=/usr/bin:~/bin"
        );
        for text in [
            r"C:\Users",
            r"C:\Users\",
            r"D:\data\Users\x",
            "/homework/x",
            "/usr/home/x",
            "/home/",
            "C:/home/x",
            "see /home",
        ] {
            assert_eq!(scrubbed(text), text, "{text}");
        }
    }

    #[test]
    fn the_header_only_gets_the_shape_based_steps() {
        let names = Names {
            hosts: vec!["src".into(), "native".into()],
            users: vec!["main".into()],
            ..Names::default()
        };
        let header = "[2026-11-07T12:34:56Z] v1.1.2 thread 'main' panicked at native/zeeterm-gpui/src/x.rs:12:3";
        assert_eq!(scrub_header(header, &names), header);
        // 同样的名字在消息里照换。
        assert_eq!(scrub("src native main", &names), "<host> <host> <user>");
        // 头里万一混进邮箱 / IP 照样清掉。
        assert_eq!(
            scrub_header("thread 'a@example.com' 192.0.2.1", &names),
            "thread '<email>' <ip>"
        );
    }

    #[test]
    fn text_without_anything_private_is_unchanged() {
        let text = "called `Option::unwrap()` on a `None` value (index 3 of 10)";
        assert_eq!(scrubbed(text), text);
        assert_eq!(scrubbed(""), "");
    }

    #[test]
    fn collecting_names_takes_address_name_and_user_of_every_host() {
        use zeeterm_core::model::{AuthMethod, Host, ProtocolSettings, ProxyMode};
        let host = Host {
            id: 1,
            group_id: None,
            name: "web-1".to_owned(),
            host: "192.0.2.10".to_owned(),
            port: 22,
            username: "deploy".to_owned(),
            auth_method: AuthMethod::Password,
            identity_id: None,
            proxy_id: None,
            jump_host_id: None,
            proxy_mode: ProxyMode::Default,
            tags: Vec::new(),
            sort: 0,
            legacy_algorithms: false,
            created_at: 0,
            updated_at: 0,
            protocol: ProtocolSettings::default(),
        };
        let names = Names::collect(&[host]);
        assert!(names.hosts.contains(&"web-1".to_owned()));
        assert!(names.hosts.contains(&"192.0.2.10".to_owned()));
        assert!(names.users.contains(&"deploy".to_owned()));
        assert_eq!(
            scrub("deploy@192.0.2.10 (web-1) failed", &names),
            "<user>@<host> (<host>) failed"
        );
        // 没有主机时只剩本机的名字，取不到也不报错。
        assert!(Names::collect(&[]).hosts.is_empty());
    }
}
