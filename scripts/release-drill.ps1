# 发版流程演练（本机，不联网，不碰真实私钥）：用临时生成的测试密钥 + 一个充数的"安装包"，
# 把 GitHub 发版用的签名步骤（release-sign.ps1）和本机备用路径（release.ps1 -DryRun）各跑一遍，
# 核对两边的产物格式一致，再做几个反例（私钥与公钥对不上、安装包被改、口令不对）确认会失败。
# 另外演练 Linux 资产：release-sign.ps1 带上 deb 和 tar.gz（用充数文件），核对它们被同一把私钥签名、latest.json 多出
# linux-x86_64-deb / linux-x86_64-tar 两个条目而 Windows 条目不变、SHA256SUMS.txt 与 Release 正文包含全部文件，再做反例。
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
    Check '不带 Linux 包时 latest.json 只有 windows-x86_64 一项' ((($latestA.platforms.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'windows-x86_64')

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
    Step 'F. Linux 资产：release-sign.ps1 带 -LinuxDebPath / -LinuxTarPath'
    $debName = "ZeeTerm-$version-linux-x86_64.deb"
    $tarName = "ZeeTerm-$version-linux-x86_64.tar.gz"
    $outF = Join-Path $runDir 'F'
    New-Item -ItemType Directory -Force -Path $outF | Out-Null
    Copy-Item -LiteralPath $fake -Destination (Join-Path $outF $installerName)
    # 同一个 Random 对象连续取：每次 New-Object System.Random 在 Windows PowerShell 5.1 里按时钟取种子，
    # 连着创建会得到完全相同的字节，deb 和 tar.gz 就成了同一份内容，"拿 A 的签名验 B"的反例就没意义了。
    $rngLinux = New-Object System.Random
    foreach ($name in @($debName, $tarName)) {
        $bytesLinux = New-Object byte[] 2048
        $rngLinux.NextBytes($bytesLinux)
        [System.IO.File]::WriteAllBytes((Join-Path $outF $name), $bytesLinux)
    }
    $signF = @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outF $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outF, '-Repo', 'MoeShinX/ZeeTerm', '-UpdaterJson', $updaterJson)
    $code = Invoke-Script ($signF + @('-LinuxDebPath', (Join-Path $outF $debName), '-LinuxTarPath', (Join-Path $outF $tarName)))
    Check 'release-sign.ps1（带 Linux 包）退出码 0' ($code -eq 0) "(退出码 $code)"
    $latestF = Get-Content -LiteralPath (Join-Path $outF 'latest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    Check 'platforms 恰好是 linux-x86_64-deb、linux-x86_64-tar、windows-x86_64' ((($latestF.platforms.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'linux-x86_64-deb,linux-x86_64-tar,windows-x86_64')
    Check 'Windows 条目不变：地址和字段都和没有 Linux 包时一样' (($latestF.platforms.'windows-x86_64'.url -eq $latestA.platforms.'windows-x86_64'.url) -and
        ((($latestF.platforms.'windows-x86_64'.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'signature,url') -and ($latestF.version -eq $latestA.version) -and ($latestF.notes -eq $latestA.notes))
    $debEntry = $latestF.platforms.'linux-x86_64-deb'
    $tarEntry = $latestF.platforms.'linux-x86_64-tar'
    Check 'deb 条目：地址按 tag 和文件名拼，字段是 signature、url' (($debEntry.url -eq "https://github.com/MoeShinX/ZeeTerm/releases/download/v$version/$debName") -and ((($debEntry.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'signature,url') -and $debEntry.signature)
    Check 'tar 条目：地址按 tag 和文件名拼，字段是 signature、url' (($tarEntry.url -eq "https://github.com/MoeShinX/ZeeTerm/releases/download/v$version/$tarName") -and ((($tarEntry.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'signature,url') -and $tarEntry.signature)
    Check '三个签名互不相同（各签各的文件）' ((@($latestF.platforms.'windows-x86_64'.signature, $debEntry.signature, $tarEntry.signature) | Sort-Object -Unique).Count -eq 3)
    $shaInstaller = (Get-FileHash -LiteralPath (Join-Path $outF $installerName) -Algorithm SHA256).Hash.ToLowerInvariant()
    $shaDeb = (Get-FileHash -LiteralPath (Join-Path $outF $debName) -Algorithm SHA256).Hash.ToLowerInvariant()
    $shaTar = (Get-FileHash -LiteralPath (Join-Path $outF $tarName) -Algorithm SHA256).Hash.ToLowerInvariant()
    $sumsF = [System.IO.File]::ReadAllText((Join-Path $outF 'SHA256SUMS.txt'))
    Check 'SHA256SUMS.txt 含全部三个文件（安装包、deb、tar.gz，各一行，格式同 sha256sum）' ($sumsF -ceq "$shaInstaller  $installerName`n$shaDeb  $debName`n$shaTar  $tarName`n")
    $relF = [System.IO.File]::ReadAllText((Join-Path $outF 'RELEASE_NOTES.md'))
    $tailF = "`n`n安装包 SHA-256：" + [char]96 + $shaInstaller + [char]96 + "`n`n$debName SHA-256：" + [char]96 + $shaDeb + [char]96 + "`n`n$tarName SHA-256：" + [char]96 + $shaTar + [char]96 + "`n"
    Check 'Release 正文：更新说明 + 安装包一行（格式不变）+ deb 一行 + tar.gz 一行' ($relF -ceq ($notesText + $tailF))
    $expectedF = @($installerName, "$installerName.sig", $debName, "$debName.sig", $tarName, "$tarName.sig", 'latest.json', 'RELEASE_NOTES.md', 'SHA256SUMS.txt') | Sort-Object
    $filesF = @(Get-ChildItem -LiteralPath $outF | ForEach-Object Name) | Sort-Object
    Check '产物文件清单：安装包、deb、tar.gz 各带 .sig，加 latest.json、RELEASE_NOTES.md、SHA256SUMS.txt' (($filesF -join '|') -eq ($expectedF -join '|'))

    Step 'G. Linux 包的独立验签（release_tool verify）'
    $sigDeb = Join-Path $outF "$debName.sig"
    $sigTar = Join-Path $outF "$tarName.sig"
    Check 'deb + 签名 + 版本验过' ((Invoke-Verify $pubkey (Join-Path $outF $debName) $sigDeb $version) -eq 0)
    Check 'tar.gz + 签名 + 版本验过' ((Invoke-Verify $pubkey (Join-Path $outF $tarName) $sigTar $version) -eq 0)
    Check 'latest.json 里的签名就是 .sig 文件的内容（deb、tar.gz）' (($debEntry.signature -eq (Get-Content -LiteralPath $sigDeb -Raw).Trim()) -and ($tarEntry.signature -eq (Get-Content -LiteralPath $sigTar -Raw).Trim()))
    Check '拿 deb 的签名去验 tar.gz 失败' ((Invoke-Verify $pubkey (Join-Path $outF $tarName) $sigDeb $version) -ne 0)
    Check '版本对不上验签失败（deb）' ((Invoke-Verify $pubkey (Join-Path $outF $debName) $sigDeb '9.9.8') -ne 0)
    $tamperedDeb = Join-Path $runDir 'tampered.deb'
    $bytesDeb = [System.IO.File]::ReadAllBytes((Join-Path $outF $debName)); $bytesDeb[0] = $bytesDeb[0] -bxor 0xFF
    [System.IO.File]::WriteAllBytes($tamperedDeb, $bytesDeb)
    Check 'deb 被改过验签失败' ((Invoke-Verify $pubkey $tamperedDeb $sigDeb $version) -ne 0)
    Check '另一把私钥的公钥验不过（deb）' ((Invoke-Verify (Get-DerivedPubkey $repoRoot $otherKey) (Join-Path $outF $debName) $sigDeb $version) -ne 0)

    Step 'H. 反例：这些 Linux 资产的情况 release-sign.ps1 必须失败'
    $outH = Join-Path $runDir 'H'
    New-Item -ItemType Directory -Force -Path $outH | Out-Null
    Copy-Item -LiteralPath $fake -Destination (Join-Path $outH $installerName)
    Copy-Item -LiteralPath (Join-Path $outF $debName) -Destination (Join-Path $outH $debName)
    Copy-Item -LiteralPath (Join-Path $outF $tarName) -Destination (Join-Path $outH $tarName)
    $signH = @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outH $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outH, '-UpdaterJson', $updaterJson)
    $code = Invoke-Script ($signH + @('-LinuxDebPath', (Join-Path $outH $debName)))
    Check '只给 deb 不给 tar.gz -> 失败' ($code -ne 0)
    $code = Invoke-Script ($signH + @('-LinuxTarPath', (Join-Path $outH $tarName)))
    Check '只给 tar.gz 不给 deb -> 失败' ($code -ne 0)
    Check '……且没有产出 latest.json' (-not (Test-Path -LiteralPath (Join-Path $outH 'latest.json')))
    $wrongName = Join-Path $outH 'renamed.deb'
    Copy-Item -LiteralPath (Join-Path $outH $debName) -Destination $wrongName
    $code = Invoke-Script ($signH + @('-LinuxDebPath', $wrongName, '-LinuxTarPath', (Join-Path $outH $tarName)))
    Check 'deb 文件名与版本不符 -> 失败' ($code -ne 0)
    $code = Invoke-Script ($signH + @('-LinuxDebPath', (Join-Path $outH $debName), '-LinuxTarPath', (Join-Path $outH 'missing.tar.gz')))
    Check 'tar.gz 不存在 -> 失败' ($code -ne 0)
    $signHOther = @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outH $installerName),
        '-KeyPath', $otherKey, '-OutDir', $outH, '-UpdaterJson', $updaterJson)
    $code = Invoke-Script ($signHOther + @('-LinuxDebPath', (Join-Path $outH $debName), '-LinuxTarPath', (Join-Path $outH $tarName)))
    Check '私钥与 updater.json 公钥不是一对（带 Linux 包）-> 失败' ($code -ne 0)
    Check '……且仍然没有产出 latest.json' (-not (Test-Path -LiteralPath (Join-Path $outH 'latest.json')))
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
