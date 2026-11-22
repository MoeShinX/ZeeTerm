// Adapted from Netcatty electron/preload.cjs (MCP marker line filtering and the
// delayed flush fallback, filterMcpChunk / flushMcpBufferedOutput) and
// electron/bridges/ai/shellUtils.cjs (formatSyntheticEcho), rewritten in Rust
// for Z-SSH.
//! 终端显示过滤（AI-2d 第 2 节，AI-2e 第 1 节修了"不退出"）：AI 命令的包装脚本
//! 不进用户的眼睛。
//!
//! 用户要看到的是"AI 直接在我的终端里操作"：提示符 + 命令原文（合成回显）、命令
//! 输出、新提示符。包装脚本的回显、开始标记行、结束标记行都不显示。
//!
//! 过滤器**只作用在发给界面的显示数据**上；环形缓冲（`read_terminal`、结束标记
//! 解析）和 [`TerminalState`](crate::ai::pty::TerminalState) 拿的都是原始数据。
//!
//! 生命周期（AI-2e 第 1 节：用完必须退出，不能拖慢用户打字）：
//!
//! - **当前命令的结束标记行**（`<marker>_E:`）处理完，过滤器立刻退出，回到直通
//!   （`push` 原样返回、不压任何东西）；
//! - **超时返回的命令**（命令还在终端里跑）：调用方 [`DisplayFilter::release`]，
//!   标记转成"待处理"，之后只做**轻量过滤**——只丢包含待处理标记的行，压尾巴按
//!   "末尾是某个待处理标记完整写法 `<标记>_E:<数字>` 的前缀"判断（见
//!   [`tail_may_become_marker_line`]；单个 `_`、`__`、`__Z` 不压）。
//!   没有待处理标记时完全直通。待处理标记在它出现、会话断开或
//!   [`PENDING_MAX_AGE`]（和 `watch_late_finish` 一致）之后清掉；
//! - "没有可见字符就压住"（丢空行用）只在吃完回显、到结束标记之间生效。
//!
//! 兜底（宁可多显示，绝不吞用户的输出）：
//! - 回显被关掉 / shell 行为和预期不同（包装脚本自己的输出先冒出来了）：退化成
//!   按行过滤（丢标记行），其余原样放行；
//! - 可疑的尾巴（可能是标记的一部分）最多压 [`HOLD_MAX_AGE`] / [`HOLD_MAX_BYTES`]，
//!   超了原样放行；
//! - 用户输出里出现**别的**标记样文字不动它（只丢我们自己用过的标记）。
//!
//! 标出 AI 命令（V2-U50，设置里可关）：给过滤器配上 [`AiMarkLabels`] 之后，每条 AI 命令在终端里多出两行
//! 只在本地显示的标记——合成回显之前一行开头（`▍AI（ZeePilot）`），命令输出之后、结束标记行原来的位置一行
//! 结尾（退出码 + 耗时）；命令超时则在超时那一刻补一行「超时」。标记是用 ANSI 调色板色（青 / 绿 / 红 / 黄）写的，
//! 颜色跟着终端配色走，核心库不认识界面主题；每一行结尾都复位样式，不会染到后面的输出。没配 [`AiMarkLabels`]
//! 时过滤器的输出和以前逐字节相同。标记只进送去显示的数据，AI 读的原始缓冲和结束标记解析不经过这里。

use std::sync::Arc;
use std::time::{Duration, Instant};

/// 可疑尾巴最多压多久（到点放行）。
pub const HOLD_MAX_AGE: Duration = Duration::from_secs(2);
/// 可疑尾巴最多压多少字节（超了放行）。
pub const HOLD_MAX_BYTES: usize = 4096;
/// 待处理标记最多留多久：和 `src-tauri` 里 `watch_late_finish` 的观察上限一致。
pub const PENDING_MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// 等包装脚本回显的字节预算：超了就当"回显被关掉"，退化成按行过滤。
const ECHO_BUDGET_BYTES: usize = 8 * 1024;
/// 合成回显之后最多丢掉几个空行（shell 执行命令时留下的换行）。
const BLANK_LINES_AFTER_ECHO: usize = 2;
/// 标记的前缀（`__ZEETERM_`）：轻量过滤判断"尾巴像不像标记行"用。
const MARKER_PREFIX: &str = "__ZEETERM_";
/// 结束标记里的后缀（`<marker>_E:`）。
const END_MARKER_SUFFIX: &str = "_E:";

/// 终端里标出 AI 命令用的文案（界面层按当前语言备好，核心库不认识语言）。
///
/// 模板里的占位：`{agent}` 智能体名、`{code}` 退出码、`{time}` 耗时（已按 [`format_elapsed`] 排好）、
/// 耗时模板里的 `{n}`（毫秒数 / 带一位小数的秒数）、`{m}` `{s}`（分、秒）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiMarkLabels {
    /// 开头标记，知道智能体名时用：`▍AI（{agent}）`。
    pub begin_named: String,
    /// 开头标记，不知道智能体名时用：`▍AI`。
    pub begin_plain: String,
    /// 结尾标记，退出码 0：`✓ 退出码 {code} · {time}`。
    pub success: String,
    /// 结尾标记，退出码非 0：`✗ 退出码 {code} · {time}`。
    pub failure: String,
    /// 结尾标记，拿不到退出码：`结束 · {time}`。
    pub unknown: String,
    /// 超时：`⧗ 超时，命令可能仍在运行`。
    pub timeout: String,
    /// 1 秒以下：`{n} 毫秒`。
    pub millis: String,
    /// 1 分钟以下：`{n} 秒`（`{n}` 保留一位小数）。
    pub seconds: String,
    /// 1 分钟以上：`{m} 分 {s} 秒`。
    pub minutes: String,
}

/// 开头标记的颜色：加粗的青色。
const STYLE_BEGIN: &str = "\x1b[1;36m";
/// 成功的结尾标记：绿色。
const STYLE_OK: &str = "\x1b[32m";
/// 失败的结尾标记：红色。
const STYLE_FAIL: &str = "\x1b[31m";
/// 超时 / 拿不到退出码：黄色。
const STYLE_WARN: &str = "\x1b[33m";
/// 每一行标记的结尾都复位，免得后面的输出被染色。
const STYLE_RESET: &str = "\x1b[0m";
/// 智能体名最多取这么多个字符（来自调用方，防止异常的超长名字把标记行撑破）。
const AGENT_NAME_MAX_CHARS: usize = 32;

/// 耗时的显示格式：1 秒以下用毫秒；1 分钟以下保留一位小数的秒；再长用「分 秒」。
///
/// 按四舍五入到十分之一秒再分档，所以 59.96 秒显示成「1 分 0 秒」而不是「60.0 秒」。
pub fn format_elapsed(millis: u64, labels: &AiMarkLabels) -> String {
    if millis < 1000 {
        return labels.millis.replace("{n}", &millis.to_string());
    }
    let tenths = millis.saturating_add(50) / 100;
    if tenths < 600 {
        let text = format!("{}.{}", tenths / 10, tenths % 10);
        return labels.seconds.replace("{n}", &text);
    }
    let total = millis.saturating_add(500) / 1000;
    labels
        .minutes
        .replace("{m}", &(total / 60).to_string())
        .replace("{s}", &(total % 60).to_string())
}

/// 一行标记：样式 + 文字 + 复位 + 换行。文字里的控制字符一律去掉（智能体名等来自外面）。
fn mark_line(style: &str, text: &str) -> Vec<u8> {
    let clean: String = text.chars().filter(|ch| !ch.is_control()).collect();
    format!("{style}{clean}{STYLE_RESET}\r\n").into_bytes()
}

/// 单条命令的标记状态（只在配了 [`AiMarkLabels`] 时有）。
#[derive(Debug)]
struct ActiveMarks {
    labels: Arc<AiMarkLabels>,
    agent: Option<String>,
    armed_at: Instant,
    /// 看到开始标记行（`<marker>_S`）的时刻：耗时从这里算，比 `armed_at` 少掉回显和 shell 起步的时间。
    started_at: Option<Instant>,
    /// 开头标记发出去了：结尾 / 超时标记只在这时才发，保证成对。
    begun: bool,
    /// 上一次放给界面的数据是不是以换行结尾（光标在不在行首）：结尾标记前要不要先换行。
    at_line_start: bool,
}

impl ActiveMarks {
    fn begin_line(&self) -> Vec<u8> {
        let text = match &self.agent {
            Some(agent) => self.labels.begin_named.replace("{agent}", agent),
            None => self.labels.begin_plain.clone(),
        };
        mark_line(STYLE_BEGIN, &text)
    }

    /// 结尾标记（命令跑完了）。`before` 是已经放给界面的数据尾巴，用来判断光标在不在行首。
    fn end_line(&self, exit_code: Option<i32>, now: Instant, before: &[u8]) -> Vec<u8> {
        let from = self.started_at.unwrap_or(self.armed_at);
        let elapsed = now.saturating_duration_since(from);
        let time = format_elapsed(
            elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
            &self.labels,
        );
        let (style, text) = match exit_code {
            Some(0) => (STYLE_OK, self.labels.success.as_str()),
            Some(_) => (STYLE_FAIL, self.labels.failure.as_str()),
            None => (STYLE_WARN, self.labels.unknown.as_str()),
        };
        let text = text
            .replace(
                "{code}",
                &exit_code.map(|code| code.to_string()).unwrap_or_default(),
            )
            .replace("{time}", &time);
        self.on_fresh_line(before, mark_line(style, &text))
    }

