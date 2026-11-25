# 给已经打好的安装包签名并生成发布要上传的全部文件：<安装包>.sig、latest.json、SHA256SUMS.txt、RELEASE_NOTES.md。
# GitHub Actions 的发版工作流（.github/workflows/release.yml）在"签名"一步调用它；本机也能跑（演练见 scripts/release-drill.ps1）。
#
# 用法：
#   powershell -File scripts/release-sign.ps1 -Version 0.4.0 -InstallerPath dist\0.4.0\ZeeTerm-Setup-0.4.0.exe `
#       -KeyPath <私钥文件> -OutDir dist\0.4.0 -Repo MoeShinX/ZeeTerm
#   带 Linux 包时再加 -LinuxDebPath dist\0.4.0\ZeeTerm-0.4.0-linux-x86_64.deb -LinuxTarPath dist\0.4.0\ZeeTerm-0.4.0-linux-x86_64.tar.gz
#   （两个一起给；用同一把私钥签名，latest.json 加 linux-x86_64-deb / linux-x86_64-tar 两个条目）。
#   带 Windows 免安装版时再加 -PortablePath dist\0.4.0\ZeeTerm-0.4.0-windows-x64-portable.zip
#   （同一把私钥签名，latest.json 加 windows-x86_64-portable 一个条目；老客户端只认自己的 key，不受影响）。
#   带 macOS 包时再加 -MacZipPath dist\0.4.0\ZeeTerm-0.4.0-macos-arm64.zip
#   （发版工作流里已由 macos-sign 用 Developer ID 签名、公证：**不用更新私钥签名、不生成 .sig、不进 latest.json**，只写进 SHA256SUMS.txt 和 Release 正文；
#   zip 的内容由发版工作流在调本脚本之前用 release-lib.ps1 的 Assert-MacosZip 核对，本脚本只核对文件名和存在）。
#
# 私钥口令只从环境变量 ZEETERM_UPDATER_KEY_PASSWORD 读（私钥没加密时不用设）；本脚本不提示输入（CI 里没有人可问）、
# 不打印私钥和口令，签名工具也不开详细输出。
# 先核对 updater.json 里的公钥与私钥是一对（占位值或对不上就失败：这样发出去的版本用户永远验不过签名）；
# 更新说明要两份：docs/release-notes/<版本>.md（中文，写进 latest.json 的 notes）和 <版本>.en.md（英文，写进 notes_en，
# 同时是 Release 正文），缺任何一份都失败；产物格式与 release.ps1 完全一致（两边共用 release-lib.ps1）。
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$InstallerPath,
    [Parameter(Mandatory = $true)][string]$KeyPath,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Repo = 'MoeShinX/ZeeTerm',
    # Linux x86_64 的 deb 和 tar.gz（GitHub 发版里由 Linux 任务编好、下载到这里）；两个一起给或都不给。
    [string]$LinuxDebPath = '',
    [string]$LinuxTarPath = '',
    # Linux ARM64 的 deb 和 tar.gz；两个一起给或都不给。
    [string]$LinuxAarch64DebPath = '',
    [string]$LinuxAarch64TarPath = '',
    # Windows 免安装版的 zip（GitHub 发版里由 Windows 任务和安装包一起打好）；不给就不发免安装版。
    [string]$PortablePath = '',
    # macOS arm64 / x64 的 zip；不签名、不进 latest.json。
    [string]$MacZipPath = '',
    [string]$MacX64ZipPath = '',
    # 默认读仓库里的 updater.json；演练时指向一份带测试公钥的副本。
    [string]$UpdaterJson = '',
    # 默认读仓库里的 docs/release-notes；演练时指向一个只放了 <版本>.md（或再加 <版本>.en.md）的临时目录。
    [string]$NotesDir = ''
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'release-lib.ps1')
if (-not $UpdaterJson) { $UpdaterJson = Join-Path $repoRoot 'native\zeeterm-update\updater.json' }

