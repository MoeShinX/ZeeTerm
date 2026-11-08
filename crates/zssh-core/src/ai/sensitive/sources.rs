//! 不靠"路径参数"命中的敏感读取：命令本身会把秘密打出来，或者搜索范围覆盖敏感目录。
//!
//! 路径表（[`super::PATH_RULES`]）管的是"读这个文件"。下面这些命令参数里没有任何敏感路径，
//! 输出里却照样有秘密，所以按**命令**判断，原因走同一条"敏感 → 要确认并写明原因"的通道：
//! - `getent shadow` / `getent gshadow` / 不认识的数据库：密码哈希；
//! - `jq` 过滤器里的 `env` / `$ENV`、`systemctl show-environment|cat|show`、不带变量名的 `env` / `printenv` /
//!   `set` / `export -p` / `declare -p`：环境变量，常有密钥和令牌；
//! - `history`、`fc -l`：shell 历史，常有命令行里敲过的密码；
//! - `grep -r` / `rg` 的搜索根目录是敏感路径的祖先（`/`、`/home`、`/root`、家目录、`/etc`、`/proc`……）：
//!   一次搜索就读遍了私钥和凭据。
//!
//! 已知的缺口（有意不猜）：搜索根目录是相对路径、而本条命令里又没有 `cd` 时，不知道当前目录在哪，不判；
//! `/var/www`、`/opt` 这类业务目录下可能有 `.env`，不在"祖先"表里；`diff -r` 等别的递归读取不在此列。

use super::{glob_match, has_glob, has_variable, SensitiveHit, SensitiveKind, PATH_RULES};
use crate::ai::readonly::{
    getent_database, getent_database_is_public, jq_reads_environment, name_looks_secret,
    program_name, rg_positional, systemctl_verb,
};
use crate::ai::safety::Word;

fn hit(kind: SensitiveKind, path: impl Into<String>) -> SensitiveHit {
    SensitiveHit {
        path: path.into(),
        exact: true,
        kind,
    }
}

/// 这一段命令的命令名 + 参数是不是在读上面说的那些来源。`cwd` 是前面 `cd` 之后的工作目录（不知道就是 `None`）。
pub(super) fn command_source_hit(words: &[Word], cwd: Option<&str>) -> Option<SensitiveHit> {
    let head = words.first()?;
    if head.expands {
        return None;
    }
    let name = program_name(&head.text)?;
    let args = &words[1..];
    let texts: Vec<String> = args.iter().map(|word| word.text.clone()).collect();
    match name {
        "getent" => {
            let database = getent_database(&texts)?;
            (!getent_database_is_public(&database))
                .then(|| hit(SensitiveKind::AccountDatabase, format!("getent {database}")))
        }
        "jq" => jq_reads_environment(args).then(|| hit(SensitiveKind::Environment, "jq")),
        "history" => texts
            .iter()
            .all(|arg| arg.chars().all(|c| c.is_ascii_digit()))
            .then(|| hit(SensitiveKind::ShellHistory, "history")),
        // fc -l / -ln / -lr：列出历史；不带 -l 的 fc 是编辑并重新执行，不是读历史
        "fc" => texts
            .iter()
            .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains('l'))
            .then(|| hit(SensitiveKind::ShellHistory, "fc -l")),
        "systemctl" => {
            let verb = systemctl_verb(&texts).or_else(|| {
                // 选项里有不认识的：退回"参数里有没有这几个子命令"
                texts
                    .iter()
                    .find(|arg| matches!(arg.as_str(), "show" | "cat" | "show-environment"))
                    .cloned()
            })?;
            matches!(verb.as_str(), "show" | "cat" | "show-environment")
                .then(|| hit(SensitiveKind::Environment, format!("systemctl {verb}")))
        }
        "env" => texts
            .iter()
            .all(|arg| matches!(arg.as_str(), "-0" | "--null"))
            .then(|| hit(SensitiveKind::Environment, "env")),
        "printenv" => {
            let options_ok = texts
                .iter()
                .filter(|arg| arg.starts_with('-'))
                .all(|arg| matches!(arg.as_str(), "-0" | "--null" | "--"));
            let names: Vec<&String> = texts.iter().filter(|arg| !arg.starts_with('-')).collect();
            (options_ok && (names.is_empty() || names.iter().any(|n| name_looks_secret(n))))
                .then(|| hit(SensitiveKind::Environment, "printenv"))
        }
        "set" => texts
            .is_empty()
            .then(|| hit(SensitiveKind::Environment, "set")),
        // 不带变量名、只带 -p / -x 的：把整份变量打出来
        "declare" | "typeset" | "export" | "readonly" => texts
            .iter()
            .all(|arg| {
                arg.len() > 1
                    && arg.starts_with('-')
                    && arg[1..].chars().all(|c| c == 'p' || c == 'x')
            })
            .then(|| hit(SensitiveKind::Environment, format!("{name} -p"))),
        "grep" | "egrep" | "fgrep" | "zgrep" | "rg" => {
            let (recursive, mut roots) = if name == "rg" {
                rg_roots(&texts)
            } else {
                grep_roots(&texts)
            };
            if !recursive {
                return None;
            }
            if roots.is_empty() {
                // 不写路径：搜当前目录
                roots.push(".".to_owned());
            }
            roots
                .into_iter()
                .find(|root| root_covers_sensitive(root, cwd))
                .map(|root| hit(SensitiveKind::RecursiveSearch, root))
        }
        _ => None,
    }
}

