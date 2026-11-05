//! 高亮规则：模型、编译缓存与后台扫描线程。
//!
//! 规则匹配不进渲染线程：编译和逐行匹配都在一个专用线程里做。请求信箱只留最新的一个
//! （新请求顶掉还没开始的旧请求，最后一屏一定会被扫到），结果带着它扫的是哪一屏
//! （[`ScanKey`]），只有和最近一次请求完全对上才显示。某条规则匹配超预算时
//! 只暂停它当前修订的匹配，其他规则和终端继续。

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use zssh_regex::{CompiledRule, DEFAULT_MATCH_LIMIT, Engine, MatchError, compile};
use zssh_terminal::RowSnapshot;

use crate::i18n::{Msg, t};

/// 本文件（高亮规则）自己的界面文案。
///
/// 只放应用自己的固定文案；规则的正则 / 关键词是用户数据，颜色值是数字，都不进这张表。
#[derive(Clone, Copy, Debug)]
enum HighlightMsg {
    ColorRed,
    ColorOrange,
    ColorYellow,
    ColorGreen,
    ColorTeal,
    ColorBlue,
    ColorPurple,
    /// 不在预设里的颜色（旧规则、同步来的）。
    Custom,
    /// 匹配超预算暂停的提示：`规则 {label} 匹配超出预算，已暂停到下次修改`。
    SuspendedPrefix,
    SuspendedSuffix,
}

impl Msg for HighlightMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::ColorRed => "红",
            Self::ColorOrange => "橙",
            Self::ColorYellow => "黄",
            Self::ColorGreen => "绿",
            Self::ColorTeal => "青",
            Self::ColorBlue => "蓝",
            Self::ColorPurple => "紫",
            Self::Custom => "自定义",
            Self::SuspendedPrefix => "规则 ",
            Self::SuspendedSuffix => " 匹配超出预算，已暂停到下次修改",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::ColorRed => "Red",
            Self::ColorOrange => "Orange",
            Self::ColorYellow => "Yellow",
            Self::ColorGreen => "Green",
            Self::ColorTeal => "Teal",
            Self::ColorBlue => "Blue",
            Self::ColorPurple => "Purple",
            Self::Custom => "Custom",
            Self::SuspendedPrefix => "Rule ",
            Self::SuspendedSuffix => " exceeded its match budget and is paused until the next edit",
        }
    }
}

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

/// 预设颜色的名字（存的是键，显示时按当前语言取）。
///
/// 单独成类型是为了让 `HIGHLIGHT_COLORS` 仍能直接放进 `Choice::new`（实现 `Into<String>`），
/// 设置页规则编辑器那边不用改。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightColor {
    Red,
    Orange,
    Yellow,
    Green,
    Teal,
    Blue,
    Purple,
}

impl HighlightColor {
    fn msg(self) -> HighlightMsg {
        match self {
            Self::Red => HighlightMsg::ColorRed,
            Self::Orange => HighlightMsg::ColorOrange,
            Self::Yellow => HighlightMsg::ColorYellow,
            Self::Green => HighlightMsg::ColorGreen,
            Self::Teal => HighlightMsg::ColorTeal,
            Self::Blue => HighlightMsg::ColorBlue,
            Self::Purple => HighlightMsg::ColorPurple,
        }
    }

    /// 这个颜色在界面上的名字，跟随界面语言。
    pub fn name(self) -> &'static str {
        t(self.msg())
    }
}

impl From<HighlightColor> for String {
    fn from(color: HighlightColor) -> Self {
        color.name().to_owned()
    }
}

/// 设置页规则编辑器里的颜色（同 0.2.14 的 `HIGHLIGHT_COLORS`）。
pub const HIGHLIGHT_COLORS: [(u32, HighlightColor); 7] = [
    (0xef4444, HighlightColor::Red),
    (0xf59e0b, HighlightColor::Orange),
    (0xeab308, HighlightColor::Yellow),
    (0x22c55e, HighlightColor::Green),
    (0x06b6d4, HighlightColor::Teal),
    (0x3b82f6, HighlightColor::Blue),
    (0xa855f7, HighlightColor::Purple),
];

