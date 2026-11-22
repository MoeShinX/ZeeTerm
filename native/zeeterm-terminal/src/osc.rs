//! 对原始输出字节做一个有界的 OSC 扫描，只认 shell 集成会发的三类序列：
//! OSC 133（提示符 / 命令边界）、OSC 633（VS Code 的同类扩展，多一个命令行）、OSC 7（当前目录）。
//!
//! Alacritty 的解析器不处理它们，会直接丢掉，所以要在字节交给它之前自己看一遍。这里只看不改：
//! 调用方照常把全部字节喂给 Alacritty，扫描器只告诉它"某条序列在这批字节的哪个位置结束"，
//! 好让终端模型在那个位置把光标坐标记下来（Prompt 输入起点 = 收到 `133;B` 时的光标位置）。
//!
//! 约束：
//! - 半截序列可以跨批续上；
//! - 单条序列的内容超过 [`MAX_OSC_PAYLOAD`] 就整条丢弃并回到初始状态，不 panic、不无限攒内存；
//! - 结尾认 BEL 和 ST（`ESC \`）两种；CAN / SUB 按 VT 惯例中止序列；
//! - 内容损坏（不是 UTF-8、百分号编码乱了、不是 `file:` URL……）只丢这一条，不影响后面的。
//!
//! 远端发来的这些序列只用来辅助判断输入状态，不是授权，也不是服务器可信的证据。

/// 单条 OSC 内容（不含 `ESC ]` 和结尾）的字节上限。命令行（633;E）可能带转义，比输入跟踪的
/// 4096 字节上限留出余量；超过说明不是正常 shell 集成。
pub const MAX_OSC_PAYLOAD: usize = 8192;

/// OSC 7 里路径的字节上限（解码后）。
pub const MAX_CWD_BYTES: usize = 4096;

/// 序列来自哪一族。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkFamily {
    /// OSC 133（FinalTerm / iTerm2 / kitty 等通用的语义提示符）。
    Osc133,
    /// OSC 633（VS Code 的扩展，带命令行）。
    Osc633,
    /// OSC 7（当前目录）。
    Osc7,
}

/// 一条识别出来的 shell 标记。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShellMarkKind {
    /// `A`：提示符开始绘制。
    PromptStart,
    /// `B`：提示符画完、用户输入从这里开始。
    PromptEnd,
    /// `C`：用户提交的命令开始执行。
    CommandStart,
    /// `D[;退出码]`：命令结束。
    CommandEnd { exit_code: Option<i32> },
    /// 633 的 `E;命令行`：shell 报告的完整命令行（已按 633 的转义还原）。
    CommandLine(String),
    /// OSC 7：当前目录。`host` 为空表示 `file:///path` 形式。
    Cwd { host: Option<String>, path: String },
}

/// 扫描器的一个产出：哪一族、什么标记。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OscMark {
    pub family: MarkFamily,
    pub kind: ShellMarkKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// 不在任何序列里。
    Ground,
    /// 刚看到 `ESC`。
    Esc,
    /// 在 OSC 内容里，正在收集。
    Collect,
    /// 在 OSC 内容里，但这条不关心或已超长：只找结尾，不再攒内容。
    Skip,
    /// OSC 内容里刚看到 `ESC`，等 `\` 才是 ST。
    CollectEsc,
    SkipEsc,
}

/// 跨批续上的 OSC 扫描器。
#[derive(Debug)]
pub struct OscScanner {
    phase: Phase,
    payload: Vec<u8>,
    /// 因为超长或损坏而丢掉的序列数（诊断用）。
    dropped: u64,
}

impl Default for OscScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl OscScanner {
    pub fn new() -> Self {
        Self {
            phase: Phase::Ground,
            payload: Vec::new(),
            dropped: 0,
        }
    }

    /// 丢掉进行到一半的序列，回到初始状态（换会话时用）。
    pub fn reset(&mut self) {
        self.phase = Phase::Ground;
        self.payload.clear();
    }

