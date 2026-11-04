//! 敏感文件路径判断（「只读自动」模式读私钥 / 凭据要确认）。
//!
//! 「只读自动」放行的是“不改动服务器”的命令，但读本身会把内容交给所选的模型服务商：
//! `cat ~/.ssh/id_rsa`、`base64 ~/.aws/credentials` 不改动任何东西，却是最该先问一句的
//! 操作。所以在这个模式里，命令参数 / 文件工具的路径命中下面的表就降级为“要确认”。
//! 「全部放行」是用户明确接受的，不看这张表；「逐条确认」本来就每条都问。
//!
//! 匹配按**路径的各段**（按 `/` 切开，大小写不敏感），不是整串子串匹配：`.ssh` 要整段
//! 等于它，`/etc/hosts`、`keyboard.txt`、`environment.md` 才不会被误伤。
//!
//! 参数里的通配符和变量在我们这里没法算出真实路径（要等 shell 展开），按“可能命中”
//! 处理：通配符段能匹配上表里的某个名字就算命中（`~/.s*/*`），含 `$变量` 的段无法判断就
//! 算命中；`~`、`$HOME`、`${HOME}` 当作普通的家目录写法。纯 `*` / `?` 的段没有任何固定
//! 字符，不去猜（否则 `ls *` 也要问），这是已知的、有意的缺口。

use super::safety::{split_segments, tokenize_marked};

/// 路径段规则：连续几段依次匹配。`last_is_prefix` 为真时最后一段按前缀匹配
/// （`shadow` 也认 `shadow-` 这个备份，`.npmrc` 也认 `.npmrc.bak`）；其余段要整段相等。
struct PathRule {
    parts: &'static [&'static str],
    last_is_prefix: bool,
}

const fn dir(parts: &'static [&'static str]) -> PathRule {
    PathRule {
        parts,
        last_is_prefix: false,
    }
}

const fn file(parts: &'static [&'static str]) -> PathRule {
    PathRule {
        parts,
        last_is_prefix: true,
    }
}

/// 按位置匹配的敏感路径（段小写）。
const PATH_RULES: &[PathRule] = &[
    dir(&[".ssh"]),
    dir(&[".gnupg"]),
    dir(&[".aws"]),
    dir(&[".azure"]),
    dir(&[".config", "gcloud"]),
    file(&[".kube", "config"]),
    file(&[".docker", "config.json"]),
    file(&[".netrc"]),
    file(&[".pgpass"]),
    file(&[".git-credentials"]),
    file(&[".npmrc"]),
    file(&[".pypirc"]),
    // 命令行 / 数据库客户端的历史：里面常有手敲进去的密码和令牌。
    file(&[".bash_history"]),
    file(&[".zsh_history"]),
    file(&[".mysql_history"]),
    file(&[".psql_history"]),
    file(&[".python_history"]),
    file(&[".vault-token"]),
    file(&["etc", "shadow"]),
    file(&["etc", "gshadow"]),
    file(&["etc", "sudoers"]),
];

/// 文件名是 `.env` 或 `.env.*`。
const ENV_FILE: &str = ".env";

/// 敏感的扩展名（小写，带点）。
const SENSITIVE_EXTENSIONS: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".kdbx"];

/// GCP 服务账号密钥常见的文件名：`credentials.json`、`service-account*.json`（`service_account*.json` 同理）。
fn is_cloud_credentials_name(name: &str) -> bool {
    segment_matches(name, "credentials.json", true)
        || ["service-account", "service_account"]
            .iter()
            .any(|prefix| name.starts_with(prefix) && name.ends_with(".json"))
}

/// sshd 的主机私钥：`ssh_host_ed25519_key`、`ssh_host_rsa_key`……（可能带备份后缀）；`.pub` 是公钥，不算。
fn is_ssh_host_key_name(name: &str) -> bool {
    name.strip_prefix("ssh_host_")
        .and_then(|rest| rest.find("_key").map(|at| &rest[at + "_key".len()..]))
        .is_some_and(|suffix| !suffix.starts_with(".pub"))
}

