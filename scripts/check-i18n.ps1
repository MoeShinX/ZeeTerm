# 英文版覆盖检查（V2-I18N）：界面文案必须走 i18n 的 Msg 表，不能把中文写死在调用点。
#
# 扫描方式：对每个 .rs 文件做 Rust 词法扫描（识别 "..."、b"..."、r"..."、r#"..."#、转义、跨行字符串、
# 字符字面量与生命周期、行注释与嵌套块注释），按真正的大括号层级判断一个字面量在哪个位置。
# 不再按"行"匹配，所以多行字面量的续行、只含全角标点的字面量、同一行里的 { } 都能正确处理。
#
# 规则：
#   1. 正式代码里不允许出现含中文（或全角标点 （）：，。「」 等）的字符串/字符字面量。
#      豁免：
#        - `fn zh(...)` 的表体（中文原文本来就该在那）；
#        - 测试和调试专用代码：`#[cfg(test)]` / `#[cfg(debug_assertions)]` / `#[test]` 后面的整个条目（不管是 mod、impl、fn、const 还是 use）、`mod *tests* { }`、
#          `tests.rs` / `*_tests.rs` / `test_*.rs`；
#        - 只在调试构建里存在的文件（`dev_*`、`sync_stub`、`terminal_demo`、`fake_tools`、`sftp/script.rs`……）；
#        - assert* / panic! / println! / eprintln! / tracing 与 log 的日志宏 的参数；
#        - 字面量所在行（或独占一行的上一行注释）写了 `// i18n-allow: 原因`；
#        - `scripts/i18n-allow.txt` 里列出的文件（按文件 burn-down）。
#   2. `fn en(...)` 的表体里不能有中文字面量（"简体中文" 这个语言名除外）。
#   3. `fn zh(...)` / `fn en(...)` 的表体里不能调用 `t(`（会递归，表里该写的是字面量）。
#
# 扫描范围：native/zssh-gpui/src，以及 native/ 下其他 zssh-* crate 的 src（会通过 Display 上屏的错误文案）。
#
# 用法：powershell -NoProfile -File scripts/check-i18n.ps1 [-List] [-SelfTest]
#   -List      只打印统计（每个 allow 文件还剩多少处），不做通过/失败判断
#   -SelfTest  只跑脚本自测（用临时目录里的样例 .rs 验证每种漏查场景都能报出来）
# 退出码：0 = 通过；1 = 有 allow 清单之外的写死中文 / en 表里混进中文 / 表体里调用了 t( / 自测失败。

[CmdletBinding()]
param([switch]$List, [switch]$SelfTest)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repoRoot

$allowFile = 'scripts/i18n-allow.txt'

# C# 版本要兼容 Windows PowerShell 5.1（C# 5）：不用字符串插值、?.、表达式体成员。
# 源码里只放 ASCII，汉字和全角标点一律用 \u 转义。
$scannerSource = @'
using System;
using System.Collections.Generic;

public sealed class I18nFinding
{
    public int Line;
    public string Rule;   // hardcoded | en-han | t-call
    public string Text;
}

public static class RustI18nScanner
{
    private sealed class Tok
    {
        public char Kind;      // i ident, p punct, s string, c char, n number
        public string Text;
        public int Line;
        public int EndLine;
    }

    private const byte RegNormal = 0;
    private const byte RegZh = 1;
    private const byte RegEn = 2;
    private const byte RegTest = 3;

    private static readonly HashSet<string> ExemptMacros = new HashSet<string>(new string[] {
        "assert", "assert_eq", "assert_ne", "debug_assert", "debug_assert_eq", "debug_assert_ne",
        "panic", "unreachable", "todo", "unimplemented",
        "println", "eprintln", "print", "eprint", "dbg",
        "trace", "debug", "info", "warn", "error"
    });

    private static bool IsIdentStart(char c) { return c == '_' || char.IsLetter(c); }
    private static bool IsIdentCont(char c) { return c == '_' || char.IsLetterOrDigit(c); }

