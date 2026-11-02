# 构建 ZeeTerm 的独立 NSIS 安装包。
# 只用 native 工作区的产物，并额外带上 AI 工具桥依赖的 zssh-mcp.exe（每次都从根 workspace 现编，
# 不拿 target 里留着的旧文件）。不启动安装包，也不改 Tauri 安装包的配置和产物，不签名。
#
# 用法：
#   pwsh native/installer/build.ps1                                   # 正式流程：版本取 native 工作区的版本
#   pwsh native/installer/build.ps1 -TestVersion 9.9.9 -OutDir <目录> -StageDir <目录>
#       仅测试包：只把安装包里的版本号（文件名、DisplayVersion、版本资源）换成临时指定的值，
#       用来测"旧版 -> 新版"的静默升级；不改仓库里任何版本号，程序本身的版本仍是仓库里的。
param(
    [string]$TestVersion = '',
    [string]$OutDir = '',
    [string]$StageDir = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

# 以脚本所在目录为基准定位仓库根目录，不依赖调用方当前目录。
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$repoRoot = (Resolve-Path (Join-Path $scriptDir '..\..')).Path
$nativeRoot = Join-Path $repoRoot 'native'
$nativeManifest = Join-Path $nativeRoot 'Cargo.toml'
$rootManifest = Join-Path $repoRoot 'Cargo.toml'
$nsiFile = Join-Path $scriptDir 'zssh-native.nsi'
if (-not $StageDir) { $StageDir = Join-Path $scriptDir 'stage' }
if (-not $OutDir) { $OutDir = Join-Path $scriptDir 'out' }

# 产品名：和 native/zssh-gpui/src/product.rs、zssh-native.nsi 里的写法一致（有测试互相核对）。
$productName = 'ZeeTerm'
$mainExeName = "$productName.exe"

function Stop-WithError([string]$Message) {
    [Console]::Error.WriteLine("错误：$Message")
    exit 1
}

function Copy-Required([string]$Source, [string]$Destination) {
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        Stop-WithError "缺少必要文件：$Source"
    }
    Copy-Item -LiteralPath $Source -Destination $Destination -Force
}

# 仓库根的 LICENSE / NOTICE 是和 Tauri 版共用的文件，里面写的产品名是 Z-SSH；
# 安装包里的副本把产品名换成 ZeeTerm（第三方署名里没有这个词，原样保留）。不改仓库里的原文件。
function Copy-Branded([string]$Source, [string]$Destination) {
    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        Stop-WithError "缺少必要文件：$Source"
    }
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    $text = [System.IO.File]::ReadAllText($Source, $utf8)
    [System.IO.File]::WriteAllText($Destination, $text.Replace('Z-SSH', $productName), $utf8)
}

# 安装包里的 NOTICE = 根 NOTICE + 空一行 + native/NOTICE（ZeeTerm 额外使用的第三方组件），再换产品名。
# 拼法和 native/zssh-gpui/src/product.rs 的 combine_notices 一致（有测试核对）：换行统一成 LF，
# 根 NOTICE 去掉末尾空白，中间只留一个空行。
function Copy-BrandedNotice([string]$RootSource, [string]$NativeSource, [string]$Destination) {
    foreach ($source in @($RootSource, $NativeSource)) {
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            Stop-WithError "缺少必要文件：$source"
        }
    }
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    $root = [System.IO.File]::ReadAllText($RootSource, $utf8).Replace("`r`n", "`n").TrimEnd()
    $native = [System.IO.File]::ReadAllText($NativeSource, $utf8).Replace("`r`n", "`n")
    $merged = $root + "`n`n" + $native
    [System.IO.File]::WriteAllText($Destination, $merged.Replace('Z-SSH', $productName), $utf8)
}