/// 颜色的名字；不在预设里（旧规则、同步来的）显示「自定义」。
pub fn color_label(color: u32) -> &'static str {
    HIGHLIGHT_COLORS
        .iter()
        .find(|(value, _)| *value == color)
        .map_or(t(HighlightMsg::Custom), |(_, name)| name.name())
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
    /// 规则集修订号。
    pub revision: u64,
    /// 这份结果扫的是哪一屏（由后台线程按请求填上）。
    pub key: ScanKey,
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
                            "{}{}{}",
                            t(HighlightMsg::SuspendedPrefix),
                            rule.label(),
                            t(HighlightMsg::SuspendedSuffix)
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

/// 一次扫描看的是"哪个窗格的哪一屏、用哪一版规则"。结果必须和请求完全对上才能显示：
/// 切标签、切分屏、重连、改规则、改大小、内容变了，任何一项不同都说明这份结果已经不是眼前这一屏的。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanKey {
    /// 窗格编号（全进程唯一，重连换窗格时沿用）。
    pub pane: u64,
    /// 连接代次（全进程唯一，重连会变）。
    pub generation: u64,
    /// 屏幕内容修订号（`Snapshot::revision`）。
    pub content: u64,
    /// 规则集修订号。
    pub rules: u64,
    /// 视口：行数、列数、滚回历史的行数。
    pub rows: usize,
    pub cols: usize,
    pub offset: usize,
}

impl ScanKey {
    /// 除了内容修订号以外都一样：同一个窗格、同一次连接、同一版规则、同一个视口。
    /// 输出持续滚动时，上一屏的结果在下一屏扫描出来之前仍可画（和以前一样慢一拍），
    /// 但换了窗格、规则、视口就不能再画。
    pub fn same_view(&self, other: &Self) -> bool {
        let strip = |key: &Self| Self { content: 0, ..*key };
        strip(self) == strip(other)
    }
}

/// 交给后台线程的一次扫描。
pub struct Request {
    pub key: ScanKey,
    pub rules: Vec<HighlightRule>,
    pub lines: Vec<LineText>,
}

/// 后台线程用的扫描器；测试里换成可控的慢扫描器。
pub trait Scanner: Send + 'static {
    fn scan(&mut self, request: &Request) -> ScanOutcome;
}

/// 真正的扫描器：规则集不变就复用编译结果。
struct RuleScanner {
    cache: Option<(u64, CompiledSet)>,
    suspended: HashSet<u64>,
}

impl Scanner for RuleScanner {
    fn scan(&mut self, request: &Request) -> ScanOutcome {
        let revision = request.key.rules;
        if self.cache.as_ref().map(|(cached, _)| *cached) != Some(revision) {
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
            self.cache = Some((revision, compiled));
            self.suspended.clear();
        }
        let compiled = self
            .cache
            .as_ref()
            .map(|(_, compiled)| compiled.as_slice())
            .unwrap_or_default();
        scan(revision, compiled, &request.lines, &mut self.suspended)
    }
}

/// 请求信箱：只保留最新的一个待处理请求。后台还在扫旧的时候又来了新的，新的直接顶掉还没开始的旧的，
/// 绝不会因为"信箱满了"而丢掉最新的那一个。
struct Mailbox {
    state: Mutex<MailState>,
    wake: Condvar,
}

struct MailState {
    pending: Option<Request>,
    closed: bool,
}

impl Mailbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, MailState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 后台扫描线程；UI 侧只发最新请求、取结果。
pub struct EngineHandle {
    mail: Arc<Mailbox>,
    rx: Receiver<ScanOutcome>,
}

impl EngineHandle {
    pub fn start() -> Self {
        Self::start_with(RuleScanner {
            cache: None,
            suspended: HashSet::new(),
        })
    }

