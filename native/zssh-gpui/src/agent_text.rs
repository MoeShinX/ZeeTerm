//! AI 对话窗口和智能体窗口共用的纯文本辅助：路径脱敏、命令可见化、输出截断、
//! 原文围栏、链接校验、危险标记。
//!
//! 全是不依赖界面的纯函数，方便单测；规则的来源在 0.2.14 的 `ui/src/components/ai`
//! （`aiHelpers.ts`、`Markdown.tsx`）和 core 的 `external_url`，这里移植或直接复用。

use serde_json::Value;
use zssh_core::external_url::validate_external_url;

/// 用户目录在界面上显示成什么。
pub const HOME_PLACEHOLDER: &str = "%USERPROFILE%";

// ---------------------------------------------------------------------------
// 路径脱敏
// ---------------------------------------------------------------------------

/// 比较路径时的"折叠"：`/` 和 `\` 视为同一个，字母不分大小写（含非 ASCII 字母）。
fn fold(c: char) -> char {
    if c == '/' {
        return '\\';
    }
    let mut lower = c.to_lowercase();
    match (lower.next(), lower.next()) {
        (Some(single), None) => single,
        // 一个字符展开成多个（极少见）：保持原样，宁可漏匹配也不要错替换。
        _ => c,
    }
}

/// 用户目录后面紧跟什么才算"目录名到此为止"：路径分隔符、空白、引号括号和常见标点。
/// 防止 `C:\Users\demo` 误伤 `C:\Users\demo2\x`。
fn ends_home_component(next: Option<char>) -> bool {
    match next {
        None => true,
        Some(c) => {
            matches!(c, '\\' | '/')
                || c.is_whitespace()
                || "\"'`()[]{}<>,;:|".contains(c)
                || "，。；：！？（）【】「」、".contains(c)
        }
    }
}

