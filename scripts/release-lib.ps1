# 发版脚本共用的函数库：被 release.ps1（本机备用路径）、bump-version.ps1（改版本号）、
# release-sign.ps1（GitHub Actions 的签名步骤）和 release-drill.ps1（演练）用 dot-source 加载，自己不能直接运行：
#   . (Join-Path $PSScriptRoot 'release-lib.ps1')
#
# 放在一处是为了"本机发版"和"GitHub 发版"产出的 latest.json / SHA256SUMS.txt / Release 正文格式只有一份实现，
# 不会各改各的、慢慢走样。这里的函数都不碰私钥内容：私钥只以文件路径传给 release_tool，口令只走环境变量
# ZEETERM_UPDATER_KEY_PASSWORD，函数不打印它们，也不开 release_tool 的详细输出。
#
# 状态：$script:VersionBackups 记录 Set-WorkspaceVersion 改过的文件的原始字节（dot-source 后在调用脚本的
# 脚本作用域里），Restore-VersionFiles 据此原样还原；调用方应放在 try/finally 里。

function Stop-WithError([string]$Message) {
    [Console]::Error.WriteLine("错误：$Message")
    exit 1
}

function Step([string]$Text) { Write-Host ''; Write-Host "==> $Text" -ForegroundColor Cyan }

# ---------- 文本读写（保留 BOM、不做换行转换） ----------

function Read-Text([string]$Path) {
    $bytes = [System.IO.File]::ReadAllBytes($Path)
    $hasBom = ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    $offset = if ($hasBom) { 3 } else { 0 }
    return [pscustomobject]@{ Text = $utf8.GetString($bytes, $offset, $bytes.Length - $offset); Bom = $hasBom; Bytes = $bytes }
}

function Write-Text([string]$Path, [string]$Text, [bool]$Bom) {
    $utf8 = New-Object System.Text.UTF8Encoding($Bom)
    [System.IO.File]::WriteAllText($Path, $Text, $utf8)
}

# ---------- 版本号：读取、比较、统一修改 ----------

$script:VersionPattern = '(?ms)(^\[workspace\.package\][^\[]*?^version\s*=\s*")([^"]+)(")'
$script:VersionBackups = @()

function Test-VersionFormat([string]$Version) {
    return ($Version -match '^(\d+)\.(\d+)\.(\d+)(-[0-9A-Za-z][0-9A-Za-z.]*)?$')
}

function Get-WorkspaceVersion([string]$Path) {
    $m = [regex]::Match((Read-Text $Path).Text, $script:VersionPattern)
    if (-not $m.Success) { Stop-WithError "$Path 里找不到 [workspace.package] 的 version。" }
    return $m.Groups[2].Value
}

# 两个工作区（根 / native）的当前版本；不一致直接报错。
function Get-CurrentVersion([string]$RepoRoot) {
    $native = Get-WorkspaceVersion (Join-Path $RepoRoot 'native\Cargo.toml')
    if ((Get-WorkspaceVersion (Join-Path $RepoRoot 'Cargo.toml')) -ne $native) {
        Stop-WithError 'Cargo.toml 和 native/Cargo.toml 的版本不一致，先修好再发版。'
    }
    return $native
}

# semver 比较：先比 X.Y.Z；同号时正式版高于预发布；预发布之间按字符串比。返回 -1 / 0 / 1。
function Compare-Version([string]$A, [string]$B) {
    $pa = [regex]::Match($A, '^(\d+)\.(\d+)\.(\d+)(?:-(.+))?$')
    $pb = [regex]::Match($B, '^(\d+)\.(\d+)\.(\d+)(?:-(.+))?$')
    for ($i = 1; $i -le 3; $i++) {
        $x = [int64]$pa.Groups[$i].Value; $y = [int64]$pb.Groups[$i].Value
        if ($x -ne $y) { return [math]::Sign($x - $y) }
    }
    $ra = $pa.Groups[4].Value; $rb = $pb.Groups[4].Value
    if ($ra -eq $rb) { return 0 }
    if ($ra -eq '') { return 1 }
    if ($rb -eq '') { return -1 }
    return [math]::Sign([string]::CompareOrdinal($ra, $rb))
}

