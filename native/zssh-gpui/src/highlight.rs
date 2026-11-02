//! 高亮规则：模型、编译缓存与后台扫描线程。
//!
//! 规则匹配不进渲染线程：编译和逐行匹配都在一个专用线程里做，请求队列只有
//! 一个槽位（最新的请求覆盖不了就丢弃，下一帧再发）。某条规则匹配超预算时
//! 只暂停它当前修订的匹配，其他规则和终端继续。

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, SyncSender, channel, sync_channel};
use std::thread;

use zssh_regex::{CompiledRule, DEFAULT_MATCH_LIMIT, Engine, MatchError, compile};
use zssh_terminal::RowSnapshot;

/// 规则最多这么多条（和配置同步的校验一致，对方的包超过就不收）。
pub const MAX_RULES: usize = zssh_core::sync::settings::HIGHLIGHT_RULES_MAX;

/// 新规则默认的颜色轮换。
pub const PALETTE: [u32; 6] = [0xef4444, 0xf59e0b, 0x22c55e, 0x3b82f6, 0xa855f7, 0x06b6d4];

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HighlightRule {
    pub id: u64,
    pub pattern: String,
    /// 把规则当正则；关掉就是按字面匹配的关键词（0.2.14 规则里的 `regex`）。
    pub regex: bool,
    pub case_sensitive: bool,
    pub foreground: u32,
    pub background: Option<u32>,
    pub enabled: bool,
}

impl Default for HighlightRule {
    fn default() -> Self {
        Self {
            id: 0,
            pattern: String::new(),
            regex: true,
            case_sensitive: false,
            foreground: PALETTE[0],
            background: None,
            enabled: true,
        }
    }
}

impl HighlightRule {
    pub fn new(id: u64, pattern: impl Into<String>, case_sensitive: bool) -> Self {
        let foreground = PALETTE[(id as usize) % PALETTE.len()];
        Self {
            id,
            pattern: pattern.into(),
            regex: true,
            case_sensitive,
            foreground,
            background: None,
            enabled: true,
        }
    }

    /// 交给编译器的正则：关键词规则把特殊字符转义成字面。
    pub fn effective_pattern(&self) -> std::borrow::Cow<'_, str> {
        if self.regex {
            return std::borrow::Cow::Borrowed(&self.pattern);
        }
        let mut out = String::with_capacity(self.pattern.len() + 4);
        for ch in self.pattern.chars() {
            if r"\.+*?()|[]{}^$".contains(ch) {
                out.push('\\');
            }
            out.push(ch);
        }
        std::borrow::Cow::Owned(out)
    }

    /// 界面上这条规则的颜色：涂背景的规则看背景色，否则看文字色。
    pub fn color(&self) -> u32 {
        self.background.unwrap_or(self.foreground)
    }

    /// 换颜色。涂背景的规则换背景色，文字改成和它反差大的黑 / 白，保证读得清。
    pub fn set_color(&mut self, color: u32) {
        if self.background.is_some() {
            self.background = Some(color);
            self.foreground = contrast_text(color);
        } else {
            self.foreground = color;
        }
    }

    /// 打开 / 关闭「涂背景」：开 = 当前颜色改涂背景、文字换成反差色；关 = 回到只改文字颜色。
    pub fn set_background(&mut self, on: bool) {
        let color = self.color();
        if on {
            self.background = Some(color);
            self.foreground = contrast_text(color);
        } else {
            self.background = None;
            self.foreground = color;
        }
    }

    pub fn label(&self) -> String {
        let mut label: String = self.pattern.chars().take(18).collect();
        if self.pattern.chars().count() > 18 {
            label.push('…');
        }
        label
    }
}

/// 设置页规则编辑器里的颜色（同 0.2.14 的 `HIGHLIGHT_COLORS`）。
pub const HIGHLIGHT_COLORS: [(u32, &str); 7] = [
    (0xef4444, "红"),
    (0xf59e0b, "橙"),
    (0xeab308, "黄"),
    (0x22c55e, "绿"),
    (0x06b6d4, "青"),
    (0x3b82f6, "蓝"),
    (0xa855f7, "紫"),
];

/// 颜色的名字；不在预设里（旧规则、同步来的）显示「自定义」。
pub fn color_label(color: u32) -> &'static str {
    HIGHLIGHT_COLORS
        .iter()
        .find(|(value, _)| *value == color)
        .map_or("自定义", |(_, label)| label)
}

/// 涂背景时文字用黑还是白：按背景亮度挑反差大的。
pub fn contrast_text(background: u32) -> u32 {
    let (r, g, b) = (
        (background >> 16) & 0xff,
        (background >> 8) & 0xff,
        background & 0xff,
    );
    // 感知亮度（Rec. 601），0～255。
    let luma = (299 * r + 587 * g + 114 * b) / 1000;
    if luma > 150 { 0x111111 } else { 0xffffff }
}