/// 把文本里出现的用户目录前缀换成 `%USERPROFILE%`。
///
/// - 按**字符**比较，不按字节切片：用户目录或路径里有中文、emoji 也不会 panic；
/// - 不区分大小写，`/` 和 `\` 等价；
/// - 整段文本里出现几次换几次（错误信息里常常夹着路径），不只处理开头；
/// - `home` 太短（没有目录层级，比如 `C:`）或为空时原样返回，免得把别的东西替换掉。
pub fn redact_home(text: &str, home: &str) -> String {
    let home = home.trim().trim_end_matches(['\\', '/']);
    if home.is_empty() || !home.contains(['\\', '/']) {
        return text.to_owned();
    }
    let needle: Vec<char> = home.chars().map(fold).collect();
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let end = index + needle.len();
        let hit = end <= chars.len()
            && chars[index..end]
                .iter()
                .zip(&needle)
                .all(|(have, want)| fold(*have) == *want)
            && ends_home_component(chars.get(end).copied());
        if hit {
            out.push_str(HOME_PLACEHOLDER);
            index = end;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

/// 当前用户的目录（读不到返回空串）。
fn current_home() -> String {
    std::env::var_os("USERPROFILE")
        .map(|home| home.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 界面上任何可能带出本机路径的文字（安装路径、错误信息、数据目录）都过这一个函数。
pub fn redact_paths(text: &str) -> String {
    redact_home(text, &current_home())
}

// ---------------------------------------------------------------------------
// 命令显示
// ---------------------------------------------------------------------------

/// 把命令里的不可见控制字符显示成可见的转义形式（审批卡片第二道防线）。
///
/// 移植自 0.2.14 `aiHelpers.ts::visibleCommandText`：控制字符会在终端里产生按键效果，
/// C1 / 方向控制字符还能让卡片显示的内容和实际执行的不一样。正常换行和制表保留，
/// 其余 C0、DEL 显示成 `\x15`，C1 / 方向控制字符显示成 `\u{009b}`。
pub fn visible_command_text(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    for ch in command.chars() {
        let code = ch as u32;
        let forbidden = ((code < 0x20 || code == 0x7f) && ch != '\n' && ch != '\t')
            || (0x80..=0x9f).contains(&code)
            || (0x202a..=0x202e).contains(&code)
            || (0x2066..=0x2069).contains(&code);
        if !forbidden {
            out.push(ch);
        } else if code <= 0x7f {
            out.push_str(&format!("\\x{code:02x}"));
        } else {
            out.push_str(&format!("\\u{{{code:04x}}}"));
        }
    }
    out
}

/// 一行摘要：只取第一行并截断，换行用 `↵` 提示后面还有内容。仅用于折叠状态的标题，
/// 审批卡片本身显示全文。
pub fn one_line_summary(text: &str, limit: usize) -> String {
    let visible = visible_command_text(text);
    let first = visible.lines().next().unwrap_or("");
    let more_lines = visible.lines().nth(1).is_some();
    let mut summary: String = first.chars().take(limit).collect();
    if first.chars().count() > limit || more_lines {
        summary.push('…');
    }
    summary
}

/// 命令框默认高度对应的行数：超过它（或者太长）就提示"展开全文"。
pub const COMMAND_FOLD_LINES: usize = 8;
/// 一行里超过多少字符也算"长"，需要展开按钮（窄窗口里一行会折成好几行）。
pub const COMMAND_FOLD_CHARS: usize = 600;

/// 命令文本是否超出默认高度、需要展开按钮。
pub fn needs_expand(text: &str) -> bool {
    text.lines().count() > COMMAND_FOLD_LINES || text.chars().count() > COMMAND_FOLD_CHARS
}

// ---------------------------------------------------------------------------
// write_file 差异（0.2.14 `aiHelpers.ts` 的 foldDiff / diffSummary）
// ---------------------------------------------------------------------------

/// 差异默认最多显示多少行，其余折叠（和 0.2.14 的 `DIFF_FOLD_LIMIT` 一致）。
pub const DIFF_FOLD_LIMIT: usize = 30;

/// 折叠后显示几行、藏了几行；展开或本来就不长时全显。
pub fn fold_diff(total: usize, expanded: bool) -> (usize, usize) {
    if expanded || total <= DIFF_FOLD_LIMIT {
        (total, 0)
    } else {
        (DIFF_FOLD_LIMIT, total - DIFF_FOLD_LIMIT)
    }
}

/// 差异摘要的两句话。
#[derive(Clone, Copy, Debug)]
enum AgentTextMsg {
    NewFilePrefix,
    LineSuffix,
    NoChange,
}

impl crate::i18n::Msg for AgentTextMsg {
    fn zh(&self) -> &'static str {
        match self {
            Self::NewFilePrefix => "新建文件，共 ",
            Self::LineSuffix => " 行",
            Self::NoChange => "内容没有变化",
        }
    }

    fn en(&self) -> &'static str {
        match self {
            Self::NewFilePrefix => "New file, ",
            Self::LineSuffix => " lines",
            Self::NoChange => "No changes",
        }
    }
}

/// 差异的一句话摘要：`新建文件，共 3 行` / `+2 −1` / `内容没有变化`。
pub fn diff_summary(added: usize, removed: usize, is_new: bool) -> String {
    if is_new {
        format!(
            "{}{}",
            crate::i18n::t(AgentTextMsg::NewFilePrefix),
            crate::i18n::count(
                added as u64,
                crate::i18n::t(AgentTextMsg::LineSuffix),
                "line",
                "lines"
            )
        )
    } else if added == 0 && removed == 0 {
        crate::i18n::t(AgentTextMsg::NoChange).to_owned()
    } else {
        format!("+{added} −{removed}")
    }
}

// ---------------------------------------------------------------------------
// 输出截断
// ---------------------------------------------------------------------------

/// 工具输出默认最多显示的行数。
pub const OUTPUT_PREVIEW_LINES: usize = 40;
/// 工具输出默认最多显示的字符数（防止单行几十万字符把界面拖死）。
pub const OUTPUT_PREVIEW_CHARS: usize = 8_000;

/// 截断的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub text: String,
    /// 原文一共多少行。
    pub total_lines: usize,
    /// 原文一共多少字符。
    pub total_chars: usize,
    /// 是否真的被截了。
    pub clipped: bool,
}

/// 取前 `max_lines` 行、最多 `max_chars` 个字符；不切断字符。
pub fn clip_text(text: &str, max_lines: usize, max_chars: usize) -> Clip {
    let total_lines = text.lines().count();
    let total_chars = text.chars().count();
    if total_lines <= max_lines && total_chars <= max_chars {
        return Clip {
            text: text.to_owned(),
            total_lines,
            total_chars,
            clipped: false,
        };
    }
    let mut kept = String::new();
    let mut chars = 0usize;
    for (index, line) in text.lines().enumerate() {
        if index >= max_lines || chars >= max_chars {
            break;
        }
        if index > 0 {
            kept.push('\n');
            chars += 1;
        }
        let room = max_chars.saturating_sub(chars);
        let piece: String = line.chars().take(room).collect();
        chars += piece.chars().count();
        kept.push_str(&piece);
    }
    Clip {
        text: kept,
        total_lines,
        total_chars,
        clipped: true,
    }
}

/// 对话一次最多画多少条消息（更早的折叠成"显示更早的消息"）。
pub const MESSAGES_PER_PAGE: usize = 40;

/// 长对话只画最后 `shown` 条：返回起始下标。`shown` 为 0 或超过总数时从头画。
pub fn visible_start(total: usize, shown: usize) -> usize {
    if shown == 0 || shown >= total {
        0
    } else {
        total - shown
    }
}

/// 助手正文超过这个字符数就先折叠（Markdown 每帧解析，超长正文会拖慢界面）。
pub const BODY_PREVIEW_CHARS: usize = 20_000;

// ---------------------------------------------------------------------------
// Markdown 原文围栏
// ---------------------------------------------------------------------------

/// 文本里最长的一串连续反引号的长度。
fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

/// 把原始 Markdown 包进代码围栏：围栏比正文里最长的反引号串多一个（至少 3 个），
/// 正文里任何一行都不可能提前结束外层代码块，也就不会"逃逸"出来被当成 Markdown 渲染。
pub fn markdown_fence(text: &str) -> String {
    let fence = "`".repeat((longest_backtick_run(text) + 1).max(3));
    format!("{fence}markdown\n{text}\n{fence}")
}

// ---------------------------------------------------------------------------
// 链接
// ---------------------------------------------------------------------------

/// 点击 Markdown 链接后该做什么。0.2.14 的行为：链接不跳转、不开浏览器，只复制网址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAction {
    /// 校验通过：把这个（规范化后的）网址复制到剪贴板。
    Copy(String),
    /// 校验没过：不复制，原因给用户看。
    Refuse(String),
}