function Backup-File([string]$Path) {
    $script:VersionBackups += [pscustomobject]@{ Path = $Path; Bytes = [System.IO.File]::ReadAllBytes($Path) }
}

# 出错 / 演练时把 Backup-File 记过的文件原样写回。
function Restore-VersionFiles {
    foreach ($item in $script:VersionBackups) { [System.IO.File]::WriteAllBytes($item.Path, $item.Bytes) }
    $restored = $script:VersionBackups.Count
    $script:VersionBackups = @()
    return $restored
}

# 把所有版本号来源从 $From 改成 $To（来源清单见 docs/RELEASING.md）：
#   Cargo.toml、native/Cargo.toml 的 [workspace.package] version；
#   Cargo.lock、native/Cargo.lock 里跟随工作区的 zssh-* 包（没有 source 的路径依赖）。
# 改完立刻用 cargo metadata --locked 核对 lock 与 Cargo.toml 一致（不一致说明漏改了什么）。
function Set-WorkspaceVersion([string]$RepoRoot, [string]$From, [string]$To) {
    $manifestFiles = @((Join-Path $RepoRoot 'Cargo.toml'), (Join-Path $RepoRoot 'native\Cargo.toml'))
    foreach ($path in $manifestFiles) {
        Backup-File $path
        $file = Read-Text $path
        # 只替换第一处：.NET 的静态 Regex.Replace 没有 count 重载，第 4 个参数会被当成 RegexOptions（1 = IgnoreCase），
        # 所以必须用实例的 Replace(输入, 替换, 次数)。
        $new = ([regex]::new($script:VersionPattern)).Replace($file.Text, { param($m) $m.Groups[1].Value + $To + $m.Groups[3].Value }, 1)
        Write-Text $path $new $file.Bom
    }
    foreach ($lockPath in @((Join-Path $RepoRoot 'Cargo.lock'), (Join-Path $RepoRoot 'native\Cargo.lock'))) {
        Backup-File $lockPath
        $file = Read-Text $lockPath
        $count = 0
        $blocks = $file.Text -split '(?=\[\[package\]\])'
        for ($i = 0; $i -lt $blocks.Length; $i++) {
            $block = $blocks[$i]
            if ($block -match '(?m)^name = "zssh-[^"]+"\r?$' -and $block -notmatch '(?m)^source = ' -and $block -match ('(?m)^version = "' + [regex]::Escape($From) + '"')) {
                $blocks[$i] = ([regex]::new('(?m)^version = "' + [regex]::Escape($From) + '"')).Replace($block, "version = `"$To`"", 1)
                $count++
            }
        }
        Write-Text $lockPath ($blocks -join '') $file.Bom
        $label = (Split-Path -Leaf (Split-Path -Parent $lockPath)) + '/' + (Split-Path -Leaf $lockPath)
        Write-Host ("  {0}：{1} 个包" -f $label, $count)
    }
    foreach ($path in $manifestFiles) { Write-Host "  $path" }
    foreach ($manifest in $manifestFiles) {
        & cargo metadata --locked --offline --format-version 1 --manifest-path $manifest | Out-Null
        if ($LASTEXITCODE -ne 0) { Stop-WithError "改完版本号后 $manifest 与它的 Cargo.lock 对不上（cargo metadata --locked 失败）。" }
    }
}

# ---------- 更新说明 ----------

# 生成 Release 正文 / latest.json 里 notes 的基础文本，写到 $OutFile。来源优先级：
#   -NotesFile > docs/release-notes/<版本>.md > CHANGELOG.md 的 "## [<版本>]" 一节。
# -RequireUserNotes（GitHub 发版用）：只认 docs/release-notes/<版本>.md，没有就报错，不退回 CHANGELOG（那份是写给开发者的）。
function Write-ReleaseNotes {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$OutFile,
        [string]$NotesFile = '',
        [switch]$DryRun,
        [switch]$RequireUserNotes
    )
    $releaseNotesFile = Join-Path $RepoRoot "docs\release-notes\$Version.md"
    if ($NotesFile -and -not $RequireUserNotes) {
        if (-not (Test-Path -LiteralPath $NotesFile -PathType Leaf)) { Stop-WithError "找不到更新说明文件：$NotesFile" }
        Copy-Item -LiteralPath $NotesFile -Destination $OutFile
        Write-Host "更新说明：取自 -NotesFile $NotesFile"
    } elseif (Test-Path -LiteralPath $releaseNotesFile -PathType Leaf) {
        # 面向用户的更新说明（几条要点）优先于 CHANGELOG：CHANGELOG 是写给开发者的，里面有脚本名和内部实现。
        $userNotes = (Read-Text $releaseNotesFile).Text -replace "`r`n", "`n"
        if ($userNotes.Trim() -eq '') {
            Stop-WithError "docs/release-notes/$Version.md 是空的。写上给用户看的更新说明，或删掉这个文件改用 CHANGELOG.md 里的一节。"
        }
        Write-Text $OutFile ($userNotes.Trim() + "`n") $false
        Write-Host "更新说明：取自 docs/release-notes/$Version.md"
    } elseif ($RequireUserNotes) {
        Stop-WithError "缺少 docs/release-notes/$Version.md（GitHub 发版必须有面向用户的更新说明）。"
    } else {
        Write-Host '更新说明：取自 CHANGELOG.md（没有 docs/release-notes/<版本>.md；那是面向用户的简短说明，建议写一份）。'
        $changelog = (Read-Text (Join-Path $RepoRoot 'CHANGELOG.md')).Text -replace "`r`n", "`n"
        $section = [regex]::Match($changelog, '(?ms)^## \[' + [regex]::Escape($Version) + '\][^\n]*\n(.*?)(?=^## \[|\z)')
        if (-not $section.Success -and $DryRun) {
            $section = [regex]::Match($changelog, '(?ms)^## \[Unreleased\][^\n]*\n(.*?)(?=^## \[|\z)')
            Write-Host '（演练）CHANGELOG 里还没有这个版本的一节，用 [Unreleased] 充当更新说明。' -ForegroundColor Yellow
        }
        if (-not $section.Success -or $section.Groups[1].Value.Trim() -eq '') {
            Stop-WithError "CHANGELOG.md 里没有 ## [$Version] 一节（或是空的）。先写更新说明，或用 -NotesFile 指定。"
        }
        Write-Text $OutFile ($section.Groups[1].Value.Trim() + "`n") $false
    }
}

# ---------- 签名工具（release_tool 示例，签名 / 验签与应用里是同一套代码） ----------

function Get-ReleaseToolArgs([string]$RepoRoot) {
    return @('run', '--quiet', '--release', '--locked', '--manifest-path', (Join-Path $RepoRoot 'native\Cargo.toml'),
        '-p', 'zssh-update', '--example', 'release_tool', '--')
}

# updater.json 里配置的公钥（base64）；没有该字段返回空串。
function Get-ConfiguredPubkey([string]$UpdaterJson) {
    return ([regex]::Match((Read-Text $UpdaterJson).Text, '"pubkey"\s*:\s*"([^"]*)"')).Groups[1].Value.Trim()
}

# 由私钥推出公钥（base64）。读不了（口令不对 / 文件坏）返回 $null，由调用方决定怎么办。
function Get-DerivedPubkey([string]$RepoRoot, [string]$KeyPath) {
    $toolArgs = Get-ReleaseToolArgs $RepoRoot
    $out = & cargo @toolArgs pubkey --key $KeyPath
    if ($LASTEXITCODE -ne 0 -or -not $out) { return $null }
    return ($out | Select-Object -Last 1).ToString().Trim()
}

# ---------- 签名 -> latest.json -> 自检 -> SHA256SUMS.txt -> Release 正文 ----------

# 前提：$NotesOut 已经由 Write-ReleaseNotes 写好（本函数会在末尾追加安装包的 SHA-256 一行）。
# 产物都写在 $OutDir：<安装包>.sig、latest.json、SHA256SUMS.txt，以及追加过 SHA-256 的 $NotesOut。
# 格式的约定（用户端读 latest.json、下载的人读 SHA256SUMS.txt，不能随便改）：
#   - latest.json 的 notes 不带 SHA-256 那一行（更新窗口里显示它），Release 正文才带；
#   - SHA256SUMS.txt 是 `<小写 sha256>  <文件名>`，两个空格，和 sha256sum 同格式。
function New-ReleaseArtifacts {
    param(
        [Parameter(Mandatory = $true)][string]$RepoRoot,
        [Parameter(Mandatory = $true)][string]$Version,
        [Parameter(Mandatory = $true)][string]$Installer,
        [Parameter(Mandatory = $true)][string]$KeyPath,
        [Parameter(Mandatory = $true)][string]$ConfiguredPubkey,
        [Parameter(Mandatory = $true)][string]$NotesOut,
        [Parameter(Mandatory = $true)][string]$OutDir,
        [Parameter(Mandatory = $true)][string]$Repo
    )
    $toolArgs = Get-ReleaseToolArgs $RepoRoot
    $installerName = Split-Path -Leaf $Installer

    Step '用更新私钥签名（签名里带 version，签完用配置里的公钥自检）'
    $sigFile = "$Installer.sig"
    & cargo @toolArgs sign --key $KeyPath --file $Installer --version $Version --pubkey $ConfiguredPubkey --out $sigFile
    if ($LASTEXITCODE -ne 0) { Stop-WithError '签名失败。' }

    Step '生成 latest.json'
    $url = "https://github.com/$Repo/releases/download/v$Version/$installerName"
    $latest = Join-Path $OutDir 'latest.json'
    $pubDate = [DateTime]::UtcNow.ToString("yyyy-MM-dd'T'HH:mm:ss'Z'")
    & cargo @toolArgs manifest --version $Version --notes-file $NotesOut --pub-date $pubDate --url $url --signature-file $sigFile --out $latest
    if ($LASTEXITCODE -ne 0) { Stop-WithError '生成 latest.json 失败。' }

    # 自检：按应用里验签的同一套代码，再验一遍成品。
    Step '自检：用 updater.json 的公钥验安装包 + 签名 + 版本绑定'
    & cargo @toolArgs verify --pubkey $ConfiguredPubkey --file $Installer --signature-file $sigFile --version $Version
    if ($LASTEXITCODE -ne 0) { Stop-WithError '自检没通过，产物不能发布。' }
    $parsed = Get-Content -LiteralPath $latest -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($parsed.version -ne $Version -or $parsed.platforms.'windows-x86_64'.url -ne $url) { Stop-WithError 'latest.json 的内容与预期不符。' }

    # 安装包的 SHA-256：写进 SHA256SUMS.txt，也附在 Release 正文末尾，让下载的人能自己核对。
    # 放在 latest.json 生成之后追加：更新窗口里显示的更新说明（latest.json 的 notes）不带这一行。
    $sha = (Get-FileHash -LiteralPath $Installer -Algorithm SHA256).Hash.ToLowerInvariant()
    $sums = Join-Path $OutDir 'SHA256SUMS.txt'
    Write-Text $sums ("$sha  $installerName`n") $false
    $notesText = (Read-Text $NotesOut).Text.TrimEnd()
    Write-Text $NotesOut ($notesText + "`n`n安装包 SHA-256：" + [char]96 + $sha + [char]96 + "`n") $false

    return [pscustomobject]@{ Installer = $Installer; Signature = $sigFile; Latest = $latest; Sums = $sums; Notes = $NotesOut; Sha256 = $sha; Url = $url }
}