/// `/proc/<pid>/environ`（含 `self`、`thread-self`、`task/<tid>/`）：进程的环境变量，常有密钥和令牌。
/// 返回 `Some(是否精确)`；中间段是通配符 / 变量时按"可能命中"。
fn proc_environ(parts: &[&str]) -> Option<bool> {
    let at = parts.iter().position(|part| *part == "proc")?;
    let rest = &parts[at + 1..];
    let (last, middle) = rest.split_last()?;
    if *last != "environ" || middle.is_empty() {
        return None;
    }
    let mut exact = true;
    for segment in middle {
        let known = matches!(*segment, "self" | "thread-self" | "task")
            || (!segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()));
        if known {
            continue;
        }
        if has_glob(segment) || has_variable(segment) {
            exact = false;
            continue;
        }
        return None;
    }
    Some(exact)
}

/// 文件名以这些开头的是 SSH 私钥（`id_rsa`、`id_rsa.pub` 一并算：公钥没有秘密，但名字
/// 一样，宁可问；表是产品定的，不在这里放宽）。
const KEY_NAME_PREFIXES: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"];

/// 这些命令的参数不是路径，不检查（`echo ~/.ssh/id_rsa` 只是打印一串字）。
const NON_PATH_COMMANDS: &[&str] = &[
    "echo", "printf", "true", "false", "sleep", "pwd", "whoami", "uname", "arch", "nproc",
    "uptime", "free", "history",
];

/// 命中的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SensitiveHit {
    /// 命中的那个路径参数（用户看到的原文）。
    pub path: String,
    /// 路径是明确写出来的（`false` = 含通配符 / 变量，按“可能命中”处理）。
    pub exact: bool,
}

impl SensitiveHit {
    /// 确认卡片上写的原因。
    pub fn reason(&self) -> String {
        let path = clip(&self.path, 120);
        if self.exact {
            format!("读取敏感文件：{path}")
        } else {
            format!("读取敏感文件：{path}（含通配符或变量，无法确定具体文件，按敏感处理）")
        }
    }
}

fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}…")
}

/// 文件工具（`read_file` / `list_dir` / `stat`）的路径：就是一个字面路径，不经过 shell。
pub fn sensitive_file_path(path: &str) -> Option<SensitiveHit> {
    matches_literal(path).then(|| SensitiveHit {
        path: path.to_owned(),
        exact: true,
    })
}

/// 命令里第一个敏感路径参数。命令名本身（每段的第一个词）不查。
pub fn sensitive_path_in_command(command: &str) -> Option<SensitiveHit> {
    for segment in split_segments(command) {
        // 引号没配平：交给 `is_read_only` 去拒，这里保守按命中。
        let Ok(words) = tokenize_marked(&segment) else {
            return Some(SensitiveHit {
                path: segment.trim().to_owned(),
                exact: false,
            });
        };
        let Some(name) = words.first().map(|word| word.text.as_str()) else {
            continue;
        };
        if NON_PATH_COMMANDS.contains(&name) {
            continue;
        }
        for word in words.iter().skip(1) {
            if let Some(hit) = check_word(&word.text, word.expands) {
                return Some(hit);
            }
        }
    }
    None
}

fn check_word(text: &str, expands: bool) -> Option<SensitiveHit> {
    // 重定向写在词前面：`cat </etc/shadow`、`2>/dev/null`。
    let text = text.trim_start_matches(['<', '>', '&']);
    let mut candidates = vec![text];
    // `--file=/etc/shadow` 这种把路径放在等号后面的写法。
    if let Some((_, value)) = text.split_once('=') {
        candidates.push(value);
    }
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        let variants = expand_braces(candidate);
        // 大括号展开不了（`{1..3}`、层数太多）：路径无法确定。
        let Some(variants) = variants else {
            return Some(SensitiveHit {
                path: text.to_owned(),
                exact: false,
            });
        };
        for variant in &variants {
            match classify(variant, expands || variants.len() > 1) {
                Verdict::Safe => {}
                Verdict::Exact => {
                    return Some(SensitiveHit {
                        path: text.to_owned(),
                        exact: variants.len() == 1 && !has_glob(variant),
                    })
                }
                Verdict::Maybe => {
                    return Some(SensitiveHit {
                        path: text.to_owned(),
                        exact: false,
                    })
                }
            }
        }
    }
    None
}