/// 所有链接 handler 共用的校验：复用 core 的 `validate_external_url`
/// （只放行 http/https、限长、拒空白和控制字符、要求有主机名）。
pub fn link_action(raw: &str) -> LinkAction {
    match validate_external_url(raw.trim()) {
        Ok(url) => LinkAction::Copy(url.as_str().to_owned()),
        Err(reason) => LinkAction::Refuse(reason),
    }
}

// ---------------------------------------------------------------------------
// 危险标记（0.2.14 `AIChatPanel.tsx` 卡片上的"危险"徽标）
// ---------------------------------------------------------------------------

/// 工具输入里标了危险吗：移植自 `aiHelpers.ts::isDangerous`（`input.danger == "dangerous"`）。
pub fn input_is_dangerous(input: &Value) -> bool {
    input.get("danger").and_then(Value::as_str) == Some("dangerous")
}

/// `write_file` 结果里"写系统路径"的原因：移植自 `aiHelpers.ts::fileDangerReason`
/// （匹配 `danger: flagged as a system path (原因)`，不区分大小写）。
pub fn file_danger_reason(output: &str) -> Option<String> {
    let lower = output.to_ascii_lowercase();
    let marker = "flagged as a system path (";
    let at = lower.find(marker)?;
    // 要求前面有 `danger:`（中间只允许空白），和原正则一致。
    let before = lower[..at].trim_end();
    if !before.ends_with("danger:") {
        return None;
    }
    let rest = &output[at + marker.len()..];
    let end = rest.find([')', '"', '\\']).unwrap_or(rest.len());
    // `rest` 在 ASCII 标记之后，下标落在字符边界上（find 返回的是字符起点）。
    let reason = rest[..end].trim();
    // 原正则要求以 `)` 结尾；没有闭合就不算。
    if rest[end..].starts_with(')') {
        Some(reason.to_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redact_home_replaces_the_prefix_case_insensitively() {
        assert_eq!(
            redact_home(r"C:\Users\alice\bin\x.exe", r"C:\Users\alice"),
            r"%USERPROFILE%\bin\x.exe"
        );
        assert_eq!(
            redact_home(r"c:\users\ALICE\bin\x.exe", r"C:\Users\alice"),
            r"%USERPROFILE%\bin\x.exe"
        );
        assert_eq!(
            redact_home("C:/Users/alice/bin/x.exe", r"C:\Users\alice"),
            "%USERPROFILE%/bin/x.exe"
        );
        assert_eq!(
            redact_home(r"C:\Tools\x.exe", r"C:\Users\alice"),
            r"C:\Tools\x.exe"
        );
    }

    #[test]
    fn redact_home_survives_multibyte_directories_without_panicking() {
        // 审查 H5：旧实现按字节切 `path[..home.len()]`，第 N 个字节落在汉字中间会 panic。
        let home = r"C:\Users\demo";
        let path = r"C:\Users\张三\bin\claude.exe";
        assert_eq!(redact_home(path, home), path, "不是同一个目录，原样返回");
        // 用户目录本身含中文、emoji。
        let home = r"C:\Users\张 三😀";
        assert_eq!(
            redact_home(r"C:\Users\张 三😀\AppData\x", home),
            r"%USERPROFILE%\AppData\x"
        );
        assert_eq!(
            redact_home(r"c:\users\张 三😀\x", home),
            r"%USERPROFILE%\x",
            "大小写不敏感"
        );
        // 路径比 home 短，末尾是多字节字符。
        assert_eq!(
            redact_home("C:\\Users\\张", r"C:\Users\demo"),
            "C:\\Users\\张"
        );
        // 非 ASCII 字母也不区分大小写。
        assert_eq!(
            redact_home(r"C:\Users\Ünal\x", r"c:\users\ünal"),
            r"%USERPROFILE%\x"
        );
    }

    #[test]
    fn redact_home_requires_a_whole_directory_name() {
        let home = r"C:\Users\demo";
        assert_eq!(
            redact_home(r"C:\Users\demo2\x", home),
            r"C:\Users\demo2\x",
            "demo 不能误伤 demo2"
        );
        assert_eq!(
            redact_home(r"C:\Users\demo.old\x", home),
            r"C:\Users\demo.old\x"
        );
        assert_eq!(redact_home(r"C:\Users\demo", home), "%USERPROFILE%");
    }

    #[test]
    fn redact_home_handles_paths_inside_longer_messages() {
        let home = r"C:\Users\alice";
        let text =
            r"没有找到 zssh-mcp.exe（应当放在 C:\Users\alice\app）；另见 'C:\Users\alice\x'。";
        assert_eq!(
            redact_home(text, home),
            r"没有找到 zssh-mcp.exe（应当放在 %USERPROFILE%\app）；另见 '%USERPROFILE%\x'。"
        );
        // 全角括号紧跟目录名也算边界。
        assert_eq!(
            redact_home(r"（C:\Users\alice）", home),
            "（%USERPROFILE%）"
        );
    }

    #[test]
    fn redact_home_ignores_unusable_homes() {
        for home in ["", "  ", "C:", r"C:\", "alice"] {
            assert_eq!(redact_home(r"C:\Users\alice\x", home), r"C:\Users\alice\x");
        }
    }

    #[test]
    fn visible_command_text_keeps_newlines_and_escapes_controls() {
        assert_eq!(visible_command_text("ls -l\n\tcd /"), "ls -l\n\tcd /");
        assert_eq!(visible_command_text("a\u{15}b"), "a\\x15b");
        assert_eq!(visible_command_text("a\u{7f}b"), "a\\x7fb");
        assert_eq!(visible_command_text("a\u{9b}b"), "a\\u{009b}b");
        assert_eq!(visible_command_text("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(visible_command_text("a\u{2066}b"), "a\\u{2066}b");
        // 回车不是"正常换行"，0.2.14 同样转义。
        assert_eq!(visible_command_text("a\rb"), "a\\x0db");
        assert_eq!(visible_command_text("中文 😀"), "中文 😀");
    }

    #[test]
    fn one_line_summary_marks_hidden_content() {
        assert_eq!(one_line_summary("ls", 80), "ls");
        assert_eq!(one_line_summary("ls\nrm -rf /", 80), "ls…");
        assert_eq!(
            one_line_summary(&"x".repeat(100), 10),
            format!("{}…", "x".repeat(10))
        );
        assert_eq!(one_line_summary("", 80), "");
    }

    #[test]
    fn needs_expand_triggers_on_many_lines_or_long_text() {
        assert!(!needs_expand("ls -l"));
        assert!(needs_expand(&"a\n".repeat(COMMAND_FOLD_LINES + 1)));
        assert!(needs_expand(&"a".repeat(COMMAND_FOLD_CHARS + 1)));
    }

    #[test]
    fn clip_text_limits_lines_and_chars_without_cutting_characters() {
        let short = clip_text("a\nb", 5, 100);
        assert!(!short.clipped);
        assert_eq!(short.text, "a\nb");
        assert_eq!(short.total_lines, 2);

        let many = (0..100)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let clip = clip_text(&many, 10, 10_000);
        assert!(clip.clipped);
        assert_eq!(clip.text.lines().count(), 10);
        assert_eq!(clip.total_lines, 100);

        // 一行超长，且全是多字节字符。
        let wide = "汉".repeat(5_000);
        let clip = clip_text(&wide, 10, 100);
        assert!(clip.clipped);
        assert_eq!(clip.text.chars().count(), 100);
        assert_eq!(clip.total_chars, 5_000);
    }

    #[test]
    fn diff_folding_and_summary_follow_the_0214_rules() {
        assert_eq!(fold_diff(10, false), (10, 0));
        assert_eq!(fold_diff(30, false), (30, 0));
        assert_eq!(fold_diff(100, false), (30, 70));
        assert_eq!(fold_diff(100, true), (100, 0));
        assert_eq!(diff_summary(3, 0, true), "新建文件，共 3 行");
        assert_eq!(diff_summary(0, 0, false), "内容没有变化");
        assert_eq!(diff_summary(2, 1, false), "+2 −1");
    }

    #[test]
    fn visible_start_pages_from_the_end() {
        assert_eq!(visible_start(10, 40), 0);
        assert_eq!(visible_start(100, 40), 60);
        assert_eq!(visible_start(100, 0), 0);
        assert_eq!(visible_start(40, 40), 0);
    }

    #[test]
    fn markdown_fence_outgrows_any_backtick_run_in_the_text() {
        assert_eq!(markdown_fence("plain"), "```markdown\nplain\n```");
        let text = "a\n````\nb";
        let fenced = markdown_fence(text);
        assert!(fenced.starts_with("`````markdown\n"), "{fenced}");
        assert!(fenced.ends_with("\n`````"), "{fenced}");
        // 正文里 40 个反引号也关不掉外层。
        let nasty = format!("x\n{}\n[click](file:///C:/a.exe)", "`".repeat(40));
        let fenced = markdown_fence(&nasty);
        let fence = "`".repeat(41);
        assert!(fenced.starts_with(&format!("{fence}markdown\n")));
        assert!(fenced.ends_with(&format!("\n{fence}")));
        // 外层之外不会再出现等长或更长的反引号串。
        let inner = &fenced[fence.len()..fenced.len() - fence.len()];
        assert_eq!(longest_backtick_run(inner), 40);
    }

    #[test]
    fn link_action_copies_only_valid_web_links() {
        assert_eq!(
            link_action("https://Example.com/a?b=1"),
            LinkAction::Copy("https://example.com/a?b=1".to_owned())
        );
        assert_eq!(
            link_action("  http://10.0.0.1:8080/x  "),
            LinkAction::Copy("http://10.0.0.1:8080/x".to_owned())
        );
        for bad in [
            "file:///C:/Windows/System32/calc.exe",
            "javascript:alert(1)",
            "ms-settings:network",
            "data:text/html,hi",
            "C:\\Windows\\notepad.exe",
            "/etc/passwd",
            "https://a b",
            "https://",
            "",
        ] {
            assert!(
                matches!(link_action(bad), LinkAction::Refuse(_)),
                "{bad} 不该被复制/打开"
            );
        }
        let long = format!("https://example.com/{}", "a".repeat(3_000));
        assert!(matches!(link_action(&long), LinkAction::Refuse(_)));
    }

    #[test]
    fn danger_markers_follow_the_0214_rules() {
        assert!(input_is_dangerous(
            &json!({ "command": "x", "danger": "dangerous" })
        ));
        assert!(!input_is_dangerous(&json!({ "danger": "safe" })));
        assert!(!input_is_dangerous(&json!({ "danger": true })));
        assert!(!input_is_dangerous(&json!("dangerous")));

        assert_eq!(
            file_danger_reason("saved\ndanger: flagged as a system path (/etc/passwd)\nok"),
            Some("/etc/passwd".to_owned())
        );
        assert_eq!(
            file_danger_reason("DANGER:  Flagged as a System Path (系统目录 /etc)"),
            Some("系统目录 /etc".to_owned())
        );
        // JSON 转义里的引号、反斜杠截断原因。
        assert_eq!(
            file_danger_reason(r#"danger: flagged as a system path (/etc\n"x)"#),
            None,
            "没有闭合括号就不算"
        );
        assert_eq!(file_danger_reason("nothing here"), None);
        assert_eq!(
            file_danger_reason("flagged as a system path (/etc)"),
            None,
            "缺少 danger: 前缀"
        );
    }
}
