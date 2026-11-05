// Adapted from nyaterm (https://github.com/nyakang/nyaterm, Apache-2.0, Copyright 2026 Kang and
// contributors; see native/NOTICE), crates/nyaterm-transport/src/telnet_prompts.rs and the
// TelnetAutoLoginState in lib.rs: control-sequence stripping, the "last N chars" tail window,
// the "Last login:" guard and the failure phrases. The prompt rules here are plain
// case-insensitive substrings from the host settings (not regexes), there are no retries (a
// wrong password is never sent twice), and the password is zeroized once used.

//! Telnet 自动登录：在输出里认"用户名 / 密码提示"，认到就把用户名 / 密码发过去。
//!
//! 只在连接最初的一小段时间里工作；用户自己敲了任何键、对端报登录失败、或者超时，就永远停手。
//! 日志里不打印用户名以外的任何登录内容，密码只在内存里，用完清零。

use std::time::{Duration, Instant};

use zeroize::Zeroizing;

/// 从连接开始算，超过这个时间还没登录完就不再自动答题。
pub const AUTO_LOGIN_TIMEOUT: Duration = Duration::from_secs(30);
/// 只保留最近这么多字符的输出用来认提示。
const TAIL_CHARS: usize = 2048;

/// 对端报登录失败的常见说法（小写）。
const FAILURE_PHRASES: &[&str] = &[
    "login incorrect",
    "authentication failed",
    "access denied",
    // 对端用中文报登录失败时的说法：这是在匹配服务器输出的数据，不是我们自己的界面文案。
    "密码错误", // i18n-allow: 匹配对端输出
    "认证失败", // i18n-allow: 匹配对端输出
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    WaitUser,
    WaitPassword,
    Done,
    Disabled,
}

pub struct AutoLogin {
    username: String,
    password: Option<Zeroizing<String>>,
    username_patterns: Vec<String>,
    password_patterns: Vec<String>,
    /// 回车键发送的字节（和手动输入一致）。
    enter: Vec<u8>,
    stage: Stage,
    tail: String,
    started: Instant,
}

impl AutoLogin {
    /// 没有任何可发的内容（用户名和密码都空）时返回 `None`。
    pub fn new(
        username: &str,
        password: Option<String>,
        username_patterns: &[String],
        password_patterns: &[String],
        enter: &[u8],
        now: Instant,
    ) -> Option<Self> {
        let username = username.trim().to_owned();
        let password = password
            .filter(|value| !value.is_empty())
            .map(Zeroizing::new);
        if username.is_empty() && password.is_none() {
            return None;
        }
        Some(Self {
            username,
            password,
            username_patterns: lowercase_patterns(username_patterns),
            password_patterns: lowercase_patterns(password_patterns),
            enter: enter.to_vec(),
            stage: Stage::WaitUser,
            tail: String::new(),
            started: now,
        })
    }

    /// 还在等提示（没完成、没被停掉）。
    pub fn is_active(&self) -> bool {
        matches!(self.stage, Stage::WaitUser | Stage::WaitPassword)
    }

    /// 用户自己键入了东西：不再自动答题（免得把密码发进一个已经被手动操作过的会话）。
    pub fn on_user_input(&mut self) {
        if self.is_active() {
            self.stage = Stage::Disabled;
            self.release();
        }
    }

    /// 喂一段刚收到的可见输出（已经是 UTF-8 文本），返回要发给远端的一行（含回车）。
    pub fn on_output(&mut self, text: &str, now: Instant) -> Option<Vec<u8>> {
        if !self.is_active() {
            return None;
        }
        if now.duration_since(self.started) > AUTO_LOGIN_TIMEOUT {
            self.stage = Stage::Disabled;
            self.release();
            return None;
        }
        self.tail.push_str(text);
        self.tail = last_chars(&self.tail, TAIL_CHARS);
        let clean = strip_control_sequences(&self.tail).replace('\r', "\n");
        let lowered = clean.to_lowercase();
        if FAILURE_PHRASES
            .iter()
            .any(|phrase| lowered.contains(phrase))
        {
            self.stage = Stage::Disabled;
            self.release();
            return None;
        }
        // 提示符还没换行：看当前这一行。
        let line = clean.rsplit('\n').next().unwrap_or("").trim_end();
        if line.trim().is_empty() {
            return None;
        }
        let line = line.to_lowercase();
        // "Last login: …" 是登录成功之后的横幅，不是用户名提示。
        if line.contains("last login") || line.contains("previous login") {
            return None;
        }
        let wants_user = matches_any(&line, &self.username_patterns);
        let wants_password = matches_any(&line, &self.password_patterns);

        if self.stage == Stage::WaitUser && wants_user && !self.username.is_empty() {
            let reply = self.line_bytes(&self.username);
            self.tail.clear();
            self.stage = Stage::WaitPassword;
            return Some(reply);
        }
        // 只问密码、不问用户名的设备，或者用户名已经答过：答密码。
        if wants_password
            && !wants_user
            && let Some(password) = self.password.as_deref()
        {
            let reply = self.line_bytes(password);
            self.stage = Stage::Done;
            self.release();
            return Some(reply);
        }
        None
    }

    fn line_bytes(&self, value: &str) -> Vec<u8> {
        let mut bytes = value.as_bytes().to_vec();
        bytes.extend_from_slice(&self.enter);
        bytes
    }

    /// 停手之后不再需要攒输出，也不再留着密码。
    fn release(&mut self) {
        self.tail = String::new();
        if self.stage != Stage::WaitPassword {
            self.password = None;
        }
    }
}

fn lowercase_patterns(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .map(|pattern| pattern.trim().to_lowercase())
        .filter(|pattern| !pattern.is_empty())
        .collect()
}