    private static bool IsHan(char c)
    {
        return (c >= '㐀' && c <= '䶿') || (c >= '一' && c <= '鿿') || (c >= '豈' && c <= '﫿');
    }

    // 全角标点：CJK 符号和标点（、-〃、〈-】、〔-〟）和全角 ASCII 变体（！-｠）。
    private static bool IsFullwidthPunct(char c)
    {
        return (c >= '、' && c <= '〃') || (c >= '〈' && c <= '】')
            || (c >= '〔' && c <= '〟') || (c >= '！' && c <= '｠');
    }

    private static bool HasCjk(string s, bool allowLanguageName)
    {
        if (allowLanguageName) s = s.Replace("简体中文", "");
        for (int i = 0; i < s.Length; i++)
        {
            if (IsHan(s[i]) || IsFullwidthPunct(s[i])) return true;
        }
        return false;
    }

    private static string Snippet(string s)
    {
        s = s.Replace("\r", "").Replace("\n", "\\n");
        return s.Length > 70 ? s.Substring(0, 70) + "..." : s;
    }

    private static int Count(string s, int from, int to, char ch)
    {
        int k = 0;
        for (int i = from; i < to && i < s.Length; i++) if (s[i] == ch) k++;
        return k;
    }

    private static void AddMarkers(string src, int from, int to, int startLine, List<int> markers)
    {
        int idx = from;
        while (true)
        {
            idx = src.IndexOf("i18n-allow", idx, StringComparison.Ordinal);
            if (idx < 0 || idx >= to) return;
            markers.Add(startLine + Count(src, from, idx, '\n'));
            idx += 10;
        }
    }

    // 普通字符串：q 指向开头的引号，返回结束引号之后的位置。
    private static int ReadString(string src, int q, ref int line, out string text, out int endLine)
    {
        int n = src.Length;
        int j = q + 1;
        int cur = line;
        while (j < n && src[j] != '"')
        {
            if (src[j] == '\\')
            {
                if (j + 1 < n && src[j + 1] == '\n') cur++;
                j += 2;
            }
            else
            {
                if (src[j] == '\n') cur++;
                j++;
            }
        }
        int end = Math.Min(j, n);
        text = src.Substring(q + 1, Math.Max(0, end - q - 1));
        endLine = cur;
        line = cur;
        return Math.Min(j + 1, n);
    }

    // 原始字符串：q 指向开头的引号，hashes 是前面的 # 个数。
    private static int ReadRaw(string src, int q, int hashes, ref int line, out string text, out int endLine)
    {
        int n = src.Length;
        string close = "\"" + new string('#', hashes);
        int end = src.IndexOf(close, q + 1, StringComparison.Ordinal);
        if (end < 0) end = n;
        text = src.Substring(q + 1, Math.Max(0, end - q - 1));
        endLine = line + Count(src, q + 1, end, '\n');
        line = endLine;
        return Math.Min(end + close.Length, n);
    }

