# 发版流程演练（本机，不联网，不碰真实私钥）：用临时生成的测试密钥 + 一个充数的"安装包"，
# 把 GitHub 发版用的签名步骤（release-sign.ps1）和本机备用路径（release.ps1 -DryRun）各跑一遍，
# 核对两边的产物格式一致，再做几个反例（私钥与公钥对不上、安装包被改、口令不对）确认会失败。
#
# 用法：
#   powershell -File scripts/release-drill.ps1                    # 测试密钥放 C:\zssh-v2-data\r2-keys\，用完删除
#   powershell -File scripts/release-drill.ps1 -WorkDir <目录>     # 换个工作目录；-KeepFiles 不删（调试用）
#
# 测试密钥带口令（模拟正式密钥有口令的情况），口令是现生成的随机串，只放在本进程的环境变量里；
# 演练完全不读 %USERPROFILE%\.zeeterm 下的东西。会改版本号的 release.ps1 -DryRun 在结束时原样还原。
param(
    [string]$WorkDir = 'C:\zssh-v2-data\r2-keys',
    [switch]$KeepFiles
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Set-Location $repoRoot
. (Join-Path $PSScriptRoot 'release-lib.ps1')

$script:failures = 0
function Check([string]$Name, [bool]$Condition, [string]$Detail = '') {
    if ($Condition) { Write-Host "  [通过] $Name" -ForegroundColor Green }
    else { Write-Host "  [失败] $Name $Detail" -ForegroundColor Red; $script:failures++ }
}
# 跑 release_tool verify，返回退出码。"预期失败"的用例会让它往 stderr 写错误：Windows PowerShell 5.1 在
# $ErrorActionPreference = 'Stop' 时会把原生命令的 stderr 当成终止错误，所以这里局部放宽。
function Invoke-Verify([string]$Pubkey, [string]$File, [string]$Signature, [string]$Version) {
    $previous = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & cargo @toolArgs verify --pubkey $Pubkey --file $File --signature-file $Signature --version $Version 2>&1 | Out-Null
        return $LASTEXITCODE
    } finally { $ErrorActionPreference = $previous }
}
# 在子进程里跑脚本（脚本里的 exit 不会带走本演练），返回退出码；输出原样打印。
function Invoke-Script([string[]]$ScriptArgs) {
    & powershell -NoProfile -File @ScriptArgs | Out-Host
    return $LASTEXITCODE
}

