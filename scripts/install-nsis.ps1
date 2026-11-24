# 给 CI / 发版工作流准备 NSIS（makensis.exe）：机器上已经有就什么都不做，没有就下载固定版本的官方 zip。
#
#   pwsh -File scripts/install-nsis.ps1 [-Destination <目录>]
#
# 为什么不用 choco：GitHub 的 Windows 镜像已经不带 NSIS，Chocolatey 社区源经常 504，
# 而且 `choco install` 装失败时退出码仍是 0，错误要到下一步打包才暴露。
# 这里只认 NSIS 官方在 SourceForge 发布的 zip，版本和 SHA-256 写死在下面；哈希不对就失败，不会用下到的文件。
# 换版本时三样一起改：版本号、文件名、SHA-256（自己下载后用 Get-FileHash 算，并和 SourceForge 文件页的 MD5 / 大小核对）。
#
# 找到或装好之后：在 GitHub Actions 里把 makensis 所在目录写进 GITHUB_PATH，后面的步骤（build.ps1）从 PATH 找到它。
param(
    [string]$Destination = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$NsisVersion = '3.13'
$NsisZip = "nsis-$NsisVersion.zip"
$NsisSha256 = 'ba63dffc4410ee89193e1cb5a41989991bd77c61068da17e3156d136b7b0b3d8'
$NsisUrls = @(
    "https://downloads.sourceforge.net/project/nsis/NSIS%203/$NsisVersion/$NsisZip",
    "https://sourceforge.net/projects/nsis/files/NSIS%203/$NsisVersion/$NsisZip/download"
)

function Stop-WithError([string]$Message) {
    Write-Error $Message -ErrorAction Continue
    exit 1
}

# 和 native/installer/build.ps1 找 makensis 的位置保持一致：这些地方已经有，就不下载。
$known = @()
if ($env:LOCALAPPDATA) {
    $known += (Join-Path $env:LOCALAPPDATA 'tauri\NSIS\makensis.exe')
    $known += (Join-Path $env:LOCALAPPDATA 'tauri\NSIS\Bin\makensis.exe')
}
foreach ($programFiles in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
    if ($programFiles) {
        $known += (Join-Path $programFiles 'NSIS\makensis.exe')
    }
}
$existing = $known | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
if (-not $existing) {
    $onPath = Get-Command makensis.exe -ErrorAction SilentlyContinue
    if ($onPath) { $existing = $onPath.Source }
}
if ($existing) {
    Write-Host "==> 已有 makensis：$existing，不下载。"
    exit 0
}

if (-not $Destination) {
    if (-not $env:RUNNER_TEMP) {
        Stop-WithError '没有 -Destination，也不在 GitHub Actions 里（没有 RUNNER_TEMP）：请指定解压目录。'
    }
    $Destination = Join-Path $env:RUNNER_TEMP 'nsis'
}
New-Item -ItemType Directory -Force -Path $Destination | Out-Null
$zipPath = Join-Path $Destination $NsisZip

# 逐个地址、每个地址重试几次：SourceForge 会跳到不同镜像，偶尔有一个慢或者坏；文件一律按 SHA-256 核对。
$downloaded = $false
foreach ($url in $NsisUrls) {
    for ($attempt = 1; $attempt -le 3; $attempt++) {
        try {
            Write-Host "==> 下载 $NsisZip（第 $attempt 次）：$url"
            Invoke-WebRequest -Uri $url -OutFile $zipPath -UseBasicParsing -MaximumRedirection 10 -TimeoutSec 120
            $actual = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($actual -eq $NsisSha256) {
                $downloaded = $true
                break
            }
            Write-Warning "SHA-256 不对：期望 $NsisSha256，实际 $actual；丢弃这个文件。"
        } catch {
            Write-Warning "下载失败：$($_.Exception.Message)"
        }
        Remove-Item -LiteralPath $zipPath -Force -ErrorAction SilentlyContinue
        Start-Sleep -Seconds (5 * $attempt)
    }
    if ($downloaded) { break }
}
if (-not $downloaded) {
    Stop-WithError "没能下载到 SHA-256 正确的 $NsisZip。"
}

Expand-Archive -LiteralPath $zipPath -DestinationPath $Destination -Force
Remove-Item -LiteralPath $zipPath -Force
$nsisDir = Join-Path $Destination "nsis-$NsisVersion"
$makensis = Join-Path $nsisDir 'makensis.exe'
if (-not (Test-Path -LiteralPath $makensis -PathType Leaf)) {
    Stop-WithError "解压后找不到 $makensis。"
}

& $makensis /VERSION
if ($LASTEXITCODE -ne 0) {
    Stop-WithError "makensis /VERSION 退出码 $LASTEXITCODE。"
}

if ($env:GITHUB_PATH) {
    Add-Content -LiteralPath $env:GITHUB_PATH -Value $nsisDir -Encoding utf8
    Write-Host "==> 已把 $nsisDir 加进 GITHUB_PATH。"
} else {
    Write-Host "==> makensis 在 $makensis（不在 GitHub Actions 里，没有改 PATH）。"
}
