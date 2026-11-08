# 发版后更新 Homebrew tap 里的 cask（MoeShinX/homebrew-tap 的 Casks/zeeterm.rb）。
#
# macOS 的 zip 是在 Mac 上打好后手动传到 Release 的，不经过发版工作流，所以 cask 的版本号和 sha256
# 只能在 zip 传上去之后再改。本脚本从 GitHub Release 下载这个 zip、算 sha256、改写 cask 里的
# `version` 和 `sha256` 两行；带 -Commit 时在 tap 仓库里提交。**不推送**，推送由人确认后自己做。
#
# 用法：
#   powershell -File scripts/update-homebrew-cask.ps1 -Version 1.0.2 -TapDir <homebrew-tap 本地克隆> [-Commit]
# 需要：已登录的 gh（能读这个仓库的 Release）。
param(
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][string]$TapDir,
  [string]$Repo = 'MoeShinX/ZeeTerm',
  [switch]$Commit
)
$ErrorActionPreference = 'Stop'

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "版本号要写成 x.y.z（不带 v）：$Version" }
$cask = Join-Path $TapDir 'Casks/zeeterm.rb'
if (-not (Test-Path $cask)) { throw "找不到 cask：$cask" }

$asset = "ZeeTerm-$Version-macos-arm64.zip"
$work = Join-Path ([System.IO.Path]::GetTempPath()) ("zt-cask-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
  & gh release download "v$Version" --repo $Repo --pattern $asset --dir $work
  if ($LASTEXITCODE -ne 0) { throw "下载 $asset 失败（Release v$Version 上传了 macOS 的 zip 吗？）" }
  $zip = Join-Path $work $asset
  if (-not (Test-Path $zip)) { throw "Release v$Version 上没有 $asset" }
  $sha = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
} finally {
  Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}

# 按字节读写，保持 cask 原来的换行符和编码。
$text = [System.IO.File]::ReadAllText($cask)
$versionPattern = '(?m)^(\s*version\s+")[^"]*(")'
$shaPattern = '(?m)^(\s*sha256\s+")[0-9a-fA-F]*(")'
foreach ($p in @($versionPattern, $shaPattern)) {
  $n = ([regex]::Matches($text, $p)).Count
  if ($n -ne 1) { throw "cask 里匹配 $p 的行有 $n 处，应当正好 1 处，没有改动" }
}
$new = [regex]::Replace($text, $versionPattern, "`${1}$Version`${2}")
$new = [regex]::Replace($new, $shaPattern, "`${1}$sha`${2}")
if ($new -eq $text) {
  Write-Host "cask 已经是 $Version（sha256 $sha），不用改。"
  exit 0
}
[System.IO.File]::WriteAllText($cask, $new, (New-Object System.Text.UTF8Encoding($false)))
Write-Host "已更新 $cask：version $Version，sha256 $sha"

if ($Commit) {
  & git -C $TapDir add -- 'Casks/zeeterm.rb'
  if ($LASTEXITCODE -ne 0) { throw 'git add 失败' }
  & git -C $TapDir commit -m "zeeterm $Version"
  if ($LASTEXITCODE -ne 0) { throw 'git commit 失败' }
  Write-Host "已在 tap 仓库提交。确认无误后推送：git -C `"$TapDir`" push"
}