    pub fn start_with<S: Scanner>(mut scanner: S) -> Self {
        let mail = Arc::new(Mailbox {
            state: Mutex::new(MailState {
                pending: None,
                closed: false,
            }),
            wake: Condvar::new(),
        });
        let (results, rx) = channel::<ScanOutcome>();
        let worker_mail = Arc::clone(&mail);
        let _ = thread::Builder::new()
            .name("zssh-highlight".to_owned())
            .spawn(move || {
                loop {
                    let request = {
                        let mut state = worker_mail.lock();
                        loop {
                            if let Some(request) = state.pending.take() {
                                break request;
                            }
                            if state.closed {
                                return;
                            }
                            state = worker_mail
                                .wake
                                .wait(state)
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                        }
                    };
                    let mut outcome = scanner.scan(&request);
                    outcome.key = request.key;
                    if results.send(outcome).is_err() {
                        return;
                    }
                }
            });
        Self { mail, rx }
    }

    /// 交一次扫描；新的请求顶掉还没开始的旧请求。没有交出去（后台线程已经没了）就把内容原样还回来，
    /// 调用方不能把这次请求当成已完成。
    pub fn request(
        &self,
        key: ScanKey,
        rules: &[HighlightRule],
        lines: Vec<LineText>,
    ) -> Result<(), Vec<LineText>> {
        // 工作线程持有一份信箱：只剩我们自己手里那份，说明它退出了（出错或没能启动）。
        if Arc::strong_count(&self.mail) < 2 {
            return Err(lines);
        }
        let mut state = self.mail.lock();
        if state.closed {
            return Err(lines);
        }
        state.pending = Some(Request {
            key,
            rules: rules.to_vec(),
            lines,
        });
        self.mail.wake.notify_one();
        Ok(())
    }

    /// 取已经扫完的结果（按完成顺序，可能有已经过时的）。
    fn drain(&self) -> Vec<ScanOutcome> {
        self.rx.try_iter().collect()
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.mail.lock().closed = true;
        self.mail.wake.notify_all();
    }
}

/// 高亮扫描的调度：什么时候提交、哪份结果能显示。
///
/// - 提交：同一个 `ScanKey` 不重复提交；提交没成功就不记，下一帧再试（不会把"没交出去"当成"扫过了"）。
/// - 显示：只收和**最近一次请求**完全对上的结果，被后来的请求取代了的旧结果一律丢掉。
pub struct HighlightScheduler {
    engine: EngineHandle,
    /// 后台线程没了（崩溃、没能启动）时用它重新起一个。
    spawn: Box<dyn Fn() -> EngineHandle + Send>,
    /// 最近一次成功交给后台的请求。
    requested: Option<ScanKey>,
    /// 正在显示的结果：它扫的是哪一屏，和每行的命中区间。
    shown: Option<(ScanKey, Vec<Vec<Span>>)>,
}

impl HighlightScheduler {
    pub fn new(spawn: Box<dyn Fn() -> EngineHandle + Send>) -> Self {
        Self {
            engine: spawn(),
            spawn,
            requested: None,
            shown: None,
        }
    }

    /// 要求扫描 `key` 这一屏。已经请求过同一个 `key` 就什么都不做；返回这次是否交出了新请求。
    pub fn want(
        &mut self,
        key: ScanKey,
        rules: &[HighlightRule],
        lines: impl FnOnce() -> Vec<LineText>,
    ) -> bool {
        if self.requested == Some(key) {
            return false;
        }
        let lines = match self.engine.request(key, rules, lines()) {
            Ok(()) => {
                self.requested = Some(key);
                return true;
            }
            // 后台线程没了：这次没交出去，不能记成已请求。换一个新的后台线程再交一次，
            // 还不行就留到下一帧（`requested` 没动，下一帧还会来）。
            Err(lines) => lines,
        };
        self.engine = (self.spawn)();
        if self.engine.request(key, rules, lines).is_err() {
            return false;
        }
        self.requested = Some(key);
        true
    }

    /// 收后台结果：和最近一次请求完全对上的才显示，返回它（给调用方取规则状态、提示）。
    pub fn poll(&mut self) -> Option<ScanOutcome> {
        let wanted = self.requested?;
        let outcome = self
            .engine
            .drain()
            .into_iter()
            .rev()
            .find(|outcome| outcome.key == wanted)?;
        let mut rows: Vec<Vec<Span>> = Vec::new();
        for span in &outcome.spans {
            if rows.len() <= span.row {
                rows.resize(span.row + 1, Vec::new());
            }
            rows[span.row].push(*span);
        }
        self.shown = Some((outcome.key, rows));
        Some(outcome)
    }