    /// 超时标记：命令还在跑，只标一下。
    fn timeout_line(&self, before: &[u8]) -> Vec<u8> {
        self.on_fresh_line(before, mark_line(STYLE_WARN, &self.labels.timeout))
    }

    /// 标记要自己占一行：光标不在行首（输出没以换行结尾）就先换行。
    fn on_fresh_line(&self, before: &[u8], line: Vec<u8>) -> Vec<u8> {
        let at_line_start = match before.last() {
            Some(&last) => last == b'\n',
            None => self.at_line_start,
        };
        if at_line_start {
            line
        } else {
            let mut out = b"\r\n".to_vec();
            out.extend_from_slice(&line);
            out
        }
    }
}

/// 结束标记行里的退出码：`<marker>_E:` 后面的数字（行里回显那一份后面跟的是引号，不算）。
fn end_exit_code(line: &[u8], marker: &str) -> Option<i32> {
    let needle = format!("{marker}{END_MARKER_SUFFIX}");
    let mut from = 0usize;
    while let Some(idx) = find(&line[from..], needle.as_bytes()) {
        let after = &line[from + idx + needle.len()..];
        let digits: Vec<u8> = after
            .iter()
            .copied()
            .take_while(u8::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return std::str::from_utf8(&digits).ok()?.parse().ok();
        }
        from += idx + 1;
    }
    None
}

