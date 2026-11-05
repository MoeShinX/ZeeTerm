# 英文版覆盖检查（V2-I18N）：界面文案必须走 i18n 的 Msg 表，不能把中文写死在调用点。
#
# 规则：native/zssh-gpui/src 下，除下列位置外，不允许出现含中文的字符串字面量：
#   - `fn zh(...)` 的表体（中文原文本来就该在那）；
#   - 测试（`mod tests { ... }`、`*_tests.rs`、`tests.rs`）；
#   - 只在调试构建里存在的文件（`dev_*`、`sync_stub`、`terminal_demo`、`fake_tools`、`sftp/script.rs`）；
#   - `scripts/i18n-allow.txt` 里列出的文件（还没搬完的，按文件 burn-down：清完一个就从清单删一行）。
#
# 另外检查每个 `fn en(...)` 表体里没有中文（漏翻时最容易出的错）。
#
# 用法：pwsh -NoProfile -File scripts/check-i18n.ps1 [-List]
#   -List  只打印统计（每个 allow 文件还剩多少处），不做通过/失败判断
# 退出码：0 = 通过；1 = 有 allow 清单之外的写死中文（或 en 表里混进中文）。

[CmdletBinding()]
param([switch]$List)

$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Set-Location (Join-Path $PSScriptRoot '..')

$sourceRoot = 'native/zssh-gpui/src'
$allowFile = 'scripts/i18n-allow.txt'

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

$allow = [System.Collections.Generic.HashSet[string]]::new()
if (Test-Path -LiteralPath $allowFile) {
    foreach ($line in Get-Content -LiteralPath $allowFile -Encoding UTF8) {
        $t = $line.Trim()
        if ($t -eq '' -or $t.StartsWith('#')) { continue }
        $allow.Add($t.Replace('\', '/')) | Out-Null
    }
}

function Test-IsTestFile([string]$name) {
    if ($name -eq 'tests.rs') { return $true }
    if ($name -match '_tests\.rs$') { return $true }
    if ($name -match '^test_.*\.rs$') { return $true }
    return $false
}

function Remove-StringLiterals([string]$line) {
    return [regex]::Replace($line, '"(?:\\.|[^"\\])*"', '""')
}

$violations = [System.Collections.Generic.List[object]]::new()
$enViolations = [System.Collections.Generic.List[object]]::new()
$remaining = @{}

$files = Get-ChildItem -LiteralPath (Join-Path $sourceRoot '') -Recurse -File -Filter *.rs
foreach ($file in $files) {
    $rel = $file.FullName.Substring((Get-Location).Path.Length + 1).Replace('\', '/')
    $name = $file.Name
    if (Test-IsTestFile $name) { continue }
    if ($devOnly -contains $name) { continue }
    if ($rel -match '/dev_acts') { continue }

    $lines = [System.IO.File]::ReadAllLines($file.FullName)
    $zhDepth = 0
    $enDepth = 0
    $testDepth = 0
    $cfgTestPending = $false
    $count = 0

    for ($i = 0; $i -lt $lines.Count; $i++) {
        $line = $lines[$i]
        $code = Remove-StringLiterals $line
        $delta = ([regex]::Matches($code, '\{')).Count - ([regex]::Matches($code, '\}')).Count

        if ($zhDepth -eq 0 -and $line -match 'fn zh\s*\(') { $zhDepth = 1 + $delta; if ($zhDepth -lt 1) { $zhDepth = 1 }; continue }
        if ($zhDepth -gt 0) {
            $zhDepth += $delta
            if ($zhDepth -le 0) { $zhDepth = 0 }
            continue
        }

        if ($enDepth -eq 0 -and $line -match 'fn en\s*\(') { $enDepth = 1 + $delta; if ($enDepth -lt 1) { $enDepth = 1 }; continue }
        if ($enDepth -gt 0) {
            $enDepth += $delta
            if ($enDepth -le 0) { $enDepth = 0 }
            if ($line -match '"[^"]*[\u4e00-\u9fff]' -and $line -notmatch '简体中文') {
                $enViolations.Add([pscustomobject]@{ File = $rel; Line = $i + 1; Text = $line.Trim() })
            }
            continue
        }

        # 测试专用条目：`#[cfg(test)]` 后面的 impl / mod / fn 整块跳过（MockBackend 之类的假实现。
        if ($testDepth -eq 0 -and $line -match '^\s*#\[cfg\(test\)\]\s*$') { $cfgTestPending = $true; continue }
        if ($cfgTestPending) {
            if ($line -match '\{') { $testDepth = 1 + $delta; if ($testDepth -lt 1) { $testDepth = 1 }; $cfgTestPending = $false }
            continue
        }
        if ($testDepth -eq 0 -and $line -match '^\s*mod \w*tests\w*\s*\{') { $testDepth = 1 + $delta; if ($testDepth -lt 1) { $testDepth = 1 }; continue }
        if ($testDepth -gt 0) {
            $testDepth += $delta
            if ($testDepth -le 0) { $testDepth = 0 }
            continue
        }

        $trim = $line.TrimStart()
        if ($trim.StartsWith('//')) { continue }
        # 中文语言分支等显式豁免：行尾写 `// i18n-allow: 原因`。
        if ($line -match 'i18n-allow') { continue }
        if ($trim -match '^(assert|debug_assert|panic!|eprintln!|println!)') { continue }
        if ($trim -match '^(tracing::|log::|debug!|trace!|warn!|info!|error!)') { continue }

        if ($line -match '"[^"]*[\u4e00-\u9fff]') {
            $count++
            if (-not $allow.Contains($rel)) {
                $violations.Add([pscustomobject]@{ File = $rel; Line = $i + 1; Text = $line.Trim() })
            }
        }
    }
    if ($count -gt 0) { $remaining[$rel] = $count }
}

if ($List) {
    Write-Host ("allow 清单里的文件还剩 {0} 个、共 {1} 处：" -f $remaining.Count, (($remaining.Values | Measure-Object -Sum).Sum))
    $remaining.GetEnumerator() | Sort-Object Value -Descending | ForEach-Object {
        Write-Host ("{0,5}  {1}" -f $_.Value, $_.Key)
    }
    exit 0
}

$failed = $false
if ($enViolations.Count -gt 0) {
    $failed = $true
    Write-Host ("en 表里混进了中文：{0} 处" -f $enViolations.Count) -ForegroundColor Red
    $enViolations | Select-Object -First 20 | ForEach-Object { Write-Host ("  {0}:{1}  {2}" -f $_.File, $_.Line, $_.Text) }
}
if ($violations.Count -gt 0) {
    $failed = $true
    Write-Host ("写死的中文文案（不在 {0} 里）：{1} 处" -f $allowFile, $violations.Count) -ForegroundColor Red
    $violations | Select-Object -First 40 | ForEach-Object { Write-Host ("  {0}:{1}  {2}" -f $_.File, $_.Line, $_.Text) }
    if ($violations.Count -gt 40) { Write-Host ("  …还有 {0} 处" -f ($violations.Count - 40)) }
}

if ($failed) {
    Write-Host '提示：把文案搬进 Msg 表；确实还没搬完的文件加进 scripts/i18n-allow.txt（按文件 burn-down）。' -ForegroundColor Yellow
    exit 1
}

$left = (($remaining.Values | Measure-Object -Sum).Sum)
Write-Host ("i18n 覆盖检查通过；allow 清单里还有 {0} 个文件 / {1} 处待搬。" -f $remaining.Count, $left)
exit 0