    /// 累计丢弃的超长 / 损坏序列数。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 扫描一批字节。每识别出一条标记，就把"这条序列的结尾在 `bytes` 里的下标（含结尾字节之后）"
    /// 和标记放进 `out`。下标单调不减，调用方可以据此分段。
    pub fn scan(&mut self, bytes: &[u8], out: &mut Vec<(usize, OscMark)>) {
        for (index, &byte) in bytes.iter().enumerate() {
            match self.phase {
                Phase::Ground => {
                    if byte == 0x1b {
                        self.phase = Phase::Esc;
                    }
                }
                Phase::Esc => match byte {
                    b']' => self.begin(),
                    // ESC ESC：还在等下一个字节。
                    0x1b => {}
                    _ => self.phase = Phase::Ground,
                },
                Phase::Collect | Phase::Skip => match byte {
                    0x07 => self.finish(index + 1, out),
                    0x1b => {
                        self.phase = if self.phase == Phase::Collect {
                            Phase::CollectEsc
                        } else {
                            Phase::SkipEsc
                        };
                    }
                    // CAN / SUB：VT 惯例，中止当前序列。
                    0x18 | 0x1a => self.abort(),
                    _ => self.push(byte),
                },
                Phase::CollectEsc | Phase::SkipEsc => match byte {
                    b'\\' => self.finish(index + 1, out),
                    // ESC ] 紧接着：旧序列作废，新序列开始。
                    b']' => {
                        self.abort();
                        self.begin();
                    }
                    0x1b => {}
                    // ESC 后面是别的：旧序列被打断（不是合法结尾），这个 ESC 序列另算。
                    _ => self.abort(),
                },
            }
        }
    }

    fn begin(&mut self) {
        self.payload.clear();
        self.phase = Phase::Collect;
    }

    fn abort(&mut self) {
        self.payload.clear();
        self.phase = Phase::Ground;
    }

    fn push(&mut self, byte: u8) {
        if self.phase == Phase::Skip {
            return;
        }
        if self.payload.len() >= MAX_OSC_PAYLOAD {
            // 超长：整条丢弃，改成只找结尾。
            self.payload.clear();
            self.dropped += 1;
            self.phase = Phase::Skip;
            return;
        }
        self.payload.push(byte);
        // 编号一读出来就能判断关不关心；不关心的立刻转 Skip，不白攒内容。
        if byte == b';' && !wanted_prefix(&self.payload) {
            self.payload.clear();
            self.phase = Phase::Skip;
        }
    }

    fn finish(&mut self, end: usize, out: &mut Vec<(usize, OscMark)>) {
        let was_collecting = matches!(self.phase, Phase::Collect | Phase::CollectEsc);
        self.phase = Phase::Ground;
        if !was_collecting {
            self.payload.clear();
            return;
        }
        match parse_payload(&self.payload) {
            Some(mark) => out.push((end, mark)),
            None => {
                // 编号对但内容坏了（或者是 `133;` 之外的子命令）：不产出事件。只有前者算"损坏"。
                if wanted_prefix(&self.payload) {
                    self.dropped += 1;
                }
            }
        }
        self.payload.clear();
    }
}

/// 内容是否以 `133;` / `633;` / `7;` 开头（含分号）。
fn wanted_prefix(payload: &[u8]) -> bool {
    payload.starts_with(b"133;") || payload.starts_with(b"633;") || payload.starts_with(b"7;")
}

fn parse_payload(payload: &[u8]) -> Option<OscMark> {
    if let Some(rest) = payload.strip_prefix(b"133;") {
        return parse_semantic(rest, MarkFamily::Osc133);
    }
    if let Some(rest) = payload.strip_prefix(b"633;") {
        return parse_semantic(rest, MarkFamily::Osc633);
    }
    if let Some(rest) = payload.strip_prefix(b"7;") {
        return parse_cwd(rest);
    }
    None
}