# 版本号只从 native 工作区的 [workspace.package] 段读取（zssh-gpui 继承它），
# 避免匹配到依赖项里的 version。
$version = '0.0.0'
if (-not (Test-Path -LiteralPath $nativeManifest -PathType Leaf)) {
    Stop-WithError "缺少必要文件：$nativeManifest"
}
$nativeManifestText = Get-Content -LiteralPath $nativeManifest -Raw -Encoding UTF8
$sectionMatch = [regex]::Match($nativeManifestText, '(?ms)^\[workspace\.package\](.*?)(?=^\[|\z)')
if ($sectionMatch.Success) {
    $versionMatch = [regex]::Match($sectionMatch.Groups[1].Value, '(?m)^\s*version\s*=\s*"([^"]+)"')
    if ($versionMatch.Success -and -not [string]::IsNullOrWhiteSpace($versionMatch.Groups[1].Value)) {
        $version = $versionMatch.Groups[1].Value.Trim()
    }
}
if ($TestVersion) {
    if ($TestVersion -notmatch '^\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?$') {
        Stop-WithError "-TestVersion 要写成 X.Y.Z（可带 -预发布 / +构建 后缀）：$TestVersion"
    }
    Write-Host "提示：测试包，安装包里的版本号临时指定为 $TestVersion（仓库版本 $version 不变）。"
    $version = $TestVersion
}

# NSIS 的 VIProductVersion 只接受四段数字，这里把语义化版本压成 X.X.X.0。
$quadMatch = [regex]::Match($version, '^(\d+)\.(\d+)\.(\d+)')
if ($quadMatch.Success) {
    $versionQuad = "$($quadMatch.Groups[1].Value).$($quadMatch.Groups[2].Value).$($quadMatch.Groups[3].Value).0"
} else {
    $versionQuad = '0.0.0.0'
}

foreach ($required in @($nativeManifest, $rootManifest, $nsiFile)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        Stop-WithError "缺少必要文件：$required"
    }
}

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Stop-WithError "找不到 cargo，请先安装 Rust 工具链并把它加入 PATH。"
}

# 1. 编译 native 工作区（release）。
Write-Host "==> cargo build --release --manifest-path native/Cargo.toml"
& cargo build --release --locked --manifest-path $nativeManifest
$buildExit = $LASTEXITCODE
if ($buildExit -ne 0) {
    Stop-WithError "cargo build 失败，退出码 $buildExit。"
}

# 2. zssh-mcp 属于仓库根工作区：每次都现编，并从 cargo 报告的产物路径取，
#    不按固定位置去找（以前按位置找会拿到别处留下的旧文件）。
Write-Host "==> cargo build --release -p zssh-mcp（根工作区）"
$mcpExe = $null
$mcpOutput = & cargo build --release --locked -p zssh-mcp --manifest-path $rootManifest --message-format=json-render-diagnostics
$mcpExit = $LASTEXITCODE
if ($mcpExit -ne 0) {
    Stop-WithError "编译 zssh-mcp 失败，退出码 $mcpExit。"
}
foreach ($line in $mcpOutput) {
    if (-not $line -or -not $line.StartsWith('{')) { continue }
    try { $message = $line | ConvertFrom-Json } catch { continue }
    if ($message.reason -eq 'compiler-artifact' -and $message.target.name -eq 'zssh-mcp' -and $message.executable) {
        $mcpExe = $message.executable
    }
}
if (-not $mcpExe -or -not (Test-Path -LiteralPath $mcpExe -PathType Leaf)) {
    Stop-WithError "cargo 没有报告 zssh-mcp.exe 的产物路径（AI 工具依赖它）。"
}
Write-Host "zssh-mcp: 已现编（$mcpExe）"