/// `cd` 之后的工作目录。`cd`（无参数）回家目录；`cd -`、含非 HOME 变量的、相对路径而前面又不知道目录的，
/// 都是"不知道"（`None`）。
pub(super) fn cwd_after(words: &[Word], cwd: Option<String>) -> Option<String> {
    let is_cd = words
        .first()
        .and_then(|word| program_name(&word.text))
        .is_some_and(|name| name == "cd");
    if !is_cd {
        return cwd;
    }
    let target = words
        .iter()
        .skip(1)
        .map(|word| word.text.as_str())
        .find(|arg| !arg.starts_with('-') || *arg == "-");
    match target {
        None => Some("~".to_owned()),
        Some("-") => None,
        Some(path) => {
            let lower = path.to_lowercase();
            let anchored = lower.starts_with('/')
                || lower.starts_with('~')
                || lower == "$home"
                || lower.starts_with("$home/")
                || lower == "${home}"
                || lower.starts_with("${home}/");
            if anchored {
                Some(path.to_owned())
            } else if lower.starts_with('$') {
                None
            } else {
                cwd.map(|base| format!("{base}/{path}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 搜索根目录
// ---------------------------------------------------------------------------

/// grep 的长选项：(名字, 种类)。0 = 不带值，1 = 必定带值，2 = 值可有可无（只认 `=`）。
const GREP_LONG: &[(&str, u8)] = &[
    ("extended-regexp", 0),
    ("fixed-strings", 0),
    ("basic-regexp", 0),
    ("perl-regexp", 0),
    ("ignore-case", 0),
    ("no-ignore-case", 0),
    ("word-regexp", 0),
    ("line-regexp", 0),
    ("no-messages", 0),
    ("invert-match", 0),
    ("version", 0),
    ("help", 0),
    ("count", 0),
    ("files-without-match", 0),
    ("files-with-matches", 0),
    ("only-matching", 0),
    ("quiet", 0),
    ("silent", 0),
    ("with-filename", 0),
    ("no-filename", 0),
    ("null", 0),
    ("byte-offset", 0),
    ("line-number", 0),
    ("line-buffered", 0),
    ("initial-tab", 0),
    ("text", 0),
    ("null-data", 0),
    ("unix-byte-offsets", 0),
    ("recursive", 0),
    ("dereference-recursive", 0),
    ("regexp", 1),
    ("file", 1),
    ("max-count", 1),
    ("label", 1),
    ("after-context", 1),
    ("before-context", 1),
    ("context", 1),
    ("group-separator", 1),
    ("include", 1),
    ("exclude", 1),
    ("exclude-from", 1),
    ("exclude-dir", 1),
    ("devices", 1),
    ("directories", 1),
    ("binary-files", 1),
    ("color", 2),
    ("colour", 2),
];

/// 长选项名（getopt 允许缩写成唯一前缀）→ 完整名字和种类。认不得返回 `None`。
fn resolve_grep_long(name: &str) -> Option<(&'static str, u8)> {
    if let Some(exact) = GREP_LONG.iter().find(|(full, _)| *full == name) {
        return Some(*exact);
    }
    let mut matches = GREP_LONG.iter().filter(|(full, _)| full.starts_with(name));
    let first = *matches.next()?;
    // `--rec` 只能是 recursive；`--re` 同时是 regexp / recursive，认不出
    matches.next().is_none().then_some(first)
}

/// grep 家族的参数：是否递归 + 搜索根目录。带 `-e` / `-f` / `--regexp` / `--file` 时所有位置参数都是路径，
/// 否则第一个位置参数是模式。选项解析不确定（不认识的长选项）时把所有位置参数都当路径——只会多问。
fn grep_roots(args: &[String]) -> (bool, Vec<String>) {
    let mut recursive = false;
    let mut pattern_option = false;
    let mut uncertain = false;
    let mut positional: Vec<String> = Vec::new();
    let mut only_positional = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        i += 1;
        if only_positional || arg == "-" || !arg.starts_with('-') {
            positional.push(arg.to_owned());
            continue;
        }
        if arg == "--" {
            only_positional = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long, None),
            };
            let Some((full, kind)) = resolve_grep_long(name) else {
                uncertain = true;
                continue;
            };
            let mut taken = value.map(str::to_owned);
            if kind == 1 && taken.is_none() {
                taken = args.get(i).cloned();
                i += 1;
            }
            match full {
                "recursive" | "dereference-recursive" => recursive = true,
                "regexp" | "file" => pattern_option = true,
                "directories" if taken.as_deref().is_some_and(|v| v.starts_with('r')) => {
                    recursive = true;
                }
                _ => {}
            }
            continue;
        }
        let cluster = &arg[1..];
        for (index, c) in cluster.char_indices() {
            match c {
                'r' | 'R' => recursive = true,
                // 带值的短选项：值是紧贴的后半截，或者下一个参数
                'A' | 'B' | 'C' | 'D' | 'd' | 'e' | 'f' | 'm' => {
                    let rest = &cluster[index + c.len_utf8()..];
                    let value = if rest.is_empty() {
                        let next = args.get(i).cloned().unwrap_or_default();
                        i += 1;
                        next
                    } else {
                        rest.to_owned()
                    };
                    match c {
                        'e' | 'f' => pattern_option = true,
                        'd' if value.starts_with('r') => recursive = true,
                        _ => {}
                    }
                    break;
                }
                _ => {}
            }
        }
    }
    let roots = if pattern_option || uncertain {
        positional
    } else {
        positional.into_iter().skip(1).collect()
    };
    (recursive, roots)
}

/// rg 默认就递归。位置参数的规则同 grep：没有 `-e` / `-f` 类选项时第一个是模式。
fn rg_roots(args: &[String]) -> (bool, Vec<String>) {
    let pattern_option = args.iter().any(|arg| {
        if let Some(long) = arg.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            return matches!(name, "regexp" | "file" | "files" | "type-list");
        }
        // 短选项簇里有 e / f 就当作带了模式选项（多认只会多出几个"路径"，不会漏）
        arg.len() > 1 && arg.starts_with('-') && arg[1..].contains(['e', 'f'])
    });
    let Some(positional) = rg_positional(args) else {
        // 有不认识的选项：所有不带 - 的词都当路径
        let all = args
            .iter()
            .filter(|a| !a.starts_with('-'))
            .cloned()
            .collect();
        return (true, all);
    };
    let roots = if pattern_option {
        positional
    } else {
        positional.into_iter().skip(1).collect()
    };
    (true, roots)
}

/// 家目录下的敏感位置（路径表里以 `.` 开头的那些，都是相对家目录的）。
fn home_anchors() -> impl Iterator<Item = &'static [&'static str]> {
    PATH_RULES
        .iter()
        .map(|rule| rule.parts)
        .filter(|parts| parts.first().is_some_and(|first| first.starts_with('.')))
}

/// `/etc` 下的敏感位置（`etc` 之后的部分）：路径表里的 + 里面放着私钥的几个目录
/// （sshd 主机私钥、`ssl/private`、证书机构签发目录）。
fn etc_anchors() -> impl Iterator<Item = &'static [&'static str]> {
    const EXTRA: &[&[&str]] = &[
        &["sudoers.d"],
        &["ssh"],
        &["ssl", "private"],
        &["letsencrypt"],
        &["pki"],
    ];
    PATH_RULES
        .iter()
        .map(|rule| rule.parts)
        .filter(|parts| parts.first() == Some(&"etc"))
        .map(|parts| &parts[1..])
        .chain(EXTRA.iter().copied())
}