enum Verdict {
    Safe,
    /// 字面上就是敏感路径。
    Exact,
    /// 含通配符 / 变量，可能是敏感路径。
    Maybe,
}

fn matches_literal(path: &str) -> bool {
    !matches!(classify(path, false), Verdict::Safe)
}

fn classify(path: &str, shell_expanded: bool) -> Verdict {
    let normalized = path.replace('\\', "/").to_lowercase();
    // `/etc/x/../shadow` 和 `/etc/shadow` 是同一个文件：先把 `.` 和 `..` 折叠掉。
    let mut parts: Vec<&str> = Vec::new();
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return Verdict::Safe;
    }
    let globbing = shell_expanded || parts.iter().any(|part| has_glob(part));

    // 路径段规则。
    for rule in PATH_RULES {
        for window in parts.windows(rule.parts.len()) {
            let matched = window
                .iter()
                .zip(rule.parts)
                .enumerate()
                .all(|(i, (seg, lit))| {
                    let prefix = rule.last_is_prefix && i + 1 == rule.parts.len();
                    segment_matches(seg, lit, prefix)
                });
            if matched {
                return if window.iter().any(|seg| has_glob(seg)) {
                    Verdict::Maybe
                } else {
                    Verdict::Exact
                };
            }
        }
    }

    if let Some(exact) = proc_environ(&parts) {
        return if exact {
            Verdict::Exact
        } else {
            Verdict::Maybe
        };
    }

    // 文件名规则（只看最后一段）。
    if let Some(name) = parts.last() {
        if filename_is_sensitive(name) {
            return if has_glob(name) {
                Verdict::Maybe
            } else {
                Verdict::Exact
            };
        }
    }

    // 没命中已知名字：含变量的段没法判断，按可能命中。`$HOME` 是普通的家目录写法。
    if globbing
        && parts
            .iter()
            .any(|part| has_variable(part) && !is_home_variable(part))
    {
        return Verdict::Maybe;
    }
    Verdict::Safe
}

fn is_home_variable(segment: &str) -> bool {
    matches!(segment, "$home" | "${home}")
}

/// 段里有 shell 会展开的变量引用：`$` 后面跟名字 / `{`；`foo$`、`a$|b`（正则的行尾）不算。
fn has_variable(segment: &str) -> bool {
    let chars: Vec<char> = segment.chars().collect();
    chars.iter().enumerate().any(|(i, c)| {
        *c == '$'
            && chars.get(i + 1).is_some_and(|next| {
                next.is_alphanumeric() || matches!(next, '_' | '{' | '@' | '*' | '#' | '?' | '!')
            })
    })
}

fn has_glob(segment: &str) -> bool {
    segment.contains(['*', '?', '['])
}

/// 一段是否对应规则里的某个固定名字。
fn segment_matches(segment: &str, literal: &str, prefix: bool) -> bool {
    if has_glob(segment) {
        // 只有通配符、没有固定字符的段（`*`、`??`）不去猜。
        if !segment.chars().any(|c| !matches!(c, '*' | '?' | '[' | ']')) {
            return false;
        }
        return glob_match(segment, literal);
    }
    if prefix {
        // `shadow-`、`shadow.bak`、`sudoers.d`、`.npmrc~` 算；`shadowsocks.json` 不算：
        // 后面紧跟字母数字说明是另一个名字。
        segment.strip_prefix(literal).is_some_and(|rest| {
            rest.chars()
                .next()
                .is_none_or(|next| !next.is_alphanumeric())
        })
    } else {
        segment == literal
    }
}

