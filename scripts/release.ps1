# ZeeTerm 本机发版脚本（备用路径）：统一改版本号 -> 门禁 -> 打安装包 -> 用更新私钥签名 -> 生成 latest.json -> 自检。
# 产物放在 dist\<版本>\ 里。**脚本不推送、不打 tag、不发布 Release**：最后只打印下一步命令，由发版人确认后自己执行。
#
# **现在的主发版路径是 GitHub Actions**（.github/workflows/release.yml）：用 scripts/bump-version.ps1 改好版本号并提交，
# 推 v* tag，由工作流编译、签名（私钥和口令在 GitHub 的 release 环境 Secrets 里）、发布 Release，本机不需要私钥。
# 本脚本只在 GitHub 那条路出问题（Actions 故障、Secrets 失效等）时手动发版用；改版本号、签名、latest.json、
# SHA256SUMS 的实现和工作流共用 scripts/release-lib.ps1，产物格式一致。流程见 docs/RELEASING.md。
#
# 用法：
#   powershell -File scripts/release.ps1 -Version 1.0.0                # 正式发版（要求在 main、工作区干净）
#   powershell -File scripts/release.ps1 -Version 1.0.0 -SkipGates     # 跳过门禁（门禁刚跑过时）
#   powershell -File scripts/release.ps1 -Version 1.0.0 -NotesFile notes.md
#   powershell -File scripts/release.ps1 -Version 9.9.9 -DryRun -KeyPath <测试私钥> -OutRoot <临时目录> `
#       -SkipGates -PrebuiltInstaller <任意文件>                         # 演练：不改仓库，不真的打包
#
# 参数：
#   -Version            新版本号，X.Y.Z（可带 -rc.1 这样的预发布后缀）；必须高于当前版本。
#   -KeyPath            更新私钥，默认 %USERPROFILE%\.zeeterm\updater.key（由 scripts/updater-keygen.ps1 生成）。
#                       私钥有口令时脚本会提示输入。
#   -OutRoot            产物根目录，默认仓库下的 dist\（已加入 .gitignore）。
#   -NotesFile          更新说明（Markdown），优先级最高。不给就先找 docs/release-notes/<版本>.md（面向用户的简短说明，
#                       会显示在用户的更新窗口里），再退回 CHANGELOG.md 里 "## [<版本>]" 一节。
#   -Repo               GitHub 仓库，默认 MoeShinX/ZeeTerm；下载地址 = https://github.com/<Repo>/releases/download/v<版本>/<安装包>。
#   -SkipGates          不跑 scripts/gates.ps1。
#   -DryRun             演练：版本号只在改完后还原（git 看不到任何变化）；不检查分支 / 工作区；公钥与私钥对不上只警告；
#                       不编译安装包（必须给 -PrebuiltInstaller，里面的文件当安装包用，产物不能发布）。
#   -Force              dist\<版本>\ 已有内容时默认直接报错退出（免得盖掉上一次的产物）；给了 -Force 才在打包前清空该目录再继续。
#   -PrebuiltInstaller  只在 -DryRun 时有效。
#
# 版本号的所有来源（统一由本脚本或 bump-version.ps1 改，见 docs/RELEASING.md；有单测 every_place_that_carries_the_version_agrees 核对）：
#   Cargo.toml、native/Cargo.toml 的 [workspace.package] version；Cargo.lock、native/Cargo.lock 里跟随工作区的 zssh-* 包；
#   安装包文件名 / 版本资源 / 卸载项显示版本（build.ps1 从 native/Cargo.toml 读）；程序里显示的版本（编译时来自同一处）。
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [string]$KeyPath = '',
    [string]$OutRoot = '',
    [string]$NotesFile = '',
    [string]$Repo = 'MoeShinX/ZeeTerm',
    [switch]$SkipGates,
    [switch]$DryRun,
    [switch]$Force,
    [string]$PrebuiltInstaller = ''
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repoRoot
. (Join-Path $PSScriptRoot 'release-lib.ps1')
if (-not $KeyPath) { $KeyPath = Join-Path $env:USERPROFILE '.zeeterm\updater.key' }
if (-not $OutRoot) { $OutRoot = Join-Path $repoRoot 'dist' }

function Test-OutDirHasContent([string]$Path) {
    return (Test-Path -LiteralPath $Path) -and @(Get-ChildItem -LiteralPath $Path -Force).Count -gt 0
}

