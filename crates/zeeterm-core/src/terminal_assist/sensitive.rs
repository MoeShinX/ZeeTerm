//! 命令里有没有秘密：采集和展示两层都用同一组纯函数（方案 7.4）。
//!
//! 命中的命令不进内存历史，也不出现在自动候选里（候选、悬浮提示、调试日志都没有）。规则只看命令文本本身，
//! 不联网、不读文件、不记状态，所以可以成组地写正反例。
//!
//! **它不承诺识别所有秘密**：只认下面这些常见写法，认不出的会被当成普通命令。宁可多排除也不少排除，
//! 所以对不确定的写法倾向于"算敏感"（例如 `--password` 不管后面有没有值都算）。
//!
//! 认的写法：
//! - 控制字符 / 换行：不是单行命令，也可能夹带终端控制序列；
//! - 密码 / 令牌类长选项：`--password`、`--passwd`、`--passphrase`、`--token`、`--secret`、`--api-key`、
//!   `--access-key`、`--credential`、`--bearer`、`--client-secret`、`--auth-token`、`--authorization`……
//!   （`--opt=值` 和 `--opt 值` 两种写法，名字里含这些词就算）；
//! - 带值的短选项：`-p`（mysql / mariadb / sshpass / 7z / rar / mongo 等把它当密码的命令；`-p` 在 ssh / docker /
//!   mkdir 等命令里是端口 / 发布 / 创建父目录，不算）、`-P`（unzip / zip / sqlcmd）、`-a`（redis-cli）、
//!   `-w`（ldap 系列）、`-u 用户:密码`（curl 一类）、`/p:`（rdp 客户端）、openssl 的 `-pass` / `-passin` / `-passout`；
//! - 认证头：`Authorization:`、`Proxy-Authorization:`、`X-API-Key:`、`X-Auth-Token:`、`X-Access-Token:`、
//!   `Cookie:` 和 `Bearer 令牌`；
//! - 秘密环境赋值：变量名里有 `SECRET` / `PASSWORD` / `PASSWD` / `PASSPHRASE` / `TOKEN` / `API_KEY` / `ACCESS_KEY` /
//!   `PRIVATE_KEY` / `CREDENTIAL`，或以 `_PWD` 结尾（`AWS_SECRET_ACCESS_KEY=…`、`export GITHUB_TOKEN=…`、
//!   `docker run -e DB_PASSWORD=…`）；
//! - 带凭据的 URL：`scheme://用户:密码@主机`（只有用户名没有密码的 `ssh://git@host` 不算）；
//! - 长得像令牌的词：`ghp_…`、`github_pat_…`、`sk-…`、`AKIA…`、`xoxb-…`、`glpat-…`、`npm_…`、JWT（`eyJ….….…`）等。

use std::fmt;

/// 命中了哪一类规则（诊断用，不含命令原文）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SensitiveReason {
    /// 含控制字符 / 换行。
    Control,
    /// 密码 / 令牌类选项。
    SecretFlag,
    /// 认证头。
    AuthHeader,
    /// 秘密环境赋值。
    SecretEnv,
    /// 带凭据的 URL。
    CredentialUrl,
    /// 长得像令牌。
    TokenShape,
}

impl fmt::Display for SensitiveReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Control => "control",
            Self::SecretFlag => "secret-flag",
            Self::AuthHeader => "auth-header",
            Self::SecretEnv => "secret-env",
            Self::CredentialUrl => "credential-url",
            Self::TokenShape => "token-shape",
        })
    }
}

/// 这条命令命中了哪类敏感规则；`None` 表示没有认出秘密（不等于一定没有）。
pub fn sensitivity(text: &str) -> Option<SensitiveReason> {
    if text.chars().any(char::is_control) {
        return Some(SensitiveReason::Control);
    }
    let lower = text.to_ascii_lowercase();
    if has_auth_header(&lower) {
        return Some(SensitiveReason::AuthHeader);
    }
    if has_credential_url(text) {
        return Some(SensitiveReason::CredentialUrl);
    }
    for segment in segments(text) {
        if let Some(reason) = check_segment(&segment) {
            return Some(reason);
        }
    }
    None
}