    private static List<Tok> Lex(string src, List<int> markers)
    {
        List<Tok> toks = new List<Tok>();
        int n = src.Length;
        int i = 0;
        int line = 1;
        while (i < n)
        {
            char c = src[i];
            if (c == '\n') { line++; i++; continue; }
            if (char.IsWhiteSpace(c)) { i++; continue; }

            if (c == '/' && i + 1 < n && src[i + 1] == '/')
            {
                int j = src.IndexOf('\n', i);
                if (j < 0) j = n;
                AddMarkers(src, i, j, line, markers);
                i = j;
                continue;
            }
            if (c == '/' && i + 1 < n && src[i + 1] == '*')
            {
                int depth = 1;
                int j = i + 2;
                while (j < n && depth > 0)
                {
                    if (src[j] == '/' && j + 1 < n && src[j + 1] == '*') { depth++; j += 2; }
                    else if (src[j] == '*' && j + 1 < n && src[j + 1] == '/') { depth--; j += 2; }
                    else j++;
                }
                AddMarkers(src, i, j, line, markers);
                line += Count(src, i, j, '\n');
                i = j;
                continue;
            }

            if (IsIdentStart(c))
            {
                int j = i;
                while (j < n && IsIdentCont(src[j])) j++;
                string id = src.Substring(i, j - i);

                if (id == "r" || id == "br" || id == "cr")
                {
                    int k = j;
                    int hashes = 0;
                    while (k < n && src[k] == '#') { hashes++; k++; }
                    if (k < n && src[k] == '"')
                    {
                        string text; int endLine; int startLine = line;
                        i = ReadRaw(src, k, hashes, ref line, out text, out endLine);
                        toks.Add(new Tok { Kind = 's', Text = text, Line = startLine, EndLine = endLine });
                        continue;
                    }
                    if (id == "r" && hashes == 1 && k < n && IsIdentStart(src[k]))
                    {
                        int m = k;
                        while (m < n && IsIdentCont(src[m])) m++;
                        toks.Add(new Tok { Kind = 'i', Text = src.Substring(k, m - k), Line = line, EndLine = line });
                        i = m;
                        continue;
                    }
                }
                if ((id == "b" || id == "c") && j < n && src[j] == '"')
                {
                    string text; int endLine; int startLine = line;
                    i = ReadString(src, j, ref line, out text, out endLine);
                    toks.Add(new Tok { Kind = 's', Text = text, Line = startLine, EndLine = endLine });
                    continue;
                }
                if (id == "b" && j < n && src[j] == '\'')
                {
                    i = j; // 落到下面的字符字面量分支
                    c = '\'';
                }
                else
                {
                    toks.Add(new Tok { Kind = 'i', Text = id, Line = line, EndLine = line });
                    i = j;
                    continue;
                }
            }

            if (char.IsDigit(c))
            {
                int j = i;
                while (j < n && IsIdentCont(src[j])) j++;
                toks.Add(new Tok { Kind = 'n', Text = src.Substring(i, j - i), Line = line, EndLine = line });
                i = j;
                continue;
            }

            if (c == '"')
            {
                string text; int endLine; int startLine = line;
                i = ReadString(src, i, ref line, out text, out endLine);
                toks.Add(new Tok { Kind = 's', Text = text, Line = startLine, EndLine = endLine });
                continue;
            }

            if (c == '\'')
            {
                if (i + 1 < n && src[i + 1] == '\\')
                {
                    int j = i + 2;
                    if (j < n) j++;
                    while (j < n && src[j] != '\'' && src[j] != '\n') j++;
                    int end = Math.Min(j, n);
                    toks.Add(new Tok { Kind = 'c', Text = src.Substring(i + 1, Math.Max(0, end - i - 1)), Line = line, EndLine = line });
                    i = Math.Min(j + 1, n);
                    continue;
                }
                int len = (i + 1 < n && char.IsHighSurrogate(src[i + 1])) ? 2 : 1;
                if (i + 1 + len < n && src[i + 1] != '\'' && src[i + 1 + len] == '\'')
                {
                    toks.Add(new Tok { Kind = 'c', Text = src.Substring(i + 1, len), Line = line, EndLine = line });
                    i += len + 2;
                    continue;
                }
                // 生命周期或循环标签：'a / 'static / 'outer
                int m = i + 1;
                while (m < n && IsIdentCont(src[m])) m++;
                i = Math.Max(m, i + 1);
                continue;
            }

            toks.Add(new Tok { Kind = 'p', Text = c.ToString(), Line = line, EndLine = line });
            i++;
        }
        return toks;
    }

    private static bool IsP(List<Tok> t, int i, char ch)
    {
        return i >= 0 && i < t.Count && t[i].Kind == 'p' && t[i].Text[0] == ch;
    }

    private static bool IsId(List<Tok> t, int i, string name)
    {
        return i >= 0 && i < t.Count && t[i].Kind == 'i' && t[i].Text == name;
    }

