# ZeeTerm 改版本号：发版第一步，只改文件，不提交、不打 tag、不推送、不碰私钥。
#
# 做什么：
#   1. 统一改所有版本号来源（Cargo.toml、native/Cargo.toml 的 [workspace.package] version，以及两个 Cargo.lock
#      里跟随工作区的 zssh-* 包；和 release.ps1 共用 scripts/release-lib.ps1 里的同一份实现）；
#   2. 把 CHANGELOG.md 里 "## [Unreleased]" 下面的内容切成 "## [x.y.z] - <今天>"，[Unreleased] 留空；
#   3. 最后打印接下来要执行的 git 命令（提交、推 main、打 tag、推 tag），由发版人自己执行。
# 开始之前先检查，任何一条不满足就报错退出、不改任何文件：
#   版本号格式正确且高于当前版本；docs/release-notes/<版本>.md 存在且不是空的；tag v<版本> 还不存在；
#   CHANGELOG.md 有 [Unreleased] 一节、还没有 [<版本>] 一节。
#
# 用法：
#   powershell -File scripts/bump-version.ps1 -Version 0.4.0
#   powershell -File scripts/bump-version.ps1 -Version 0.4.0 -DryRun     # 演练：照常检查和修改，结束后原样还原
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [switch]$DryRun
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repoRoot
. (Join-Path $PSScriptRoot 'release-lib.ps1')

foreach ($tool in @('cargo', 'git')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { Stop-WithError "找不到 $tool。" }
}

# ---------- 检查（全部通过才动文件） ----------
if (-not (Test-VersionFormat $Version)) {
    Stop-WithError "版本号要写成 X.Y.Z（可带 -rc.1 这类预发布后缀，不带 v）：$Version"
}
$currentVersion = Get-CurrentVersion $repoRoot
if ((Compare-Version $Version $currentVersion) -le 0) {
    Stop-WithError "新版本 $Version 必须高于当前版本 $currentVersion。"
}

$notesPath = Join-Path $repoRoot "docs\release-notes\$Version.md"
if (-not (Test-Path -LiteralPath $notesPath -PathType Leaf)) {
    Stop-WithError "缺少 docs/release-notes/$Version.md。先写好面向用户的更新说明（格式见 docs/release-notes/README.md），再改版本号。"
}
if ((Read-Text $notesPath).Text.Trim() -eq '') {
    Stop-WithError "docs/release-notes/$Version.md 是空的。先写好面向用户的更新说明。"
}

if (@(& git tag --list "v$Version").Count -gt 0) { Stop-WithError "tag v$Version 已经存在。" }

$changelogPath = Join-Path $repoRoot 'CHANGELOG.md'
$changelog = Read-Text $changelogPath
$newline = if ($changelog.Text.Contains("`r`n")) { "`r`n" } else { "`n" }
$unreleased = [regex]::Match($changelog.Text, '(?m)^## \[Unreleased\][^\r\n]*\r?\n')
if (-not $unreleased.Success) { Stop-WithError 'CHANGELOG.md 里找不到 "## [Unreleased]" 一节。' }
if ([regex]::IsMatch($changelog.Text, '(?m)^## \[' + [regex]::Escape($Version) + '\]')) {
    Stop-WithError "CHANGELOG.md 里已经有 ## [$Version] 一节。"
}
$afterUnreleased = $changelog.Text.Substring($unreleased.Index + $unreleased.Length)
$nextSection = [regex]::Match($afterUnreleased, '(?m)^## \[')
$unreleasedBody = if ($nextSection.Success) { $afterUnreleased.Substring(0, $nextSection.Index) } else { $afterUnreleased }
if ($unreleasedBody.Trim() -eq '') {
    Write-Host '提示：CHANGELOG.md 的 [Unreleased] 下面是空的，新版本一节会是空的；确认这是对的（更新说明取自 docs/release-notes）。' -ForegroundColor Yellow
}

# ---------- 修改（出错或演练时在 finally 里还原） ----------
$ok = $false
try {
    Step "统一版本号 $currentVersion -> $Version$(if ($DryRun) { '（演练：结束后还原）' })"
    Set-WorkspaceVersion $repoRoot $currentVersion $Version

    Step 'CHANGELOG.md：[Unreleased] 的内容切成新版本一节'
    Backup-File $changelogPath
    $date = (Get-Date).ToString('yyyy-MM-dd')
    $heading = $unreleased.Value.TrimEnd("`r", "`n")
    # 在 [Unreleased] 标题后插入新版本标题：原来紧跟在标题后面的内容（以空行开头）自然落到新标题下面。
    $replacement = $heading + $newline + $newline + "## [$Version] - $date" + $newline
    $newText = $changelog.Text.Substring(0, $unreleased.Index) + $replacement + $afterUnreleased
    Write-Text $changelogPath $newText $changelog.Bom
    Write-Host "  CHANGELOG.md：## [$Version] - $date"
    $ok = $true
} finally {
    if ($DryRun -or -not $ok) {
        if ($script:VersionBackups.Count -gt 0) {
            Restore-VersionFiles | Out-Null
            Write-Host ''
            Write-Host '（已改的文件已原样还原）' -ForegroundColor DarkGray
        }
    }
}

# ---------- 下一步 ----------
Write-Host ''
if ($DryRun) {
    Write-Host "这是演练：以上都已还原，没有任何文件被改动。去掉 -DryRun 才会真的改 $currentVersion -> $Version。" -ForegroundColor Yellow
    exit 0
}
Write-Host "版本号已改到 $Version。接下来由你执行（脚本不提交、不打 tag、不推送）：" -ForegroundColor Cyan
Write-Host '  1. 检查改动：'
Write-Host '       git diff'
Write-Host '  2. 提交，推到 main，等 main 上的 CI 通过：'
Write-Host "       git add Cargo.toml Cargo.lock native/Cargo.toml native/Cargo.lock CHANGELOG.md docs/release-notes/$Version.md"
Write-Host "       git commit -m `"chore(release): v$Version`""
Write-Host '       git push origin main'
Write-Host '  3. 打 tag 并推送（推 tag 会触发 GitHub Actions 的发版工作流：编译、签名、发布 Release）：'
Write-Host "       git tag v$Version"
Write-Host "       git push origin v$Version"
Write-Host '  4. 在 Actions 页看 Release 工作流；成功后下载安装包核对 SHA-256，再用旧版本点一次「检查更新」验证。'
exit 0
