# 给已经打好的安装包签名并生成发布要上传的全部文件：<安装包>.sig、latest.json、SHA256SUMS.txt、RELEASE_NOTES.md。
# GitHub Actions 的发版工作流（.github/workflows/release.yml）在"签名"一步调用它；本机也能跑（演练见 scripts/release-drill.ps1）。
#
# 用法：
#   powershell -File scripts/release-sign.ps1 -Version 0.4.0 -InstallerPath dist\0.4.0\ZeeTerm-Setup-0.4.0.exe `
#       -KeyPath <私钥文件> -OutDir dist\0.4.0 -Repo MoeShinX/ZeeTerm
#
# 私钥口令只从环境变量 ZEETERM_UPDATER_KEY_PASSWORD 读（私钥没加密时不用设）；本脚本不提示输入（CI 里没有人可问）、
# 不打印私钥和口令，签名工具也不开详细输出。
# 先核对 updater.json 里的公钥与私钥是一对（占位值或对不上就失败：这样发出去的版本用户永远验不过签名）；
# 更新说明只认 docs/release-notes/<版本>.md；产物格式与 release.ps1 完全一致（两边共用 release-lib.ps1）。
param(
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$InstallerPath,
    [Parameter(Mandatory = $true)][string]$KeyPath,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [string]$Repo = 'MoeShinX/ZeeTerm',
    # 默认读仓库里的 updater.json；演练时指向一份带测试公钥的副本。
    [string]$UpdaterJson = ''
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'release-lib.ps1')
if (-not $UpdaterJson) { $UpdaterJson = Join-Path $repoRoot 'native\zssh-update\updater.json' }

if (-not (Test-VersionFormat $Version)) { Stop-WithError "版本号格式不对：$Version" }
if (-not (Test-Path -LiteralPath $InstallerPath -PathType Leaf)) { Stop-WithError "找不到安装包：$InstallerPath" }
if (-not (Test-Path -LiteralPath $KeyPath -PathType Leaf)) { Stop-WithError "找不到更新私钥文件：$KeyPath" }
$installerName = Split-Path -Leaf $InstallerPath
if ($installerName -ne "ZeeTerm-Setup-$Version.exe") {
    Stop-WithError "安装包文件名应为 ZeeTerm-Setup-$Version.exe（latest.json 里的下载地址按它拼），实际是 $installerName。"
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
Write-ReleaseNotes -RepoRoot $repoRoot -Version $Version -OutFile $notesOut -RequireUserNotes
$artifacts = New-ReleaseArtifacts -RepoRoot $repoRoot -Version $Version -Installer $InstallerPath -KeyPath $KeyPath `
    -ConfiguredPubkey $configured -NotesOut $notesOut -OutDir $OutDir -Repo $Repo

Write-Host ''
Write-Host "发布文件：$OutDir" -ForegroundColor Green
Get-ChildItem -LiteralPath $OutDir | ForEach-Object { Write-Host ("  {0}  ({1:N0} 字节)" -f $_.Name, $_.Length) }
Write-Host "  安装包 SHA-256：$($artifacts.Sha256)"
exit 0
