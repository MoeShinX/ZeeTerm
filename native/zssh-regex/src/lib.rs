//! 终端高亮的双正则引擎。
//!
//! 现有实现用浏览器 JS RegExp（`ui/src/terminal/highlight.ts`）：`g` / `gi` 两个
//! flag，跳过零长度匹配，规则按顺序优先。Rust 标准 `regex` 是线性时间但不支持
//! 环视和反向引用；`fancy-regex` 支持这些语法，但是回溯引擎，必须设上限。
//!
//! 分流不靠字符串猜测：先用 `regex` 编译，失败再交给 `fancy-regex`。这样普通规则
//! 保持线性时间，只有真正需要环视/反向引用的规则才进入回溯路径。
//!
//! 已知差异（不在本层静默修补，交给 G02 的真实语料继续核对）：
//! - JS `\d` / `\w` 是 ASCII；Rust 默认是 Unicode（全角数字也算 `\d`）。
//! - JS `\b` 是 ASCII 单词边界；Rust 默认把中文当单词字符。这里把 `\b` / `\B`
//!   改写成 `(?-u:\b)`，让「中文error中文」仍能命中，与 JS 一致。
//! - JS 没有 `(?<name>...)` 之外的 Rust 专有语法；反向引用 `\1` 只在 fancy 路径可用。

use std::ops::Range;
use std::time::Instant;

/// 回溯路径的步数上限。超过就返回 `BudgetExceeded`，由调用方只暂停当前规则。
pub const DEFAULT_BACKTRACK_LIMIT: usize = 1_000_000;
/// 单行最多匹配多少个；与现有 UI 的每次扫描上限同一量级。
pub const DEFAULT_MATCH_LIMIT: usize = 200;
/// 单行输入上限，防止异常超长行把 CPU 拖住。
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// 规则长度上限。
pub const MAX_PATTERN_BYTES: usize = 4 * 1024;
/// 编译超时保护：fancy 编译本身也可能很慢。
pub const COMPILE_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// 标准 `regex`，线性时间。
    Linear,
    /// `fancy-regex`，回溯，受 `backtrack_limit` 限制。
    Backtracking,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    EmptyPattern,
    TooLong,
    MatchesEmpty,
    Unsupported(String),
    TimedOut,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyPattern => write!(f, "不能为空"), // i18n-allow: 日志/测试用的 Display；界面按错误类型生成文案（highlight_editor::compile_error_text）
            Self::TooLong => write!(f, "规则太长"), // i18n-allow: 日志/测试用的 Display；界面按错误类型生成文案（highlight_editor::compile_error_text）
            Self::MatchesEmpty => write!(f, "这个正则能匹配空字符串，会标满整屏"), // i18n-allow: 日志/测试用的 Display；界面按错误类型生成文案（highlight_editor::compile_error_text）
            Self::Unsupported(message) => write!(f, "{message}"),
            Self::TimedOut => write!(f, "正则编译超时"), // i18n-allow: 日志/测试用的 Display；界面按错误类型生成文案（highlight_editor::compile_error_text）
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchError {
    /// 回溯步数用完；只应影响这条规则，不影响其他规则或界面。
    BudgetExceeded,
    LineTooLong,
}

pub struct CompiledRule {
    engine: Engine,
    linear: Option<regex::Regex>,
    fancy: Option<fancy_regex::Regex>,
}

impl CompiledRule {
    pub fn engine(&self) -> Engine {
        self.engine
    }

    /// 返回字节区间；零长度匹配按 JS 实现跳过。
    pub fn find(&self, line: &str, limit: usize) -> Result<Vec<Range<usize>>, MatchError> {
        if line.len() > MAX_LINE_BYTES {
            return Err(MatchError::LineTooLong);
        }

        let mut found = Vec::new();
        match (&self.linear, &self.fancy) {
            (Some(re), _) => {
                for m in re.find_iter(line) {
                    if m.start() == m.end() {
                        continue;
                    }
                    found.push(m.start()..m.end());
                    if found.len() >= limit {
                        break;
                    }
                }
            }
            (_, Some(re)) => {
                // find_iter 的每一步都可能触发回溯；额外给一个迭代上限，
                // 避免空匹配或零宽断言导致无限循环。
                let max_steps = limit.saturating_mul(8).max(64);
                let mut steps = 0usize;
                for m in re.find_iter(line) {
                    steps += 1;
                    if steps > max_steps {
                        return Err(MatchError::BudgetExceeded);
                    }
                    let m = m.map_err(|_| MatchError::BudgetExceeded)?;
                    if m.start() == m.end() {
                        continue;
                    }
                    found.push(m.start()..m.end());
                    if found.len() >= limit {
                        break;
                    }
                }
            }
            _ => {}
        }
        Ok(found)
    }
}