# ---------- 0. 参数检查 ----------
if (-not (Test-VersionFormat $Version)) {
    Stop-WithError "版本号要写成 X.Y.Z（可带 -rc.1 这类预发布后缀，不带 v）：$Version"
}
if ($PrebuiltInstaller -and -not $DryRun) {
    Stop-WithError '-PrebuiltInstaller 只能和 -DryRun 一起用：正式发版的安装包必须由本脚本从当前代码现编。'
}
if ($DryRun -and -not $PrebuiltInstaller) {
    Stop-WithError '-DryRun 需要 -PrebuiltInstaller <文件>（演练不编译安装包）。'
}
if ($PrebuiltInstaller -and -not (Test-Path -LiteralPath $PrebuiltInstaller -PathType Leaf)) {
    Stop-WithError "找不到 -PrebuiltInstaller 指定的文件：$PrebuiltInstaller"
}
# 产物目录有内容就在最前面拒绝（比打完包才发现省时间，也不会先改了版本号再失败）；-Force 才允许继续。
$outDir = Join-Path $OutRoot $Version
if ((Test-OutDirHasContent $outDir) -and -not $Force) {
    Stop-WithError "$outDir 已经有内容，拒绝覆盖。确认可以丢弃就加 -Force（会先清空该目录），或换个版本号。"
}
if (-not (Test-Path -LiteralPath $KeyPath -PathType Leaf)) {
    Stop-WithError "找不到更新私钥：$KeyPath（先跑 scripts/updater-keygen.ps1，或用 -KeyPath 指定）"
}
foreach ($tool in @('cargo', 'git')) {
    if (-not (Get-Command $tool -ErrorAction SilentlyContinue)) { Stop-WithError "找不到 $tool。" }
}

# ---------- 当前版本与版本比较 ----------
$currentVersion = Get-CurrentVersion $repoRoot
if ((Compare-Version $Version $currentVersion) -le 0) {
    Stop-WithError "新版本 $Version 必须高于当前版本 $currentVersion。"
}