/// 命中任何一类规则。
pub fn is_sensitive(text: &str) -> bool {
    sensitivity(text).is_some()
}

// ---- 认证头 / 凭据 URL ----

const AUTH_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "x-auth-token",
    "x-access-token",
    "cookie",
];

/// `Name:` 形式的认证头（名字和冒号之间允许空白），或者 `Bearer 令牌`。输入已经转成小写。
fn has_auth_header(lower: &str) -> bool {
    for name in AUTH_HEADERS {
        let mut from = 0;
        while let Some(found) = lower[from..].find(name) {
            let end = from + found + name.len();
            let rest = lower[end..].trim_start_matches([' ', '\t']);
            if rest.starts_with(':') {
                return true;
            }
            from = end;
        }
    }
    // `Bearer 令牌`（认证头之外的写法）。单独一个词 `bearer` 不算。
    let words: Vec<&str> = lower
        .split_whitespace()
        .map(|word| word.trim_matches(['"', '\'']))
        .collect();
    words
        .windows(2)
        .any(|pair| pair[0] == "bearer" && !pair[1].is_empty())
}

/// `scheme://用户:密码@主机`。
fn has_credential_url(text: &str) -> bool {
    let mut from = 0;
    while let Some(found) = text[from..].find("://") {
        let start = from + found + 3;
        let authority: String = text[start..]
            .chars()
            .take_while(|c| !matches!(c, '/' | '?' | '#' | '\'' | '"') && !c.is_whitespace())
            .collect();
        if let Some((userinfo, _host)) = authority.rsplit_once('@') {
            if userinfo.contains(':') {
                return true;
            }
        }
        from = start;
    }
    false
}

// ---- 分词 ----

/// 一个命令段（用 `;` `|` `&` 隔开）里的词，引号已去掉。
type Segment = Vec<String>;

/// 按空白分词并去引号，遇到未加引号的 `; | &` 开一个新的命令段。不是完整的 shell 解析：
/// 够用来认"这一段的命令是谁、带了哪些选项"。
fn segments(text: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut words: Segment = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    let flush = |word: &mut String, in_word: &mut bool, words: &mut Segment| {
        if *in_word {
            words.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else if c == '\\' && q == '"' {
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                } else {
                    word.push(c);
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    in_word = true;
                }
                '\\' => {
                    in_word = true;
                    if let Some(next) = chars.next() {
                        word.push(next);
                    }
                }
                ';' | '|' | '&' => {
                    flush(&mut word, &mut in_word, &mut words);
                    if !words.is_empty() {
                        out.push(std::mem::take(&mut words));
                    }
                }
                c if c.is_whitespace() => flush(&mut word, &mut in_word, &mut words),
                c => {
                    in_word = true;
                    word.push(c);
                }
            },
        }
    }
    flush(&mut word, &mut in_word, &mut words);
    if !words.is_empty() {
        out.push(words);
    }
    out
}

// ---- 一个命令段 ----

/// 长选项名里出现这些词就算（小写比较）。
const SECRET_WORDS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "token",
    "secret",
    "api-key",
    "api_key",
    "apikey",
    "access-key",
    "access_key",
    "credential",
    "bearer",
    "private-key-pass",
];

/// 长选项名正好等于这些就算（名字太短，不能按"包含"判断，例如 `--author`）。
const SECRET_EXACT: &[&str] = &["pass", "pwd", "auth", "authorization", "key-pass"];

/// 环境变量名里出现这些词就算（大写比较）。
const ENV_WORDS: &[&str] = &[
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "TOKEN",
    "API_KEY",
    "APIKEY",
    "ACCESS_KEY",
    "PRIVATE_KEY",
    "CREDENTIAL",
];