/// 按现有 JS 语义编译：`caseSensitive=false` 等价于 `i` flag。
pub fn compile(pattern: &str, case_sensitive: bool) -> Result<CompiledRule, CompileError> {
    if pattern.is_empty() {
        return Err(CompileError::EmptyPattern);
    }
    if pattern.len() > MAX_PATTERN_BYTES {
        return Err(CompileError::TooLong);
    }

    let translated = ascii_word_boundaries(pattern);
    let started = Instant::now();

    if let Ok(re) = regex::RegexBuilder::new(&translated)
        .case_insensitive(!case_sensitive)
        .size_limit(1 << 20)
        .build()
    {
        if re.is_match("") {
            return Err(CompileError::MatchesEmpty);
        }
        return Ok(CompiledRule {
            engine: Engine::Linear,
            linear: Some(re),
            fancy: None,
        });
    }

    if started.elapsed() > COMPILE_BUDGET {
        return Err(CompileError::TimedOut);
    }

    let fancy = fancy_regex::RegexBuilder::new(&translated)
        .case_insensitive(!case_sensitive)
        .backtrack_limit(DEFAULT_BACKTRACK_LIMIT)
        .build()
        .map_err(|err| CompileError::Unsupported(err.to_string()))?;

    if fancy.is_match("").unwrap_or(false) {
        return Err(CompileError::MatchesEmpty);
    }

    Ok(CompiledRule {
        engine: Engine::Backtracking,
        linear: None,
        fancy: Some(fancy),
    })
}