/// 包装脚本里的命令原文转成合成回显（换行折成 `\r\n`，末尾补一个）。
///
/// 和 Netcatty 的 `formatSyntheticEcho` 一样：xterm 里裸 `\n` 是"下移不回车"，
/// 多行命令会走成阶梯。
pub fn synthetic_echo(command: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(command.len() + 2);
    for ch in command.chars() {
        match ch {
            '\n' => out.extend_from_slice(b"\r\n"),
            '\r' => {}
            other => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// 过滤结果。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DisplayOutput {
    /// 要显示给用户的字节（可能为空：整段都被过滤掉了）。
    pub data: Vec<u8>,
    /// 被过滤掉的字节数。上层要替界面把这部分 ack 掉（界面只能 ack 它收到的
    /// 字节，被吞掉的没人 ack 就会把流控门卡住）。
    pub suppressed: usize,
}

/// 显示过滤器。
#[derive(Debug, Default)]
pub struct DisplayFilter {
    /// 当前正在跑的命令（见到结束标记就退出）。
    active: Option<Active>,
    /// 超时返回、结束标记还没出现的命令：迟到的那一行还要丢掉。
    pending: Vec<Pending>,
    /// 压着还没决定的尾巴（两种模式共用）。
    hold: Vec<u8>,
    hold_since: Option<Instant>,
    /// 终端里标出 AI 命令的文案；`None` = 不标（V2-U50，设置里关掉时就是这个）。
    marks: Option<Arc<AiMarkLabels>>,
}

#[derive(Debug)]
struct Active {
    /// 这条命令的标记。
    marker: String,
    /// 包装脚本原文（去掉清输入前缀和末尾换行）——回显要逐字节匹配它。
    echo: Vec<u8>,
    /// 合成回显（提示符后面显示的命令原文）。
    synthetic: Vec<u8>,
    matched: usize,
    phase: Phase,
    /// 进这个状态之后处理过的字节数（兜底预算）。
    budget: usize,
    /// 合成回显之后还允许丢几个空行。
    blanks_left: usize,
    /// 终端里标出这条命令用的状态；没配文案就是 `None`。
    marks: Option<ActiveMarks>,
}

/// 一条超时命令的待处理标记。
#[derive(Debug)]
struct Pending {
    marker: String,
    since: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 还没匹配到包装脚本回显的开头。
    SeekEcho,
    /// 正在吃回显（可能被行编辑器按终端宽度折行）。
    EatEcho,
    /// 回显吃完了：正常放行，只丢标记行。
    Streaming,
}

impl DisplayFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记下一条马上要写进终端的 AI 命令（**写在写之前**）。
    ///
    /// `payload` 是整行原文（含清输入前缀，末尾可能带换行）；`command` 是用户要
    /// 看到的命令原文。返回值是上一条命令压着的尾巴（要先显示出去，不能吞）。
    pub fn arm(&mut self, marker: &str, payload: &str, command: &str) -> DisplayOutput {
        self.arm_at(marker, payload, command, Instant::now())
    }

    /// 配上 / 去掉「标出 AI 命令」的文案（V2-U50）。只影响之后 `arm` 的命令，已经在跑的不变。
    pub fn set_marks(&mut self, labels: Option<Arc<AiMarkLabels>>) {
        self.marks = labels;
    }

    /// 同 [`Self::arm`]，另外告诉过滤器这条命令是哪个智能体发的（开头标记里要写）。
    pub fn arm_for(
        &mut self,
        marker: &str,
        payload: &str,
        command: &str,
        agent: Option<&str>,
    ) -> DisplayOutput {
        self.arm_for_at(marker, payload, command, agent, Instant::now())
    }

    /// 可测的 `arm`：时间由调用方给。
    pub fn arm_at(
        &mut self,
        marker: &str,
        payload: &str,
        command: &str,
        now: Instant,
    ) -> DisplayOutput {
        self.arm_for_at(marker, payload, command, None, now)
    }

    /// 可测的 [`Self::arm_for`]。
    pub fn arm_for_at(
        &mut self,
        marker: &str,
        payload: &str,
        command: &str,
        agent: Option<&str>,
        now: Instant,
    ) -> DisplayOutput {
        let mut out = DisplayOutput::default();
        // 上一条命令还没结束（比如它超时了、AI 又发了一条）：它的标记转成"待处理"，
        // 迟到的结束标记行还要丢掉。
        if let Some(old) = self.active.take() {
            self.pending.push(Pending {
                marker: old.marker,
                since: now,
            });
        }
        // 压着的尾巴先放行（不能吞）。
        out.data = std::mem::take(&mut self.hold);
        self.hold_since = None;
        let echo = payload
            .trim_start_matches(['\x0b', '\x15'])
            .trim_end_matches(['\n', '\r']);
        self.active = Some(Active {
            marker: marker.to_string(),
            echo: echo.as_bytes().to_vec(),
            synthetic: synthetic_echo(command),
            matched: 0,
            phase: Phase::SeekEcho,
            budget: 0,
            blanks_left: BLANK_LINES_AFTER_ECHO,
            marks: self.marks.as_ref().map(|labels| ActiveMarks {
                labels: Arc::clone(labels),
                agent: agent
                    .map(|name| {
                        name.chars()
                            .filter(|ch| !ch.is_control())
                            .take(AGENT_NAME_MAX_CHARS)
                            .collect::<String>()
                    })
                    .filter(|name| !name.trim().is_empty()),
                armed_at: now,
                started_at: None,
                begun: false,
                at_line_start: false,
            }),
        });
        out
    }

    /// 这条命令超时返回了（命令还在终端里跑，结束标记以后才出现）。
    ///
    /// 过滤器退出完整过滤（不再压用户的输入），标记转成"待处理"，之后只做轻量
    /// 过滤；压着的尾巴立即放行。调用方把返回值发给界面。
    pub fn release(&mut self, marker: &str) -> DisplayOutput {
        self.release_at(marker, Instant::now())
    }

    /// 可测的 `release`：时间由调用方给。
    pub fn release_at(&mut self, marker: &str, now: Instant) -> DisplayOutput {
        let mut out = DisplayOutput::default();
        if self.active.as_ref().is_some_and(|a| a.marker == marker) {
            let old = self.active.take();
            out.data = std::mem::take(&mut self.hold);
            self.hold_since = None;
            if let Some(old) = old {
                // 超时标记：开头标记发过才发，和开头成对；放在压着的尾巴之后，顺序和终端里的一致。
                if let Some(marks) = old.marks.as_ref().filter(|marks| marks.begun) {
                    let line = marks.timeout_line(&out.data);
                    out.data.extend_from_slice(&line);
                }
                self.pending.push(Pending {
                    marker: old.marker,
                    since: now,
                });
            }
        }
        out
    }

    /// 喂一段终端原始输出，返回要显示的数据。
    pub fn push(&mut self, bytes: &[u8]) -> DisplayOutput {
        self.push_at(bytes, Instant::now())
    }

    /// 时间兜底：压着的尾巴等太久了就放行（会话任务定时调用）。
    pub fn poll_at(&mut self, now: Instant) -> DisplayOutput {
        self.prune_pending(now);
        let mut out = DisplayOutput::default();
        if let Some(since) = self.hold_since {
            if now.saturating_duration_since(since) >= HOLD_MAX_AGE {
                out.data = std::mem::take(&mut self.hold);
                self.hold_since = None;
            }
        }
        out
    }

    /// 会话结束：压着的数据全部放行，待处理标记清掉。
    pub fn flush(&mut self) -> DisplayOutput {
        let out = DisplayOutput {
            data: std::mem::take(&mut self.hold),
            ..DisplayOutput::default()
        };
        self.hold_since = None;
        self.pending.clear();
        out
    }

    /// 可测的 `push`：时间由调用方给。
    pub fn push_at(&mut self, bytes: &[u8], now: Instant) -> DisplayOutput {
        self.prune_pending(now);
        let mut out = DisplayOutput::default();

        // 压着的尾巴 + 新数据一起处理。
        let mut input = std::mem::take(&mut self.hold);
        let held_since = self.hold_since.take();
        input.extend_from_slice(bytes);
        if let Some(since) = held_since {
            if now.saturating_duration_since(since) >= HOLD_MAX_AGE {
                // 压太久了：整段放行（兜底）。
                out.data.extend_from_slice(&input);
                return out;
            }
        }

        let DisplayFilter {
            active,
            pending,
            hold,
            hold_since,
            marks: _,
        } = self;
        let mut pos = 0usize;
        while pos < input.len() {
            let rest = &input[pos..];
            let mut finished = false;
            match active.as_mut() {
                // 没有正在跑的 AI 命令：直通；有待处理标记时做轻量过滤。
                None => {
                    if pending.is_empty() {
                        out.data.extend_from_slice(rest);
                        break;
                    }
                    let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
                        // 没有完整行：只有"还可能长成待处理标记的结束标记行"才压住，
                        // 用户敲的单个 `_`、`__`、`__Z` 一律立即放行。
                        if tail_may_become_marker_line(rest, pending) {
                            *hold = rest.to_vec();
                            *hold_since = Some(now);
                        } else {
                            out.data.extend_from_slice(rest);
                        }
                        break;
                    };
                    let line = &rest[..=nl];
                    pos += line.len();
                    let mut satisfied = Vec::new();
                    let mut from: Option<usize> = None;
                    for (index, entry) in pending.iter().enumerate() {
                        if let Some(at) = find(line, entry.marker.as_bytes()) {
                            if from.is_none_or(|current| at < current) {
                                from = Some(at);
                            }
                            if line_ends_command(line, &entry.marker) {
                                satisfied.push(index);
                            }
                        }
                    }
                    match from {
                        Some(at) => emit_marker_line(line, at, &mut out),
                        None => out.data.extend_from_slice(line),
                    }
                    for index in satisfied.into_iter().rev() {
                        pending.remove(index);
                    }
                }
                Some(act) => match act.phase {
                    Phase::SeekEcho => {
                        // 回显开头（回显本身也带着标记；顺序反了会把回显当成
                        // "没有回显"的按行模式，合成回显就丢了）。
                        let needle_len = act.echo.len().min(12);
                        let needle = act.echo[..needle_len].to_vec();
                        if let Some(idx) = find(rest, &needle) {
                            // 回显前面那些杂音（readline 清空输入时响的铃 \x07
                            // 之类）不发给界面——不然每条 AI 命令都会响一下。
                            out.suppressed += append_without_bel(&mut out.data, &rest[..idx]);
                            act.matched = needle.len();
                            act.phase = Phase::EatEcho;
                            pos += idx + needle.len();
                            continue;
                        }
                        // 回显被关掉 / shell 行为不同：包装脚本自己的输出先到了
                        // （开始标记行以标记开头）→ 直接按行过滤。
                        //
                        // 不能用"任意位置出现标记"判断：回显的开头本身是
                        // " __M__"，`contains` 会踩这个坑把回显当成输出。
                        if line_starts_with(rest, act.marker.as_bytes()) {
                            act.phase = Phase::Streaming;
                            continue;
                        }
                        // 把可能是回显开头的前缀压住，其余放行。
                        let keep = trailing_prefix_len(rest, &needle);
                        let emit = rest.len() - keep;
                        out.suppressed += append_without_bel(&mut out.data, &rest[..emit]);
                        if keep > 0 {
                            *hold = rest[emit..].to_vec();
                            *hold_since = Some(now);
                        }
                        break;
                    }
                    Phase::EatEcho => {
                        let echo_len = act.echo.len();
                        let mut i = 0usize;
                        while i < rest.len() && act.matched < echo_len {
                            let byte = rest[i];
                            if byte == act.echo[act.matched] {
                                act.matched += 1;
                                out.suppressed += 1;
                                i += 1;
                                continue;
                            }
                            if is_wrap_noise(byte) {
                                // 折行插入的换行、回显边上的控制字符：一起吃掉。
                                out.suppressed += 1;
                                i += 1;
                                continue;
                            }
                            // 不是回显的内容（转义序列、用户敲的键）：照常显示，
                            // 终端的模式切换不能吞。
                            out.data.push(byte);
                            i += 1;
                        }
                        act.budget += i;
                        pos += i;
                        if act.matched >= echo_len {
                            // 回显吃完了：发合成回显，之后正常放行。开了标记的话，开头标记在合成回显之前。
                            act.phase = Phase::Streaming;
                            if let Some(marks) = act.marks.as_mut() {
                                out.data.extend_from_slice(&marks.begin_line());
                                marks.begun = true;
                                marks.at_line_start = true;
                            }
                            out.data.extend_from_slice(&act.synthetic);
                            continue;
                        }
                        if act.budget > echo_len + ECHO_BUDGET_BYTES {
                            // 一直匹配不上：退化成按行过滤，不再吞字节。
                            act.phase = Phase::Streaming;
                        }
                        break;
                    }
                    Phase::Streaming => {
                        let marker = act.marker.clone();
                        let Some(nl) = rest.iter().position(|&b| b == b'\n') else {
                            // 没有完整行。压住两种情况：可能是标记的一部分，或者这一行
                            // 到目前为止没有任何要显示的东西（`\r` 先到、`\n` 在下一段
                            // ——要和整段一起按空行处理，不然块边界会改变显示结果）。
                            //
                            // 这条"没有可见字符就压住"只在**当前命令的结束标记之前**
                            // 用（AI-2e：命令结束后过滤器就退出了，用户的 vim /
                            // less 光标移动不能被压）。
                            if !has_visible_content(rest)
                                || streaming_tail_is_suspicious(rest, &marker, pending)
                            {
                                *hold = rest.to_vec();
                                *hold_since = Some(now);
                            } else {
                                out.data.extend_from_slice(rest);
                            }
                            break;
                        };
                        let line = &rest[..=nl];
                        pos += line.len();
                        if let Some(at) = find(line, marker.as_bytes()) {
                            // 当前命令的标记行：丢掉（行首整行丢，行内只丢标记）。
                            emit_marker_line(line, at, &mut out);
                            if line_ends_command(line, &marker) {
                                // 结束标记行处理完：退出过滤器，回到直通。结尾标记就放在这条标记行原来的位置：
                                // 命令输出之后、下一个提示符之前。
                                finished = true;
                                if let Some(marks) = act.marks.as_ref().filter(|marks| marks.begun)
                                {
                                    let code = end_exit_code(line, &marker);
                                    let tail = marks.end_line(code, now, &out.data);
                                    out.data.extend_from_slice(&tail);
                                }
                            } else if let Some(marks) = act.marks.as_mut() {
                                if marks.started_at.is_none()
                                    && find(line, format!("{marker}_S").as_bytes()).is_some()
                                {
                                    marks.started_at = Some(now);
                                }
                            }
                        } else if let Some(at) = pending_line_hit(line, pending, &mut out) {
                            let _ = at;
                        } else if is_blank_line(line) {
                            if act.blanks_left > 0 {
                                act.blanks_left -= 1;
                                out.suppressed += line.len();
                            } else {
                                out.data.extend_from_slice(line);
                            }
                        } else {
                            act.blanks_left = 0;
                            out.data.extend_from_slice(line);
                        }
                    }
                },
            }
            if finished {
                *active = None;
                // 压着的尾巴立即放行（退出时不压任何东西）。
                if !hold.is_empty() {
                    out.data.extend_from_slice(hold);
                    *hold = Vec::new();
                    *hold_since = None;
                }
            }
        }

        // 记下界面上光标是不是在行首：超时标记（`release`）要不要先换行靠它。
        if let (Some(marks), Some(&last)) = (
            active.as_mut().and_then(|act| act.marks.as_mut()),
            out.data.last(),
        ) {
            marks.at_line_start = last == b'\n';
        }

        // 没处理完的（压住的尾巴）留到下一段。
        if !hold.is_empty() && hold_since.is_none() {
            *hold_since = Some(now);
        } else if hold.len() > HOLD_MAX_BYTES {
            // 压得太多了：放行（兜底，绝不吞输出）。
            out.data.extend_from_slice(hold);
            *hold = Vec::new();
            *hold_since = None;
        }
        out
    }

    /// 超时的待处理标记到点就清（会话任务定时调 `poll_at` 时顺手做）。
    fn prune_pending(&mut self, now: Instant) {
        self.pending
            .retain(|entry| now.saturating_duration_since(entry.since) < PENDING_MAX_AGE);
    }
}

/// 把标记行按"丢掉标记本身、保留前面的用户输出"处理。
///
/// 标记在行首（正常情况）时 `before` 是空的或只有换行之类，整行丢掉；标记在行
/// 中间（命令输出没有换行时结束标记会跟在输出后面）时，前面的用户输出留着。
fn emit_marker_line(line: &[u8], at: usize, out: &mut DisplayOutput) {
    let before = &line[..at];
    if before.iter().all(|&b| is_wrap_noise(b)) {
        out.suppressed += line.len();
    } else {
        out.data.extend_from_slice(before);
        out.suppressed += line.len() - before.len();
    }
}

/// 行里是不是有**待处理标记**；有就按"丢标记、留前面的输出"处理并返回命中的位置。
///
/// 顺带把"这一行是那个标记的结束标记行"的待处理项清掉。
fn pending_line_hit(
    line: &[u8],
    pending: &mut Vec<Pending>,
    out: &mut DisplayOutput,
) -> Option<usize> {
    let mut satisfied = Vec::new();
    let mut from: Option<usize> = None;
    for (index, entry) in pending.iter().enumerate() {
        if let Some(at) = find(line, entry.marker.as_bytes()) {
            if from.is_none_or(|current| at < current) {
                from = Some(at);
            }
            if line_ends_command(line, &entry.marker) {
                satisfied.push(index);
            }
        }
    }
    let from = from?;
    emit_marker_line(line, from, out);
    for index in satisfied.into_iter().rev() {
        pending.remove(index);
    }
    Some(from)
}