$statusBefore = @(& git status --porcelain --untracked-files=no) -join "`n"
$passwordEnv = 'ZEETERM_UPDATER_KEY_PASSWORD'
$previousPassword = [Environment]::GetEnvironmentVariable($passwordEnv, 'Process')
$runDir = Join-Path $WorkDir ('drill-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
try {
    New-Item -ItemType Directory -Force -Path $runDir | Out-Null
    $script:toolArgs = Get-ReleaseToolArgs $repoRoot
    $toolArgs = $script:toolArgs

    Step '生成测试密钥（带随机口令）'
    $password = [guid]::NewGuid().ToString('N')
    [Environment]::SetEnvironmentVariable($passwordEnv, $password, 'Process')
    $keyDir = Join-Path $runDir 'keys'
    & cargo @toolArgs keygen --out-dir $keyDir | Out-Null
    if ($LASTEXITCODE -ne 0) { Stop-WithError '生成测试密钥失败。' }
    $keyPath = Join-Path $keyDir 'updater.key'
    $pubkey = Get-DerivedPubkey $repoRoot $keyPath
    if (-not $pubkey) { Stop-WithError '读不了刚生成的测试私钥。' }
    $otherDir = Join-Path $runDir 'other-keys'
    & cargo @toolArgs keygen --out-dir $otherDir | Out-Null
    $otherKey = Join-Path $otherDir 'updater.key'

    # 测试用的 updater.json 副本（带测试公钥），仓库里的不动。
    $updaterJson = Join-Path $runDir 'updater.json'
    $json = (Read-Text (Join-Path $repoRoot 'native\zssh-update\updater.json')).Text
    Write-Text $updaterJson ([regex]::Replace($json, '"pubkey"\s*:\s*"[^"]*"', '"pubkey": "' + $pubkey + '"')) $false

    $version = Get-CurrentVersion $repoRoot   # 当前版本有 docs/release-notes/<版本>.md，直接拿来演练
    $notesText = ((Read-Text (Join-Path $repoRoot "docs\release-notes\$version.md")).Text -replace "`r`n", "`n").Trim()
    $installerName = "ZeeTerm-Setup-$version.exe"
    $fake = Join-Path $runDir 'fake-installer.bin'
    $rng = New-Object byte[] 4096
    (New-Object System.Random).NextBytes($rng)
    [System.IO.File]::WriteAllBytes($fake, $rng)

    Step "A. GitHub 路径：release-sign.ps1（版本 $version）"
    $outA = Join-Path $runDir 'A'
    New-Item -ItemType Directory -Force -Path $outA | Out-Null
    Copy-Item -LiteralPath $fake -Destination (Join-Path $outA $installerName)
    $code = Invoke-Script @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outA $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outA, '-Repo', 'MoeShinX/ZeeTerm', '-UpdaterJson', $updaterJson)
    Check 'release-sign.ps1 退出码 0' ($code -eq 0) "(退出码 $code)"

    Step 'B. 本机备用路径：release.ps1 -DryRun（版本 9.9.9，更新说明用同一份）'
    $outBRoot = Join-Path $runDir 'B'
    $notesFile = Join-Path $runDir 'notes.md'
    Write-Text $notesFile ($notesText + "`n") $false
    $code = Invoke-Script @((Join-Path $PSScriptRoot 'release.ps1'), '-Version', '9.9.9', '-DryRun', '-KeyPath', $keyPath, '-OutRoot', $outBRoot,
        '-SkipGates', '-PrebuiltInstaller', $fake, '-NotesFile', $notesFile)
    Check 'release.ps1 -DryRun 退出码 0' ($code -eq 0) "(退出码 $code)"
    $outB = Join-Path $outBRoot '9.9.9'

    Step 'C. 对比两边的产物格式'
    $latestA = Get-Content -LiteralPath (Join-Path $outA 'latest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $latestB = Get-Content -LiteralPath (Join-Path $outB 'latest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    $fieldsA = ($latestA.PSObject.Properties.Name | Sort-Object) -join ','
    $fieldsB = ($latestB.PSObject.Properties.Name | Sort-Object) -join ','
    Check 'latest.json 顶层字段相同' ($fieldsA -eq $fieldsB) "($fieldsA / $fieldsB)"
    $platA = ($latestA.platforms.'windows-x86_64'.PSObject.Properties.Name | Sort-Object) -join ','
    $platB = ($latestB.platforms.'windows-x86_64'.PSObject.Properties.Name | Sort-Object) -join ','
    Check 'platforms.windows-x86_64 字段相同（url、signature）' ($platA -eq $platB -and $platA -eq 'signature,url') "($platA / $platB)"
    Check 'latest.json.version 正确' ($latestA.version -eq $version -and $latestB.version -eq '9.9.9')
    Check 'latest.json.notes 一致且不带 SHA-256 行' ($latestA.notes -eq $latestB.notes -and $latestA.notes -eq $notesText -and $latestA.notes -notmatch 'SHA-256')
    Check 'pub_date 都是 UTC 秒级 RFC3339' (($latestA.pub_date -match '^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$') -and ($latestB.pub_date -match '^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$'))
    Check '下载地址格式一致' (($latestA.platforms.'windows-x86_64'.url -eq "https://github.com/MoeShinX/ZeeTerm/releases/download/v$version/$installerName") -and
        ($latestB.platforms.'windows-x86_64'.url -eq "https://github.com/MoeShinX/ZeeTerm/releases/download/v9.9.9/ZeeTerm-Setup-9.9.9.exe"))

    $shaA = (Get-FileHash -LiteralPath (Join-Path $outA $installerName) -Algorithm SHA256).Hash.ToLowerInvariant()
    $sumsA = [System.IO.File]::ReadAllText((Join-Path $outA 'SHA256SUMS.txt'))
    $sumsB = [System.IO.File]::ReadAllText((Join-Path $outB 'SHA256SUMS.txt'))
    Check 'SHA256SUMS.txt 格式：<小写 sha256>两个空格<文件名>换行' (($sumsA -ceq "$shaA  $installerName`n") -and ($sumsB -cmatch '^[0-9a-f]{64}  ZeeTerm-Setup-9\.9\.9\.exe\n$'))
    $relA = [System.IO.File]::ReadAllText((Join-Path $outA 'RELEASE_NOTES.md'))
    $relB = [System.IO.File]::ReadAllText((Join-Path $outB 'RELEASE_NOTES.md'))
    $tail = "`n`n安装包 SHA-256：" + [char]96 + $shaA + [char]96 + "`n"
    Check 'Release 正文 = 更新说明 + 空行 + “安装包 SHA-256：`哈希`”' ($relA -ceq ($notesText + $tail))
    Check 'Release 正文末尾格式两边一致' (($relB -cmatch ('^' + [regex]::Escape($notesText) + '\n\n安装包 SHA-256：`[0-9a-f]{64}`\n$')))
    $expectedFiles = @($installerName, "$installerName.sig", 'latest.json', 'RELEASE_NOTES.md', 'SHA256SUMS.txt') | Sort-Object
    $filesA = @(Get-ChildItem -LiteralPath $outA | ForEach-Object Name) | Sort-Object
    Check '产物文件清单一致（安装包、.sig、latest.json、RELEASE_NOTES.md、SHA256SUMS.txt）' (($filesA -join '|') -eq ($expectedFiles -join '|'))

    Step 'D. 独立验签（release_tool verify，等同应用里的验签）'
    $sigA = Join-Path $outA "$installerName.sig"
    Check '正确的安装包 + 签名 + 版本验过' ((Invoke-Verify $pubkey (Join-Path $outA $installerName) $sigA $version) -eq 0)
    Check '版本对不上验签失败' ((Invoke-Verify $pubkey (Join-Path $outA $installerName) $sigA '9.9.8') -ne 0)
    $tampered = Join-Path $runDir 'tampered.exe'
    $bytes = [System.IO.File]::ReadAllBytes((Join-Path $outA $installerName)); $bytes[0] = $bytes[0] -bxor 0xFF
    [System.IO.File]::WriteAllBytes($tampered, $bytes)
    Check '安装包被改过验签失败' ((Invoke-Verify $pubkey $tampered $sigA $version) -ne 0)

    Step 'E. 反例：这些情况 release-sign.ps1 必须失败'
    $outE = Join-Path $runDir 'E'
    New-Item -ItemType Directory -Force -Path $outE | Out-Null
    Copy-Item -LiteralPath $fake -Destination (Join-Path $outE $installerName)
    $code = Invoke-Script @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outE $installerName),
        '-KeyPath', $otherKey, '-OutDir', $outE, '-UpdaterJson', $updaterJson)
    Check '私钥与 updater.json 公钥不是一对 -> 失败' ($code -ne 0)
    Check '……且没有产出 latest.json' (-not (Test-Path -LiteralPath (Join-Path $outE 'latest.json')))
    [Environment]::SetEnvironmentVariable($passwordEnv, 'wrong-password', 'Process')
    $code = Invoke-Script @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outE $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outE, '-UpdaterJson', $updaterJson)
    Check '口令不对 -> 失败' ($code -ne 0)
    [Environment]::SetEnvironmentVariable($passwordEnv, $password, 'Process')
    $placeholderJson = Join-Path $runDir 'placeholder.json'
    Write-Text $placeholderJson '{ "pubkey": "PLACEHOLDER", "endpoints": [] }' $false
    $code = Invoke-Script @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outE $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outE, '-UpdaterJson', $placeholderJson)
    Check '公钥还是占位值 -> 失败' ($code -ne 0)
    $code = Invoke-Script @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', '0.0.1', '-InstallerPath', (Join-Path $outE $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outE, '-UpdaterJson', $updaterJson)
    Check '安装包文件名与版本不符 -> 失败' ($code -ne 0)
    $code = Invoke-Script @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', '0.0.1', '-InstallerPath', (Join-Path $outE 'ZeeTerm-Setup-0.0.1.exe'),
        '-KeyPath', $keyPath, '-OutDir', $outE, '-UpdaterJson', $updaterJson)
    Check '安装包不存在 -> 失败' ($code -ne 0)
} finally {
    [Environment]::SetEnvironmentVariable($passwordEnv, $previousPassword, 'Process')
    if (-not $KeepFiles -and (Test-Path -LiteralPath $runDir)) { Remove-Item -LiteralPath $runDir -Recurse -Force -ErrorAction SilentlyContinue }
    # 演练前后已跟踪文件的状态必须一样（release.ps1 -DryRun 改过的版本号要原样还原）。
    $statusAfter = @(& git status --porcelain --untracked-files=no) -join "`n"
    if ($statusAfter -ne $statusBefore) { Write-Host '注意：演练前后已跟踪文件的状态不一样（版本号没还原？）：' -ForegroundColor Red; $script:failures++ }
}

Write-Host ''
if ($script:failures -gt 0) { Write-Host "演练有 $($script:failures) 项失败。" -ForegroundColor Red; exit 1 }
Write-Host '演练全部通过。' -ForegroundColor Green
exit 0