# 3. 准备暂存目录，只放安装包需要的东西。
if (Test-Path -LiteralPath $StageDir) {
    Remove-Item -LiteralPath $StageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

Copy-Required (Join-Path $nativeRoot 'target\release\zssh-gpui.exe') (Join-Path $StageDir $mainExeName)
Copy-Required $mcpExe (Join-Path $StageDir 'zssh-mcp.exe')
Copy-Branded (Join-Path $repoRoot 'LICENSE') (Join-Path $StageDir 'LICENSE')
Copy-BrandedNotice (Join-Path $repoRoot 'NOTICE') (Join-Path $nativeRoot 'NOTICE') (Join-Path $StageDir 'NOTICE')
# 图标放在 native/installer/assets/，和 0.2.14 Tauri 包的是同一个文件（带着 Tauri 版的仓库里有测试
# 核对字节一致）；不依赖 Tauri 版的目录，原生版单独成库后也能打包。
Copy-Required (Join-Path $scriptDir 'assets\icon.ico') (Join-Path $StageDir 'icon.ico')

# 暂存目录的内容必须正好是这几个文件：多了少了都说明脚本和安装脚本对不上。
# 没有字体文件：ZeeTerm 用系统字体，没有随包带的字体资源。
$expected = @($mainExeName, 'zssh-mcp.exe', 'LICENSE', 'NOTICE', 'icon.ico') | Sort-Object
$actual = @(Get-ChildItem -LiteralPath $StageDir -File | ForEach-Object { $_.Name }) | Sort-Object
if (($expected -join '|') -ne ($actual -join '|')) {
    Stop-WithError "暂存目录内容不对：期望 [$($expected -join ', ')]，实际 [$($actual -join ', ')]。"
}

# 4. 按固定顺序查找 makensis：tauri 缓存的两个常见位置，再退回 PATH。
$makensis = $null
$makensisCandidates = @(
    (Join-Path $env:LOCALAPPDATA 'tauri\NSIS\makensis.exe'),
    (Join-Path $env:LOCALAPPDATA 'tauri\NSIS\Bin\makensis.exe')
)
# CI 的 runner 上 NSIS 装在 Program Files 里（不一定在 PATH）。
foreach ($programFiles in @(${env:ProgramFiles(x86)}, $env:ProgramFiles)) {
    if ($programFiles) {
        $makensisCandidates += (Join-Path $programFiles 'NSIS\makensis.exe')
    }
}
foreach ($candidate in $makensisCandidates) {
    if (Test-Path -LiteralPath $candidate -PathType Leaf) {
        $makensis = $candidate
        break
    }
}
if (-not $makensis) {
    $pathCommand = Get-Command makensis.exe -ErrorAction SilentlyContinue
    if ($pathCommand) {
        $makensis = $pathCommand.Source
    }
}
if (-not $makensis) {
    Stop-WithError "找不到 makensis.exe。已查找：$($makensisCandidates -join '；')；请安装 NSIS，或先运行一次 tauri 打包以获取 NSIS。"
}
Write-Host "==> makensis: $makensis"

# 5. 编译安装包。输出目录先准备好，避免 OutFile 因目录不存在而失败。
if (-not (Test-Path -LiteralPath $OutDir)) {
    New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
}
$outFile = Join-Path $OutDir "$productName-Setup-$version.exe"
if (Test-Path -LiteralPath $outFile) {
    Remove-Item -LiteralPath $outFile -Force
}

& $makensis /V2 `
    "/DAPP_VERSION=$version" `
    "/DAPP_VERSION_QUAD=$versionQuad" `
    "/DSTAGE_DIR=$StageDir" `
    "/DOUT_FILE=$outFile" `
    $nsiFile
$nsisExit = $LASTEXITCODE
if ($nsisExit -ne 0) {
    Stop-WithError "makensis 失败，退出码 $nsisExit。"
}
if (-not (Test-Path -LiteralPath $outFile -PathType Leaf)) {
    Stop-WithError "makensis 报告成功，但没有生成安装包：$outFile"
}

$size = (Get-Item -LiteralPath $outFile).Length
Write-Host ""
Write-Host "安装包已生成：$outFile"
Write-Host ("大小：{0:N0} 字节（{1:N2} MiB）" -f $size, ($size / 1MB))
exit 0