    // t[open] 是 { 或 [ 或 (，返回与它配对的收括号下标；找不到返回最后一个下标。
    private static int MatchClose(List<Tok> t, int open)
    {
        char o = t[open].Text[0];
        char c = o == '{' ? '}' : (o == '[' ? ']' : ')');
        int depth = 0;
        for (int j = open; j < t.Count; j++)
        {
            if (t[j].Kind != 'p') continue;
            char ch = t[j].Text[0];
            if (ch == o) depth++;
            else if (ch == c)
            {
                depth--;
                if (depth == 0) return j;
            }
        }
        return t.Count - 1;
    }

    // 从 start 起的"一个条目"的结尾：第一个括号外的 { ... } 配对处，或第一个括号外的 ;。
    private static int ItemEnd(List<Tok> t, int start)
    {
        int depth = 0;
        for (int j = start; j < t.Count; j++)
        {
            if (t[j].Kind != 'p') continue;
            char ch = t[j].Text[0];
            if (ch == '(' || ch == '[') depth++;
            else if (ch == ')' || ch == ']') depth--;
            else if (depth <= 0 && ch == '{') return MatchClose(t, j);
            else if (depth <= 0 && ch == ';') return j;
        }
        return t.Count - 1;
    }

    private static bool AttrIsTest(List<Tok> t, int from, int to)
    {
        // 属性内容是 tokens[from..to]（不含 [ ]）。
        if (from > to) return false;
        if (t[from].Kind != 'i') return false;
        string first = t[from].Text;
        if (first == "test" && from == to) return true;
        if (first == "tokio" || first == "rstest" || first == "async_std")
        {
            return IsId(t, to, "test");
        }
        if (first == "cfg" || first == "cfg_attr")
        {
            // 只在测试或调试构建里存在的条目：cfg(test)、cfg(debug_assertions)、cfg(any(test, debug_assertions))、
            // cfg(all(test, ...)) 等。里面出现别的条件（feature、target_os……）的 any(...) 不算，因为发行版也可能编进去。
            bool hasDev = false, hasNot = false, hasOther = false, hasAny = false;
            for (int j = from + 1; j <= to; j++)
            {
                if (t[j].Kind != 'i') continue;
                string id = t[j].Text;
                if (id == "test" || id == "debug_assertions") hasDev = true;
                else if (id == "not") hasNot = true;
                else if (id == "any") hasAny = true;
                else if (id != "all") hasOther = true;
            }
            return first == "cfg" && hasDev && !hasNot && !(hasAny && hasOther);
        }
        return false;
    }

    private static void Mark(byte[] region, int from, int to, byte kind, bool overwrite)
    {
        for (int j = Math.Max(0, from); j <= to && j < region.Length; j++)
        {
            if (overwrite || region[j] == RegNormal) region[j] = kind;
        }
    }