/// 把 `\b` / `\B` 改写成 ASCII 单词边界，和 JS（无 `u` flag）一致。
/// 字符类里的 `\b` 是退格，保持原样；`\\b` 这种转义过的反斜杠也保持原样。
fn ascii_word_boundaries(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 16);
    let mut chars = pattern.chars();
    let mut in_class = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\\' => match chars.next() {
                Some('b') if !in_class => out.push_str("(?-u:\\b)"),
                Some('B') if !in_class => out.push_str("(?-u:\\B)"),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '[' => {
                in_class = true;
                out.push(ch);
            }
            ']' => {
                in_class = false;
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// 字节区间转 UTF-16 下标，用来和 JS 结果对照。
pub fn byte_range_to_utf16(line: &str, range: Range<usize>) -> Range<usize> {
    let to_utf16 = |byte: usize| line[..byte].encode_utf16().count();
    to_utf16(range.start)..to_utf16(range.end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 与 `js-regex-corpus.mjs` 的输出逐条对应（UTF-16 下标）；第二个字段是 JS 的 `i` flag。
    type Case = (&'static str, bool, &'static str, &'static [(usize, usize)]);
    const CORPUS: &[Case] = &[
        (
            r"\b(errors?|fail(ed|ure|s)?|fatal|denied|refused|panic|critical)\b",
            true,
            "connect failed: connection refused",
            &[(8, 14), (27, 34)],
        ),
        (
            r"\b(warn(ings?)?|deprecated|timeout|timed out)\b",
            true,
            "WARN timeout after 5s",
            &[(0, 4), (5, 12)],
        ),
        (
            r"\b(ok|success(ful|fully)?|succeeded|passed|active|running)\b",
            true,
            "service active, status OK",
            &[(8, 14), (23, 25)],
        ),
        (
            r"\b(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?\b",
            true,
            "addr 192.168.1.10/24 port 22",
            &[(5, 20)],
        ),
        (
            r#"https?://[^\s'"<>]+"#,
            true,
            "see https://example.com/a?b=1 now",
            &[(4, 29)],
        ),
        (r"\berror\b", true, "中文error中文", &[(2, 7)]),
        (r"foo(?=bar)", false, "foobar foobaz", &[(0, 3)]),
        (r"(\w+)\s+\1", false, "hello hello world", &[(0, 11)]),
    ];

    #[test]
    fn matches_the_javascript_corpus() {
        for (pattern, case_insensitive, line, expected) in CORPUS {
            let rule = compile(pattern, !*case_insensitive).expect("规则应能编译");
            let found: Vec<(usize, usize)> = rule
                .find(line, DEFAULT_MATCH_LIMIT)
                .expect("匹配不应超预算")
                .into_iter()
                .map(|range| {
                    let r = byte_range_to_utf16(line, range);
                    (r.start, r.end)
                })
                .collect();
            assert_eq!(&found[..], *expected, "规则 {pattern} 在 {line:?} 上不一致");
        }
    }

    #[test]
    fn simple_rules_stay_linear() {
        for (pattern, case_insensitive, _, _) in CORPUS
            .iter()
            .filter(|(p, ..)| !p.contains("(?=") && !p.contains("\\1"))
        {
            let rule = compile(pattern, !*case_insensitive).expect("规则应能编译");
            assert_eq!(
                rule.engine(),
                Engine::Linear,
                "规则 {pattern} 不应进入回溯引擎"
            );
        }
    }

    #[test]
    fn advanced_syntax_falls_back_to_backtracking() {
        for pattern in [r"foo(?=bar)", r"(?<=foo)bar", r"(\w+)\s+\1"] {
            let rule = compile(pattern, false).expect("规则应能编译");
            assert_eq!(rule.engine(), Engine::Backtracking, "规则 {pattern}");
        }
    }

    #[test]
    fn ascii_word_boundary_keeps_cjk_neighbours_matching() {
        // Rust 默认把中文当单词字符，`\berror\b` 在「中文error中文」里不会命中；
        // 改写后与 JS 一致。
        let rule = compile(r"\berror\b", false).expect("规则应能编译");
        let found = rule
            .find("中文error中文", DEFAULT_MATCH_LIMIT)
            .expect("匹配");
        assert_eq!(found, vec![6..11]);
    }

    #[test]
    fn patterns_matching_empty_are_rejected() {
        for pattern in ["a*", "(?:)", "(foo)?"] {
            assert_eq!(
                compile(pattern, false).err(),
                Some(CompileError::MatchesEmpty),
                "规则 {pattern}"
            );
        }
    }

    #[test]
    fn catastrophic_backtracking_is_stopped() {
        // 环视在重复里强制走 fancy 的回溯 VM；40 个 a 后面不是 b，会指数级展开。
        let rule = compile(r"^(?:(?=a)a+)+$", false).expect("规则应能编译");
        let line = format!("{}c", "a".repeat(40));
        let started = std::time::Instant::now();
        let result = rule.find(&line, DEFAULT_MATCH_LIMIT);
        let elapsed = started.elapsed();
        assert_eq!(result, Err(MatchError::BudgetExceeded));
        assert!(
            elapsed < Duration::from_secs(5),
            "回溯保护太慢：{elapsed:?}"
        );
    }

    #[test]
    fn zero_length_matches_are_skipped() {
        // `a?` 能匹配空串，会被编译期拒绝；用 `(a|)` 验证运行期也不会卡住。
        assert_eq!(
            compile("(a|)", false).err(),
            Some(CompileError::MatchesEmpty)
        );
    }

    #[test]
    fn js_feature_matrix_is_covered() {
        // 命名组
        let rule = compile(r"(?<word>foo)+", false).expect("命名组应能编译");
        assert_eq!(rule.find("foofoo", 16).expect("匹配"), vec![0..6]);

        // 前向/后向环视
        let rule = compile(r"foo(?=bar)", false).expect("前向环视应能编译");
        assert_eq!(rule.find("foobar", 16).expect("匹配"), vec![0..3]);
        let rule = compile(r"(?<=foo)bar", false).expect("后向环视应能编译");
        assert_eq!(rule.find("foobar", 16).expect("匹配"), vec![3..6]);

        // 反向引用
        let rule = compile(r"(\w+)\s+\1", false).expect("反向引用应能编译");
        assert_eq!(rule.find("hi hi", 16).expect("匹配"), vec![0..5]);

        // Unicode property
        let rule = compile(r"\p{Han}+", false).expect("Unicode property 应能编译");
        assert_eq!(rule.find("中文abc", 16).expect("匹配"), vec![0..6]);

        // 全局匹配返回多段
        let rule = compile("ab", false).expect("普通模式");
        assert_eq!(
            rule.find("ab ab ab", 16).expect("匹配"),
            vec![0..2, 3..5, 6..8]
        );
    }

    #[test]
    fn utf16_ranges_count_surrogate_pairs() {
        let line = "a😀b";
        let rule = compile("b", false).expect("模式");
        let ranges = rule.find(line, 4).expect("匹配");
        assert_eq!(ranges, vec![5..6]);
        assert_eq!(byte_range_to_utf16(line, ranges[0].clone()), 3..4);
    }
}
