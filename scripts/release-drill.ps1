# 发版流程演练（本机，不联网，不碰真实私钥）：用临时生成的测试密钥 + 一个充数的"安装包"，
# 把 GitHub 发版用的签名步骤（release-sign.ps1）和本机备用路径（release.ps1 -DryRun）各跑一遍，
# 核对两边的产物格式一致，再做几个反例（私钥与公钥对不上、安装包被改、口令不对）确认会失败。
# 另外演练 Linux 资产：release-sign.ps1 带上 deb 和 tar.gz（用充数文件），核对它们被同一把私钥签名、latest.json 多出
# linux-x86_64-deb / linux-x86_64-tar 两个条目而 Windows 条目不变、SHA256SUMS.txt 与 Release 正文包含全部文件，再做反例。
# 更新说明要中英两份：latest.json 的 notes 是中文、notes_en 是英文，Release 正文是英文加 SHA-256 一节；缺英文 / 英文是空的 /
# 缺中文都会失败（-NotesDir 指向临时目录，不碰仓库）。
# 免安装版同理：release-sign.ps1 带 -PortablePath（用充数 zip）时，latest.json 多出 windows-x86_64-portable 一项而安装包条目不变
# （老客户端只认自己的 key），zip 用同一把私钥签名、验得过，SHA256SUMS.txt 和 Release 正文含它，文件名 / 缺文件 / 私钥不对都会失败。
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

    $version = Get-CurrentVersion $repoRoot   # 当前版本有 docs/release-notes/<版本>.md 和 <版本>.en.md，直接拿来演练
    $notesText = ((Read-Text (Join-Path $repoRoot "docs\release-notes\$version.md")).Text -replace "`r`n", "`n").Trim()
    $notesEnText = ((Read-Text (Join-Path $repoRoot "docs\release-notes\$version.en.md")).Text -replace "`r`n", "`n").Trim()
    # Release 正文（英文）末尾的 SHA-256 一节：`n`n### SHA-256`n`n- `文件名`: `哈希` 每个文件一行，最后一个换行。
    function Get-ExpectedShaTail([object[]]$Pairs) {
        $tick = [string][char]96
        return "`n`n### SHA-256`n`n" + ((@($Pairs | ForEach-Object { "- $tick$($_.Name)${tick}: $tick$($_.Sha)$tick" })) -join "`n") + "`n"
    }
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
    $notesEnFile = Join-Path $runDir 'notes.en.md'
    Write-Text $notesEnFile ($notesEnText + "`n") $false
    $code = Invoke-Script @((Join-Path $PSScriptRoot 'release.ps1'), '-Version', '9.9.9', '-DryRun', '-KeyPath', $keyPath, '-OutRoot', $outBRoot,
        '-SkipGates', '-PrebuiltInstaller', $fake, '-NotesFile', $notesFile, '-NotesEnFile', $notesEnFile)
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
    Check 'latest.json 顶层字段是 version、notes、notes_en、pub_date、platforms' ($fieldsA -eq 'notes,notes_en,platforms,pub_date,version')
    Check 'latest.json.notes_en 是英文说明，两边一致，不带 SHA-256 行，且 notes 仍是中文那份（老客户端只认它）' ($latestA.notes_en -eq $latestB.notes_en -and $latestA.notes_en -eq $notesEnText -and $latestA.notes_en -notmatch 'SHA-256' -and $latestA.notes -ne $latestA.notes_en)
    Check 'pub_date 都是 UTC 秒级 RFC3339' (($latestA.pub_date -match '^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$') -and ($latestB.pub_date -match '^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$'))
    Check '下载地址格式一致' (($latestA.platforms.'windows-x86_64'.url -eq "https://github.com/MoeShinX/ZeeTerm/releases/download/v$version/$installerName") -and
        ($latestB.platforms.'windows-x86_64'.url -eq "https://github.com/MoeShinX/ZeeTerm/releases/download/v9.9.9/ZeeTerm-Setup-9.9.9.exe"))

    $shaA = (Get-FileHash -LiteralPath (Join-Path $outA $installerName) -Algorithm SHA256).Hash.ToLowerInvariant()
    $sumsA = [System.IO.File]::ReadAllText((Join-Path $outA 'SHA256SUMS.txt'))
    $sumsB = [System.IO.File]::ReadAllText((Join-Path $outB 'SHA256SUMS.txt'))
    Check 'SHA256SUMS.txt 格式：<小写 sha256>两个空格<文件名>换行' (($sumsA -ceq "$shaA  $installerName`n") -and ($sumsB -cmatch '^[0-9a-f]{64}  ZeeTerm-Setup-9\.9\.9\.exe\n$'))
    $relA = [System.IO.File]::ReadAllText((Join-Path $outA 'RELEASE_NOTES.md'))
    $relB = [System.IO.File]::ReadAllText((Join-Path $outB 'RELEASE_NOTES.md'))
    $tail = Get-ExpectedShaTail @(@{ Name = $installerName; Sha = $shaA })
    Check 'Release 正文 = 英文更新说明 + “### SHA-256”一节（每个文件一行“- `文件名`: `哈希`”）' ($relA -ceq ($notesEnText + $tail))
    Check 'Release 正文不含中文说明（GitHub 上是英文）' (-not $relA.Contains($notesText))
    Check 'Release 正文末尾格式两边一致' (($relB -cmatch ('^' + [regex]::Escape($notesEnText) + '\n\n### SHA-256\n\n- `ZeeTerm-Setup-9\.9\.9\.exe`: `[0-9a-f]{64}`\n$')))
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
    $tailF = Get-ExpectedShaTail @(@{ Name = $installerName; Sha = $shaInstaller }, @{ Name = $debName; Sha = $shaDeb }, @{ Name = $tarName; Sha = $shaTar })
    Check 'Release 正文：英文更新说明 + SHA-256 一节（安装包、deb、tar.gz 各一行）' ($relF -ceq ($notesEnText + $tailF))
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

    Step 'I. Windows 免安装版：release-sign.ps1 带 -PortablePath（单独、以及和 Linux 包一起）'
    $portableName = "ZeeTerm-$version-windows-x64-portable.zip"
    $outI = Join-Path $runDir 'I'
    New-Item -ItemType Directory -Force -Path $outI | Out-Null
    Copy-Item -LiteralPath $fake -Destination (Join-Path $outI $installerName)
    $rngPortable = New-Object System.Random
    $bytesPortable = New-Object byte[] 3072
    $rngPortable.NextBytes($bytesPortable)
    [System.IO.File]::WriteAllBytes((Join-Path $outI $portableName), $bytesPortable)
    $signI = @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outI $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outI, '-Repo', 'MoeShinX/ZeeTerm', '-UpdaterJson', $updaterJson)
    $code = Invoke-Script ($signI + @('-PortablePath', (Join-Path $outI $portableName)))
    Check 'release-sign.ps1（带免安装版）退出码 0' ($code -eq 0) "(退出码 $code)"
    $latestI = Get-Content -LiteralPath (Join-Path $outI 'latest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    Check 'platforms 恰好是 windows-x86_64、windows-x86_64-portable' ((($latestI.platforms.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'windows-x86_64,windows-x86_64-portable')
    Check 'Windows 安装包条目不变：老客户端选到的包和没有免安装版时一样（地址、字段、版本、说明）' (($latestI.platforms.'windows-x86_64'.url -eq $latestA.platforms.'windows-x86_64'.url) -and
        ((($latestI.platforms.'windows-x86_64'.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'signature,url') -and ($latestI.version -eq $latestA.version) -and ($latestI.notes -eq $latestA.notes))
    $portableEntry = $latestI.platforms.'windows-x86_64-portable'
    Check '免安装版条目：地址按 tag 和文件名拼，字段是 signature、url' (($portableEntry.url -eq "https://github.com/MoeShinX/ZeeTerm/releases/download/v$version/$portableName") -and ((($portableEntry.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'signature,url') -and $portableEntry.signature)
    Check '免安装版和安装包的签名互不相同（各签各的文件）' ($portableEntry.signature -ne $latestI.platforms.'windows-x86_64'.signature)
    $shaInstallerI = (Get-FileHash -LiteralPath (Join-Path $outI $installerName) -Algorithm SHA256).Hash.ToLowerInvariant()
    $shaPortable = (Get-FileHash -LiteralPath (Join-Path $outI $portableName) -Algorithm SHA256).Hash.ToLowerInvariant()
    $sumsI = [System.IO.File]::ReadAllText((Join-Path $outI 'SHA256SUMS.txt'))
    Check 'SHA256SUMS.txt 含安装包和免安装版（各一行，格式同 sha256sum）' ($sumsI -ceq "$shaInstallerI  $installerName`n$shaPortable  $portableName`n")
    $relI = [System.IO.File]::ReadAllText((Join-Path $outI 'RELEASE_NOTES.md'))
    $tailI = Get-ExpectedShaTail @(@{ Name = $installerName; Sha = $shaInstallerI }, @{ Name = $portableName; Sha = $shaPortable })
    Check 'Release 正文：英文更新说明 + SHA-256 一节（安装包、免安装版各一行）' ($relI -ceq ($notesEnText + $tailI))
    $expectedI = @($installerName, "$installerName.sig", $portableName, "$portableName.sig", 'latest.json', 'RELEASE_NOTES.md', 'SHA256SUMS.txt') | Sort-Object
    $filesI = @(Get-ChildItem -LiteralPath $outI | ForEach-Object Name) | Sort-Object
    Check '产物文件清单：安装包、免安装版各带 .sig，加 latest.json、RELEASE_NOTES.md、SHA256SUMS.txt' (($filesI -join '|') -eq ($expectedI -join '|'))
    $sigPortable = Join-Path $outI "$portableName.sig"
    Check '免安装版 + 签名 + 版本验过' ((Invoke-Verify $pubkey (Join-Path $outI $portableName) $sigPortable $version) -eq 0)
    Check 'latest.json 里的签名就是 .sig 文件的内容' ($portableEntry.signature -eq (Get-Content -LiteralPath $sigPortable -Raw).Trim())
    Check '拿安装包的签名去验免安装版失败' ((Invoke-Verify $pubkey (Join-Path $outI $portableName) (Join-Path $outI "$installerName.sig") $version) -ne 0)
    Check '版本对不上验签失败（免安装版）' ((Invoke-Verify $pubkey (Join-Path $outI $portableName) $sigPortable '9.9.8') -ne 0)
    $tamperedPortable = Join-Path $runDir 'tampered.zip'
    $bytesTampered = [System.IO.File]::ReadAllBytes((Join-Path $outI $portableName)); $bytesTampered[0] = $bytesTampered[0] -bxor 0xFF
    [System.IO.File]::WriteAllBytes($tamperedPortable, $bytesTampered)
    Check '免安装版被改过验签失败' ((Invoke-Verify $pubkey $tamperedPortable $sigPortable $version) -ne 0)

    # 免安装版 + Linux 包一起：四个条目，免安装版排在 Linux 前面，其余格式不变。
    $outI2 = Join-Path $runDir 'I2'
    New-Item -ItemType Directory -Force -Path $outI2 | Out-Null
    foreach ($name in @($installerName, $portableName, $debName, $tarName)) {
        $source = if ($name -eq $installerName) { Join-Path $outI $installerName } elseif ($name -eq $portableName) { Join-Path $outI $portableName } else { Join-Path $outF $name }
        Copy-Item -LiteralPath $source -Destination (Join-Path $outI2 $name)
    }
    $signI2 = @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outI2 $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outI2, '-Repo', 'MoeShinX/ZeeTerm', '-UpdaterJson', $updaterJson)
    $code = Invoke-Script ($signI2 + @('-PortablePath', (Join-Path $outI2 $portableName), '-LinuxDebPath', (Join-Path $outI2 $debName), '-LinuxTarPath', (Join-Path $outI2 $tarName)))
    Check 'release-sign.ps1（免安装版 + Linux 包）退出码 0' ($code -eq 0) "(退出码 $code)"
    $latestI2 = Get-Content -LiteralPath (Join-Path $outI2 'latest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    Check 'platforms 是 linux 两个、windows 安装包、免安装版共四项' ((($latestI2.platforms.PSObject.Properties.Name | Sort-Object) -join ',') -eq 'linux-x86_64-deb,linux-x86_64-tar,windows-x86_64,windows-x86_64-portable')
    $sumsI2 = [System.IO.File]::ReadAllText((Join-Path $outI2 'SHA256SUMS.txt'))
    Check 'SHA256SUMS.txt 四个文件各一行' (($sumsI2 -split "`n" | Where-Object { $_ }).Count -eq 4 -and $sumsI2.Contains("  $portableName`n"))

    Step 'J. 反例：这些免安装版的情况 release-sign.ps1 必须失败'
    $outJ = Join-Path $runDir 'J'
    New-Item -ItemType Directory -Force -Path $outJ | Out-Null
    Copy-Item -LiteralPath $fake -Destination (Join-Path $outJ $installerName)
    Copy-Item -LiteralPath (Join-Path $outI $portableName) -Destination (Join-Path $outJ $portableName)
    $wrongPortable = Join-Path $outJ 'renamed.zip'
    Copy-Item -LiteralPath (Join-Path $outJ $portableName) -Destination $wrongPortable
    $signJ = @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outJ $installerName),
        '-KeyPath', $keyPath, '-OutDir', $outJ, '-UpdaterJson', $updaterJson)
    $code = Invoke-Script ($signJ + @('-PortablePath', $wrongPortable))
    Check '免安装版文件名与版本不符 -> 失败' ($code -ne 0)
    $code = Invoke-Script ($signJ + @('-PortablePath', (Join-Path $outJ 'missing.zip')))
    Check '免安装版不存在 -> 失败' ($code -ne 0)
    $signJOther = @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $outJ $installerName),
        '-KeyPath', $otherKey, '-OutDir', $outJ, '-UpdaterJson', $updaterJson)
    $code = Invoke-Script ($signJOther + @('-PortablePath', (Join-Path $outJ $portableName)))
    Check '私钥与 updater.json 公钥不是一对（带免安装版）-> 失败' ($code -ne 0)
    Check '……且没有产出 latest.json' (-not (Test-Path -LiteralPath (Join-Path $outJ 'latest.json')))

    Step 'K. 更新说明要中英两份：缺英文 / 英文是空的 / 缺中文，release-sign.ps1 必须失败（-NotesDir 指向临时目录，不碰仓库）'
    function Invoke-ScriptWithText([string[]]$ScriptArgs) {
        $previous = $ErrorActionPreference
        $ErrorActionPreference = 'Continue'
        try {
            $text = (& powershell -NoProfile -File @ScriptArgs 2>&1 | Out-String)
            return [pscustomobject]@{ Code = $LASTEXITCODE; Text = $text }
        } finally { $ErrorActionPreference = $previous }
    }
    function New-SignCase([string]$Name, [string]$ZhText, [string]$EnText) {
        $caseOut = Join-Path $runDir "K-$Name"
        $caseNotes = Join-Path $runDir "K-$Name-notes"
        New-Item -ItemType Directory -Force -Path $caseOut, $caseNotes | Out-Null
        Copy-Item -LiteralPath $fake -Destination (Join-Path $caseOut $installerName)
        if ($null -ne $ZhText) { Write-Text (Join-Path $caseNotes "$version.md") $ZhText $false }
        if ($null -ne $EnText) { Write-Text (Join-Path $caseNotes "$version.en.md") $EnText $false }
        $result = Invoke-ScriptWithText @((Join-Path $PSScriptRoot 'release-sign.ps1'), '-Version', $version, '-InstallerPath', (Join-Path $caseOut $installerName),
            '-KeyPath', $keyPath, '-OutDir', $caseOut, '-UpdaterJson', $updaterJson, '-NotesDir', $caseNotes)
        return [pscustomobject]@{ Code = $result.Code; Text = $result.Text; Out = $caseOut }
    }
    $caseNoEn = New-SignCase 'no-en' "中文说明`n" $null
    Check '缺英文说明 -> 失败' ($caseNoEn.Code -ne 0)
    Check '……错误里写清缺的是哪个文件、每次发版要两份' (($caseNoEn.Text.Contains("$version.en.md")) -and $caseNoEn.Text.Contains('两份'))
    Check '……且没有产出 latest.json 和 Release 正文' (-not (Test-Path -LiteralPath (Join-Path $caseNoEn.Out 'latest.json')))
    $caseEmptyEn = New-SignCase 'empty-en' "中文说明`n" "  `n`n"
    Check '英文说明是空的 -> 失败' (($caseEmptyEn.Code -ne 0) -and $caseEmptyEn.Text.Contains('是空的'))
    $caseNoZh = New-SignCase 'no-zh' $null "English notes`n"
    Check '缺中文说明 -> 失败' (($caseNoZh.Code -ne 0) -and $caseNoZh.Text.Contains("$version.md"))
    $caseOk = New-SignCase 'ok' "### 新功能`n`n- 中文要点`n" "### New`n`n- English point`n"
    Check '中英两份都有 -> 成功' ($caseOk.Code -eq 0) "(退出码 $($caseOk.Code))：$($caseOk.Text)"
    $latestK = Get-Content -LiteralPath (Join-Path $caseOk.Out 'latest.json') -Raw -Encoding UTF8 | ConvertFrom-Json
    Check 'notes 是中文、notes_en 是英文' (($latestK.notes -eq "### 新功能`n`n- 中文要点") -and ($latestK.notes_en -eq "### New`n`n- English point"))
    $relK = [System.IO.File]::ReadAllText((Join-Path $caseOk.Out 'RELEASE_NOTES.md'))
    Check 'Release 正文是英文说明加 SHA-256 一节，不含中文说明' ($relK.StartsWith("### New`n`n- English point`n`n### SHA-256`n") -and -not $relK.Contains('中文要点'))
    Check '中文说明的临时文件没有留在产物目录里' ((@(Get-ChildItem -LiteralPath $caseOk.Out | ForEach-Object Name) | Sort-Object) -join '|' -eq ((@($installerName, "$installerName.sig", 'latest.json', 'RELEASE_NOTES.md', 'SHA256SUMS.txt') | Sort-Object) -join '|'))
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