# ---------- 1. 工作区检查 ----------
Step '检查分支和工作区'
$branch = (& git rev-parse --abbrev-ref HEAD).Trim()
# 未跟踪文件也要查：重构里新增的模块如果忘了 `git add`，发出去的包会缺文件。
$dirty = @(& git status --porcelain --untracked-files=all)
if ($DryRun) {
    if ($branch -ne 'main') { Write-Host "（演练）当前分支是 $branch，正式发版要求在 main。" -ForegroundColor Yellow }
    if ($dirty.Count -gt 0) { Write-Host '（演练）工作区有未提交的改动，正式发版要求干净。' -ForegroundColor Yellow }
} else {
    if ($branch -ne 'main') { Stop-WithError "必须在 main 分支发版，当前是 $branch。" }
    if ($dirty.Count -gt 0) { Stop-WithError "工作区有未提交/未跟踪的改动，先提交或还原：`n$($dirty -join "`n")" }
    if (@(& git tag --list "v$Version").Count -gt 0) { Stop-WithError "tag v$Version 已经存在。" }
}

# ---------- 2. 更新公钥与私钥必须是一对 ----------
Step '核对更新公钥'
$updaterJson = Join-Path $repoRoot 'native\zssh-update\updater.json'
$configuredPubkey = Get-ConfiguredPubkey $updaterJson

# 私钥有口令就问一次，只放进本进程的环境变量。
$passwordEnv = 'ZEETERM_UPDATER_KEY_PASSWORD'
$previousPassword = [Environment]::GetEnvironmentVariable($passwordEnv, 'Process')
try {
    $derived = Get-DerivedPubkey $repoRoot $KeyPath
    if (-not $derived) {
        $secure = Read-Host -AsSecureString '更新私钥有口令，请输入'
        $bstr = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
        try { $plain = [System.Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
        finally { [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
        [Environment]::SetEnvironmentVariable($passwordEnv, $plain, 'Process')
        $derived = Get-DerivedPubkey $repoRoot $KeyPath
    }
    if (-not $derived) { Stop-WithError '读不了更新私钥（口令不对，或文件损坏）。' }

    $placeholder = $configuredPubkey.ToUpperInvariant().StartsWith('PLACEHOLDER')
    if ($placeholder -or $configuredPubkey -ne $derived) {
        $reason = if ($placeholder) { 'native/zssh-update/updater.json 里的 pubkey 还是占位值' } else { 'updater.json 里的公钥与这把私钥不是一对' }
        if ($DryRun) {
            Write-Host "（演练）$reason；演练改用私钥自己的公钥来自检。" -ForegroundColor Yellow
            $configuredPubkey = $derived
        } else {
            Stop-WithError "$reason。先运行 scripts/updater-keygen.ps1 并把公钥填进去（或确认 -KeyPath 指对了私钥）。已发布的程序用这个公钥验签，对不上用户就装不上更新。"
        }
    } else {
        Write-Host 'updater.json 的公钥与私钥匹配。'
    }

    # ---------- 3. 统一改版本号（演练：改完在 finally 里还原） ----------
    Step "统一版本号 $currentVersion -> $Version$(if ($DryRun) { '（演练：结束后还原）' })"
    Set-WorkspaceVersion $repoRoot $currentVersion $Version

    # ---------- 4. 门禁 ----------
    if ($SkipGates) {
        Step '跳过门禁（-SkipGates）'
    } else {
        Step '跑门禁 scripts/gates.ps1'
        & powershell -NoProfile -File (Join-Path $repoRoot 'scripts\gates.ps1')
        if ($LASTEXITCODE -ne 0) { Stop-WithError '门禁没有全部通过，不发版。' }
    }

    # ---------- 5. 安装包 ----------
    if (Test-OutDirHasContent $outDir) {
        # 到这里只可能是给了 -Force（最前面已拦过）；门禁之后才清，前面失败时不会丢掉旧产物。
        if (-not $Force) { Stop-WithError "$outDir 已经有内容，拒绝覆盖。确认可以丢弃就加 -Force，或换个版本号。" }
        Step "-Force：清空 $outDir"
        Get-ChildItem -LiteralPath $outDir -Force | Remove-Item -Recurse -Force
    }
    New-Item -ItemType Directory -Force -Path $outDir | Out-Null
    $installerName = "ZeeTerm-Setup-$Version.exe"
    $installer = Join-Path $outDir $installerName
    if ($DryRun) {
        Step "（演练）不编译安装包，使用 -PrebuiltInstaller 的文件充当 $installerName"
        Copy-Item -LiteralPath $PrebuiltInstaller -Destination $installer
    } else {
        Step '打安装包 native/installer/build.ps1'
        & powershell -NoProfile -File (Join-Path $repoRoot 'native\installer\build.ps1') -OutDir $outDir
        if ($LASTEXITCODE -ne 0) { Stop-WithError '打安装包失败。' }
        if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) { Stop-WithError "没有找到预期的安装包：$installer" }
    }

    # ---------- 6. 更新说明 -> 签名 -> latest.json -> 自检 -> SHA256SUMS ----------
    $notesOut = Join-Path $outDir 'RELEASE_NOTES.md'
    Write-ReleaseNotes -RepoRoot $repoRoot -Version $Version -OutFile $notesOut -NotesFile $NotesFile -DryRun:$DryRun
    $artifacts = New-ReleaseArtifacts -RepoRoot $repoRoot -Version $Version -Installer $installer -KeyPath $KeyPath `
        -ConfiguredPubkey $configuredPubkey -NotesOut $notesOut -OutDir $outDir -Repo $Repo
    $sha = $artifacts.Sha256
    $latest = $artifacts.Latest
    $sums = $artifacts.Sums
} finally {
    [Environment]::SetEnvironmentVariable($passwordEnv, $previousPassword, 'Process')
    # 演练（或中途出错）时把版本号相关文件原样还原；正式发版成功时保留，让发版人提交。
    $restore = $DryRun -or ($null -eq $sha)
    if ($restore -and $script:VersionBackups.Count -gt 0) {
        Restore-VersionFiles | Out-Null
        Write-Host ''
        Write-Host '（版本号相关文件已还原）' -ForegroundColor DarkGray
    }
}

# ---------- 7. 汇总和下一步 ----------
Write-Host ''
Write-Host "发版产物：$outDir" -ForegroundColor Green
Get-ChildItem -LiteralPath $outDir | ForEach-Object { Write-Host ("  {0}  ({1:N0} 字节)" -f $_.Name, $_.Length) }
Write-Host "  安装包 SHA-256：$sha"
if ($DryRun) {
    Write-Host ''
    Write-Host '这是演练：产物里的"安装包"不是真的安装包，不能发布。' -ForegroundColor Yellow
    exit 0
}
Write-Host ''
Write-Host '下一步（脚本不会替你执行；确认无误后手动运行）。这是 GitHub 出问题时的备用路径，平时用推 tag 触发 Actions：' -ForegroundColor Cyan
Write-Host '  1. 版本号相关文件已改好，检查并提交（可能还要在 CHANGELOG.md 里把 [Unreleased] 改成这个版本）：'
Write-Host '       git diff'
$addFiles = 'Cargo.toml Cargo.lock native/Cargo.toml native/Cargo.lock CHANGELOG.md'
if (Test-Path -LiteralPath (Join-Path $repoRoot "docs\release-notes\$Version.md") -PathType Leaf) { $addFiles += " docs/release-notes/$Version.md" }
Write-Host "       git add $addFiles"
Write-Host "       git commit -m `"chore(release): v$Version`""
Write-Host "  2. 打 tag 并推送。推 v* tag 会触发 GitHub 的发版工作流、与下面手动建 Release 冲突，所以手动发版前先停用它，发完再启用："
Write-Host "       gh workflow disable release.yml --repo $Repo"
Write-Host "       git tag v$Version"
Write-Host "       git push origin main v$Version"
Write-Host '  3. 创建 Release 并上传安装包、latest.json 和 SHA256SUMS.txt（latest.json 是用户端检查更新读的那份；正文末尾已带安装包的 SHA-256）：'
Write-Host "       gh release create v$Version `"$installer`" `"$latest`" `"$sums`" --repo $Repo --title `"ZeeTerm $Version`" --notes-file `"$notesOut`""
Write-Host "  4. 发布后重新启用工作流：gh workflow enable release.yml --repo $Repo；再用旧版本程序点一次「检查更新」验证；出问题的回滚办法见 docs/RELEASING.md。"
exit 0