/// 单横线 + 多个字母的选项（openssl 风格）正好等于这些就算。
const SINGLE_DASH_WORDS: &[&str] = &[
    "pass", "passin", "passout", "password", "passwd", "pwd", "token", "secret",
];

/// 把短选项当密码用的命令：(命令名, 选项字母们)。命令名按词在命令段里出现与否判断
/// （这样 `sudo mysql -p…`、`env X=1 mysql -p…` 也认得出来）。
const SHORT_SECRET_FLAGS: &[(&str, &[char])] = &[
    ("mysql", &['p']),
    ("mysqldump", &['p']),
    ("mysqladmin", &['p']),
    ("mysqlimport", &['p']),
    ("mysqlcheck", &['p']),
    ("mysqlshow", &['p']),
    ("mysqlpump", &['p']),
    ("mysqlbinlog", &['p']),
    ("mariadb", &['p']),
    ("mariadb-dump", &['p']),
    ("mariadb-admin", &['p']),
    ("sshpass", &['p']),
    ("7z", &['p']),
    ("7za", &['p']),
    ("7zr", &['p']),
    ("rar", &['p']),
    ("unrar", &['p']),
    ("mongo", &['p']),
    ("mongosh", &['p']),
    ("mongodump", &['p']),
    ("mongorestore", &['p']),
    ("mongoexport", &['p']),
    ("mongoimport", &['p']),
    ("useradd", &['p']),
    ("usermod", &['p']),
    ("chpasswd", &['p']),
    ("unzip", &['P']),
    ("zip", &['P']),
    ("sqlcmd", &['P']),
    ("redis-cli", &['a']),
    ("ldapsearch", &['w']),
    ("ldapadd", &['w']),
    ("ldapmodify", &['w']),
    ("ldapdelete", &['w']),
    ("ldappasswd", &['w', 'a', 's']),
];

fn check_segment(words: &[String]) -> Option<SensitiveReason> {
    let commands = command_names(words);
    let short_flags: Vec<char> = SHORT_SECRET_FLAGS
        .iter()
        .filter(|(name, _)| commands.iter().any(|command| command == name))
        .flat_map(|(_, flags)| flags.iter().copied())
        .collect();
    // `-u 用户:密码` / `--user 用户:密码` 只对 curl 一类命令算：docker 的 `-u 1000:1000` 是 uid:gid。
    let colon_user = commands.iter().any(|command| command == "curl");
    for (index, word) in words.iter().enumerate() {
        if looks_like_token(word) {
            return Some(SensitiveReason::TokenShape);
        }
        if is_secret_env(word) {
            return Some(SensitiveReason::SecretEnv);
        }
        if has_secret_query(word) {
            return Some(SensitiveReason::SecretFlag);
        }
        if let Some(rest) = word.strip_prefix("--") {
            let (name, value) = match rest.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (rest, None),
            };
            let lower = name.to_ascii_lowercase();
            if SECRET_EXACT.contains(&lower.as_str())
                || SECRET_WORDS.iter().any(|secret| lower.contains(secret))
            {
                return Some(SensitiveReason::SecretFlag);
            }
            // `--env=NAME=值` / `--build-arg=NAME=值`
            if value.is_some_and(is_secret_env) {
                return Some(SensitiveReason::SecretEnv);
            }
            // `--user=name:pass` / `--user name:pass`
            if colon_user && lower == "user" {
                let given = value.or_else(|| words.get(index + 1).map(String::as_str));
                if given.is_some_and(|v| v.contains(':')) {
                    return Some(SensitiveReason::SecretFlag);
                }
            }
            continue;
        }
        if let Some(rest) = word.strip_prefix('-').filter(|rest| !rest.is_empty()) {
            let lower = rest.to_ascii_lowercase();
            if rest.len() > 1 && SINGLE_DASH_WORDS.contains(&lower.as_str()) {
                return Some(SensitiveReason::SecretFlag);
            }
            let first = rest.chars().next().unwrap_or(' ');
            if short_flags.contains(&first) {
                return Some(SensitiveReason::SecretFlag);
            }
            // `-u 用户:密码`：只有带冒号的值才算，curl 没给密码时（`-u 用户`）由它自己提示，不算。
            if colon_user && first == 'u' {
                let attached = &rest[1..];
                let value = if attached.is_empty() {
                    words.get(index + 1).map(String::as_str)
                } else {
                    Some(attached)
                };
                if value.is_some_and(|v| v.contains(':') && !v.contains("://")) {
                    return Some(SensitiveReason::SecretFlag);
                }
            }
            continue;
        }
        // rdp 客户端：`/p:密码`、`/password:密码`。
        let lower = word.to_ascii_lowercase();
        if lower.starts_with("/p:") || lower.starts_with("/password:") {
            return Some(SensitiveReason::SecretFlag);
        }
    }
    None
}