    /// `view` 这一屏现在该画的命中区间；正在显示的结果不属于这个视图（换了窗格、规则、视口）就是空。
    pub fn spans(&self, view: &ScanKey) -> &[Vec<Span>] {
        match &self.shown {
            Some((key, rows)) if key.same_view(view) => rows,
            _ => &[],
        }
    }

    /// 丢掉显示的结果和"已请求"的记录（高亮开关、整体失效）：之后会按需重新请求，在途的旧结果不会再被收下。
    pub fn forget(&mut self) {
        self.requested = None;
        self.shown = None;
    }

    #[cfg(test)]
    fn requested(&self) -> Option<ScanKey> {
        self.requested
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
        let engine = EngineHandle::start();
        let line = LineText {
            text: "error here".to_owned(),
            columns: (0..10).collect(),
        };
        let key = ScanKey {
            rules: 1,
            ..ScanKey::default()
        };
        assert!(engine.request(key, &default_rules(), vec![line]).is_ok());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(outcome) = engine.drain().pop() {
                assert_eq!(outcome.revision, 1);
                assert_eq!(outcome.key, key, "结果带着它扫的是哪一屏");
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

    // ---- R04：请求归属与"只留最新" ----

    use std::sync::mpsc::{Receiver as StdReceiver, Sender as StdSender};
    use std::time::{Duration, Instant};

    /// 可控的慢扫描器：每扫一屏先报告"开始扫了某某"，再等测试放行才出结果。
    /// 内容里的第一个字符就是这一屏的标识，结果里每个命中区间的 `fg` 带着它。
    struct SlowScanner {
        started: StdSender<char>,
        permits: StdReceiver<()>,
    }

    impl Scanner for SlowScanner {
        fn scan(&mut self, request: &Request) -> ScanOutcome {
            let tag = request
                .lines
                .first()
                .and_then(|line| line.text.chars().next())
                .unwrap_or('?');
            let _ = self.started.send(tag);
            // 超时只是兜底，免得测试写错时一直挂着。
            let _ = self.permits.recv_timeout(Duration::from_secs(10));
            ScanOutcome {
                spans: vec![Span {
                    row: 0,
                    start: 0,
                    end: 0,
                    fg: u32::from(tag),
                    bg: None,
                }],
                ..ScanOutcome::default()
            }
        }
    }

    struct Slow {
        scheduler: HighlightScheduler,
        started: StdReceiver<char>,
        permit: StdSender<()>,
    }

    fn slow_scheduler() -> Slow {
        let (started_tx, started) = std::sync::mpsc::channel();
        let (permit, permits) = std::sync::mpsc::channel();
        let engine = Mutex::new(Some(EngineHandle::start_with(SlowScanner {
            started: started_tx,
            permits,
        })));
        Slow {
            scheduler: HighlightScheduler::new(Box::new(move || {
                engine
                    .lock()
                    .ok()
                    .and_then(|mut slot| slot.take())
                    .unwrap_or_else(EngineHandle::start)
            })),
            started,
            permit,
        }
    }

    fn screen(tag: char) -> Vec<LineText> {
        vec![LineText {
            text: tag.to_string(),
            columns: vec![0],
        }]
    }

    /// 同一个窗格、同一版规则，不同内容修订号。
    fn key(pane: u64, content: u64, rules: u64) -> ScanKey {
        ScanKey {
            pane,
            generation: 1,
            content,
            rules,
            rows: 24,
            cols: 80,
            offset: 0,
        }
    }

    /// 轮询到调度器收下一份结果（或超时失败）。
    fn poll_until_applied(scheduler: &mut HighlightScheduler) -> ScanOutcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(outcome) = scheduler.poll() {
                return outcome;
            }
            assert!(Instant::now() < deadline, "等不到匹配最近一次请求的结果");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn shown_tags(scheduler: &HighlightScheduler, view: &ScanKey) -> Vec<char> {
        scheduler
            .spans(view)
            .iter()
            .flatten()
            .filter_map(|span| char::from_u32(span.fg))
            .collect()
    }

    /// 慢扫描器依次收到 A、B、C，期间 A 还没扫完：B 被 C 顶掉从来没扫，A 的迟到结果不能显示，
    /// 最终静止在 C 时只显示 C。这同时是"队列满了、输出随后停止，最后一屏仍必须被扫到"的场景：
    /// 以前信箱只有一格，C 提交失败却被记成已请求，之后同一个 key 不再提交，C 永远没人扫。
    #[test]
    fn slow_scans_of_a_b_c_end_up_showing_only_c_and_the_last_frame_is_always_scanned() {
        let Slow {
            mut scheduler,
            started,
            permit,
        } = slow_scheduler();
        let (a, b, c) = (key(1, 1, 1), key(1, 2, 1), key(1, 3, 1));

        assert!(scheduler.want(a, &default_rules(), || screen('A')));
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok('A'));
        // A 还在扫的时候，B、C 先后到来；输出到这里就停了，之后再没有新请求。
        assert!(scheduler.want(b, &default_rules(), || screen('B')));
        assert!(scheduler.want(c, &default_rules(), || screen('C')));
        assert!(
            !scheduler.want(c, &default_rules(), || screen('C')),
            "同一屏不重复提交"
        );
        assert!(scheduler.poll().is_none());
        assert!(shown_tags(&scheduler, &c).is_empty());

        // A 扫完：它的结果已经被后来的请求取代，不能显示。后台接着开始扫 C（B 被顶掉，从没开始）；
        // C 还没出结果的这段时间里，A 的迟到结果既不能被收下，也不能被画出来。
        permit.send(()).unwrap();
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok('C'));
        std::thread::sleep(Duration::from_millis(100));
        assert!(scheduler.poll().is_none(), "A 的迟到结果被收下了");
        assert!(shown_tags(&scheduler, &c).is_empty());
        assert!(shown_tags(&scheduler, &a).is_empty());

        permit.send(()).unwrap();
        let applied = poll_until_applied(&mut scheduler);
        assert_eq!(applied.key, c);
        assert_eq!(shown_tags(&scheduler, &c), vec!['C']);
        assert!(started.try_recv().is_err(), "B 不该被扫");
    }