/// 和旧前端 `ui/src/terminal/highlightSamples.ts` 的默认规则保持一致。
pub fn default_rules() -> Vec<HighlightRule> {
    [
        (
            r"\b(errors?|fail(ed|ure|s)?|fatal|denied|refused|panic|critical)\b",
            0xef4444,
        ),
        (r"\b(warn(ings?)?|deprecated|timeout|timed out)\b", 0xf59e0b),
        (
            r"\b(ok|success(ful|fully)?|succeeded|passed|active|running)\b",
            0x22c55e,
        ),
        (r"\b(?:\d{1,3}\.){3}\d{1,3}(?:/\d{1,2})?\b", 0xa855f7),
        (r#"https?://[^\s'"<>]+"#, 0x3b82f6),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (pattern, color))| {
        let mut rule = HighlightRule::new(index as u64, pattern, false);
        rule.foreground = color;
        rule
    })
    .collect()
}

/// 默认规则里 `rules` 还没有的那些（按模式对比：用户改过颜色、开关的不算缺）。
/// 给「添加常用规则」用；返回的规则编号是占位，由调用方重新分配。
pub fn missing_samples(rules: &[HighlightRule]) -> Vec<HighlightRule> {
    default_rules()
        .into_iter()
        .filter(|sample| !rules.iter().any(|rule| rule.pattern == sample.pattern))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleStatus {
    pub id: u64,
    pub label: String,
    pub enabled: bool,
    pub engine: Option<Engine>,
    pub error: Option<String>,
    pub suspended: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub row: usize,
    pub start: usize,
    pub end: usize,
    pub fg: u32,
    pub bg: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanOutcome {
    pub revision: u64,
    pub spans: Vec<Span>,
    pub statuses: Vec<RuleStatus>,
    pub notices: Vec<String>,
}

/// 一行文本和每个字符对应的列号；宽字符占位格不产生字符。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LineText {
    pub text: String,
    pub columns: Vec<usize>,
}

pub fn line_text(row: &RowSnapshot) -> LineText {
    let mut out = LineText::default();
    for (col, cell) in row.cells.iter().enumerate() {
        if cell.spacer || cell.ch == '\0' {
            continue;
        }
        out.text.push(cell.ch);
        out.columns.push(col);
        if let Some(zerowidth) = &cell.zerowidth {
            for ch in zerowidth {
                out.text.push(*ch);
                out.columns.push(col);
            }
        }
    }
    out
}

/// 已编译规则集合：规则本身加编译结果。
type CompiledSet = Vec<(HighlightRule, Result<CompiledRule, String>)>;

/// 纯扫描：给定已编译规则和可见行，返回要叠加的颜色。
pub fn scan(
    revision: u64,
    rules: &[(HighlightRule, Result<CompiledRule, String>)],
    lines: &[LineText],
    suspended: &mut HashSet<u64>,
) -> ScanOutcome {
    let mut outcome = ScanOutcome {
        revision,
        ..Default::default()
    };

    for (rule, compiled) in rules {
        outcome.statuses.push(RuleStatus {
            id: rule.id,
            label: rule.label(),
            enabled: rule.enabled,
            engine: compiled.as_ref().ok().map(CompiledRule::engine),
            error: compiled.as_ref().err().cloned(),
            suspended: suspended.contains(&rule.id),
        });
    }

    for (rule, compiled) in rules {
        let Ok(compiled) = compiled else { continue };
        if !rule.enabled || suspended.contains(&rule.id) {
            continue;
        }
        for (row, line) in lines.iter().enumerate() {
            match compiled.find(&line.text, DEFAULT_MATCH_LIMIT) {
                Ok(ranges) => {
                    for range in ranges {
                        let start_char = line.text[..range.start].chars().count();
                        let end_char = line.text[..range.end].chars().count().saturating_sub(1);
                        let (Some(&start), Some(&end)) =
                            (line.columns.get(start_char), line.columns.get(end_char))
                        else {
                            continue;
                        };
                        outcome.spans.push(Span {
                            row,
                            start,
                            end,
                            fg: rule.foreground,
                            bg: rule.background,
                        });
                    }
                }
                Err(MatchError::BudgetExceeded) => {
                    if suspended.insert(rule.id) {
                        outcome.notices.push(format!(
                            "规则 {} 匹配超出预算，已暂停到下次修改",
                            rule.label()
                        ));
                        if let Some(status) = outcome.statuses.iter_mut().find(|s| s.id == rule.id)
                        {
                            status.suspended = true;
                        }
                    }
                    break;
                }
                Err(MatchError::LineTooLong) => continue,
            }
        }
    }

    outcome
}

struct Request {
    revision: u64,
    rules: Vec<HighlightRule>,
    lines: Vec<LineText>,
}

/// 后台扫描线程；UI 侧只发最新请求、取最新结果。
pub struct EngineHandle {
    tx: SyncSender<Request>,
    rx: Receiver<ScanOutcome>,
}

impl EngineHandle {
    pub fn start() -> Self {
        let (tx, requests) = sync_channel::<Request>(1);
        let (results, rx) = channel::<ScanOutcome>();
        let _ = thread::Builder::new()
            .name("zssh-highlight".to_owned())
            .spawn(move || {
                let mut cache: Option<(u64, CompiledSet)> = None;
                let mut suspended = HashSet::new();
                while let Ok(request) = requests.recv() {
                    if cache.as_ref().map(|(revision, _)| *revision) != Some(request.revision) {
                        let compiled = request
                            .rules
                            .iter()
                            .map(|rule| {
                                (
                                    rule.clone(),
                                    compile(&rule.effective_pattern(), rule.case_sensitive)
                                        .map_err(|error| error.to_string()),
                                )
                            })
                            .collect::<Vec<_>>();
                        cache = Some((request.revision, compiled));
                        suspended.clear();
                    }
                    let Some((revision, compiled)) = cache.as_ref() else {
                        continue;
                    };
                    let outcome = scan(*revision, compiled, &request.lines, &mut suspended);
                    if results.send(outcome).is_err() {
                        return;
                    }
                }
            });
        Self { tx, rx }
    }

    /// 发一次扫描；队列满时丢弃，调用方下一帧再发。
    pub fn request(&mut self, revision: u64, rules: &[HighlightRule], lines: Vec<LineText>) {
        let request = Request {
            revision,
            rules: rules.to_vec(),
            lines,
        };
        let _ = self.tx.try_send(request);
    }

    /// 取最新结果；返回是否有新结果。
    pub fn poll(&mut self) -> Option<ScanOutcome> {
        let mut latest = None;
        while let Ok(outcome) = self.rx.try_recv() {
            latest = Some(outcome);
        }
        latest
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zssh_terminal::{Size, TerminalModel};

    fn lines(texts: &[&str]) -> Vec<LineText> {
        texts
            .iter()
            .map(|text| LineText {
                text: (*text).to_owned(),
                columns: (0..text.chars().count()).collect(),
            })
            .collect()
    }

    fn compile_all(rules: &[HighlightRule]) -> Vec<(HighlightRule, Result<CompiledRule, String>)> {
        rules
            .iter()
            .map(|rule| {
                (
                    rule.clone(),
                    compile(&rule.effective_pattern(), rule.case_sensitive)
                        .map_err(|e| e.to_string()),
                )
            })
            .collect()
    }

    #[test]
    fn default_rules_color_the_usual_lines() {
        let rules = default_rules();
        let compiled = compile_all(&rules);
        let mut suspended = HashSet::new();
        let outcome = scan(
            1,
            &compiled,
            &lines(&[
                "ok  line 0001",
                "error: connection refused",
                "warn: deprecated",
                "see https://example.com/x now",
            ]),
            &mut suspended,
        );
        assert!(outcome.statuses.iter().all(|status| status.error.is_none()));
        // error 行命中 error/refused，warn 行命中 warn/deprecated，ok 与 URL 各一次。
        assert_eq!(outcome.spans.len(), 6);
        let rows: HashSet<usize> = outcome.spans.iter().map(|span| span.row).collect();
        assert_eq!(rows, HashSet::from([0, 1, 2, 3]));
        assert!(outcome.notices.is_empty());
    }

    #[test]
    fn keyword_rules_match_literally_and_regex_rules_do_not() {
        let mut keyword = HighlightRule::new(1, "a.c(", false);
        keyword.regex = false;
        let regex = HighlightRule::new(2, "a.c", false);
        let compiled = compile_all(&[keyword, regex]);
        let mut suspended = HashSet::new();
        let outcome = scan(1, &compiled, &lines(&["abc a.c("]), &mut suspended);
        // 关键词 `a.c(` 只命中字面的那一处；正则 `a.c` 命中 abc 和 a.c 两处。
        assert!(outcome.statuses.iter().all(|status| status.error.is_none()));
        assert_eq!(outcome.spans.len(), 3);
    }

    #[test]
    fn old_saved_rules_without_the_regex_field_stay_regex_rules() {
        let rule: HighlightRule =
            serde_json::from_str(r#"{"id":3,"pattern":"err(or)?"}"#).expect("旧规则照常读");
        assert!(rule.regex, "旧规则一直是正则");
        assert_eq!(rule.effective_pattern(), "err(or)?");
    }

    #[test]
    fn background_rules_swap_to_a_readable_text_color_and_back() {
        let mut rule = HighlightRule::new(1, "x", false);
        rule.set_color(0xef4444);
        assert_eq!((rule.foreground, rule.background), (0xef4444, None));
        rule.set_background(true);
        assert_eq!(rule.background, Some(0xef4444));
        assert_ne!(rule.foreground, 0xef4444, "文字不能和背景同色");
        // 涂背景时换颜色换的是背景。
        rule.set_color(0xeab308);
        assert_eq!(rule.background, Some(0xeab308));
        assert_eq!(rule.foreground, 0x111111, "亮色背景配深色字");
        assert_eq!(rule.color(), 0xeab308);
        rule.set_background(false);
        assert_eq!((rule.foreground, rule.background), (0xeab308, None));
        assert_eq!(contrast_text(0x1e293b), 0xffffff);
    }

    #[test]
    fn samples_are_only_offered_when_missing() {
        assert!(missing_samples(&default_rules()).is_empty());
        assert_eq!(missing_samples(&[]).len(), default_rules().len());
        let mut some = default_rules();
        some.remove(0);
        assert_eq!(missing_samples(&some).len(), 1);
        assert_eq!(color_label(0xef4444), "红");
        assert_eq!(color_label(0x123456), "自定义");
    }

    #[test]
    fn disabled_rules_do_not_match() {
        let mut rules = default_rules();
        rules[1].enabled = false;
        let compiled = compile_all(&rules);
        let mut suspended = HashSet::new();
        let outcome = scan(1, &compiled, &lines(&["warn: deprecated"]), &mut suspended);
        assert!(outcome.spans.is_empty());
        assert!(!outcome.statuses[1].enabled);
    }

    #[test]
    fn bad_pattern_only_reports_itself() {
        let mut rules = default_rules();
        rules.push(HighlightRule::new(99, "(", false));
        let compiled = compile_all(&rules);
        let mut suspended = HashSet::new();
        let outcome = scan(1, &compiled, &lines(&["error: x"]), &mut suspended);
        assert!(outcome.statuses[5].error.is_some());
        assert_eq!(outcome.spans.len(), 1);
    }

    #[test]
    fn budget_exceeded_suspends_once_and_leaves_other_rules() {
        let mut rules = default_rules();
        rules.push(HighlightRule::new(42, r"^(?:(?=a)a+)+$", false));
        let compiled = compile_all(&rules);
        let mut suspended = HashSet::new();
        let line = format!("{}b", "a".repeat(40));
        let outcome = scan(1, &compiled, &lines(&[&line, "ok"]), &mut suspended);
        assert!(suspended.contains(&42));
        assert!(outcome.notices.iter().any(|notice| notice.contains("暂停")));
        // 其他规则仍然工作，且被暂停的规则不再报告。
        assert_eq!(outcome.spans.len(), 1);
        let again = scan(1, &compiled, &lines(&[&line, "ok"]), &mut suspended);
        assert!(again.notices.is_empty());
    }

    #[test]
    fn editing_a_rule_retries_after_suspension() {
        let mut rules = default_rules();
        rules.push(HighlightRule::new(42, r"^(?:(?=a)a+)+$", false));
        let compiled = compile_all(&rules);
        let mut suspended = HashSet::new();
        let line = format!("{}b", "a".repeat(40));
        scan(1, &compiled, &lines(&[&line]), &mut suspended);
        assert!(suspended.contains(&42));
        // 新修订：清空暂停，改好的规则重新参与。
        suspended.clear();
        let mut fixed = compile_all(&rules);
        fixed[5].0.pattern = "a+b".to_owned();
        fixed[5].1 = compile("a+b", false).map_err(|e| e.to_string());
        let outcome = scan(2, &fixed, &lines(&[&line]), &mut suspended);
        assert!(!suspended.contains(&42));
        assert!(outcome.spans.iter().any(|span| span.row == 0));
    }

    #[test]
    fn engine_delivers_spans_from_the_worker() {
        let mut engine = EngineHandle::start();
        let line = LineText {
            text: "error here".to_owned(),
            columns: (0..10).collect(),
        };
        engine.request(1, &default_rules(), vec![line]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(outcome) = engine.poll() {
                assert_eq!(outcome.revision, 1);
                assert!(
                    outcome
                        .spans
                        .iter()
                        .any(|span| span.start == 0 && span.end == 4)
                );
                break;
            }
            assert!(std::time::Instant::now() < deadline, "后台线程没有返回结果");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn line_text_maps_wide_characters_to_columns() {
        let mut term = TerminalModel::new(Size { rows: 2, cols: 20 }, 4);
        term.feed("中文 error".as_bytes());
        let snapshot = term.snapshot();
        let line = line_text(&snapshot.rows[0]);
        assert!(line.text.starts_with("中文 error"));
        assert_eq!(line.columns[0], 0);
        assert_eq!(line.columns[2], 4);
        assert_eq!(line.columns[3], 5);
    }
}