/// URL 的查询串里带密码 / 令牌参数：`?token=…`、`&api_key=…`、`&password=…`。
fn has_secret_query(word: &str) -> bool {
    let Some((_, query)) = word.split_once('?') else {
        return false;
    };
    query.split('&').any(|pair| {
        pair.split_once('=').is_some_and(|(name, _)| {
            let lower = name.to_ascii_lowercase();
            SECRET_WORDS.iter().any(|secret| lower.contains(secret))
                || matches!(lower.as_str(), "key" | "sig" | "signature" | "auth")
        })
    })
}

/// 命令段里"可能是命令名"的词：不是选项、不是赋值的词，取文件名部分，去掉 `.exe`，转小写。
/// 包含 `sudo` / `env` 之类前缀后面的真命令（它们也是"不是选项的词"）。
fn command_names(words: &[String]) -> Vec<String> {
    words
        .iter()
        .filter(|word| !word.starts_with('-') && !word.contains('='))
        .map(|word| {
            let name = word.rsplit(['/', '\\']).next().unwrap_or(word);
            let name = name.to_ascii_lowercase();
            name.strip_suffix(".exe").map(str::to_owned).unwrap_or(name)
        })
        .collect()
}

/// `NAME=值` 且变量名里含秘密词。
fn is_secret_env(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || name.starts_with(|c: char| c.is_ascii_digit())
    {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    ENV_WORDS.iter().any(|secret| upper.contains(secret)) || upper.ends_with("_PWD")
}

/// 词本身（或 `=` / `:` / `@` 后面的部分）长得像常见的令牌。
fn looks_like_token(word: &str) -> bool {
    token_shaped(word) || word.split(['=', ':', '@']).skip(1).any(token_shaped)
}

fn token_shaped(word: &str) -> bool {
    let word = word.trim_matches(['"', '\'']);
    let tail_is = |prefix: &str, min: usize, ok: fn(char) -> bool| {
        word.strip_prefix(prefix)
            .is_some_and(|tail| tail.chars().count() >= min && tail.chars().all(ok))
    };
    let alnum = |c: char| c.is_ascii_alphanumeric();
    let alnum_dash = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|prefix| tail_is(prefix, 20, alnum))
        || tail_is("github_pat_", 20, alnum_dash)
        || tail_is("glpat-", 16, alnum_dash)
        || tail_is("sk-", 20, alnum_dash)
        || tail_is("npm_", 30, alnum)
        || tail_is("AKIA", 16, |c: char| {
            c.is_ascii_uppercase() || c.is_ascii_digit()
        })
        || tail_is("AIza", 30, alnum_dash)
        || (["xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-"]
            .iter()
            .any(|prefix| tail_is(prefix, 10, alnum_dash)))
        || is_jwt(word)
}

/// `eyJ…`.`…`.`…`（三段 base64url）。
fn is_jwt(word: &str) -> bool {
    if !word.starts_with("eyJ") {
        return false;
    }
    let parts: Vec<&str> = word.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
        })
        && word.len() >= 30
}

#[cfg(test)]
mod tests;