/// `133;` / `633;` 后面的部分：第一个字符是子命令，其余是以分号分隔的参数。
fn parse_semantic(rest: &[u8], family: MarkFamily) -> Option<OscMark> {
    let (&command, params) = rest.split_first()?;
    // 子命令后面要么结束，要么紧跟 `;`（`133;Aextra` 不是合法形式）。
    if !(params.is_empty() || params[0] == b';') {
        return None;
    }
    let kind = match command {
        b'A' => ShellMarkKind::PromptStart,
        b'B' => ShellMarkKind::PromptEnd,
        b'C' => ShellMarkKind::CommandStart,
        b'D' => {
            let code = params
                .strip_prefix(b";")
                .and_then(|p| p.split(|b| *b == b';').next())
                .and_then(|text| std::str::from_utf8(text).ok())
                .and_then(|text| text.trim().parse::<i32>().ok());
            ShellMarkKind::CommandEnd { exit_code: code }
        }
        // 只有 633 有命令行。133 里没有这个子命令，不产出事件。
        b'E' if family == MarkFamily::Osc633 => {
            let line = params.strip_prefix(b";")?;
            let line = line.split(|b| *b == b';').next().unwrap_or(&[]);
            ShellMarkKind::CommandLine(unescape_633(line)?)
        }
        _ => return None,
    };
    Some(OscMark { family, kind })
}