fn filename_is_sensitive(name: &str) -> bool {
    if has_glob(name) {
        // 没有固定字符的不猜；有的话拿几个代表性的名字去试。
        if !name.chars().any(|c| !matches!(c, '*' | '?' | '[' | ']')) {
            return false;
        }
        let samples = [
            ".env",
            ".env.local",
            "id_rsa",
            "id_ed25519",
            "id_ecdsa",
            "id_dsa",
            "a.pem",
            "a.key",
            "a.p12",
            "a.pfx",
            "a.kdbx",
            "ssh_host_rsa_key",
            "credentials.json",
            "service-account.json",
        ];
        return samples.iter().any(|sample| glob_match(name, sample))
            || SENSITIVE_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
            || KEY_NAME_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix));
    }
    name == ENV_FILE
        || name.starts_with(".env.")
        || is_ssh_host_key_name(name)
        || is_cloud_credentials_name(name)
        || SENSITIVE_EXTENSIONS.iter().any(|ext| name.ends_with(ext))
        || KEY_NAME_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// 通配符匹配：`*` 任意串，`?` 任意一个字符，`[...]` 当作任意一个字符（宁可多认）。
/// 两边都已经小写。
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_from(&pattern, &text)
}

fn glob_from(pattern: &[char], text: &[char]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some('*') => (0..=text.len()).any(|skip| glob_from(&pattern[1..], &text[skip..])),
        Some('?') => !text.is_empty() && glob_from(&pattern[1..], &text[1..]),
        Some('[') => {
            let close = pattern.iter().position(|c| *c == ']');
            match close {
                Some(end) => !text.is_empty() && glob_from(&pattern[end + 1..], &text[1..]),
                // 没配平的 `[` 当普通字符。
                None => text.first() == Some(&'[') && glob_from(&pattern[1..], &text[1..]),
            }
        }
        Some(c) => text.first() == Some(c) && glob_from(&pattern[1..], &text[1..]),
    }
}

/// 展开 `{a,b}`（一层一层，最多 64 个结果）。没有大括号返回原词；`{1..3}`、没配平、
/// 结果太多返回 `None`（调用方按“无法确定”处理）。`${VAR}` 和不含逗号的 `{x}` 不是
/// 大括号展开（bash 原样保留），不算。
fn expand_braces(word: &str) -> Option<Vec<String>> {
    const MAX_VARIANTS: usize = 64;
    // 用不会出现在路径里的控制字符把“不是展开”的大括号暂时遮住，最后再还原。
    const OPEN_MASK: char = '\u{1}';
    const CLOSE_MASK: char = '\u{2}';
    let mut pending = vec![word.replace("${", &format!("${OPEN_MASK}"))];
    let mut done = Vec::new();
    while let Some(current) = pending.pop() {
        let Some(open) = current.find('{') else {
            done.push(current);
            if done.len() + pending.len() > MAX_VARIANTS {
                return None;
            }
            continue;
        };
        // 找和它配对的 `}`，同时记下最外层的逗号。
        let mut depth = 0usize;
        let mut commas = Vec::new();
        let mut close = None;
        for (offset, c) in current[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close = Some(open + offset);
                        break;
                    }
                }
                ',' if depth == 1 => commas.push(open + offset),
                _ => {}
            }
        }
        let close = close?;
        if commas.is_empty() {
            if current[open..close].contains("..") {
                // 序列表达式 `{1..3}`：值要 shell 算，无法确定。
                return None;
            }
            // 普通的 `{x}`：bash 原样保留，遮住后继续找下一对。
            let mut masked = String::with_capacity(current.len());
            masked.push_str(&current[..open]);
            masked.push(OPEN_MASK);
            masked.push_str(&current[open + 1..close]);
            masked.push(CLOSE_MASK);
            masked.push_str(&current[close + 1..]);
            pending.push(masked);
            continue;
        }
        let prefix = &current[..open];
        let suffix = &current[close + 1..];
        let mut starts = vec![open + 1];
        starts.extend(commas.iter().map(|comma| comma + 1));
        let mut ends = commas.clone();
        ends.push(close);
        for (start, end) in starts.into_iter().zip(ends) {
            pending.push(format!("{prefix}{}{suffix}", &current[start..end]));
        }
        if pending.len() + done.len() > MAX_VARIANTS {
            return None;
        }
    }
    done.reverse();
    Some(
        done.into_iter()
            .map(|variant| variant.replace(OPEN_MASK, "{").replace(CLOSE_MASK, "}"))
            .collect(),
    )
}

#[cfg(test)]
mod tests;
