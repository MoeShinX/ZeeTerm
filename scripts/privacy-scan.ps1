# 公开前的隐私扫描：对"要放进公开仓库的每一个文件"（含二进制）查找开发机的个人信息和密钥。
#
# 用法（在仓库根目录）：
#   powershell -File scripts/privacy-scan.ps1                 # 扫描 git 跟踪的 + 未被忽略的新文件（排除不公开的目录）
#   powershell -File scripts/privacy-scan.ps1 -Root <目录>     # 扫描某个目录下的所有文件（例如检出的 public 分支）
#   powershell -File scripts/privacy-scan.ps1 -SelfTest       # 自检：造一个带"假敏感值"的文件，必须命中
#   powershell -File scripts/privacy-scan.ps1 -ExtraPatternsFile <仓库外的文件>
#       额外的私有模式：文件里每行一条字面量（大小写不敏感、按字面子串匹配，空行忽略，不支持注释），
#       用来查"不能写进仓库"的具体值（别的用户名、私人 IP、内部域名、本机路径……）。
#       这个文件必须放在仓库外面；输出里只报"文件:位置 + 类别"，不打印命中的内容，也不打印模式本身。
#   powershell -File scripts/privacy-scan.ps1 -AllowListFile <文件>
#       放行清单：每行一条 .NET 正则（空行和 # 开头的行忽略），从命中的位置（或它前面最多 64 个字符，好让
#       `C:\Users\` 这样的前缀也算进占位里）起头匹配（\G），匹配上就当作中性占位放行。不给这个参数就默认用
#       scripts/privacy-allow.txt，所以按上面的用法直接跑就带着放行清单；给了就只用给的那份。
#       放行对所有命中都生效——外部模式文件的条目，也包括内置的用户名 / 用户目录一类——但有两道闸，
#       免得真值溜过去：(1) 占位的匹配必须把"整段命中"都盖住，只盖住一部分不算；(2) 占位文字里含本机真实的
#       用户目录、或 `\Users\<本机用户名>` 时一律不放行（开发机的用户名恰好撞上占位名也一样要报）。
#       固定样式（令牌、私钥、公网 IP、邮箱）不看放行清单，照查。
#       用法举例：外部模式里有 `C:\Users\` 这样的宽模式，而代码里确实要写 `C:\Users\<用户>`、`C:\Users\alice`
#       这类占位来测路径脱敏——把"哪些占位算中性"写进放行清单，清单里不能放任何真实的个人值。
#
# 敏感值全部在运行时从环境变量和当前系统读取，脚本里不写任何真实值：
#   当前 Windows 用户名和用户目录、计算机名、git 的 user.name / user.email、
#   ZSSH_TEST_* 变量里的值（服务器地址、用户名、密码、代理地址等）、
#   本机智能体配置里用户自己配的 MCP 服务名（读不了就当没有，只读不改）。
# 另外查固定样式：令牌 / 密钥 / 私钥头、公网 IPv4、非 noreply 的邮箱。
#
# 输出只写"文件:位置 + 类别"，不打印命中的内容（二进制文件写字节偏移）。有命中退出码 1。
# 故意写在测试里的假值：令牌 / 密钥样式（ghp_、github_pat_、AKIA、sk-）只看**命中的值本身**——值里带 fake / dummy / example /
# placeholder / redacted / xxxx / test / 连续 7 个以上的 0、或者前缀之后整串是同一个字符，才算假值；不看整行
# （同一行上写个 `latest=` 或 `# test` 不能把真令牌放过去）。私钥头命中的值里没有可判断的内容，仍按行判断，
# 但标记必须是整词（mock / fake / dummy / example / placeholder / sample / test / redacted），`latest` 之类不算。