/// `line` 里有没有 `<marker>_E:`（命令的结束标记）。
fn line_ends_command(line: &[u8], marker: &str) -> bool {
    let needle = format!("{marker}{END_MARKER_SUFFIX}");
    find(line, needle.as_bytes()).is_some()
}

/// 折行 / 回显之类可以一起吃掉的控制字节。
fn is_wrap_noise(byte: u8) -> bool {
    matches!(byte, b'\r' | b'\n' | 0x07 | 0x0b | 0x15)
}

/// 把 `bytes` 里除了 BEL（`\x07`）之外的内容追加到 `out`，返回丢掉的字节数。
///
/// 回显前面那个 BEL 是 readline 清空输入时响的铃，发给界面就会响一下/闪一下。
fn append_without_bel(out: &mut Vec<u8>, bytes: &[u8]) -> usize {
    let before = out.len();
    out.extend(bytes.iter().copied().filter(|&byte| byte != 0x07));
    bytes.len() - (out.len() - before)
}

fn is_blank_line(line: &[u8]) -> bool {
    !has_visible_content(line)
}

/// 这一段里有没有"要显示的东西"：去掉 ANSI 序列（CSI / OSC）和换行之类之后还有
/// 可见字符就是有。空行判断要连转义序列一起忽略——`\x1b[?2004l\r\r\n` 这种行
/// 在用户眼里是空行（括号粘贴的开关看不见）。
fn has_visible_content(bytes: &[u8]) -> bool {
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                // CSI：吃到终止字节（0x40..=0x7e）。
                i += 1;
                while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else if i < bytes.len() && bytes[i] == b']' {
                // OSC：吃到 BEL 或者 ESC \。
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        if !is_wrap_noise(byte) {
            return true;
        }
        i += 1;
    }
    false
}

/// `haystack` 里有没有一行以 `needle` 开头（行首 = 字符串开头或者紧跟换行 / 回车）。
fn line_starts_with(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return false;
    }
    let mut from = 0usize;
    while let Some(idx) = find(&haystack[from..], needle) {
        let at = from + idx;
        if at == 0 || matches!(haystack[at - 1], b'\n' | b'\r') {
            return true;
        }
        from = at + 1;
    }
    false
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

/// `haystack` 末尾有多少个字节是 `needle` 的前缀（没拼完的开头）。
fn trailing_prefix_len(haystack: &[u8], needle: &[u8]) -> usize {
    let max = (needle.len() - 1).min(haystack.len());
    (1..=max)
        .rev()
        .find(|&len| haystack.ends_with(&needle[..len]))
        .unwrap_or(0)
}

/// 还没换行的尾巴，会不会长成某个待处理标记的结束标记行（AI-3 第 0 节）。
///
/// AI-2e 的原写法只认"末尾是完整的 `__ZEETERM_` 前缀 + 若干十六进制"，标记比较长、
/// 被拆在中间时（尾巴末尾成了 `__`）会被误判成普通文字放行，迟到的结束标记就漏到
/// 屏幕上。改成按**完整写法 `<标记>_E:<数字>` 的前缀**判断：
///
/// - 标记还没收全：尾巴是这个标记自己字节的前缀，且至少到了 `__ZEETERM_`（10 个字符；
///   标记本身比这还短时就是整个标记）——单个 `_`、`__`、`__Z` 都不压；
/// - 标记已收全：它后面跟的是 `_E:` + 若干数字（可能只写了一半，数字后可以有一个
///   `\r`），行还没结束，压住等新行到来后整行丢掉。
fn tail_may_become_marker_line(tail: &[u8], pending: &[Pending]) -> bool {
    pending
        .iter()
        .any(|entry| marker_line_possible(tail, &entry.marker))
}

/// 尾巴末尾的一部分，会不会长成 `marker` 的结束标记行。
fn marker_line_possible(tail: &[u8], marker: &str) -> bool {
    let marker = marker.as_bytes();
    if marker.is_empty() {
        return false;
    }

    // ① 标记还没收全：尾巴末尾必须对上这个标记自己的字节，不能只看常量前缀。
    let least = MARKER_PREFIX.len().min(marker.len());
    let longest = marker.len().min(tail.len());
    for len in least..=longest {
        if tail.ends_with(&marker[..len]) {
            return true;
        }
    }

    // ② 标记已收全：后面是 `_E:` + 数字的前缀（数字后可以跟一个 `\r`）。
    let Some(at) = rfind(tail, marker) else {
        return false;
    };
    let after = &tail[at + marker.len()..];
    let suffix = END_MARKER_SUFFIX.as_bytes();
    if after.len() < suffix.len() {
        return suffix.starts_with(after);
    }
    let rest = &after[suffix.len()..];
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    let end = &rest[digits..];
    end.is_empty() || end == b"\r"
}

