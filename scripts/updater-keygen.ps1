# 生成 ZeeTerm 的更新签名密钥（minisign 格式）。发版人自己跑一次，不需要装 minisign.exe。
#
# 用法（在仓库里任意目录都行）：
#   powershell -File scripts/updater-keygen.ps1                    # 私钥写到 %USERPROFILE%\.zeeterm\updater.key
#   powershell -File scripts/updater-keygen.ps1 -WriteUpdaterJson  # 同时把公钥写进 native/zssh-update/updater.json
#   powershell -File scripts/updater-keygen.ps1 -NoPassword        # 私钥不设口令（不推荐）
#   powershell -File scripts/updater-keygen.ps1 -KeyDir <目录>      # 把密钥放到别处（测试用）
#
# 做什么：
#   1. 目录里已经有 updater.key / updater.pub 就拒绝（绝不覆盖现有私钥）；
#   2. 提示输入口令（可以留空 = 不加密；口令只在本进程的环境变量里传给工具，不进命令行和日志）；
#   3. 调 zssh-update 的 release_tool 生成一对密钥：updater.key（私钥，保密）、updater.pub（公钥）；
#   4. 打印公钥（base64），这就是 native/zssh-update/updater.json 里 "pubkey" 要填的值。
#
# 私钥永远只在 KeyDir 里：不会写进仓库，也不会被 git 跟踪。生成之后请立刻备份私钥和口令：
# 丢了私钥，已安装的用户就收不到更新了（旧公钥验不了新签名，只能让用户重新下载安装包）。
param(
    [string]$KeyDir = '',
    [switch]$NoPassword,
    [switch]$WriteUpdaterJson
)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $KeyDir) { $KeyDir = Join-Path $env:USERPROFILE '.zeeterm' }

$keyFile = Join-Path $KeyDir 'updater.key'
$pubFile = Join-Path $KeyDir 'updater.pub'
if ((Test-Path -LiteralPath $keyFile) -or (Test-Path -LiteralPath $pubFile)) {
    [Console]::Error.WriteLine("错误：$KeyDir 里已经有密钥文件，拒绝覆盖。")
    [Console]::Error.WriteLine('如果确实要换一对新密钥，先自己确认旧私钥不再需要（换公钥后，装着旧版本的用户收不到更新），再手动移走旧文件。')
    exit 1
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    [Console]::Error.WriteLine('错误：找不到 cargo，请先安装 Rust 工具链。')
    exit 1
}

# 口令：读成 SecureString，转成明文只放进当前进程的环境变量，用完立刻清掉。
function Read-PlainPassword([string]$Prompt) {
    $secure = Read-Host -AsSecureString $Prompt
    $bstr = [System.Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try { return [System.Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr) }
    finally { [System.Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) }
}

$passwordEnv = 'ZEETERM_UPDATER_KEY_PASSWORD'
$previous = [Environment]::GetEnvironmentVariable($passwordEnv, 'Process')
$password = ''
if (-not $NoPassword) {
    $password = Read-PlainPassword '给私钥设一个口令（直接回车 = 不加密，不推荐）'
    if ($password) {
        $again = Read-PlainPassword '再输入一遍口令'
        if ($password -ne $again) {
            [Console]::Error.WriteLine('错误：两次输入的口令不一致。')
            exit 1
        }
    } else {
        Write-Host '提示：没有设口令，私钥文件泄露就等于别人能发更新。' -ForegroundColor Yellow
    }
}

try {
    [Environment]::SetEnvironmentVariable($passwordEnv, $(if ($password) { $password } else { $null }), 'Process')
    $manifest = Join-Path $repoRoot 'native\Cargo.toml'
    # --release：口令加密用的 scrypt 在 debug 构建里会慢得多。
    $output = & cargo run --quiet --release --locked --manifest-path $manifest -p zssh-update --example release_tool -- keygen --out-dir $KeyDir
    $code = $LASTEXITCODE
} finally {
    [Environment]::SetEnvironmentVariable($passwordEnv, $previous, 'Process')
}
if ($code -ne 0) { exit $code }

$pubkey = $null
foreach ($line in $output) {
    if ($line -match '^PUBKEY=(.+)$') { $pubkey = $Matches[1].Trim() }
}
if (-not $pubkey) {
    [Console]::Error.WriteLine('错误：工具没有输出公钥。')
    exit 1
}

if ($WriteUpdaterJson) {
    $jsonPath = Join-Path $repoRoot 'native\zssh-update\updater.json'
    $utf8 = New-Object System.Text.UTF8Encoding($false)
    $text = [System.IO.File]::ReadAllText($jsonPath, $utf8)
    $updated = [regex]::Replace($text, '("pubkey"\s*:\s*")[^"]*(")', { param($m) $m.Groups[1].Value + $pubkey + $m.Groups[2].Value })
    [System.IO.File]::WriteAllText($jsonPath, $updated, $utf8)
    Write-Host "已把公钥写进 $jsonPath（记得提交这个文件）。"
}

Write-Host ''
Write-Host '密钥已生成：'
Write-Host "  私钥（保密、请备份）：$keyFile"
Write-Host "  公钥文件：            $pubFile"
Write-Host ''
Write-Host '公钥（native/zssh-update/updater.json 里 "pubkey" 的值）：'
Write-Host $pubkey
Write-Host ''
Write-Host '下一步：'
if (-not $WriteUpdaterJson) {
    Write-Host '  1. 把上面的公钥填进 native/zssh-update/updater.json 的 "pubkey"（替换占位值），提交。'
} else {
    Write-Host '  1. 提交 native/zssh-update/updater.json（里面是公钥，可以公开）。'
}
Write-Host '  2. 备份私钥文件和口令（密码管理器 / 加密U盘）。私钥不要放进仓库、不要发给任何人。'
Write-Host '  3. 之后用 scripts/release.ps1 发版，它会用这把私钥给安装包签名。'
exit 0