param(
    [string]$Root = "",
    [string]$ExtraPatternsFile = "",
    [string]$AllowListFile = "",
    [switch]$SelfTest
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repo = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

# 没指定放行清单就用仓库里这份：文档给的命令（不带 -AllowListFile）也要带着它，否则它形同虚设。
$defaultAllowList = Join-Path $PSScriptRoot "privacy-allow.txt"
$usingDefaultAllowList = -not $AllowListFile
if ($usingDefaultAllowList) { $AllowListFile = $defaultAllowList }

# 本机真实的身份（小写、反斜杠形式）：占位文字里出现它们就不能当占位放行。
$realIdentities = New-Object System.Collections.Generic.List[string]
foreach ($id in @($env:USERPROFILE, $(if ($env:USERNAME) { "\users\" + $env:USERNAME }))) {
    if ([string]::IsNullOrWhiteSpace($id)) { continue }
    $realIdentities.Add((($id.Trim() -replace '/', '\').TrimEnd('\')).ToLowerInvariant())
}

# 不公开的东西：开发过程记录、发版产物。
$excludedPrefixes = @("docs/internal/", "dist/", ".claude/")

function Get-Needles {
    $needles = New-Object System.Collections.Generic.List[object]
    $add = {
        param([string]$kind, [string]$value)
        if ([string]::IsNullOrWhiteSpace($value)) { return }
        $v = $value.Trim()
        if ($v.Length -lt 3) { return }
        $needles.Add([pscustomobject]@{ Kind = $kind; Value = $v })
    }
    & $add "本机用户名" $env:USERNAME
    & $add "用户目录" $env:USERPROFILE
    & $add "计算机名" $env:COMPUTERNAME
    & $add "git 用户名" (& git config user.name 2>$null)
    & $add "git 邮箱" (& git config user.email 2>$null)
    foreach ($item in (Get-ChildItem Env: | Where-Object { $_.Name -like "ZSSH_TEST_*" -or $_.Name -like "ZSSH_SHOT_*" })) {
        $value = [string]$item.Value
        if ($item.Name -match "(PORT|FULL|E2E)$") { continue }
        if ($item.Name -match "SOCKS5|PROXY" -and $value -match "^(.+):\d+$") { $value = $Matches[1] }
        if ($value -in @("127.0.0.1", "localhost", "1", "0", "true", "false")) { continue }
        # 太通用的用户名（root、admin 之类）满篇都是，不当敏感值；其余照查。
        if ($item.Name -like "*_USER*" -and $value -in @("root", "admin", "administrator", "ubuntu", "user", "test", "ec2-user", "debian")) { continue }
        & $add ("环境变量 " + $item.Name) $value
    }
    # 用户自己配的 MCP 服务名：从常见智能体配置里读 key，读不了就跳过。
    $profileDir = $env:USERPROFILE
    if ($profileDir) {
        $jsonFiles = @((Join-Path $profileDir ".claude.json"), (Join-Path $profileDir ".config\opencode\opencode.json"), (Join-Path $profileDir ".claude\settings.json"))
        foreach ($file in $jsonFiles) {
            try {
                if (-not (Test-Path -LiteralPath $file)) { continue }
                $json = Get-Content -LiteralPath $file -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
                foreach ($prop in @("mcpServers", "mcp")) {
                    $node = $json.$prop
                    if ($node) { foreach ($name in $node.PSObject.Properties.Name) { & $add "MCP 服务名" $name } }
                }
            } catch { }
        }
        $toml = Join-Path $profileDir ".codex\config.toml"
        try {
            if (Test-Path -LiteralPath $toml) {
                foreach ($m in [regex]::Matches((Get-Content -LiteralPath $toml -Raw), '(?m)^\[mcp_servers\.([^\].]+)')) { & $add "MCP 服务名" $m.Groups[1].Value }
            }
        } catch { }
    }
    # 外部模式文件：按字面子串匹配（不加单词边界），所以 Literal = $true。读不了就报错退出，
    # 不能因为读不到就当"没有这些模式"。
    if ($ExtraPatternsFile) {
        if (-not (Test-Path -LiteralPath $ExtraPatternsFile -PathType Leaf)) {
            Write-Host "读不到 -ExtraPatternsFile 指定的文件。" -ForegroundColor Red
            exit 2
        }
        $extraCount = 0
        foreach ($line in [System.IO.File]::ReadAllLines($ExtraPatternsFile, (New-Object System.Text.UTF8Encoding($false)))) {
            $v = $line.TrimStart([char]0xFEFF).Trim()
            if ($v.Length -eq 0) { continue }
            $needles.Add([pscustomobject]@{ Kind = "外部模式文件"; Value = $v; Literal = $true })
            $extraCount++
        }
        Write-Host ("外部模式文件：{0} 条模式" -f $extraCount)
    }
    return @($needles | Sort-Object Kind, Value -Unique)
}

# 固定样式。正则在 Latin1 字节串上跑，所以二进制文件也能扫。
$patterns = @(
    @{ Kind = "令牌样式 ghp_";        Re = 'ghp_[A-Za-z0-9]{8,}';                  Fake = "value" },
    @{ Kind = "令牌样式 github_pat_"; Re = 'github_pat_[A-Za-z0-9_]{8,}';          Fake = "value" },
    @{ Kind = "密钥样式 AKIA";        Re = 'AKIA[0-9A-Z]{12,}';                    Fake = "value" },
    @{ Kind = "私钥头";               Re = '-----BEGIN [A-Z ]*PRIVATE KEY';        Fake = "line" },
    @{ Kind = "密钥样式 sk-";         Re = '(?<![A-Za-z0-9])sk-[A-Za-z0-9]{16,}';  Fake = "value" },
    @{ Kind = "邮箱";                 Re = '[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}'; Fake = "" }
)
$ipv4 = '(?<![0-9.])(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})(?![0-9.])'
# 令牌 / 密钥样式：命中的值本身带这些标记之一才算假值（大小写不敏感）。
$fakeValueMarker = '(?i)fake|dummy|example|placeholder|redacted|xxxx|test|0{7,}'
# 私钥头：命中值里没有内容，按所在行判断；标记必须是整词（前后不是字母数字，`_` `-` 当分隔），
# 所以 `latest`、`testing`、`contest` 都不再把整行放过。
$fakeLineMarker = '(?i)(?<![A-Za-z0-9])(mock|fake|dummy|example|placeholder|sample|test|redacted)(?![A-Za-z0-9])'

# 令牌 / 密钥样式的命中值是不是明显的假值：带假值标记，或者去掉前缀之后整串是同一个字符重复。
function Test-FakeTokenValue([string]$value) {
    if ($value -match $fakeValueMarker) { return $true }
    $body = $value -creplace '^(ghp_|github_pat_|sk-|AKIA)', ''
    return ($body.Length -gt 0 -and $body -cmatch '^(.)\1*$')
}

function Test-PublicIpv4([string]$a, [string]$b, [string]$c, [string]$d) {
    $o = @([int]$a, [int]$b, [int]$c, [int]$d)
    foreach ($x in $o) { if ($x -gt 255) { return $false } }
    if ($o[0] -eq 10 -or $o[0] -eq 127 -or $o[0] -eq 0 -or $o[0] -ge 224) { return $false }
    if ($o[0] -eq 192 -and $o[1] -eq 168) { return $false }
    if ($o[0] -eq 172 -and $o[1] -ge 16 -and $o[1] -le 31) { return $false }
    if ($o[0] -eq 169 -and $o[1] -eq 254) { return $false }
    # 文档约定的示例地址
    if ($o[0] -eq 192 -and $o[1] -eq 0 -and $o[2] -eq 2) { return $false }
    if ($o[0] -eq 198 -and $o[1] -eq 51 -and $o[2] -eq 100) { return $false }
    if ($o[0] -eq 203 -and $o[1] -eq 0 -and $o[2] -eq 113) { return $false }
    return $true
}

# 要扫描的文件清单。
# - 指定 $RootDir（导出目录，没有 git）：目录下所有文件都列出来，包括不公开目录里的——导出物里不该有它们，
#   由 Invoke-Scan 的"不该进仓库的文件"检查报出来。
# - 否则按 git：已跟踪的文件一律列出（哪怕在不公开目录里，那正是要报的误跟踪）；未跟踪的本地私人目录
#   （$excludedPrefixes）照旧排除，不扫也不报。
function Get-FileList([string]$RepoDir, [string]$RootDir) {
    if ($RootDir) {
        $base = (Resolve-Path $RootDir).Path
        return @(Get-ChildItem -LiteralPath $base -Recurse -File -Force | Where-Object { $_.FullName -notmatch '[\\/]\.git[\\/]' } | ForEach-Object {
            $rel = $_.FullName.Substring($base.Length).TrimStart('\', '/').Replace('\', '/')
            [pscustomobject]@{ Rel = $rel; Full = $_.FullName; Missing = $false }
        })
    }
    # -z：文件名不被 git 转义成带引号的形式（带中文或特殊字符的名字否则认不出前缀）。
    Push-Location $RepoDir
    try {
        $trackedRaw = (& git ls-files -z --cached) -join "`n"
        $allRaw = (& git ls-files -z --cached --others --exclude-standard) -join "`n"
    } finally { Pop-Location }
    $nul = [string][char]0
    $tracked = New-Object System.Collections.Generic.HashSet[string] ([System.StringComparer]::Ordinal)
    foreach ($n in $trackedRaw.Split($nul, [System.StringSplitOptions]::RemoveEmptyEntries)) { [void]$tracked.Add($n.Trim("`n")) }
    $out = @()
    foreach ($n in $allRaw.Split($nul, [System.StringSplitOptions]::RemoveEmptyEntries)) {
        $n = $n.Trim("`n")
        $inPrivateDir = $false
        foreach ($p in $excludedPrefixes) { if ($n.StartsWith($p)) { $inPrivateDir = $true; break } }
        if ($inPrivateDir -and -not $tracked.Contains($n)) { continue }
        $full = Join-Path $RepoDir $n
        if (Test-Path -LiteralPath $full -PathType Leaf) { $out += [pscustomobject]@{ Rel = $n; Full = $full; Missing = $false } }
        # 索引里还有、工作区已删：路径本身照样要报，没有内容可扫。
        elseif ($inPrivateDir) { $out += [pscustomobject]@{ Rel = $n; Full = $full; Missing = $true } }
    }
    return $out
}

# 不该出现在公开仓库 / 导出物里的路径：本地私人目录、日志、.env（.env.example 除外）。
function Test-ForbiddenPath([string]$rel) {
    if ($rel -match '\.env\.example$') { return $false }
    if ($rel -match '(^|/)\.claude/|\.log$|(^|/)\.env') { return $true }
    foreach ($p in $excludedPrefixes) { if ($rel.StartsWith($p)) { return $true } }
    return $false
}

function Get-Position([string]$hay, [int]$index, [bool]$isBinary) {
    if ($isBinary) { return "字节偏移 $index" }
    $line = 1 + ([regex]::Matches($hay.Substring(0, $index), "`n")).Count
    return "第 $line 行"
}

function Get-LineAt([string]$hay, [int]$index) {
    $start = $hay.LastIndexOf("`n", [Math]::Max(0, $index - 1)) + 1
    $end = $hay.IndexOf("`n", $index); if ($end -lt 0) { $end = $hay.Length }
    return $hay.Substring($start, $end - $start)
}

# 放行清单（见文件头）：读不了或里面的正则写错，都报错退出，不悄悄当成"没有清单"。
function Get-AllowRegexes {
    $list = New-Object System.Collections.Generic.List[regex]
    if (-not $AllowListFile) { return , $list }
    if (-not (Test-Path -LiteralPath $AllowListFile -PathType Leaf)) {
        if ($usingDefaultAllowList) { Write-Host "读不到默认放行清单 scripts/privacy-allow.txt。" -ForegroundColor Red }
        else { Write-Host "读不到 -AllowListFile 指定的文件。" -ForegroundColor Red }
        exit 2
    }
    foreach ($line in [System.IO.File]::ReadAllLines($AllowListFile, (New-Object System.Text.UTF8Encoding($false)))) {
        $v = $line.TrimStart([char]0xFEFF).Trim()
        if ($v.Length -eq 0 -or $v.StartsWith("#")) { continue }
        try { $list.Add((New-Object System.Text.RegularExpressions.Regex($v, 'IgnoreCase'))) }
        catch { Write-Host "放行清单里有不合法的正则：$($_.Exception.Message)" -ForegroundColor Red; exit 2 }
    }
    return , $list
}

# 这个命中是不是一段完整的中性占位的一部分。放行正则从命中处或它前面最多 64 个字符起头匹配，
# 必须把整段命中盖住；占位文字里含本机真实用户目录 / 用户名时不放行。
# $ctx 是命中周围的文字，$hitStart / $hitLength 是命中在 $ctx 里的位置和长度（字符数）。
function Test-Waived($allow, [string]$ctx, [int]$hitStart, [int]$hitLength) {
    $from = [Math]::Max(0, $hitStart - 64)
    for ($p = $hitStart; $p -ge $from; $p--) {
        foreach ($ar in $allow) {
            $m = $ar.Match($ctx, $p)
            if (-not $m.Success -or $m.Index -ne $p -or ($p + $m.Length) -lt ($hitStart + $hitLength)) { continue }
            $covered = ($m.Value -replace '\\\\', '\' -replace '/', '\').ToLowerInvariant()
            $real = $false
            foreach ($id in $realIdentities) { if ($covered.Contains($id)) { $real = $true; break } }
            if (-not $real) { return $true }
        }
    }
    return $false
}

function Invoke-Scan($files, $needles) {
    $allow = Get-AllowRegexes
    $latin1 = [System.Text.Encoding]::GetEncoding(28591)
    $hits = New-Object System.Collections.Generic.List[string]
    foreach ($f in $files) {
        if (Test-ForbiddenPath $f.Rel) {
            $hits.Add(("{0}  [不该进仓库的文件]" -f $f.Rel))
        }
        if ($f.Missing) { continue }
        $bytes = [System.IO.File]::ReadAllBytes($f.Full)
        $isBinary = $false
        $probe = [Math]::Min($bytes.Length, 8000)
        for ($i = 0; $i -lt $probe; $i++) { if ($bytes[$i] -eq 0) { $isBinary = $true; break } }
        $text = $latin1.GetString($bytes)
        # 文本文件多半是 UTF-8（中文用户名按 UTF-8 再看一遍）；二进制文件再按 UTF-16 看一遍。
        $views = @($text)
        if (-not $isBinary) { $views += [System.Text.Encoding]::UTF8.GetString($bytes) }
        else { $views += [System.Text.Encoding]::Unicode.GetString($bytes) }
        $seen = @{}
        $viewNo = 0
        foreach ($view in $views) {
            $viewNo++
            foreach ($n in $needles) {
                $re = '(?<![A-Za-z0-9])' + [regex]::Escape($n.Value) + '(?![A-Za-z0-9])'
                if ($n.Literal) { $re = [regex]::Escape($n.Value) }
                foreach ($m in [regex]::Matches($view, $re, 'IgnoreCase')) {
                    if ($allow.Count -gt 0) {
                        # 第一个视图是 Latin1（下标 = 字节偏移）：放行正则要按 UTF-8 看命中处前后的字，否则带中文的占位匹配不上。
                        $ctx = $view; $at = $m.Index; $len = $m.Length
                        if ($viewNo -eq 1) {
                            $back = [Math]::Min(64, $m.Index)
                            $take = [Math]::Min(96 + $m.Length, $bytes.Length - $m.Index)
                            $pre = [System.Text.Encoding]::UTF8.GetString($bytes, $m.Index - $back, $back)
                            $ctx = $pre + [System.Text.Encoding]::UTF8.GetString($bytes, $m.Index, $take)
                            $at = $pre.Length
                            $len = [System.Text.Encoding]::UTF8.GetString($bytes, $m.Index, $m.Length).Length
                        }
                        if (Test-Waived $allow $ctx $at $len) { continue }
                    }
                    $pos = Get-Position $view $m.Index $isBinary
                    $key = "$($n.Kind)|$pos"
                    if ($seen[$key]) { continue }
                    $seen[$key] = $true
                    $hits.Add(("{0}:{1}  [{2}]" -f $f.Rel, $pos, $n.Kind))
                }
            }
        }
        # WOFF2 字体是 Brotli 压缩过的数据：随机字节会碰巧凑成"邮箱 / IP"的样子，按格式匹配没有意义
        # （里面的文字都压缩了，真有敏感内容也看不出来）。具体的敏感值（上面的 $needles）照常查。
        $shapeScan = $f.Rel -notmatch '\.woff2$'
        foreach ($p in $patterns) {
            if (-not $shapeScan) { break }
            foreach ($m in [regex]::Matches($text, $p.Re)) {
                if ($p.Fake -eq "value" -and (Test-FakeTokenValue $m.Value)) { continue }
                if ($p.Fake -eq "line" -and ((Get-LineAt $text $m.Index) -match $fakeLineMarker)) { continue }
                if ($p.Kind -eq "邮箱" -and ($m.Value -match 'noreply|@example\.|@([A-Za-z0-9-]+\.)+example\.(com|net|org)$|@localhost|\.example$|\.internal$|@openssh\.com$|\.(png|jpe?g|gif|svg|ico|webp|icns)$')) { continue }
                if ($p.Kind -eq "邮箱" -and $f.Rel -match '(^|/)(package-lock\.json|Cargo\.lock)$') { continue }
                $pos = Get-Position $text $m.Index $isBinary
                $key = "$($p.Kind)|$pos"
                if ($seen[$key]) { continue }
                $seen[$key] = $true
                $hits.Add(("{0}:{1}  [{2}]" -f $f.Rel, $pos, $p.Kind))
            }
        }
        # SVG 的路径数据是一长串小数（四段小数连在一起很常见），按 IP 的样子查全是误报；具体的敏感值照常查。
        $ipScan = $shapeScan -and $f.Rel -notmatch '\.svg$'
        foreach ($m in [regex]::Matches($text, $ipv4)) {
            if (-not $ipScan) { break }
            if (Test-PublicIpv4 $m.Groups[1].Value $m.Groups[2].Value $m.Groups[3].Value $m.Groups[4].Value) {
                $pos = Get-Position $text $m.Index $isBinary
                $key = "ip|$pos"
                if ($seen[$key]) { continue }
                $seen[$key] = $true
                $hits.Add(("{0}:{1}  [公网 IPv4 样式]" -f $f.Rel, $pos))
            }
        }
    }
    return $hits
}

$needles = Get-Needles
if ($SelfTest) {
    # 造一个带"运行时才拼出来的假敏感值"的文件：用户名、令牌样式、公网 IP、邮箱各一个，外加一个二进制文件。
    $dir = Join-Path ([System.IO.Path]::GetTempPath()) ("zssh-privacy-selftest-" + [guid]::NewGuid().ToString("N").Substring(0, 8))
    New-Item -ItemType Directory -Force $dir | Out-Null
    try {
        $userNeedle = ($needles | Where-Object { $_.Kind -eq "本机用户名" } | Select-Object -First 1).Value
        # 外部模式文件：造一个只含一条假模式的文件，经过和真实使用同一条读取路径；命中要按子串、不分大小写。
        $fakeLiteral = "zz-fake-literal-" + "9"
        $patternFile = Join-Path $dir "patterns.txt"
        $fakeLong = "zz-fake-long-9"
        [System.IO.File]::WriteAllLines($patternFile, @($fakeLiteral, ($fakeLong + "-tail"), ""))
        $script:ExtraPatternsFile = $patternFile
        $needles = Get-Needles
        # 内置那类（非字面量）命中也要吃放行清单：造一个假的"用户目录"类敏感值，不碰真实的。
        $fakeUser = "zz-fake-user-9"
        $needles = @($needles) + [pscustomobject]@{ Kind = "用户目录"; Value = $fakeUser }
        $lines = @(
            ("owner=" + $userNeedle),
            ("t=" + "ghp" + "_" + ("A1b2C3d4" * 3)),
            ("ip=" + ((8, 8, 4, 4) -join ".")),
            ("mail=" + "someone" + "@" + "corp-mail.net"),
            ("extra=Prefix-" + $fakeLiteral.ToUpper() + "-suffix")
        )
        [System.IO.File]::WriteAllLines((Join-Path $dir "sample.txt"), $lines)
        $bin = [byte[]](0, 1, 2) + [System.Text.Encoding]::ASCII.GetBytes($userNeedle) + [byte[]](0, 3)
        [System.IO.File]::WriteAllBytes((Join-Path $dir "sample.bin"), $bin)
        # 放行清单：只在外部模式命中的位置上起头匹配；匹配上的放行，别处的同一个模式照样命中。
        $allowFile = Join-Path $dir "allow.txt"
        $allowLines = @(
            "# 注释", "",
            ("\G" + [regex]::Escape($fakeLiteral) + "-allowed"),
            ("\G" + [regex]::Escape($fakeLiteral) + "-张"),
            # 占位只盖住了命中的一半：不算整段占位，不放行。
            ("\G" + [regex]::Escape($fakeLong)),
            # 内置类命中：`C:\Users\<假名字>` 整段是占位。
            ("\GC:(\\|/)Users(\\|/)" + [regex]::Escape($fakeUser) + "(?![\p{L}\p{N}])")
        )
        # 把本机真实用户名也写成"占位"：不管清单里怎么写，真值不能被放行。
        if ($env:USERNAME) { $allowLines += ("\GC:(\\|/)Users(\\|/)" + [regex]::Escape($env:USERNAME)) }
        [System.IO.File]::WriteAllLines($allowFile, $allowLines)
        $script:AllowListFile = $allowFile
        $script:usingDefaultAllowList = $false
        [System.IO.File]::WriteAllLines((Join-Path $dir "waived-builtin.txt"), @("p=C:\Users\" + $fakeUser + "\bin"))
        [System.IO.File]::WriteAllLines((Join-Path $dir "bare-builtin.txt"), @("name=" + $fakeUser))
        [System.IO.File]::WriteAllLines((Join-Path $dir "partial.txt"), @("x=" + $fakeLong + "-tail"))
        if ($env:USERNAME) { [System.IO.File]::WriteAllLines((Join-Path $dir "real-path.txt"), @("p=C:\Users\" + $env:USERNAME + "\x")) }
        [System.IO.File]::WriteAllLines((Join-Path $dir "waived.txt"), @("x=" + $fakeLiteral + "-allowed"))
        # 放行正则要能认出带中文的占位（命中处在 Latin1 视图里，按 UTF-8 看后面的字）。
        [System.IO.File]::WriteAllLines((Join-Path $dir "waived-cjk.txt"), @("x=" + $fakeLiteral + "-张"), (New-Object System.Text.UTF8Encoding($false)))
        # 令牌样式是否放行只看命中的值：同一行写了 latest= 的随机令牌必须命中；值本身带假值标记的才放行。
        [System.IO.File]::WriteAllLines((Join-Path $dir "tok-latest.txt"), @("latest=" + "ghp" + "_" + ("Q9w8E7r6" * 3)))
        [System.IO.File]::WriteAllLines((Join-Path $dir "tok-fakevalue.txt"), @("t=" + "ghp" + "_" + "FAKEFAKEFAKEFAKE"))
        $files = @(Get-ChildItem -LiteralPath $dir -File | ForEach-Object { [pscustomobject]@{ Rel = $_.Name; Full = $_.FullName } })
        $hits = Invoke-Scan $files $needles
        $kinds = @($hits | ForEach-Object { ($_ -split '\[')[-1].TrimEnd(']') })
        $need = @("令牌样式 ghp_", "公网 IPv4 样式", "邮箱", "本机用户名", "外部模式文件")
        $missing = @($need | Where-Object { $kinds -notcontains $_ })
        $binHit = @($hits | Where-Object { $_ -like "sample.bin:*" }).Count
        if (@($hits | Where-Object { $_ -like "waived*.txt:*" }).Count -gt 0) {
            Write-Host "自检失败：放行清单没有放行该放行的命中。" -ForegroundColor Red
            exit 1
        }
        # 不该放行的必须仍然命中：光秃秃的假值（没有占位前缀）、占位只盖住一部分、本机真实用户名。
        if (@($hits | Where-Object { $_ -like "tok-fakevalue.txt:*" }).Count -gt 0) {
            Write-Host "自检失败：值本身带假值标记的令牌样式不该命中。" -ForegroundColor Red
            exit 1
        }
        $mustHit = @("bare-builtin.txt", "partial.txt", "tok-latest.txt")
        if ($env:USERNAME) { $mustHit += "real-path.txt" }
        $leaked = @($mustHit | Where-Object { $name = $_; @($hits | Where-Object { $_ -like "${name}:*" }).Count -eq 0 })
        if ($leaked.Count -gt 0) {
            Write-Host ("自检失败：这些文件里的命中被错误放行了：" + ($leaked -join "、")) -ForegroundColor Red
            exit 1
        }
        if ($missing.Count -gt 0 -or $binHit -eq 0) {
            Write-Host ("自检失败：没有命中 " + ($missing -join "、") + $(if ($binHit -eq 0) { "；二进制文件里的用户名也没命中" } else { "" })) -ForegroundColor Red
            exit 1
        }
        # 不公开目录（.claude/、docs/internal/、dist/）：隔离的临时 git 仓库里造 已跟踪 / 未跟踪 / 跟踪后又删掉 三种，
        # 再用 -Root 的方式扫同一个目录。已跟踪的必须报"不该进仓库的文件"，未跟踪的本地私人目录在 git 模式下照旧排除，
        # -Root 模式（导出物，没有 git）里这些目录下的文件一律要报；普通文件任何模式下都不报。
        $fixture = Join-Path $dir "path-fixture"
        New-Item -ItemType Directory -Force $fixture | Out-Null
        & git -C $fixture init -q 2>$null
        if ($LASTEXITCODE -ne 0) { Write-Host "自检失败：造不出临时 git 仓库。" -ForegroundColor Red; exit 1 }
        $trackedRel = @("src/ok.txt", ".claude/tracked.txt", "docs/internal/tracked.md", "dist/tracked.txt", ".claude/gone.txt")
        $untrackedRel = @(".claude/untracked.txt", "docs/internal/untracked.md", "dist/untracked.txt")
        foreach ($rel in ($trackedRel + $untrackedRel)) {
            $target = Join-Path $fixture $rel
            New-Item -ItemType Directory -Force (Split-Path $target -Parent) | Out-Null
            [System.IO.File]::WriteAllText($target, "placeholder`n")
        }
        # -f：不让全局 / 仓库的忽略规则挡住"误跟踪"的情形。
        # 换行转换的提示走 stderr，在 Stop 模式下会变成终止错误：关掉转换，并临时放宽错误模式。
        $prevEap = $ErrorActionPreference; $ErrorActionPreference = "Continue"
        & git -C $fixture -c core.autocrlf=false -c core.safecrlf=false add -f -- $trackedRel 2>$null
        $addCode = $LASTEXITCODE
        $ErrorActionPreference = $prevEap
        if ($addCode -ne 0) { Write-Host "自检失败：临时仓库里 git add 失败。" -ForegroundColor Red; exit 1 }
        Remove-Item -LiteralPath (Join-Path $fixture ".claude/gone.txt") -Force
        $forbiddenSuffix = "  [不该进仓库的文件]"
        $forbiddenFrom = { param($hitList) @($hitList | Where-Object { $_ -and $_.EndsWith($forbiddenSuffix) } | ForEach-Object { $_.Substring(0, $_.Length - $forbiddenSuffix.Length) }) }

        $gitList = Get-FileList $fixture ""
        $gitRels = @($gitList | ForEach-Object { $_.Rel })
        $gitForbidden = & $forbiddenFrom (Invoke-Scan $gitList @())
        $problems = New-Object System.Collections.Generic.List[string]
        foreach ($rel in @(".claude/tracked.txt", "docs/internal/tracked.md", "dist/tracked.txt", ".claude/gone.txt")) {
            if ($gitForbidden -notcontains $rel) { $problems.Add("git 模式没报已跟踪的 $rel") }
        }
        foreach ($rel in $untrackedRel) {
            if ($gitRels -contains $rel) { $problems.Add("git 模式扫了未跟踪的本地目录 $rel") }
        }
        if ($gitForbidden -contains "src/ok.txt") { $problems.Add("git 模式误报了普通文件") }
        if ($gitRels -notcontains "src/ok.txt") { $problems.Add("git 模式漏了普通文件") }

        $rootList = Get-FileList $repo $fixture
        $rootForbidden = & $forbiddenFrom (Invoke-Scan $rootList @())
        foreach ($rel in @(".claude/tracked.txt", ".claude/untracked.txt", "docs/internal/untracked.md", "dist/untracked.txt")) {
            if ($rootForbidden -notcontains $rel) { $problems.Add("-Root 模式没报 $rel") }
        }
        if ($rootForbidden -contains "src/ok.txt") { $problems.Add("-Root 模式误报了普通文件") }
        if ($problems.Count -gt 0) {
            Write-Host ("自检失败（不公开目录）：" + ($problems -join "；")) -ForegroundColor Red
            exit 1
        }
        Write-Host ("自检通过：5 类假值（含外部模式文件）和二进制文件里的用户名都命中了；放行清单对内置类命中也生效，且不放行光秃秃的值、只盖住一部分的占位和本机真实用户名；不公开目录：已跟踪的会报、未跟踪的 git 模式下排除、-Root 模式下都报（{0} 条命中）" -f $hits.Count) -ForegroundColor Green
        exit 0
    } finally { Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue }
}

$files = Get-FileList $repo $Root
Write-Host ("扫描 {0} 个文件；敏感值来源 {1} 项（类别：{2}）" -f $files.Count, $needles.Count, ((($needles | ForEach-Object { $_.Kind }) | Sort-Object -Unique) -join "、"))
Write-Host ("放行清单：{0}（{1} 条）" -f $(if ($usingDefaultAllowList) { "scripts/privacy-allow.txt（默认）" } else { "-AllowListFile 指定的文件" }), (Get-AllowRegexes).Count)
$hits = Invoke-Scan $files $needles
if ($hits.Count -eq 0) {
    Write-Host "没有命中。" -ForegroundColor Green
    exit 0
}
Write-Host ("命中 {0} 处（只列文件、位置、类别，不显示内容）：" -f $hits.Count) -ForegroundColor Red
$hits | Sort-Object | ForEach-Object { Write-Host "  $_" }
exit 1