/// 当前命令阶段里，还没换行的尾巴要不要压住：里面有当前命令的标记或待处理标记了
/// （等它的行尾），或者尾巴正好是某个标记的前缀（下一段可能就是它）。
fn streaming_tail_is_suspicious(tail: &[u8], active_marker: &str, pending: &[Pending]) -> bool {
    let markers = std::iter::once(active_marker)
        .filter(|marker| !marker.is_empty())
        .chain(pending.iter().map(|entry| entry.marker.as_str()));
    for marker in markers {
        let bytes = marker.as_bytes();
        if find(tail, bytes).is_some() || trailing_prefix_len(tail, bytes) > 0 {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写进终端那一行的包装脚本（不含清输入前缀和末尾换行）。
    fn wrapped_of(marker: &str, command: &str) -> String {
        format!(
            " {marker}=0; {{ printf '\\n%s\\n' '{marker}_S'; trap ':' INT; {marker}_cmd='{command}'; eval \"${marker}_cmd\"; {marker}_rc=$?; trap - INT; printf '%s\\n' '{marker}_E:'\"${marker}_rc\"; }}"
        )
    }

    /// `arm` 时要给的整行原文（清输入前缀 + 包装脚本）。
    fn payload_of(marker: &str, command: &str) -> String {
        format!("\x0b\x15{}", wrapped_of(marker, command))
    }

    /// 一条完整的 AI 命令在终端里产生的原始输出（真实布局，见联网测试的采样）。
    ///
    /// 回显 = readline 的 BEL（清输入前缀不进回显）+ 包装脚本原文 + 回车；随后是
    /// 执行时 bash 关掉括号粘贴、开始标记行、命令输出、结束标记行、新提示符。
    fn command_stream(marker: &str, command: &str, output: &str, code: i32) -> Vec<u8> {
        let wrapped = wrapped_of(marker, command);
        format!(
            "\x07{wrapped}\r\n\x1b[?2004l\r\r\n{marker}_S\r\n{output}{marker}_E:{code}\r\n\x1b[?2004h$ "
        )
        .into_bytes()
    }

    /// 跑一遍：arm + 整段输出一次喂进去。
    fn filter_one(marker: &str, command: &str, output: &str, code: i32) -> DisplayOutput {
        let mut filter = DisplayFilter::new();
        let stream = command_stream(marker, command, output, code);
        let mut out = filter.arm(marker, &payload_of(marker, command), command);
        let pushed = filter.push(&stream);
        out.data.extend_from_slice(&pushed.data);
        out.suppressed += pushed.suppressed;
        out
    }

    /// 跑完一条完整的 AI 命令（用来测"之后"的行为）。
    fn run_one_command(filter: &mut DisplayFilter, marker: &str, command: &str) -> DisplayOutput {
        let stream = command_stream(marker, command, "hi\r\n", 0);
        let mut out = filter.arm(marker, &payload_of(marker, command), command);
        let pushed = filter.push(&stream);
        out.data.extend_from_slice(&pushed.data);
        out.suppressed += pushed.suppressed;
        out
    }

    /// 组一个"超时命令、标记待处理"的过滤器（轻量过滤的状态，AI-3 第 0 节用）。
    fn filter_with_pending_marker(marker: &str, now: Instant) -> DisplayFilter {
        let mut filter = DisplayFilter::new();
        let _ = filter.arm_at(marker, &payload_of(marker, "sleep 30"), "sleep 30", now);
        let _ = filter.push_at(
            format!(
                "\x07{}\r\n{marker}_S\r\npartial output",
                wrapped_of(marker, "sleep 30")
            )
            .as_bytes(),
            now,
        );
        let _ = filter.release_at(marker, now);
        assert_eq!(filter.pending.len(), 1, "标记要转成待处理");
        filter
    }

    #[test]
    fn synthetic_echo_normalizes_newlines() {
        assert_eq!(synthetic_echo("echo hi"), b"echo hi\r\n".to_vec());
        assert_eq!(synthetic_echo("a\nb"), b"a\r\nb\r\n".to_vec());
        assert_eq!(synthetic_echo("a\r\nb"), b"a\r\nb\r\n".to_vec());
    }

    #[test]
    fn hides_the_wrapper_and_shows_the_command() {
        let out = filter_one("__M__", "echo hi", "hi\r\n", 0);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            !text.contains("__M__"),
            "包装脚本和标记都不该显示：{text:?}"
        );
        assert!(text.contains("echo hi\r\n"), "要有合成回显：{text:?}");
        assert!(text.contains("hi\r\n"), "命令输出要在：{text:?}");
        assert!(text.contains("$ "), "最后的提示符要放行：{text:?}");
        assert!(out.suppressed > 0, "确实吞掉了包装脚本那一段");
        assert!(
            !text.contains('\x07'),
            "回显前的 BEL 不能发给界面：{text:?}"
        );
    }

    #[test]
    fn the_same_holds_when_the_stream_is_split_anywhere() {
        // 标记（和包装脚本）被拆在两段、三段数据里也不能出错。
        let stream = command_stream("__M__", "echo hi", "hi\r\n", 0);
        let reference = {
            let mut filter = DisplayFilter::new();
            let mut out = filter.arm("__M__", &payload_of("__M__", "echo hi"), "echo hi");
            let pushed = filter.push(&stream);
            out.data.extend_from_slice(&pushed.data);
            out
        };
        for cut in 1..stream.len() {
            let mut filter = DisplayFilter::new();
            let mut out = filter.arm("__M__", &payload_of("__M__", "echo hi"), "echo hi");
            for piece in [&stream[..cut], &stream[cut..]] {
                out.data.extend_from_slice(&filter.push(piece).data);
            }
            assert_eq!(
                out.data, reference.data,
                "在第 {cut} 字节切开之后显示结果要一样"
            );
        }
    }

    #[test]
    fn tolerates_a_wrapped_command_echo() {
        // 行编辑器按终端宽度折行：回显里被插入 \r\n。
        let marker = "__M__";
        let command = "echo hi";
        let wrapped = wrapped_of(marker, command);
        let mut folded = String::new();
        for (index, chunk) in wrapped.as_bytes().chunks(40).enumerate() {
            if index > 0 {
                folded.push_str("\r\n");
            }
            folded.push_str(&String::from_utf8_lossy(chunk));
        }
        let stream = format!(
            "\x07{folded}\r\n\x1b[?2004l\r\r\n{marker}_S\r\nhi\r\n{marker}_E:0\r\n\x1b[?2004h$ "
        );
        let mut filter = DisplayFilter::new();
        let mut out = filter.arm(marker, &payload_of(marker, command), command);
        out.data
            .extend_from_slice(&filter.push(stream.as_bytes()).data);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(!text.contains(marker), "折行回显也要吃掉：{text:?}");
        assert!(text.contains("echo hi\r\n"), "合成回显在：{text:?}");
        assert!(text.contains("hi\r\n"), "输出在：{text:?}");
    }

    #[test]
    fn colored_output_passes_through_untouched() {
        let out = filter_one("__M__", "ls", "\x1b[31mred\x1b[0m\r\n", 0);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            text.contains("\x1b[31mred\x1b[0m"),
            "带颜色的输出要原样放行：{text:?}"
        );
    }

    #[test]
    fn output_that_looks_like_another_marker_is_kept() {
        // 命令输出里恰好有**别的**标记样文字：原样显示。
        let out = filter_one(
            "__M__",
            "cat notes.txt",
            "这里有个 __ZEETERM_deadbeefdeadbeefdeadbeef___E:7 样的字符串\r\n",
            0,
        );
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            text.contains("__ZEETERM_deadbeefdeadbeefdeadbeef___E:7"),
            "不是我们这次用的标记，不能动：{text:?}"
        );
    }

    #[test]
    fn inline_end_marker_keeps_the_output_before_it() {
        // 命令输出没有以换行结尾：结束标记跟在输出后面，只丢标记本身。
        let marker = "__M__";
        let stream = command_stream(marker, "printf abc", "abc", 0);
        let mut filter = DisplayFilter::new();
        let mut out = filter.arm(marker, &payload_of(marker, "printf abc"), "printf abc");
        out.data.extend_from_slice(&filter.push(&stream).data);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(text.contains("abc"), "行内的用户输出要留着：{text:?}");
        assert!(!text.contains(marker), "行内的结束标记要丢掉：{text:?}");
        assert!(text.contains("printf abc\r\n"), "合成回显在：{text:?}");
    }

    #[test]
    fn falls_back_to_line_filtering_when_the_echo_is_off() {
        // 兜底：回显被关掉，包装脚本自己的输出直接出来了。什么都别吞，
        // 只把标记行丢掉。
        let marker = "__M__";
        let stream = format!("\n{marker}_S\r\nZEETERM_ECHO_OFF\r\n{marker}_E:0\r\n\x1b[?2004h$ ");
        let mut filter = DisplayFilter::new();
        let mut out = filter.arm(
            marker,
            &payload_of(marker, "echo ZEETERM_ECHO_OFF"),
            "echo ZEETERM_ECHO_OFF",
        );
        out.data
            .extend_from_slice(&filter.push(stream.as_bytes()).data);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            text.contains("ZEETERM_ECHO_OFF\r\n"),
            "命令输出一定要显示出来：{text:?}"
        );
        assert!(!text.contains(marker), "标记行还是要丢掉：{text:?}");
        assert!(text.contains("$ "), "提示符放行：{text:?}");
    }

    #[test]
    fn holds_suspicious_tails_but_releases_them_on_timeout() {
        // 尾巴正好是标记的前缀：先压住，等不到下文就按时间放行（兜底）。
        let marker = "__M__";
        let now = Instant::now();
        let mut filter = DisplayFilter::new();
        let mut out = filter.arm(marker, &payload_of(marker, "echo hi"), "echo hi");
        // 回显 + 开始标记 + 一段以标记前缀结尾的输出（没有换行）。
        let first = format!(
            "\x07{}\r\n{marker}_S\r\npartial output __M",
            wrapped_of(marker, "echo hi")
        );
        out.data
            .extend_from_slice(&filter.push_at(first.as_bytes(), now).data);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            !text.contains("__M"),
            "可能是标记前缀的尾巴先压住：{text:?}"
        );
        // 时间到：放行。
        let later = now + HOLD_MAX_AGE + Duration::from_millis(1);
        let released = filter.poll_at(later);
        let text = String::from_utf8_lossy(&released.data).into_owned();
        assert!(
            text.contains("partial output __M"),
            "兜底必须放行，不能吞输出：{text:?}"
        );
    }

    #[test]
    fn tail_like_ordinary_text_is_shown_immediately() {
        // 普通尾巴（不是标记前缀）不压：直接显示。
        let marker = "__M__";
        let mut filter = DisplayFilter::new();
        let mut out = filter.arm(marker, &payload_of(marker, "echo hi"), "echo hi");
        let first = format!(
            "\x07{}\r\n{marker}_S\r\ndone, no newline",
            wrapped_of(marker, "echo hi")
        );
        out.data
            .extend_from_slice(&filter.push(first.as_bytes()).data);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            text.contains("done, no newline"),
            "普通尾巴要立刻显示：{text:?}"
        );
    }

    #[test]
    fn multi_line_command_echo_is_normalized() {
        let marker = "__M__";
        let command = "echo one\necho two";
        let out = filter_one(marker, command, "one\r\ntwo\r\n", 0);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            text.contains("echo one\r\necho two\r\n"),
            "多行命令的合成回显要按 \r\n 折：{text:?}"
        );
    }

    // ---------- AI-2e 第 1 节：用完必须退出 ----------

    #[test]
    fn after_a_command_everything_passes_through_immediately() {
        // 跑完一条完整的 AI 命令之后，用户敲什么都要立即、原样地过去。
        let now = Instant::now();
        let mut filter = DisplayFilter::new();
        run_one_command(&mut filter, "__M__", "echo hi");
        for chunk in [
            b"_".as_slice(),
            b"__".as_slice(),
            b"__Z".as_slice(),
            b"\x1b[5;10H", // vim / less 里移动光标
            b"\r",
            b"\x1b[?25l", // 隐藏光标
            b"\x1b[2J",
        ] {
            let out = filter.push_at(chunk, now);
            assert_eq!(
                out,
                DisplayOutput {
                    data: chunk.to_vec(),
                    suppressed: 0
                },
                "命令结束之后 {:?} 要原样立即返回",
                String::from_utf8_lossy(chunk)
            );
        }
    }

    #[test]
    fn suspicious_but_harmless_tails_are_not_held_after_a_command() {
        // 同上，但按"压住"的反面测：这些块之后 poll 不应该吐出任何东西
        // （说明它们当时就发出去了，而不是被压着等超时）。
        let now = Instant::now();
        let mut filter = DisplayFilter::new();
        run_one_command(&mut filter, "__M__", "echo hi");
        for chunk in [
            b"my_var".as_slice(),
            b"\x1b[5;10H".as_slice(),
            b"_".as_slice(),
        ] {
            let _ = filter.push_at(chunk, now);
        }
        let polled = filter.poll_at(now + HOLD_MAX_AGE + Duration::from_millis(1));
        assert_eq!(polled.data, Vec::<u8>::new(), "没有东西被压着");
    }

    #[test]
    fn a_timed_out_command_keeps_only_light_filtering() {
        // 超时返回：release 之后只做轻量过滤——迟到的结束标记行丢掉，
        // 用户敲的 `_` 立即显示。
        let marker = "__M__";
        let now = Instant::now();
        let mut filter = DisplayFilter::new();
        let stream = format!(
            "\x07{}\r\n{marker}_S\r\npartial output",
            wrapped_of(marker, "sleep 30")
        );
        let _ = filter.arm_at(marker, &payload_of(marker, "sleep 30"), "sleep 30", now);
        let mut out = filter.push_at(stream.as_bytes(), now);
        out.data
            .extend_from_slice(&filter.release_at(marker, now).data);

        // 用户接着用终端：`_` 必须立即出来。
        let typed = filter.push_at(b"_", now);
        assert_eq!(
            typed.data,
            b"_".to_vec(),
            "超时之后用户敲的 `_` 不能被压：{:?}",
            String::from_utf8_lossy(&typed.data)
        );
        let vim = filter.push_at(b"\x1b[5;10H", now);
        assert_eq!(vim.data, b"\x1b[5;10H".to_vec(), "光标移动要立即过去");

        // 迟到的结束标记行：丢掉；待处理标记也就清了。
        let late = format!("\r\n{marker}_E:0\r\n");
        let dropped = filter.push_at(late.as_bytes(), now);
        assert!(
            !String::from_utf8_lossy(&dropped.data).contains(marker),
            "迟到的结束标记行要丢掉：{:?}",
            String::from_utf8_lossy(&dropped.data)
        );
        assert!(dropped.suppressed > 0, "确实丢了东西");
        assert!(filter.pending.is_empty(), "标记出现之后要清掉");

        // 清掉之后再出现同样的文字（用户 cat 出来的）就直通了。
        let again = filter.push_at(late.as_bytes(), now);
        assert_eq!(
            again.data,
            late.as_bytes().to_vec(),
            "没有待处理标记时完全直通"
        );
    }

    #[test]
    fn arming_a_new_command_keeps_the_previous_marker_pending() {
        // 上一条命令没结束（超时），下一条命令又来了：两条命令的标记行都要丢。
        let now = Instant::now();
        let mut filter = DisplayFilter::new();
        let first = "__M1__";
        let _ = filter.arm_at(first, &payload_of(first, "sleep 30"), "sleep 30", now);
        let _ = filter.push_at(
            format!("\x07{}\r\n{first}_S\r\n", wrapped_of(first, "sleep 30")).as_bytes(),
            now,
        );
        // 第二条命令：第一条的标记转成待处理。
        let second = "__M2__";
        let out = filter.arm_at(second, &payload_of(second, "echo hi"), "echo hi", now);
        assert_eq!(out.data, Vec::<u8>::new());
        let stream = command_stream(second, "echo hi", "hi\r\n", 0);
        let shown = filter.push_at(&stream, now);
        let text = String::from_utf8_lossy(&shown.data).into_owned();
        assert!(!text.contains(second), "第二条命令的包装脚本也不显示");
        assert!(text.contains("hi\r\n"), "第二条命令的输出在");

        // 第一条的迟到标记：丢掉。
        let late = format!("{first}_E:0\r\n");
        let dropped = filter.push_at(late.as_bytes(), now);
        assert!(
            !String::from_utf8_lossy(&dropped.data).contains(first),
            "上一条命令迟到的标记行也要丢"
        );
        // 第二条命令结束之后：完全直通。
        let after = filter.push_at(b"_", now);
        assert_eq!(after.data, b"_".to_vec());
    }

    #[test]
    fn end_marker_split_across_chunks_still_exits() {
        // 结束标记被拆在两段数据里：还是要正确丢掉并退出。
        let marker = "__M__";
        let now = Instant::now();
        let mut filter = DisplayFilter::new();
        let stream = command_stream(marker, "echo hi", "hi\r\n", 0);
        let cut = stream
            .windows(marker.len() + END_MARKER_SUFFIX.len())
            .position(|w| w == format!("{marker}{END_MARKER_SUFFIX}").as_bytes())
            .expect("找得到结束标记")
            + marker.len()
            + 1; // 切在 `_E:` 的数字中间
        let mut out = filter.arm_at(marker, &payload_of(marker, "echo hi"), "echo hi", now);
        for piece in [&stream[..cut], &stream[cut..]] {
            let pushed = filter.push_at(piece, now);
            out.data.extend_from_slice(&pushed.data);
            out.suppressed += pushed.suppressed;
        }
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(!text.contains(marker), "结束标记不能漏出来：{text:?}");
        assert!(filter.active.is_none(), "结束标记处理完要退出");
        // 退出之后立即直通。
        let after = filter.push_at(b"\x1b[5;10H", now);
        assert_eq!(after.data, b"\x1b[5;10H".to_vec());
    }

    #[test]
    fn pending_markers_expire_after_thirty_minutes() {
        let marker = "__M__";
        let now = Instant::now();
        let mut filter = DisplayFilter::new();
        let _ = filter.arm_at(marker, &payload_of(marker, "sleep 30"), "sleep 30", now);
        let _ = filter.release_at(marker, now);
        assert_eq!(filter.pending.len(), 1);
        let _ = filter.poll_at(now + PENDING_MAX_AGE + Duration::from_secs(1));
        assert!(filter.pending.is_empty(), "30 分钟后清掉");
    }

    #[test]
    fn a_late_pending_marker_split_at_any_position_does_not_show() {
        // AI-3 第 0 节：超时命令迟到的结束标记，尾巴末尾落到 `__`（标记的收尾，
        // 不再像常量前缀）时以前会被放行，标记就整个显示在屏幕上。现在按"某个
        // 待处理标记完整写法 `<标记>_E:<数字>` 的前缀"判断，从 `__ZEETERM_` 这 10 个
        // 字符在同一段里收全之后的**每一个拆点**都不能漏。
        let marker = "__ZEETERM_1f2e3d4c__";
        let now = Instant::now();
        let line = format!("output {marker}_E:0\r\n$ ");
        // 拆点从"第一段里已经有完整的 `__ZEETERM_`"开始（更早的碎片和用户敲的
        // `_` / `__` 字节完全一样，卡里明确要求不压，见下一个测试）。
        let covered = "output ".len() + MARKER_PREFIX.len();

        for cut in covered..line.len() {
            let mut filter = filter_with_pending_marker(marker, now);
            let mut shown = filter.push_at(&line.as_bytes()[..cut], now).data;
            shown.extend_from_slice(&filter.push_at(&line.as_bytes()[cut..], now).data);
            let shown = String::from_utf8_lossy(&shown).into_owned();
            assert!(
                !shown.contains("__ZEETERM_"),
                "拆在 {cut} 时标记漏出来了：{shown:?}"
            );
            assert!(
                !shown.contains("_E:"),
                "拆在 {cut} 时结束标记拖尾漏出来了：{shown:?}"
            );
            assert!(
                shown.contains("output ") && shown.contains("$ "),
                "拆在 {cut} 时画面缺内容：{shown:?}"
            );
        }

        // 卡片里报的三段式（`__ZEETERM_b` / `bbbb…bb__` / `_E:0`）原样演一遍。
        let b_marker = "__ZEETERM_bbbbbbbb__";
        let mut filter = filter_with_pending_marker(b_marker, now);
        let mut shown = filter.push_at(b"output __ZEETERM_b", now).data;
        shown.extend_from_slice(&filter.push_at(b"bbbbbbb__", now).data);
        shown.extend_from_slice(&filter.push_at(b"_E:0\r\n$ ", now).data);
        let shown = String::from_utf8_lossy(&shown).into_owned();
        assert!(
            !shown.contains("__ZEETERM_") && !shown.contains("_E:"),
            "三段式拆开时标记漏出来了：{shown:?}"
        );
    }

    #[test]
    fn a_late_pending_marker_never_shows_even_pushed_byte_by_byte() {
        // 同上，但余下部分按单个字节推进来（第 0 节要求逐字节拆一遍）。
        let marker = "__ZEETERM_1f2e3d4c__";
        let now = Instant::now();
        let line = format!("output {marker}_E:0\r\n$ ");
        let (head, rest) = line.split_at("output __ZEETERM_".len());
        let mut filter = filter_with_pending_marker(marker, now);
        let mut shown = filter.push_at(head.as_bytes(), now).data;
        for byte in rest.as_bytes() {
            shown.extend_from_slice(&filter.push_at(&[*byte], now).data);
        }
        let shown = String::from_utf8_lossy(&shown).into_owned();
        assert!(
            !shown.contains("__ZEETERM_"),
            "逐字节推时标记漏出来了：{shown:?}"
        );
        assert!(
            !shown.contains("_E:"),
            "逐字节推时结束标记拖尾漏出来了：{shown:?}"
        );
        assert!(
            shown.contains("output ") && shown.contains("$ "),
            "逐字节推时画面缺内容：{shown:?}"
        );
    }

    #[test]
    fn other_marker_like_text_from_the_user_is_shown_immediately() {
        // 用户自己输出 `__ZEETERM_cafe__` 这类别的文字：既不能丢，也不能压。
        let marker = "__ZEETERM_1f2e3d4c__";
        let now = Instant::now();
        let mut filter = filter_with_pending_marker(marker, now);

        let line = b"echo __ZEETERM_cafe__\r\n__ZEETERM_cafe__\r\n";
        let out = filter.push_at(line, now);
        assert_eq!(out.data, line.to_vec(), "别的标记样文字要原样显示");

        // 不带换行的尾巴也一样立即放行。
        let tail = b"ab__ZEETERM_cafe__";
        let out = filter.push_at(tail, now);
        assert_eq!(out.data, tail.to_vec(), "别的标记样尾巴不能压");
    }

    #[test]
    fn short_tails_are_not_held_but_the_prefix_is() {
        // 单个 `_`、`__`、`__Z`、`__ZEETERM` 不压（用户敲变量名不能被拖慢）；
        // 从完整的 `__ZEETERM_` 起才压，而且 2 秒兜底到点原样放行。
        let marker = "__ZEETERM_1f2e3d4c__";
        let now = Instant::now();

        for short in [
            b"_".as_slice(),
            b"__".as_slice(),
            b"__Z".as_slice(),
            b"__ZEETERM".as_slice(),
            b"my_".as_slice(),
        ] {
            let mut filter = filter_with_pending_marker(marker, now);
            let out = filter.push_at(short, now);
            assert_eq!(
                out.data,
                short.to_vec(),
                "{:?} 要立即放行",
                String::from_utf8_lossy(short)
            );
        }

        let mut filter = filter_with_pending_marker(marker, now);
        let held = filter.push_at(b"xx__ZEETERM_", now);
        assert!(
            held.data.is_empty(),
            "完整前缀要压住：{:?}",
            String::from_utf8_lossy(&held.data)
        );
        let released = filter.poll_at(now + HOLD_MAX_AGE + Duration::from_millis(1));
        assert_eq!(
            released.data,
            b"xx__ZEETERM_".to_vec(),
            "兜底到点要原样放行"
        );
    }

    #[test]
    fn flush_releases_the_hold_and_clears_pending() {
        let now = Instant::now();
        let mut filter = DisplayFilter::new();
        let marker = "__M__";
        let _ = filter.arm_at(marker, &payload_of(marker, "echo hi"), "echo hi", now);
        // 尾巴是标记的前缀 `__M`：会被压住。
        let _ = filter.push_at(
            format!(
                "\x07{}\r\n{marker}_S\r\nhello __M",
                wrapped_of(marker, "echo hi")
            )
            .as_bytes(),
            now,
        );
        let marker2 = "__M2__";
        let armed = filter.arm_at(marker2, &payload_of(marker2, "echo"), "echo", now);
        assert_eq!(
            armed.data,
            b"hello __M".to_vec(),
            "换命令的时候压着的尾巴要放行"
        );
        // 上一条的标记变成待处理，flush 之后清掉。
        assert_eq!(filter.pending.len(), 1);
        let flushed = filter.flush();
        assert!(flushed.data.is_empty(), "尾巴已经放行过了");
        assert!(filter.pending.is_empty(), "会话结束清掉待处理标记");
    }

    // ---- V2-U50：终端里标出 AI 命令 ----

    fn labels() -> Arc<AiMarkLabels> {
        Arc::new(AiMarkLabels {
            begin_named: "▍AI（{agent}）".to_owned(),
            begin_plain: "▍AI".to_owned(),
            success: "✓ 退出码 {code} · {time}".to_owned(),
            failure: "✗ 退出码 {code} · {time}".to_owned(),
            unknown: "结束 · {time}".to_owned(),
            timeout: "⧗ 超时，命令可能仍在运行".to_owned(),
            millis: "{n} 毫秒".to_owned(),
            seconds: "{n} 秒".to_owned(),
            minutes: "{m} 分 {s} 秒".to_owned(),
        })
    }

    /// 一行标记的原始字节（样式 + 文字 + 复位 + 回车换行）。
    fn line(style: &str, text: &str) -> String {
        format!("{style}{text}\x1b[0m\r\n")
    }

    fn begin(agent: &str) -> String {
        line(STYLE_BEGIN, &format!("▍AI（{agent}）"))
    }

    #[allow(clippy::too_many_arguments)]
    /// 按给定时刻跑一条完整的 AI 命令：`arm` 在 `t0`，开始标记行在 `t0 + start_after`，结束标记行在 `t0 + end_after`。
    fn run_marked(
        filter: &mut DisplayFilter,
        marker: &str,
        command: &str,
        output: &str,
        code: i32,
        agent: Option<&str>,
        start_after: Duration,
        end_after: Duration,
    ) -> String {
        let t0 = Instant::now();
        let wrapped = wrapped_of(marker, command);
        let mut out = filter.arm_for_at(marker, &payload_of(marker, command), command, agent, t0);
        let first = format!("\x07{wrapped}\r\n\x1b[?2004l\r\r\n");
        out.data
            .extend_from_slice(&filter.push_at(first.as_bytes(), t0).data);
        let start = format!("{marker}_S\r\n{output}");
        out.data
            .extend_from_slice(&filter.push_at(start.as_bytes(), t0 + start_after).data);
        let end = format!("{marker}_E:{code}\r\n\x1b[?2004h$ ");
        out.data
            .extend_from_slice(&filter.push_at(end.as_bytes(), t0 + end_after).data);
        String::from_utf8_lossy(&out.data).into_owned()
    }

    fn marked_filter() -> DisplayFilter {
        let mut filter = DisplayFilter::new();
        filter.set_marks(Some(labels()));
        filter
    }

    #[test]
    fn a_successful_command_gets_a_begin_line_before_the_echo_and_an_end_line_after_the_output() {
        let mut filter = marked_filter();
        let text = run_marked(
            &mut filter,
            "__M__",
            "echo hi",
            "hi\r\n",
            0,
            Some("ZeePilot"),
            Duration::from_millis(100),
            Duration::from_millis(1300),
        );
        let expected = format!(
            "{}echo hi\r\nhi\r\n{}\x1b[?2004h$ ",
            begin("ZeePilot"),
            line(STYLE_OK, "✓ 退出码 0 · 1.2 秒"),
        );
        // 回显前面的 `\r\r\n`（shell 关括号粘贴时留的空行）已经被现有逻辑吃掉；标记不改变这一点。
        assert_eq!(text.replace("\x1b[?2004l", ""), expected);
    }

    #[test]
    fn a_failed_command_ends_in_the_danger_colour_and_keeps_its_exit_code() {
        let mut filter = marked_filter();
        let text = run_marked(
            &mut filter,
            "__M__",
            "false",
            "",
            2,
            None,
            Duration::from_millis(10),
            Duration::from_millis(360),
        );
        assert!(
            text.contains(&line(STYLE_BEGIN, "▍AI")),
            "拿不到智能体名只写 AI：{text:?}"
        );
        assert!(
            text.contains(&line(STYLE_FAIL, "✗ 退出码 2 · 350 毫秒")),
            "{text:?}"
        );
        assert!(!text.contains("\x1b[32m"), "失败不能带成功的颜色：{text:?}");
    }

    #[test]
    fn an_exit_code_that_cannot_be_read_says_finished_with_the_time_only() {
        let mut filter = marked_filter();
        // 退出码数字大到装不进 i32：当成拿不到。
        let text = run_marked(
            &mut filter,
            "__M__",
            "weird",
            "x\r\n",
            0,
            Some("Codex"),
            Duration::from_millis(10),
            Duration::from_millis(2010),
        );
        assert!(text.contains("退出码 0"), "先确认正常路径");
        let mut filter = marked_filter();
        let t0 = Instant::now();
        let mut out = filter.arm_for_at(
            "__M__",
            &payload_of("__M__", "weird"),
            "weird",
            Some("Codex"),
            t0,
        );
        let first = "\x07".to_owned() + &wrapped_of("__M__", "weird") + "\r\n__M___S\r\nx\r\n";
        out.data
            .extend_from_slice(&filter.push_at(first.as_bytes(), t0).data);
        out.data.extend_from_slice(
            &filter
                .push_at(b"__M___E:99999999999999\r\n$ ", t0 + Duration::from_secs(2))
                .data,
        );
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            text.contains(&line(STYLE_WARN, "结束 · 2.0 秒")),
            "{text:?}"
        );
    }

    #[test]
    fn a_timed_out_command_gets_a_timeout_line_and_its_late_end_marker_adds_nothing() {
        let marker = "__M__";
        let now = Instant::now();
        let mut filter = marked_filter();
        let mut out = filter.arm_for_at(
            marker,
            &payload_of(marker, "sleep 30"),
            "sleep 30",
            Some("Claude Code"),
            now,
        );
        let first = format!(
            "\x07{}\r\n\x1b[?2004l\r\r\n{marker}_S\r\nworking",
            wrapped_of(marker, "sleep 30")
        );
        out.data
            .extend_from_slice(&filter.push_at(first.as_bytes(), now).data);
        let released = filter.release_at(marker, now + Duration::from_secs(5));
        out.data.extend_from_slice(&released.data);
        // 输出停在行中间（`working` 没换行）：超时标记先换行再占一行。
        let text = String::from_utf8_lossy(&out.data).into_owned();
        let expected = format!(
            "{}sleep 30\r\nworking\r\n{}",
            begin("Claude Code"),
            line(STYLE_WARN, "⧗ 超时，命令可能仍在运行")
        );
        assert_eq!(text.replace("\x1b[?2004l", ""), expected);
        // 命令后来自己跑完：迟到的结束标记行照旧悄悄丢掉，不再补结尾标记。
        let late = filter.push_at(
            format!(" done\r\n{marker}_E:0\r\n$ ").as_bytes(),
            now + Duration::from_secs(40),
        );
        let late = String::from_utf8_lossy(&late.data).into_owned();
        assert_eq!(late, " done\r\n$ ");
    }

    #[test]
    fn a_release_for_a_command_that_never_began_adds_no_mark() {
        // 回显还没出现就超时了：没有开头标记，也就不能有孤零零的超时标记。
        let marker = "__M__";
        let now = Instant::now();
        let mut filter = marked_filter();
        let _ = filter.arm_for_at(marker, &payload_of(marker, "ls"), "ls", None, now);
        let released = filter.release_at(marker, now);
        assert!(released.data.is_empty(), "{:?}", released.data);
    }

    #[test]
    fn back_to_back_commands_each_get_their_own_paired_marks_and_nothing_leaks_colour() {
        let mut filter = marked_filter();
        let a = run_marked(
            &mut filter,
            "__A__",
            "echo 1",
            "1\r\n",
            0,
            Some("ZeePilot"),
            Duration::from_millis(5),
            Duration::from_millis(50),
        );
        let b = run_marked(
            &mut filter,
            "__B__",
            "false",
            "",
            1,
            Some("ZeePilot"),
            Duration::from_millis(5),
            Duration::from_millis(80),
        );
        for text in [&a, &b] {
            // 开头 / 结尾各一行，每一行都以复位结束（复位个数 = 标记行个数）。
            assert_eq!(text.matches("\x1b[0m").count(), 2, "{text:?}");
            assert_eq!(text.matches(STYLE_BEGIN).count(), 1);
        }
        assert!(a.contains(&line(STYLE_OK, "✓ 退出码 0 · 45 毫秒")));
        assert!(b.contains(&line(STYLE_FAIL, "✗ 退出码 1 · 75 毫秒")));
    }

    #[test]
    fn a_timeout_followed_by_a_new_command_keeps_the_new_commands_marks_in_order() {
        let now = Instant::now();
        let mut filter = marked_filter();
        let mut text = String::new();
        let mut out = filter.arm_for_at(
            "__A__",
            &payload_of("__A__", "sleep 30"),
            "sleep 30",
            None,
            now,
        );
        out.data.extend_from_slice(
            &filter
                .push_at(
                    format!(
                        "\x07{}\r\n\x1b[?2004l\r\r\n__A___S\r\n",
                        wrapped_of("__A__", "sleep 30")
                    )
                    .as_bytes(),
                    now,
                )
                .data,
        );
        out.data
            .extend_from_slice(&filter.release_at("__A__", now).data);
        text.push_str(&String::from_utf8_lossy(&out.data));
        // 第一条还在跑时又来一条新的：新命令的标记照常成对。
        let second = run_marked(
            &mut filter,
            "__B__",
            "echo 2",
            "2\r\n",
            0,
            None,
            Duration::from_millis(5),
            Duration::from_millis(15),
        );
        text.push_str(&second);
        let begins = text.matches(STYLE_BEGIN).count();
        let timeouts = text.matches("⧗").count();
        let oks = text.matches("✓").count();
        assert_eq!((begins, timeouts, oks), (2, 1, 1), "{text:?}");
        let timeout_at = text.find("⧗").expect("timeout");
        let second_begin = text.rfind(STYLE_BEGIN).expect("begin");
        assert!(
            timeout_at < second_begin,
            "顺序：先超时、再开始新命令：{text:?}"
        );
    }

    #[test]
    fn an_inline_end_marker_puts_the_end_line_on_its_own_row() {
        // 输出没有换行（`printf abc`）：结束标记跟在 `abc` 后面，结尾标记不能接在 `abc` 同一行。
        let mut filter = marked_filter();
        let t0 = Instant::now();
        let marker = "__M__";
        let mut out = filter.arm_for_at(
            marker,
            &payload_of(marker, "printf abc"),
            "printf abc",
            None,
            t0,
        );
        let stream = format!(
            "\x07{}\r\n\x1b[?2004l\r\r\n{marker}_S\r\nabc{marker}_E:0\r\n$ ",
            wrapped_of(marker, "printf abc")
        );
        out.data
            .extend_from_slice(&filter.push_at(stream.as_bytes(), t0).data);
        let text = String::from_utf8_lossy(&out.data).into_owned();
        assert!(
            text.contains(&format!("abc\r\n{}", line(STYLE_OK, "✓ 退出码 0 · 0 毫秒"))),
            "{text:?}"
        );
    }

    #[test]
    fn marks_are_independent_of_how_the_stream_is_split() {
        let marker = "__M__";
        let stream = command_stream(marker, "echo hi", "hi\r\n", 0);
        let reference = {
            let mut filter = marked_filter();
            let t0 = Instant::now();
            let mut out = filter.arm_for_at(
                marker,
                &payload_of(marker, "echo hi"),
                "echo hi",
                Some("Codex"),
                t0,
            );
            out.data
                .extend_from_slice(&filter.push_at(&stream, t0).data);
            out.data
        };
        for cut in 1..stream.len() {
            let mut filter = marked_filter();
            let t0 = Instant::now();
            let mut out = filter.arm_for_at(
                marker,
                &payload_of(marker, "echo hi"),
                "echo hi",
                Some("Codex"),
                t0,
            );
            for piece in [&stream[..cut], &stream[cut..]] {
                out.data.extend_from_slice(&filter.push_at(piece, t0).data);
            }
            assert_eq!(
                out.data, reference,
                "在第 {cut} 字节切开之后标记和显示都要一样"
            );
        }
    }

    #[test]
    fn with_the_marks_off_the_display_is_byte_for_byte_what_it_was() {
        // 开关关着（没配文案）：和以前逐字节相同——对一批形状各异的命令流，
        // 开着标记的输出把标记行去掉之后必须正好等于关着的输出。
        let cases: [(&str, &str, &str, i32); 5] = [
            ("echo hi", "hi\r\n", "hi", 0),
            ("false", "", "", 1),
            ("ls", "\x1b[34mblue\x1b[0m\r\nplain\r\n", "ls", 0),
            ("cat big", "line\r\n\r\n\r\nend\r\n", "cat", 3),
            ("echo 中文", "中文\r\n", "echo", 0),
        ];
        for (command, output, _, code) in cases {
            let marker = "__M__";
            let stream = command_stream(marker, command, output, code);
            let off = {
                let mut filter = DisplayFilter::new();
                let mut out = filter.arm(marker, &payload_of(marker, command), command);
                let pushed = filter.push(&stream);
                out.data.extend_from_slice(&pushed.data);
                (out.data, out.suppressed + pushed.suppressed)
            };
            let on = {
                let mut filter = marked_filter();
                let t0 = Instant::now();
                let mut out = filter.arm_for_at(
                    marker,
                    &payload_of(marker, command),
                    command,
                    Some("ZeePilot"),
                    t0,
                );
                let pushed = filter.push_at(&stream, t0);
                out.data.extend_from_slice(&pushed.data);
                (out.data, out.suppressed + pushed.suppressed)
            };
            assert_eq!(on.1, off.1, "被吞掉的字节数不受标记影响：{command}");
            let mut stripped = String::from_utf8_lossy(&on.0).into_owned();
            for style in [STYLE_BEGIN, STYLE_OK, STYLE_FAIL, STYLE_WARN] {
                while let Some(start) = stripped.find(style) {
                    let end = stripped[start..]
                        .find("\x1b[0m\r\n")
                        .map(|at| start + at + "\x1b[0m\r\n".len())
                        .expect("每行标记都有复位 + 换行");
                    stripped.replace_range(start..end, "");
                }
            }
            assert_eq!(
                stripped,
                String::from_utf8_lossy(&off.0),
                "去掉标记行之后要和关着时一样：{command}"
            );
        }
    }

    #[test]
    fn the_ai_output_is_read_from_the_raw_stream_so_marks_never_reach_it() {
        // AI 拿到的输出来自原始缓冲（`extract_output`），根本不经过显示过滤器：
        // 同一条原始流，不管标记开不开，AI 读到的都一样、也不含标记文字。
        let marker = "__M__";
        let raw = String::from_utf8(command_stream(marker, "echo hi", "hi\r\n", 0)).expect("utf8");
        let ai_view = crate::ai::pty::extract_output(&raw, marker);
        assert_eq!(ai_view, "hi");
        let mut filter = marked_filter();
        let _ = run_marked(
            &mut filter,
            marker,
            "echo hi",
            "hi\r\n",
            0,
            Some("ZeePilot"),
            Duration::ZERO,
            Duration::ZERO,
        );
        assert_eq!(crate::ai::pty::extract_output(&raw, marker), ai_view);
        assert!(!ai_view.contains('▍') && !ai_view.contains('✓'));
    }

    #[test]
    fn the_marker_text_filters_out_control_characters_from_the_agent_name() {
        let mut filter = marked_filter();
        let text = run_marked(
            &mut filter,
            "__M__",
            "echo hi",
            "hi\r\n",
            0,
            Some("Evil\x1b[31m\x07Name"),
            Duration::ZERO,
            Duration::ZERO,
        );
        assert!(text.contains(&begin("Evil[31mName")), "{text:?}");
        assert!(!text.contains("Evil\x1b"), "{text:?}");
    }

    #[test]
    fn elapsed_times_are_formatted_in_three_tiers() {
        let labels = labels();
        let f = |ms| format_elapsed(ms, &labels);
        assert_eq!(f(0), "0 毫秒");
        assert_eq!(f(350), "350 毫秒");
        assert_eq!(f(999), "999 毫秒");
        assert_eq!(f(1000), "1.0 秒");
        assert_eq!(f(1234), "1.2 秒");
        assert_eq!(f(1250), "1.3 秒");
        assert_eq!(f(59_940), "59.9 秒");
        // 四舍五入后够 60 秒就换成「分 秒」，不显示 60.0 秒。
        assert_eq!(f(59_960), "1 分 0 秒");
        assert_eq!(f(60_000), "1 分 0 秒");
        assert_eq!(f(125_000), "2 分 5 秒");
        assert_eq!(f(3_725_400), "62 分 5 秒");
        let total = u64::MAX / 1000;
        assert_eq!(f(u64::MAX), format!("{} 分 {} 秒", total / 60, total % 60));
    }
}