/// 这一行是不是以某个提示结尾。
fn matches_any(line: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .any(|pattern| line.ends_with(pattern.as_str()))
}

/// 取最后 `max_chars` 个字符。
pub fn last_chars(value: &str, max_chars: usize) -> String {
    let len = value.chars().count();
    if len <= max_chars {
        return value.to_owned();
    }
    value.chars().skip(len - max_chars).collect()
}

/// 去掉 ANSI 控制序列和其他不可见控制字符（保留 CR / LF / Tab）。
pub fn strip_control_sequences(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == 0x1b {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                index += 1;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            } else {
                index += 1;
            }
            continue;
        }
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        if ch != '\u{7f}' && (!ch.is_control() || matches!(ch, '\r' | '\n' | '\t')) {
            stripped.push(ch);
        }
        index += ch.len_utf8();
    }
    stripped
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_core::model::TelnetAutoLogin;

    fn login(username: &str, password: Option<&str>) -> AutoLogin {
        let defaults = TelnetAutoLogin::default();
        AutoLogin::new(
            username,
            password.map(str::to_owned),
            &defaults.username_patterns,
            &defaults.password_patterns,
            b"\r\n",
            Instant::now(),
        )
        .expect("有内容可发")
    }

    #[test]
    fn answers_the_user_prompt_then_the_password_prompt() {
        let mut login = login("operator", Some("s3cret"));
        let now = Instant::now();
        assert_eq!(
            login.on_output("Welcome\r\nhost login: ", now),
            Some(b"operator\r\n".to_vec())
        );
        // 回显 + 空行之后才出现密码提示。
        assert_eq!(login.on_output("operator\r\n", now), None);
        assert_eq!(
            login.on_output("Password: ", now),
            Some(b"s3cret\r\n".to_vec())
        );
        assert!(!login.is_active());
        // 登录完成之后什么提示都不再答。
        assert_eq!(login.on_output("\r\nlogin: ", now), None);
    }

    #[test]
    fn prompts_split_across_chunks_and_wrapped_in_escape_codes_are_recognized() {
        let mut login = login("root", Some("pw"));
        let now = Instant::now();
        assert_eq!(login.on_output("\x1b[1mlog", now), None);
        assert_eq!(
            login.on_output("in:\x1b[0m ", now),
            Some(b"root\r\n".to_vec())
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_custom_patterns_work() {
        let mut login = AutoLogin::new(
            "admin",
            Some("pw".to_owned()),
            &["Enter account>".to_owned()],
            &["Enter secret>".to_owned()],
            b"\r",
            Instant::now(),
        )
        .unwrap();
        let now = Instant::now();
        assert_eq!(
            login.on_output("ENTER ACCOUNT>", now),
            Some(b"admin\r".to_vec())
        );
        assert_eq!(
            login.on_output("\r\nenter secret>", now),
            Some(b"pw\r".to_vec())
        );
    }

    #[test]
    fn a_device_that_only_asks_for_a_password_gets_it() {
        let mut login = login("", Some("pw"));
        assert_eq!(
            login.on_output("User Access Verification\r\nPassword: ", Instant::now()),
            Some(b"pw\r\n".to_vec())
        );
    }

    #[test]
    fn the_last_login_banner_is_not_a_username_prompt() {
        let mut login = login("root", Some("pw"));
        assert_eq!(login.on_output("Last login:", Instant::now()), None);
    }

    #[test]
    fn user_typing_stops_auto_login_for_good() {
        let mut login = login("root", Some("pw"));
        login.on_user_input();
        assert!(!login.is_active());
        assert_eq!(login.on_output("login: ", Instant::now()), None);
    }

    #[test]
    fn a_login_failure_message_disables_it_so_the_password_is_never_sent_twice() {
        let mut login = login("root", Some("pw"));
        let now = Instant::now();
        assert!(login.on_output("login: ", now).is_some());
        assert!(login.on_output("Password: ", now).is_some());
        assert_eq!(login.on_output("\r\nLogin incorrect\r\nlogin: ", now), None);
        assert_eq!(login.on_output("Password: ", now), None);

        let mut login = self::login("root", Some("pw"));
        assert!(login.on_output("login: ", now).is_some());
        assert_eq!(
            login.on_output("\r\nLogin incorrect\r\nPassword: ", now),
            None
        );
    }

    #[test]
    fn it_gives_up_after_the_timeout() {
        let mut login = login("root", Some("pw"));
        let late = Instant::now() + AUTO_LOGIN_TIMEOUT + Duration::from_secs(1);
        assert_eq!(login.on_output("login: ", late), None);
        assert!(!login.is_active());
    }

    #[test]
    fn nothing_to_send_means_no_auto_login() {
        let defaults = TelnetAutoLogin::default();
        assert!(
            AutoLogin::new(
                "  ",
                None,
                &defaults.username_patterns,
                &defaults.password_patterns,
                b"\r",
                Instant::now()
            )
            .is_none()
        );
    }

    #[test]
    fn no_stored_password_means_the_password_prompt_is_left_to_the_user() {
        let mut login = login("root", None);
        let now = Instant::now();
        assert!(login.on_output("login: ", now).is_some());
        assert_eq!(login.on_output("Password: ", now), None);
    }

    #[test]
    fn tail_is_bounded() {
        assert_eq!(last_chars("abcdef", 3), "def");
        assert_eq!(last_chars("ab", 3), "ab");
        let mut login = login("root", Some("pw"));
        for _ in 0..10_000 {
            login.on_output("xxxxxxxxxxxxxxxx\r\n", Instant::now());
        }
        assert!(login.tail.chars().count() <= TAIL_CHARS);
    }
}