/// 用户写的这一段能不能对上锚点里的那一段（通配符、变量按"可能"算）。
fn segment_may_equal(user: &str, anchor: &str) -> bool {
    if has_glob(user) {
        return glob_match(user, anchor);
    }
    has_variable(user) || user == anchor
}

/// `rest` 是不是某个锚点的前缀（含相等）：搜索根目录在锚点之上或就是锚点，搜下去就会碰到它。
fn covers_any(rest: &[&str], anchors: impl Iterator<Item = &'static [&'static str]>) -> bool {
    anchors.into_iter().any(|anchor| {
        rest.len() <= anchor.len()
            && rest
                .iter()
                .zip(anchor)
                .all(|(user, anchor)| segment_may_equal(user, anchor))
    })
}

fn is_pid_like(segment: &str) -> bool {
    matches!(segment, "self" | "thread-self")
        || (!segment.is_empty() && segment.chars().all(|c| c.is_ascii_digit()))
        || has_glob(segment)
        || has_variable(segment)
}

/// `/proc/<pid>/environ`、`/proc/<pid>/task/<tid>/environ` 之上（含 `/proc` 本身）。
fn proc_covers_environ(rest: &[&str]) -> bool {
    match rest {
        [] => true,
        [pid] => is_pid_like(pid),
        [pid, next] => {
            is_pid_like(pid)
                && (segment_may_equal(next, "environ") || segment_may_equal(next, "task"))
        }
        [pid, task, tid] => is_pid_like(pid) && segment_may_equal(task, "task") && is_pid_like(tid),
        [pid, task, tid, last] => {
            is_pid_like(pid)
                && segment_may_equal(task, "task")
                && is_pid_like(tid)
                && segment_may_equal(last, "environ")
        }
        _ => false,
    }
}

