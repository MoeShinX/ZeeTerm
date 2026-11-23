//! 语法层：一行拆成关键字和参数，以及 `Host` / `Include` 用的通配符匹配。

/// 一行配置：`Key value…` 或 `Key=value…`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Directive {
    /// 关键字小写，用来比较。
    pub key: String,
    /// 关键字的原写法，用来给用户看。
    pub key_raw: String,
    pub args: Vec<String>,
}

/// 解析一行；空行和 `#` 开头的整行注释返回 `None`。
pub(super) fn parse_line(line: &str) -> Option<Directive> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let key_end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let key_raw = &line[..key_end];
    if key_raw.is_empty() {
        return None;
    }
    // 关键字和值之间：空白，或者（可以带空白的）一个等号。
    let mut rest = line[key_end..].trim_start();
    if let Some(after) = rest.strip_prefix('=') {
        rest = after.trim_start();
    }
    Some(Directive {
        key: key_raw.to_ascii_lowercase(),
        key_raw: key_raw.to_owned(),
        args: split_args(rest),
    })
}

/// 按空白切参数；双引号里的空白不切，引号本身去掉；不在引号里、出现在参数开头的 `#` 起，后面是行尾注释。
///
/// 反斜杠不当转义（Windows 路径里全是）；引号里只认 `\"`。
fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' => quoted = false,
                '\\' if chars.peek() == Some(&'"') => {
                    current.push('"');
                    chars.next();
                }
                _ => current.push(c),
            }
            continue;
        }
        match c {
            '"' => {
                quoted = true;
                started = true;
            }
            c if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '#' if !started => break,
            _ => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// 含不含通配符（`*`、`?`）。
pub(super) fn has_wildcard(text: &str) -> bool {
    text.contains(['*', '?'])
}

/// 通配符匹配：`*` 任意多个字符，`?` 一个字符；不分大小写。迭代实现，没有指数级回溯。
pub(super) fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let (mut p, mut t) = (0, 0);
    // 最近一个 `*` 的位置，和它当前吃到文本的哪里。
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, t));
            p += 1;
        } else if let Some((star_p, star_t)) = star {
            p = star_p + 1;
            t = star_t + 1;
            star = Some((star_p, star_t + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// `Host` 行的模式列表是否匹配这个别名：有一个肯定模式匹配，并且没有任何否定（`!`）模式匹配。
/// OpenSSH 的规则是只要否定模式匹配上了，整行就作废，不管别的模式匹不匹配。
pub(super) fn host_line_matches(patterns: &[String], alias: &str) -> bool {
    let mut matched = false;
    for pattern in patterns {
        match pattern.strip_prefix('!') {
            Some(negated) => {
                if glob_match(negated, alias) {
                    return false;
                }
            }
            None => matched |= glob_match(pattern, alias),
        }
    }
    matched
}

/// 这个 `Host` 模式能不能当一台主机导入：不含通配符、不是否定、不为空。
pub(super) fn is_importable_alias(pattern: &str) -> bool {
    !pattern.is_empty() && !pattern.starts_with('!') && !has_wildcard(pattern)
}