/// 633;E 的转义：`\\` 是反斜杠，`\xAB` 是一个字节（分号写成 `\x3b`）。
/// 还原后必须是合法 UTF-8，否则整条丢。
fn unescape_633(raw: &[u8]) -> Option<String> {
    let mut out = Vec::with_capacity(raw.len());
    let mut index = 0;
    while index < raw.len() {
        let byte = raw[index];
        if byte != b'\\' {
            out.push(byte);
            index += 1;
            continue;
        }
        match raw.get(index + 1) {
            Some(b'\\') => {
                out.push(b'\\');
                index += 2;
            }
            Some(b'x') => {
                let high = hex_value(*raw.get(index + 2)?)?;
                let low = hex_value(*raw.get(index + 3)?)?;
                out.push(high << 4 | low);
                index += 4;
            }
            _ => return None,
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// OSC 7 的内容：`file://主机/路径`。只接受 `file` 方案；路径百分号解码后必须是 UTF-8、
/// 绝对路径、不含控制字符。
fn parse_cwd(rest: &[u8]) -> Option<OscMark> {
    let text = std::str::from_utf8(rest).ok()?;
    let (scheme, after) = text.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("file") {
        return None;
    }
    let slash = after.find('/')?;
    let (host, raw_path) = after.split_at(slash);
    let path = percent_decode(raw_path)?;
    if path.len() > MAX_CWD_BYTES || path.chars().any(char::is_control) {
        return None;
    }
    let host = percent_decode(host)?;
    if host.chars().any(char::is_control) {
        return None;
    }
    Some(OscMark {
        family: MarkFamily::Osc7,
        kind: ShellMarkKind::Cwd {
            host: (!host.is_empty()).then_some(host),
            path,
        },
    })
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            out.push(high << 4 | low);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// 交给底层解析器的单条 OSC 内容的字节上限。
///
/// 底层解析器（vte，std 特性下）的 OSC 缓冲是不设上限的 `Vec`：远端只要发 `ESC ] 0 ;` 然后一直不
/// 结束，内存就跟着输入无限涨。正常的 OSC（标题、超链接、通知、shell 集成）都是几十到几千字节，
/// 解析器真正解释的也只有这个量级，1 MiB 已经远超任何合法用途，只用来给内存封顶。
pub const MAX_OSC_STREAM_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GuardPhase {
    Ground,
    /// 刚看到 `ESC`。
    Esc,
    /// 在 OSC 内容里。
    Osc,
    /// OSC 内容里刚看到 `ESC`。
    OscEsc,
    /// 这条 OSC 已经超限并让解析器复位，后面的内容一律丢掉，直到它自己结束。
    Dropping,
    /// 丢弃中又看到 `ESC`：下一个字节决定是 ST（一并丢）还是别的序列（补回 ESC）。
    DroppingEsc,
}

/// OSC 长度护栏：放在 [`OscScanner`] 和解析器之前，给"不终止的 OSC"封顶。
///
/// 镜像解析器对 OSC 边界的判断（`ESC ]` 开始；BEL、`ESC \`、CAN / SUB 结束；`ESC` 加别的字符打断）。
/// 一条 OSC 超过 [`MAX_OSC_STREAM_BYTES`] 时，在超限的位置插一个 CAN（0x18）让解析器结束并复位这条序列，
/// 之后这条 OSC 剩下的内容直接丢掉、不再进解析器——否则解析器回到初始状态后，剩下的垃圾会当成普通文字打到屏幕上。
/// 注意解析器遇到 CAN 仍会把已攒的内容当一条 OSC 派发一次，所以派发出去的只是长度不超过上限的前缀。
#[derive(Debug)]
pub struct OscGuard {
    phase: GuardPhase,
    len: usize,
    limit: usize,
    overflowed: u64,
}

impl Default for OscGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl OscGuard {
    pub fn new() -> Self {
        Self::with_limit(MAX_OSC_STREAM_BYTES)
    }

    /// 测试用：换一个小上限，省得灌 MiB 级数据。
    pub fn with_limit(limit: usize) -> Self {
        Self {
            phase: GuardPhase::Ground,
            len: 0,
            limit,
            overflowed: 0,
        }
    }

    /// 丢掉进行到一半的状态（换会话时用，和解析器一起复位）。
    pub fn reset(&mut self) {
        self.phase = GuardPhase::Ground;
        self.len = 0;
    }

    /// 累计因超限被截断的 OSC 条数（诊断用）。
    pub fn overflowed(&self) -> u64 {
        self.overflowed
    }

    /// 过一遍一批字节。没有超限时原样借回（零拷贝，常见情况）；超限时返回改写后的字节。
    pub fn filter<'a>(&mut self, bytes: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        let mut out: Vec<u8> = Vec::new();
        let mut diverged = false;
        // 第一次需要改写时，先把前面原样放行的部分补进去。
        macro_rules! diverge {
            ($index:expr) => {
                if !diverged {
                    out.extend_from_slice(&bytes[..$index]);
                    diverged = true;
                }
            };
        }
        for (index, &byte) in bytes.iter().enumerate() {
            // `keep`：这个字节原样放行；其余情况在 match 里自己处理输出。
            let mut keep = true;
            match self.phase {
                GuardPhase::Ground => {
                    if byte == 0x1b {
                        self.phase = GuardPhase::Esc;
                    }
                }
                GuardPhase::Esc => match byte {
                    b']' => {
                        self.phase = GuardPhase::Osc;
                        self.len = 0;
                    }
                    0x1b => {}
                    _ => self.phase = GuardPhase::Ground,
                },
                GuardPhase::Osc => match byte {
                    0x07 | 0x18 | 0x1a => self.phase = GuardPhase::Ground,
                    0x1b => self.phase = GuardPhase::OscEsc,
                    _ => {
                        self.len += 1;
                        if self.len > self.limit {
                            diverge!(index);
                            out.push(0x18);
                            self.overflowed += 1;
                            self.phase = GuardPhase::Dropping;
                            self.len = 0;
                            keep = false;
                        }
                    }
                },
                GuardPhase::OscEsc => match byte {
                    b']' => {
                        self.phase = GuardPhase::Osc;
                        self.len = 0;
                    }
                    0x1b => {}
                    _ => self.phase = GuardPhase::Ground,
                },
                GuardPhase::Dropping => {
                    keep = false;
                    match byte {
                        0x07 | 0x18 | 0x1a => self.phase = GuardPhase::Ground,
                        0x1b => self.phase = GuardPhase::DroppingEsc,
                        _ => {}
                    }
                }
                GuardPhase::DroppingEsc => match byte {
                    // ST：这条被丢弃的 OSC 到此结束，ESC 和 `\` 一起丢。
                    b'\\' => {
                        keep = false;
                        self.phase = GuardPhase::Ground;
                    }
                    // 新的 OSC 开始：之前吞掉的 ESC 要补回来。
                    b']' => {
                        diverge!(index);
                        out.push(0x1b);
                        self.phase = GuardPhase::Osc;
                        self.len = 0;
                    }
                    0x1b => keep = false,
                    // 别的序列：丢弃的 OSC 被打断，补回 ESC 让这个新序列照常解析。
                    _ => {
                        diverge!(index);
                        out.push(0x1b);
                        self.phase = GuardPhase::Ground;
                    }
                },
            }
            if diverged && keep {
                out.push(byte);
            } else if !diverged && !keep {
                diverge!(index);
            }
        }
        if diverged {
            std::borrow::Cow::Owned(out)
        } else {
            std::borrow::Cow::Borrowed(bytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(chunks: &[&[u8]]) -> Vec<ShellMarkKind> {
        let mut scanner = OscScanner::new();
        let mut kinds = Vec::new();
        for chunk in chunks {
            let mut out = Vec::new();
            scanner.scan(chunk, &mut out);
            kinds.extend(out.into_iter().map(|(_, mark)| mark.kind));
        }
        kinds
    }

    fn one(bytes: &[u8]) -> Option<ShellMarkKind> {
        scan_all(&[bytes]).into_iter().next()
    }

    #[test]
    fn osc_133_abcd_with_bel_and_st_endings() {
        assert_eq!(one(b"\x1b]133;A\x07"), Some(ShellMarkKind::PromptStart));
        assert_eq!(one(b"\x1b]133;B\x1b\\"), Some(ShellMarkKind::PromptEnd));
        assert_eq!(one(b"\x1b]133;C\x07"), Some(ShellMarkKind::CommandStart));
        assert_eq!(
            one(b"\x1b]133;D;0\x07"),
            Some(ShellMarkKind::CommandEnd { exit_code: Some(0) })
        );
        assert_eq!(
            one(b"\x1b]133;D;127\x1b\\"),
            Some(ShellMarkKind::CommandEnd {
                exit_code: Some(127)
            })
        );
        assert_eq!(
            one(b"\x1b]133;D\x07"),
            Some(ShellMarkKind::CommandEnd { exit_code: None })
        );
        assert_eq!(
            one(b"\x1b]133;D;abc\x07"),
            Some(ShellMarkKind::CommandEnd { exit_code: None }),
            "退出码读不懂只是没有退出码，标记本身仍然成立"
        );
    }

    #[test]
    fn osc_133_extra_options_are_ignored() {
        assert_eq!(
            one(b"\x1b]133;A;cl=m;aid=7\x07"),
            Some(ShellMarkKind::PromptStart)
        );
        assert_eq!(one(b"\x1b]133;Bextra\x07"), None, "子命令后面不是分号不算");
    }

    #[test]
    fn osc_633_has_the_same_commands_plus_the_command_line() {
        assert_eq!(one(b"\x1b]633;A\x07"), Some(ShellMarkKind::PromptStart));
        assert_eq!(one(b"\x1b]633;B\x07"), Some(ShellMarkKind::PromptEnd));
        assert_eq!(one(b"\x1b]633;C\x07"), Some(ShellMarkKind::CommandStart));
        assert_eq!(
            one(b"\x1b]633;D;2\x07"),
            Some(ShellMarkKind::CommandEnd { exit_code: Some(2) })
        );
        assert_eq!(
            one(b"\x1b]633;E;ls -la\x07"),
            Some(ShellMarkKind::CommandLine("ls -la".into()))
        );
        // 133 没有 E 子命令。
        assert_eq!(one(b"\x1b]133;E;ls\x07"), None);
    }

    #[test]
    fn osc_633_command_line_is_unescaped_and_nonce_is_dropped() {
        assert_eq!(
            one(b"\x1b]633;E;echo a\\x3bb\\\\c;nonce123\x07"),
            Some(ShellMarkKind::CommandLine("echo a;b\\c".into()))
        );
        // 转义不完整、不是 UTF-8：整条丢。
        assert_eq!(one(b"\x1b]633;E;bad\\x3\x07"), None);
        assert_eq!(one(b"\x1b]633;E;bad\\q\x07"), None);
        assert_eq!(one(b"\x1b]633;E;\\xff\\xfe\x07"), None);
        assert_eq!(
            one("\x1b]633;E;echo 你好\x07".as_bytes()),
            Some(ShellMarkKind::CommandLine("echo 你好".into()))
        );
    }

    #[test]
    fn osc_7_accepts_only_file_urls_and_decodes_percent_escapes() {
        assert_eq!(
            one(b"\x1b]7;file://web-1/home/user/my%20dir\x07"),
            Some(ShellMarkKind::Cwd {
                host: Some("web-1".into()),
                path: "/home/user/my dir".into()
            })
        );
        assert_eq!(
            one(b"\x1b]7;file:///tmp\x1b\\"),
            Some(ShellMarkKind::Cwd {
                host: None,
                path: "/tmp".into()
            })
        );
        assert_eq!(
            one(b"\x1b]7;FILE://h/%E4%BD%A0%E5%A5%BD\x07"),
            Some(ShellMarkKind::Cwd {
                host: Some("h".into()),
                path: "/你好".into()
            })
        );
        assert_eq!(one(b"\x1b]7;http://h/tmp\x07"), None, "不是 file URL");
        assert_eq!(one(b"\x1b]7;/tmp\x07"), None, "不是 URL");
        assert_eq!(one(b"\x1b]7;file://h\x07"), None, "没有路径");
        assert_eq!(one(b"\x1b]7;file://h/a%zz\x07"), None, "百分号编码坏了");
        assert_eq!(one(b"\x1b]7;file://h/a%0Ab\x07"), None, "解出控制字符");
        assert_eq!(one(b"\x1b]7;file://h/a%ff\x07"), None, "解出来不是 UTF-8");
    }

    #[test]
    fn sequences_split_across_chunks_continue() {
        let whole: &[u8] = b"\x1b]133;D;5\x07";
        for cut in 1..whole.len() {
            let kinds = scan_all(&[&whole[..cut], &whole[cut..]]);
            assert_eq!(
                kinds,
                vec![ShellMarkKind::CommandEnd { exit_code: Some(5) }],
                "在 {cut} 处切开"
            );
        }
        let st: &[u8] = b"\x1b]133;B\x1b\\";
        for cut in 1..st.len() {
            assert_eq!(
                scan_all(&[&st[..cut], &st[cut..]]),
                vec![ShellMarkKind::PromptEnd],
                "ST 结尾在 {cut} 处切开"
            );
        }
        // 一个字节一个字节喂。
        let chunks: Vec<&[u8]> = whole.chunks(1).collect();
        assert_eq!(
            scan_all(&chunks),
            vec![ShellMarkKind::CommandEnd { exit_code: Some(5) }]
        );
    }

    #[test]
    fn marks_report_where_their_terminator_ends_inside_the_chunk() {
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        let data = b"abc\x1b]133;A\x07$ \x1b]133;B\x07xyz";
        scanner.scan(data, &mut out);
        let ends: Vec<usize> = out.iter().map(|(end, _)| *end).collect();
        assert_eq!(ends, vec![11, 21]);
        assert_eq!(&data[..ends[0]], b"abc\x1b]133;A\x07");
        assert_eq!(&data[ends[0]..ends[1]], b"$ \x1b]133;B\x07");
    }

    #[test]
    fn marks_mixed_with_ansi_and_text_are_found_in_order() {
        let kinds = scan_all(&[
            b"\x1b[32mok\x1b[0m\x1b]133;D;0\x07\x1b]7;file:///a\x07\x1b]133;A\x07\x1b[1m$ \x1b[0m\x1b]133;B\x07",
        ]);
        assert_eq!(kinds.len(), 4);
        assert_eq!(kinds[0], ShellMarkKind::CommandEnd { exit_code: Some(0) });
        assert!(matches!(kinds[1], ShellMarkKind::Cwd { .. }));
        assert_eq!(kinds[2], ShellMarkKind::PromptStart);
        assert_eq!(kinds[3], ShellMarkKind::PromptEnd);
    }

    #[test]
    fn other_osc_sequences_are_ignored_but_do_not_break_the_stream() {
        // 标题、超链接、颜色查询：不产出事件，后面的 133 照常识别。
        let kinds = scan_all(&[
            b"\x1b]0;title\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\x1b]11;?\x07\x1b]133;A\x07",
        ]);
        assert_eq!(kinds, vec![ShellMarkKind::PromptStart]);
    }

    #[test]
    fn an_over_long_sequence_is_dropped_and_the_scanner_recovers() {
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        let mut data = b"\x1b]633;E;".to_vec();
        data.extend(std::iter::repeat_n(b'a', MAX_OSC_PAYLOAD + 100));
        data.push(0x07);
        data.extend_from_slice(b"\x1b]133;A\x07");
        scanner.scan(&data, &mut out);
        assert_eq!(out.len(), 1, "超长那条丢掉，后面的识别出来：{out:?}");
        assert_eq!(out[0].1.kind, ShellMarkKind::PromptStart);
        assert_eq!(scanner.dropped(), 1);
        assert!(
            scanner.payload.capacity() <= MAX_OSC_PAYLOAD * 4,
            "不能无限攒内存"
        );
    }

    #[test]
    fn a_sequence_exactly_at_the_limit_is_still_accepted() {
        let mut data = b"\x1b]633;E;".to_vec();
        let used = data.len() - 2;
        data.extend(std::iter::repeat_n(b'a', MAX_OSC_PAYLOAD - used));
        data.push(0x07);
        match one(&data) {
            Some(ShellMarkKind::CommandLine(line)) => {
                assert_eq!(line.len(), MAX_OSC_PAYLOAD - used)
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_unterminated_sequence_then_a_new_one_does_not_leak_into_it() {
        // 第一条没有结尾就被 CAN 打断；第二条完整。
        let kinds = scan_all(&[b"\x1b]133;D;9\x18\x1b]133;A\x07"]);
        assert_eq!(kinds, vec![ShellMarkKind::PromptStart]);
        // ESC ] 直接打断旧序列，开新的。
        let kinds = scan_all(&[b"\x1b]133;D;9\x1b]133;B\x07"]);
        assert_eq!(kinds, vec![ShellMarkKind::PromptEnd]);
        // ESC 后面是别的字符：旧序列作废，不会把后面的 BEL 当成它的结尾。
        let kinds = scan_all(&[b"\x1b]133;D;9\x1bXjunk\x07\x1b]133;C\x07"]);
        assert_eq!(kinds, vec![ShellMarkKind::CommandStart]);
    }

    #[test]
    fn reset_drops_a_half_finished_sequence() {
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        scanner.scan(b"\x1b]133;D;", &mut out);
        scanner.reset();
        scanner.scan(b"7\x07", &mut out);
        assert!(out.is_empty(), "复位后残余的 `7\\x07` 不是序列");
    }

    /// 灌一条永不结束的 OSC：放行给解析器的字节必须有上限，不随输入增长。
    #[test]
    fn guard_caps_an_unterminated_osc() {
        let mut guard = OscGuard::with_limit(1024);
        let mut passed = guard.filter(b"\x1b]0;").len();
        let chunk = vec![b'a'; 4096];
        for _ in 0..1000 {
            passed += guard.filter(&chunk).len();
        }
        // 约 4 MiB 输入，放行的只有 "ESC ] 0 ;" + 上限 + 一个 CAN。
        assert!(passed <= 4 + 1024 + 1, "放行了 {passed} 字节");
        assert_eq!(guard.overflowed(), 1);
    }

    /// 超限后注入的是 CAN，且超限那条 OSC 剩下的内容、BEL 结尾都被丢掉；后面的普通文字照常放行。
    #[test]
    fn guard_resets_the_parser_then_drops_the_rest_of_the_osc() {
        let mut guard = OscGuard::with_limit(4);
        let out = guard.filter(b"\x1b]0;abcdefghij\x07after");
        assert_eq!(&out[..], b"\x1b]0;ab\x18after");
    }

    /// 超限的 OSC 以 ST（ESC \）结尾，ESC 和 `\` 一起丢；以别的 ESC 序列打断，则补回 ESC。
    #[test]
    fn guard_handles_every_way_the_dropped_osc_can_end() {
        let mut guard = OscGuard::with_limit(3);
        assert_eq!(
            &guard.filter(b"\x1b]0;abcdef\x1b\\ok")[..],
            b"\x1b]0;a\x18ok"
        );
        let mut guard = OscGuard::with_limit(3);
        assert_eq!(
            &guard.filter(b"\x1b]0;abcdef\x1b[1mok")[..],
            b"\x1b]0;a\x18\x1b[1mok"
        );
        // 紧跟着一条新的 OSC：ESC ] 要完整补回，新的 OSC 重新计数。
        let mut guard = OscGuard::with_limit(3);
        assert_eq!(
            &guard.filter(b"\x1b]0;abcdef\x1b]0;x\x07")[..],
            b"\x1b]0;a\x18\x1b]0;x\x07"
        );
        // CAN 结尾。
        let mut guard = OscGuard::with_limit(3);
        assert_eq!(&guard.filter(b"\x1b]0;abcdef\x18ok")[..], b"\x1b]0;a\x18ok");
    }

    /// 状态跨批次延续：上限和丢弃都不依赖批次怎么切。
    #[test]
    fn guard_state_survives_arbitrary_chunking() {
        let input = b"x\x1b]0;abcdefghij\x1b\\y\x1b]2;ok\x07z";
        let mut whole = OscGuard::with_limit(3);
        let expected = whole.filter(input).into_owned();
        for split in 1..input.len() {
            let mut guard = OscGuard::with_limit(3);
            let mut got = guard.filter(&input[..split]).into_owned();
            got.extend_from_slice(&guard.filter(&input[split..]));
            assert_eq!(got, expected, "在 {split} 处切开");
        }
    }

    /// 没超限的正常流量（含 shell 集成标记）原样放行，而且是零拷贝借用。
    #[test]
    fn guard_borrows_normal_traffic_untouched() {
        let mut guard = OscGuard::new();
        let input = b"\x1b]133;A\x07$ ls\r\n\x1b]7;file:///tmp\x1b\\";
        assert!(matches!(
            guard.filter(input),
            std::borrow::Cow::Borrowed(b) if b == input
        ));
        assert_eq!(guard.overflowed(), 0);
    }

    #[test]
    fn garbage_never_panics() {
        let mut scanner = OscScanner::new();
        let mut out = Vec::new();
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let alphabet: &[u8] = b"\x1b]\\\x07\x18\x1a;0123456789ABCDEx%/:file7633 \xff\xfe";
        for _ in 0..20_000 {
            let mut chunk = Vec::new();
            for _ in 0..(seed % 17) {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                chunk.push(alphabet[(seed >> 33) as usize % alphabet.len()]);
            }
            scanner.scan(&chunk, &mut out);
            out.clear();
        }
    }
}