if (-not (Test-VersionFormat $Version)) { Stop-WithError "版本号格式不对：$Version" }
if (-not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) { Stop-WithError "找不到安装包：$InstallerPath" }
if (-not (Test-Path -LiteralPath $KeyPath -PathType Leaf)) { Stop-WithError "找不到更新私钥文件：$KeyPath" }
$installerName = Split-Path -Leaf $InstallerPath
if ($installerName -ne "ZeeTerm-Setup-$Version.exe") {
    Stop-WithError "安装包文件名应为 ZeeTerm-Setup-$Version.exe（latest.json 里的下载地址按它拼），实际是 $installerName。"
}
if (([bool]$LinuxDebPath) -ne ([bool]$LinuxTarPath)) { Stop-WithError '-LinuxDebPath 和 -LinuxTarPath 要一起给（latest.json 里两个 Linux 条目缺一不可）。' }
if (([bool]$LinuxAarch64DebPath) -ne ([bool]$LinuxAarch64TarPath)) { Stop-WithError '-LinuxAarch64DebPath 和 -LinuxAarch64TarPath 要一起给（latest.json 里两个 ARM64 Linux 条目缺一不可）。' }
$extraAssets = @()
if ($PortablePath) {
    $portableName = "ZeeTerm-$Version-windows-x64-portable.zip"
    if (-not (Test-Path -LiteralPath $PortablePath -PathType Leaf)) { Stop-WithError "找不到免安装版：$PortablePath" }
    $portableLeaf = Split-Path -Leaf $PortablePath
    if ($portableLeaf -ne $portableName) { Stop-WithError "免安装版文件名应为 $portableName（latest.json 里的下载地址按它拼），实际是 $portableLeaf。" }
    $extraAssets += @{ Key = 'windows-x86_64-portable'; Path = $PortablePath }
}
if ($LinuxDebPath -or $LinuxAarch64DebPath) {
    foreach ($item in @(@{ Key = 'linux-x86_64-deb'; Path = $LinuxDebPath; Name = "ZeeTerm-$Version-linux-x86_64.deb" },
                        @{ Key = 'linux-x86_64-tar'; Path = $LinuxTarPath; Name = "ZeeTerm-$Version-linux-x86_64.tar.gz" },
                        @{ Key = 'linux-aarch64-deb'; Path = $LinuxAarch64DebPath; Name = "ZeeTerm-$Version-linux-aarch64.deb" },
                        @{ Key = 'linux-aarch64-tar'; Path = $LinuxAarch64TarPath; Name = "ZeeTerm-$Version-linux-aarch64.tar.gz" })) {
        if (-not $item.Path) { continue }
        if (-not (Test-Path -LiteralPath $item.Path -PathType Leaf)) { Stop-WithError "找不到 Linux 包：$($item.Path)" }
        $leaf = Split-Path -Leaf $item.Path
        if ($leaf -ne $item.Name) { Stop-WithError "Linux 包文件名应为 $($item.Name)（latest.json 里的下载地址按它拼），实际是 $leaf。" }
        $extraAssets += @{ Key = $item.Key; Path = $item.Path }
    }
}
foreach ($mac in @(@{ Path = $MacZipPath; Arch = 'arm64' }, @{ Path = $MacX64ZipPath; Arch = 'x64' })) {
    if (-not $mac.Path) { continue }
    $macName = Get-MacosZipName $Version $mac.Arch
    if (-not (Test-Path -LiteralPath $mac.Path -PathType Leaf)) { Stop-WithError "找不到 macOS 包：$($mac.Path)" }
    $macLeaf = Split-Path -Leaf $mac.Path
    if ($macLeaf -cne $macName) { Stop-WithError "macOS 包文件名应为 $macName（Homebrew cask 和发版核对都按它找），实际是 $macLeaf。" }
}
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Step '核对更新公钥'
$configured = Get-ConfiguredPubkey $UpdaterJson
if ($configured -eq '' -or $configured.ToUpperInvariant().StartsWith('PLACEHOLDER')) {
    Stop-WithError "$UpdaterJson 里的 pubkey 还是占位值或为空：先用 scripts/updater-keygen.ps1 -WriteUpdaterJson 填好公钥并提交。"
}
$derived = Get-DerivedPubkey $repoRoot $KeyPath
if (-not $derived) { Stop-WithError '读不了更新私钥（口令不对，或私钥文件内容不对）。' }
if ($derived -ne $configured) {
    Stop-WithError 'updater.json 里的公钥与这把私钥不是一对：已发布的程序用该公钥验签，对不上用户就装不上更新。检查 Secrets 里的私钥是不是当初生成这个公钥的那一把。'
}
Write-Host 'updater.json 的公钥与私钥匹配。'

$notesOut = Join-Path $OutDir 'RELEASE_NOTES.md'
# 中文说明（latest.json 的 notes）只是中间文件，放在临时目录里用完就删，不进产物目录；英文说明就是 Release 正文 RELEASE_NOTES.md。
$notesZh = Join-Path ([System.IO.Path]::GetTempPath()) ("zeeterm-notes-zh-" + [guid]::NewGuid().ToString('N') + '.md')
try {
    Write-ReleaseNotes -RepoRoot $repoRoot -Version $Version -OutFile $notesZh -EnOutFile $notesOut -NotesDir $NotesDir -RequireUserNotes
    $artifacts = New-ReleaseArtifacts -RepoRoot $repoRoot -Version $Version -Installer $InstallerPath -KeyPath $KeyPath `
        -ConfiguredPubkey $configured -NotesOut $notesOut -NotesZh $notesZh -OutDir $OutDir -Repo $Repo -ExtraAssets $extraAssets `
        -MacZip $MacZipPath -MacX64Zip $MacX64ZipPath
} finally {
    if (Test-Path -LiteralPath $notesZh) { Remove-Item -LiteralPath $notesZh -Force }
}

Write-Host ''
Write-Host "发布文件：$OutDir" -ForegroundColor Green
Get-ChildItem -LiteralPath $OutDir | ForEach-Object { Write-Host ("  {0}  ({1:N0} 字节)" -f $_.Name, $_.Length) }
Write-Host "  安装包 SHA-256：$($artifacts.Sha256)"
if ($MacZipPath -or $MacX64ZipPath) { Write-Host '  macOS 包没有用更新私钥签名，也不在 latest.json 里：只写进了 SHA256SUMS.txt 和 Release 正文。' }
exit 0