    public static List<I18nFinding> Scan(string src)
    {
        List<I18nFinding> found = new List<I18nFinding>();
        List<int> markers = new List<int>();
        List<Tok> t = Lex(src, markers);
        int n = t.Count;

        HashSet<int> tokenLines = new HashSet<int>();
        for (int i = 0; i < n; i++)
        {
            for (int l = t[i].Line; l <= t[i].EndLine; l++) tokenLines.Add(l);
        }

        byte[] region = new byte[n];

        // 第一遍：测试条目。
        for (int i = 0; i < n; i++)
        {
            if (IsP(t, i, '#'))
            {
                int j = i + 1;
                bool inner = false;
                if (IsP(t, j, '!')) { inner = true; j++; }
                if (IsP(t, j, '['))
                {
                    int k = MatchClose(t, j);
                    if (AttrIsTest(t, j + 1, k - 1))
                    {
                        if (inner) return found; // #![cfg(test)]：整个文件都是测试
                        int end = ItemEnd(t, k + 1);
                        Mark(region, i, end, RegTest, true);
                    }
                }
            }
            else if (IsId(t, i, "mod") && i + 2 < n && t[i + 1].Kind == 'i' && IsP(t, i + 2, '{'))
            {
                string name = t[i + 1].Text.ToLowerInvariant();
                if (name == "test" || name.Contains("tests"))
                {
                    Mark(region, i, MatchClose(t, i + 2), RegTest, true);
                }
            }
        }

        // 第二遍：fn zh / fn en 表体。
        for (int i = 0; i + 1 < n; i++)
        {
            if (!IsId(t, i, "fn")) continue;
            byte kind = 0;
            if (IsId(t, i + 1, "zh")) kind = RegZh;
            else if (IsId(t, i + 1, "en")) kind = RegEn;
            if (kind == 0) continue;
            if (region[i] == RegTest) continue;
            int depth = 0;
            for (int j = i + 2; j < n; j++)
            {
                if (t[j].Kind != 'p') continue;
                char ch = t[j].Text[0];
                if (ch == '(' || ch == '[') depth++;
                else if (ch == ')' || ch == ']') depth--;
                else if (depth <= 0 && ch == ';') break;
                else if (depth <= 0 && ch == '{')
                {
                    Mark(region, j, MatchClose(t, j), kind, false);
                    break;
                }
            }
        }

        // 第三遍：逐个 token 检查，用定界符栈记住当前在哪些宏调用里面。
        List<string> stack = new List<string>();
        for (int i = 0; i < n; i++)
        {
            Tok tk = t[i];
            if (tk.Kind == 'p')
            {
                char ch = tk.Text[0];
                if (ch == '(' || ch == '[' || ch == '{')
                {
                    string macro = null;
                    if (IsP(t, i - 1, '!') && i >= 2 && t[i - 2].Kind == 'i') macro = t[i - 2].Text;
                    stack.Add(macro);
                }
                else if (ch == ')' || ch == ']' || ch == '}')
                {
                    if (stack.Count > 0) stack.RemoveAt(stack.Count - 1);
                }
                continue;
            }
            byte reg = region[i];
            if (reg == RegTest) continue;

            if (tk.Kind == 'i')
            {
                if ((reg == RegZh || reg == RegEn) && tk.Text == "t" && IsP(t, i + 1, '(')
                    && !IsP(t, i - 1, '.') && !IsId(t, i - 1, "fn"))
                {
                    found.Add(new I18nFinding { Line = tk.Line, Rule = "t-call", Text = "t(" });
                }
                continue;
            }

            if (tk.Kind != 's' && tk.Kind != 'c') continue;
            if (reg == RegZh) continue;
            if (!HasCjk(tk.Text, reg == RegEn))
            {
                continue;
            }
            if (reg == RegEn)
            {
                found.Add(new I18nFinding { Line = tk.Line, Rule = "en-han", Text = Snippet(tk.Text) });
                continue;
            }

            bool exempt = false;
            for (int s = 0; s < stack.Count && !exempt; s++)
            {
                if (stack[s] != null && ExemptMacros.Contains(stack[s])) exempt = true;
            }
            for (int m = 0; m < markers.Count && !exempt; m++)
            {
                int ml = markers[m];
                if (ml >= tk.Line && ml <= tk.EndLine) exempt = true;
                else if (ml == tk.Line - 1 && !tokenLines.Contains(ml)) exempt = true;
            }
            if (exempt) continue;
            found.Add(new I18nFinding { Line = tk.Line, Rule = "hardcoded", Text = Snippet(tk.Text) });
        }
        return found;
    }
}
'@

if (-not ('RustI18nScanner' -as [type])) {
    Add-Type -TypeDefinition $scannerSource -Language CSharp
}