fn is_home_variable_prefix(path: &str) -> bool {
    ["$home", "${home}"].iter().any(|home| {
        path == *home
            || path
                .strip_prefix(home)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// 递归搜索这个根目录，会不会一路搜到敏感位置（它是不是敏感路径的祖先，或者就是）。
fn root_covers_sensitive(root: &str, cwd: Option<&str>) -> bool {
    let mut path = root.replace('\\', "/").to_lowercase();
    if path.is_empty() {
        return false;
    }
    let anchored = path.starts_with('/') || path.starts_with('~') || is_home_variable_prefix(&path);
    if !anchored {
        // 其他变量开头：路径不确定，`check_word` 已经按"可能命中"处理了
        if path.starts_with('$') {
            return false;
        }
        // 相对路径：只有前面 `cd` 过才知道从哪搜；否则不知道当前目录，不猜
        let Some(base) = cwd else {
            return false;
        };
        path = format!("{}/{path}", base.replace('\\', "/").to_lowercase());
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    let Some(first) = parts.first().copied() else {
        // `/`：整个文件系统
        return true;
    };
    // `~`、`~user`、`$HOME`、`${HOME}`：家目录
    if first.starts_with('~') || first == "$home" || first == "${home}" {
        return covers_any(&parts[1..], home_anchors());
    }
    if first.starts_with('$') {
        return false;
    }
    // 第一段是通配符（`/*`、`/h*`）：对每个可能的顶层目录都试一遍
    let top_levels: Vec<&str> = if has_glob(first) {
        ["home", "root", "etc", "proc"]
            .into_iter()
            .filter(|candidate| glob_match(first, candidate))
            .collect()
    } else {
        vec![first]
    };
    top_levels.into_iter().any(|top| match top {
        // `/home`（所有人的家目录）、`/home/<用户>`、`/home/<用户>/…`
        "home" => parts.len() == 1 || covers_any(&parts[2..], home_anchors()),
        "root" => covers_any(&parts[1..], home_anchors()),
        "etc" => covers_any(&parts[1..], etc_anchors()),
        "proc" => proc_covers_environ(&parts[1..]),
        _ => false,
    })
}
