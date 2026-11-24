# 发版后更新 Homebrew tap 里的 cask（MoeShinX/homebrew-tap 的 Casks/zeeterm.rb）。
#
# 同时支持两种 cask 格式：
#   - 旧格式：只有一个 `sha256`，只对应 Apple Silicon；脚本只更新 arm64，不动 Intel。
#   - 新格式：`arch arm: "arm64", intel: "x64"` + `sha256 arm: "...", intel: "..."`；
#     脚本下载两个 zip，分别核对并更新两个 sha。
#     tap 什么时候迁到新格式由维护者在 Intel 真机验收后决定。
#
# 本脚本从 GitHub Release 下载 macOS 的 zip、算 sha256、改写 cask 里的 version/sha256；
# 带 -Commit 时在 tap 仓库里提交。**不推送**，推送由人确认后自己做。
#
# 用法：
#   powershell -File scripts/update-homebrew-cask.ps1 -Version 1.0.2 -TapDir <homebrew-tap 本地克隆> [-Commit]
#   powershell -File scripts/update-homebrew-cask.ps1 -Version 1.0.2 -TapDir <...> -AssetDir <含两个 zip 的目录>
# 需要：已登录的 gh（能读这个仓库的 Release）；给了 -AssetDir 时不需要联网。
param(
  [Parameter(Mandatory = $true)][string]$Version,
  [Parameter(Mandatory = $true)][string]$TapDir,
  [string]$Repo = 'MoeShinX/ZeeTerm',
  [string]$AssetDir = '',
  [switch]$Commit
)
$ErrorActionPreference = 'Stop'

if ($Version -notmatch '^\d+\.\d+\.\d+$') { throw "版本号要写成 x.y.z（不带 v）：$Version" }
$cask = Join-Path $TapDir 'Casks/zeeterm.rb'
if (-not (Test-Path $cask)) { throw "找不到 cask：$cask" }

$text = [System.IO.File]::ReadAllText($cask)
$versionPattern = '(?m)^(\s*version\s+")[^"]*(")'
if (([regex]::Matches($text, $versionPattern)).Count -ne 1) {
  throw "cask 里匹配 version 的行不是正好 1 处，没有改动"
}

$shaArmPattern = '(?m)^(\s*sha256\s+arm:\s*")[0-9a-fA-F]*(")'
$shaIntelPattern = '(?m)(\bintel:\s*")[0-9a-fA-F]*(")'
$shaOldPattern = '(?m)^(\s*sha256\s+")[0-9a-fA-F]*(")'
$isDual = ([regex]::Matches($text, $shaArmPattern)).Count -eq 1 -and
          ([regex]::Matches($text, $shaIntelPattern)).Count -eq 1

$assets = if ($isDual) {
  @("ZeeTerm-$Version-macos-arm64.zip", "ZeeTerm-$Version-macos-x64.zip")
} else {
  @("ZeeTerm-$Version-macos-arm64.zip")
}
if (-not $isDual -and ([regex]::Matches($text, $shaOldPattern)).Count -ne 1) {
  throw "cask 的 sha256 既不是旧格式的单行，也不是新格式的 sha256 arm:/intel:，没有改动"
}

$work = Join-Path ([System.IO.Path]::GetTempPath()) ("zt-cask-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null
try {
  $hashes = @{}
  foreach ($asset in $assets) {
    $zip = if ($AssetDir) {
      Join-Path $AssetDir $asset
    } else {
      & gh release download "v$Version" --repo $Repo --pattern $asset --dir $work
      if ($LASTEXITCODE -ne 0) { throw "下载 $asset 失败（Release v$Version 上传了 macOS 的 zip 吗？）" }
      Join-Path $work $asset
    }
    if (-not (Test-Path -LiteralPath $zip -PathType Leaf)) { throw "找不到 $asset：$zip" }
    if ((Get-Item -LiteralPath $zip).Length -le 0) { throw "$asset 是空文件" }
    $hashes[$asset] = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
  }
} finally {
  Remove-Item -Recurse -Force $work -ErrorAction SilentlyContinue
}

$new = [regex]::Replace($text, $versionPattern, "`${1}$Version`${2}")
if ($isDual) {
  $new = [regex]::Replace($new, $shaArmPattern, "`${1}$($hashes["ZeeTerm-$Version-macos-arm64.zip"])`${2}")
  $new = [regex]::Replace($new, $shaIntelPattern, "`${1}$($hashes["ZeeTerm-$Version-macos-x64.zip"])`${2}")
  $summary = "arm64 $($hashes["ZeeTerm-$Version-macos-arm64.zip"])，x64 $($hashes["ZeeTerm-$Version-macos-x64.zip"])"
} else {
  $new = [regex]::Replace($new, $shaOldPattern, "`${1}$($hashes["ZeeTerm-$Version-macos-arm64.zip"])`${2}")
  $summary = "arm64 $($hashes["ZeeTerm-$Version-macos-arm64.zip"])（旧格式，未改 Intel）"
}
if ($new -eq $text) {
  Write-Host "cask 已经是 $Version（$summary），不用改。"
  exit 0
}
[System.IO.File]::WriteAllText($cask, $new, (New-Object System.Text.UTF8Encoding($false)))
Write-Host "已更新 $cask：version $Version，sha256 $summary"

if ($Commit) {
  & git -C $TapDir add -- 'Casks/zeeterm.rb'
  if ($LASTEXITCODE -ne 0) { throw 'git add 失败' }
  & git -C $TapDir commit -m "zeeterm $Version"
  if ($LASTEXITCODE -ne 0) { throw 'git commit 失败' }
  Write-Host "已在 tap 仓库提交。确认无误后推送：git -C `"$TapDir`" push"
}