# 只在调试构建里存在的文件：界面上永远不会出现它们的文案。
$devOnly = @(
    'sync_stub.rs',
    'terminal_demo.rs',
    'fake_tools.rs',
    'script.rs',
    'dev_script.rs',
    'installer_manifest.rs',
    'dev.rs',
    'dev_seed.rs',
    'demo.rs',
    'fake.rs',
    'builtin_fake.rs',
    'editor_demo.rs',
    'fake_telnet.rs'
)

function Test-IsTestFile([string]$name) {
    if ($name -eq 'tests.rs') { return $true }
    if ($name -match '_tests\.rs$') { return $true }
    if ($name -match '^test_.*\.rs$') { return $true }
    return $false
}

# 扫描一个目录下所有 .rs，返回 { File, Line, Rule, Text } 列表（File 相对 $baseDir）。
function Invoke-Scan([string[]]$roots, [string]$baseDir) {
    $results = [System.Collections.Generic.List[object]]::new()
    foreach ($root in $roots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        foreach ($file in (Get-ChildItem -LiteralPath $root -Recurse -File -Filter *.rs)) {
            $rel = $file.FullName.Substring($baseDir.Length).TrimStart('\', '/').Replace('\', '/')
            $name = $file.Name
            if (Test-IsTestFile $name) { continue }
            if ($devOnly -contains $name) { continue }
            if ($rel -match '/dev_acts') { continue }
            if ($rel -match '/(tests|benches|examples)/') { continue }
            $text = [System.IO.File]::ReadAllText($file.FullName, [System.Text.Encoding]::UTF8)
            foreach ($f in [RustI18nScanner]::Scan($text)) {
                $results.Add([pscustomobject]@{ File = $rel; Line = $f.Line; Rule = $f.Rule; Text = $f.Text })
            }
        }
    }
    return $results
}

function Invoke-SelfTest {
    $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("zssh-check-i18n-selftest-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $dir | Out-Null
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    try {
        # 每个样例：文件内容 + 期望的 "规则:行号" 列表（按行号排序）。
        $cases = @(
            @{ Name = 'mod_tests_then_prod.rs'; Expect = @('hardcoded:9')
               Src = @'
#[cfg(test)]
mod tests {
    fn a() {
        let _ = "测试里的中文";
        if true { let _ = "}{"; }
    }
}

fn prod() { let _ = "正式代码里的中文"; }
'@ },
            @{ Name = 'zh_before_en.rs'; Expect = @('en-han:8', 'hardcoded:12')
               Src = @'
enum M { A }
impl Msg for M {
    fn zh(&self) -> &'static str {
        match self { M::A => "中文表" }
    }
    fn en(&self) -> &'static str {
        match self {
            M::A => "混进来的中文",
        }
    }
}
fn prod() { let _ = "表后面的正式代码"; }
'@ },
            @{ Name = 'single_line_cfg_test.rs'; Expect = @('hardcoded:7', 'hardcoded:8')
               Src = @'
#[cfg(test)]
const X: &str = "单行测试项";
#[cfg(test)]
use std::{fmt, io};
#[cfg(test)] fn helper() -> &'static str { "测试辅助" }

fn prod() { let _ = "正式一"; }
fn prod2() { let _ = "正式二"; }
'@ },
            @{ Name = 'raw_strings.rs'; Expect = @('hardcoded:2', 'hardcoded:3', 'hardcoded:5')
               Src = @'
fn f() {
    let a = r#"含"引号"的中文"#;
    let b = r"中文";
    let c = "} { no cjk";
    let d = br#"字节原始串中文"#;
    // 注释里的中文 "和引号 { 都不算
    /* 块注释 "中文" /* 嵌套 */ 中文 */
}
'@ },
            @{ Name = 'fullwidth_and_multiline.rs'; Expect = @('hardcoded:2', 'hardcoded:3', 'hardcoded:4', 'hardcoded:8')
               Src = @'
fn f(a: &str) {
    let _ = format!("（{}）", a);
    let _ = format!("{}：{}", a, a);
    let _ = "a，b";
    let _ = "plain
        second line plain
        ";
    let _ = "first line
        第二行有，全角逗号
        ";
}
'@ },
            @{ Name = 't_call_in_tables.rs'; Expect = @('t-call:3', 't-call:6')
               Src = @'
impl Msg for M {
    fn zh(&self) -> &'static str {
        t(Other::A)
    }
    fn en(&self) -> &'static str {
        crate::i18n::t(Other::B)
    }
}
fn prod() { let s = x.t(1); let _ = t(M::A); }
'@ },
            @{ Name = 'exempt.rs'; Expect = @('hardcoded:9')
               Src = @'
fn f() {
    println!("中文");
    tracing::info!("日志中文 {}", format!("嵌套{}", 1));
    assert_eq!(a,
        "断言里的中文");
    let _ = "有标记的中文"; // i18n-allow: 原因
    // i18n-allow: 上一行标记
    let _ = "上一行标记的中文";
    let _ = "没有标记的中文";
}
'@ },
            @{ Name = 'lifetimes_and_chars.rs'; Expect = @('hardcoded:5', 'hardcoded:6')
               Src = @'
fn f<'a>(x: &'a str) -> &'a str {
    let c = '"';
    let d = '\'';
    let e = '{';
    let s = "中文";
    let h = '中';
    'outer: loop { break 'outer; }
    x
}
'@ },
            @{ Name = 'debug_only_items.rs'; Expect = @('hardcoded:8', 'hardcoded:10')
               Src = @'
#[cfg(debug_assertions)]
pub fn dev_text() -> String { "调试专用".to_owned() }
#[cfg(any(test, debug_assertions))]
pub fn quit_prompt() -> &'static str { "测试或调试" }
#[cfg(all(debug_assertions, windows))]
const A: &str = "调试加平台";
#[cfg(not(debug_assertions))]
const B: &str = "发行版里的中文";
#[cfg(any(feature = "x", debug_assertions))]
const C: &str = "可能编进发行版";
'@ },
            @{ Name = 'tests.rs'; Expect = @()
               Src = @'
fn only_in_tests_file() { let _ = "整个文件是测试"; }
'@ },
            @{ Name = 'inner_cfg_test.rs'; Expect = @()
               Src = @'
#![cfg(test)]
fn helper() { let _ = "整个文件是测试"; }
'@ }
        )
        foreach ($case in $cases) {
            [System.IO.File]::WriteAllText((Join-Path $dir $case.Name), ($case.Src -replace "`r`n", "`n"), $utf8)
        }
        $got = @{}
        foreach ($r in (Invoke-Scan @($dir) $dir)) {
            if (-not $got.ContainsKey($r.File)) { $got[$r.File] = [System.Collections.Generic.List[string]]::new() }
            $got[$r.File].Add(("{0}:{1}" -f $r.Rule, $r.Line))
        }
        $bad = 0
        foreach ($case in $cases) {
            $name = $case.Name
            $actual = @()
            if ($got.ContainsKey($name)) { $actual = @($got[$name]) }
            $expected = @($case.Expect)
            # 比较时按行号排序，规则相同的位置一一对应。
            $a = ($actual | Sort-Object { [int]($_ -split ':')[1] }) -join ' '
            $e = ($expected | Sort-Object { [int]($_ -split ':')[1] }) -join ' '
            if ($a -ne $e) {
                $bad++
                Write-Host ("自测失败 {0}：期望 [{1}]，实际 [{2}]" -f $name, $e, $a) -ForegroundColor Red
            }
        }
        if ($bad -gt 0) { return $false }
        Write-Host ("check-i18n 自测通过（{0} 个样例）。" -f $cases.Count)
        return $true
    }
    finally {
        Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
    }
}

if (-not (Invoke-SelfTest)) { exit 1 }
if ($SelfTest) { exit 0 }

# allow 清单：一行一个文件。`native/` 开头的按仓库根相对路径，其余按 native/zssh-gpui/src 下的相对路径。
$allow = [System.Collections.Generic.HashSet[string]]::new()
if (Test-Path -LiteralPath $allowFile) {
    foreach ($line in Get-Content -LiteralPath $allowFile -Encoding UTF8) {
        $t = $line.Trim()
        if ($t -eq '' -or $t.StartsWith('#')) { continue }
        $t = $t.Replace('\', '/')
        if (-not $t.StartsWith('native/')) { $t = 'native/zssh-gpui/src/' + $t }
        $allow.Add($t) | Out-Null
    }
}

# 扫描范围：界面本体 + 其他 zssh-* crate 的 src。
$roots = @('native/zssh-gpui/src')
foreach ($crate in (Get-ChildItem -LiteralPath 'native' -Directory -Filter 'zssh-*')) {
    if ($crate.Name -eq 'zssh-gpui') { continue }
    $roots += ("native/{0}/src" -f $crate.Name)
}
$roots = $roots | ForEach-Object { Join-Path $repoRoot $_ }

$all = Invoke-Scan $roots ($repoRoot + [System.IO.Path]::DirectorySeparatorChar)

$violations = @($all | Where-Object { $_.Rule -eq 'hardcoded' -and -not $allow.Contains($_.File) })
$enViolations = @($all | Where-Object { $_.Rule -eq 'en-han' })
$tCalls = @($all | Where-Object { $_.Rule -eq 't-call' })
$remaining = @{}
foreach ($v in ($all | Where-Object { $_.Rule -eq 'hardcoded' -and $allow.Contains($_.File) })) {
    if ($remaining.ContainsKey($v.File)) { $remaining[$v.File]++ } else { $remaining[$v.File] = 1 }
}

if ($List) {
    Write-Host ("allow 清单里的文件还剩 {0} 个、共 {1} 处：" -f $remaining.Count, (($remaining.Values | Measure-Object -Sum).Sum))
    $remaining.GetEnumerator() | Sort-Object Value -Descending | ForEach-Object {
        Write-Host ("{0,5}  {1}" -f $_.Value, $_.Key)
    }
    Write-Host ("allow 清单之外：写死 {0} 处，en 表中文 {1} 处，表体 t( {2} 处。" -f $violations.Count, $enViolations.Count, $tCalls.Count)
    exit 0
}

$failed = $false
if ($enViolations.Count -gt 0) {
    $failed = $true
    Write-Host ("en 表里混进了中文：{0} 处" -f $enViolations.Count) -ForegroundColor Red
    $enViolations | Select-Object -First 40 | ForEach-Object { Write-Host ("  {0}:{1}  {2}" -f $_.File, $_.Line, $_.Text) }
}
if ($tCalls.Count -gt 0) {
    $failed = $true
    Write-Host ("zh / en 表体里调用了 t(（会递归）：{0} 处" -f $tCalls.Count) -ForegroundColor Red
    $tCalls | Select-Object -First 40 | ForEach-Object { Write-Host ("  {0}:{1}" -f $_.File, $_.Line) }
}
if ($violations.Count -gt 0) {
    $failed = $true
    Write-Host ("写死的中文文案（不在 {0} 里）：{1} 处" -f $allowFile, $violations.Count) -ForegroundColor Red
    $violations | Select-Object -First 200 | ForEach-Object { Write-Host ("  {0}:{1}  {2}" -f $_.File, $_.Line, $_.Text) }
    if ($violations.Count -gt 200) { Write-Host ("  …还有 {0} 处" -f ($violations.Count - 200)) }
}

if ($failed) {
    Write-Host '提示：把文案搬进 Msg 表；确实还没搬完的文件加进 scripts/i18n-allow.txt（按文件 burn-down）。' -ForegroundColor Yellow
    exit 1
}

$left = (($remaining.Values | Measure-Object -Sum).Sum)
if ($null -eq $left) { $left = 0 }
Write-Host ("i18n 覆盖检查通过；allow 清单里还有 {0} 个文件 / {1} 处待搬。" -f $remaining.Count, $left)
exit 0