    /// 切标签 / 切分屏 / 改规则：每一种变化都让之前的请求作废，最后只显示最后一个视图的结果，
    /// 之前的视图拿不到任何命中区间。
    #[test]
    fn switching_pane_or_rules_never_shows_the_previous_views_result() {
        let Slow {
            mut scheduler,
            started,
            permit,
        } = slow_scheduler();
        let on_pane_1 = key(1, 5, 1);
        let on_pane_2 = key(2, 5, 1);
        let after_rule_edit = key(2, 5, 2);

        assert!(scheduler.want(on_pane_1, &default_rules(), || screen('A')));
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok('A'));
        assert!(scheduler.want(on_pane_2, &default_rules(), || screen('B')));
        assert!(scheduler.want(after_rule_edit, &default_rules(), || screen('C')));
        permit.send(()).unwrap();
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok('C'));
        std::thread::sleep(Duration::from_millis(100));
        assert!(scheduler.poll().is_none(), "旧视图的迟到结果被收下了");
        assert!(shown_tags(&scheduler, &on_pane_1).is_empty());
        permit.send(()).unwrap();

        let applied = poll_until_applied(&mut scheduler);
        assert_eq!(applied.key, after_rule_edit);
        assert_eq!(shown_tags(&scheduler, &after_rule_edit), vec!['C']);
        // 旧的视图（另一个窗格、旧规则）一条都不画。
        assert!(shown_tags(&scheduler, &on_pane_1).is_empty());
        assert!(shown_tags(&scheduler, &on_pane_2).is_empty());
    }

    /// 迟到的结果与眼前这一屏对不上（例如切走又切回来之前扫的）：整份丢掉，而不是装进去。
    #[test]
    fn a_late_result_for_a_forgotten_request_is_dropped() {
        let Slow {
            mut scheduler,
            started,
            permit,
        } = slow_scheduler();
        let a = key(1, 1, 1);
        assert!(scheduler.want(a, &default_rules(), || screen('A')));
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok('A'));
        // 高亮被关掉又打开（整体失效）：在途的 A 不再是"最近一次请求"。
        scheduler.forget();
        permit.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(scheduler.poll().is_none());
        assert!(shown_tags(&scheduler, &a).is_empty());
        // 重新请求同一屏：这一次能收下。
        assert!(scheduler.want(a, &default_rules(), || screen('A')));
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok('A'));
        permit.send(()).unwrap();
        assert_eq!(poll_until_applied(&mut scheduler).key, a);
        assert_eq!(shown_tags(&scheduler, &a), vec!['A']);
    }

    /// 输出持续滚动时，上一屏的结果在新一屏扫出来之前仍可画（只差内容修订号），但换视口就不能画。
    #[test]
    fn the_previous_screens_spans_stay_until_the_viewport_or_pane_changes() {
        let Slow {
            mut scheduler,
            started,
            permit,
        } = slow_scheduler();
        let first = key(1, 1, 1);
        assert!(scheduler.want(first, &default_rules(), || screen('A')));
        assert_eq!(started.recv_timeout(Duration::from_secs(5)), Ok('A'));
        permit.send(()).unwrap();
        poll_until_applied(&mut scheduler);

        let next_frame = key(1, 2, 1);
        assert_eq!(shown_tags(&scheduler, &next_frame), vec!['A']);
        let resized = ScanKey {
            cols: 100,
            ..next_frame
        };
        let scrolled = ScanKey {
            offset: 3,
            ..next_frame
        };
        let reconnected = ScanKey {
            generation: 2,
            ..next_frame
        };
        for view in [resized, scrolled, reconnected] {
            assert!(shown_tags(&scheduler, &view).is_empty(), "{view:?}");
        }
    }

    struct ExplodingScanner;

    impl Scanner for ExplodingScanner {
        fn scan(&mut self, _request: &Request) -> ScanOutcome {
            panic!("测试：扫描线程故意崩溃");
        }
    }

    /// 提交没成功（后台线程已经没了）不能记成"已请求"；重启后台之后同一屏会被重新提交并最终显示。
    #[test]
    fn a_failed_submit_is_not_recorded_as_done_and_is_retried_on_a_fresh_worker() {
        let (started_tx, _started) = std::sync::mpsc::channel();
        let (permit, permits) = std::sync::mpsc::channel();
        permit.send(()).unwrap();
        // 第一个后台线程一扫就崩，第二个是正常的。
        let spares = Mutex::new(std::collections::VecDeque::from([
            EngineHandle::start_with(ExplodingScanner),
            EngineHandle::start_with(SlowScanner {
                started: started_tx,
                permits,
            }),
        ]));
        let mut scheduler = HighlightScheduler::new(Box::new(move || {
            spares
                .lock()
                .ok()
                .and_then(|mut queue| queue.pop_front())
                .unwrap_or_else(EngineHandle::start)
        }));

        let first = key(1, 1, 1);
        assert!(scheduler.want(first, &default_rules(), || screen('A')));
        // 等它真的退出：之后直接交给它的请求都会被拒绝。
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler
            .engine
            .request(first, &default_rules(), screen('A'))
            .is_ok()
        {
            assert!(Instant::now() < deadline, "后台线程没有退出");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(scheduler.requested(), Some(first));

        // 下一屏：发现提交失败（没记账），换新的后台线程重新提交，结果最终显示。
        let second = key(1, 2, 1);
        assert!(scheduler.want(second, &default_rules(), || screen('B')));
        assert_eq!(scheduler.requested(), Some(second));
        assert_eq!(poll_until_applied(&mut scheduler).key, second);
        assert_eq!(shown_tags(&scheduler, &second), vec!['B']);
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
