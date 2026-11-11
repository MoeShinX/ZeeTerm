# ZeeTerm 全部检查：核心库（根工作区）和原生应用（native 工作区）各自的 fmt / clippy / test，
# 再加原生应用的 release 类配置检查：只跑 dev_hooks 的 release 断言（release 里测试钩子读不到环境变量，
# 这是它唯一的自动防线），以及一次构建。逐条报告退出码和耗时，任何一条失败整体退出码就是 1。
#
# 这两步用 `--profile gate`（Cargo.toml 里的自定义配置，inherits release，只关掉 LTO、放开并行代码生成单元），
# 不用完整的 release：thin LTO + 单个代码生成单元让这两步各要四五分钟，而这里要验证的是
# `cfg(debug_assertions)` 为 false 时的行为和 release 下才冒出来的 warning，二者 gate 配置下都和 release 一致。
# 正式发版（native/installer/build.ps1、release.yml、scripts/release.ps1）仍然是完整的 release。
#
# gate 的两步（DenyWarnings）还要求编译输出里没有任何 warning：release 下才会冒出来的"从未使用"
# （只给 debug 调试脚本用的函数）平时没人看。不用 RUSTFLAGS=-D warnings 是因为 RUSTFLAGS 一变，cargo 会把
# 整个 target 缓存按新参数重编，和平时不带它的 debug 构建互相顶掉；所以改成扫输出里的 `warning` 行，缓存不受影响
# （cargo 会把缓存命中的 crate 的 warning 原样重放，不会因为命中缓存而漏掉）。
#
# 用法（任意目录都行）：powershell -File scripts/gates.ps1
#
# 联网用例不在这里跑：没设 ZSSH_TEST_* 环境变量时它们会自己跳过并打印原因。
# 自动化一律不碰用户的真实数据：没设 ZSSH_DATA_DIR 时，本脚本给它指一个临时目录，跑完删掉。
$ErrorActionPreference = 'Continue'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Set-Location (Join-Path $PSScriptRoot '..')

# 联网测试靠 ZSSH_TEST_* 环境变量连真实的测试服务器；门禁要的是可重复的结果，开发者 shell 里常驻的这些变量
# 会让「没设就跳过」的用例悄悄跑起来（碰真实服务器、结果还因人而异）。所以门禁一开始就在本进程里清掉它们，
# 只清这个前缀：ZSSH_DATA_DIR 是产品功能、下面自己会处理，别的 ZSSH_* 各有用途，不在这里动。
# 输出里只说名字，不打印值（里面可能有密码）。
$clearedTestVars = @(Get-ChildItem Env: | Where-Object { $_.Name -like 'ZSSH_TEST_*' } | ForEach-Object { $_.Name })
foreach ($name in $clearedTestVars) { Remove-Item -LiteralPath ("Env:" + $name) -ErrorAction SilentlyContinue }
if ($clearedTestVars.Count -gt 0) {
    Write-Host ('已清除本进程里的 ZSSH_TEST_* 环境变量（联网用例按「没设置」跳过）：' + ($clearedTestVars -join '、'))
}

# 另外两个开关也会让 `cargo test` 悄悄走到真实环境：ZSSH_AGENT_E2E=1 让测试去拉起真实的智能体（claude / codex
# 等，会用到开发者自己的登录），ZSSH_LIVE_FULL=1 让联网测试跑完整档（SFTP 全套、断线续传、代理跳板……）。
# 开发者 shell 里留着它们时门禁的结果就不可重复，所以同样在本进程里清掉；只清这两个固定名字，输出里只说名字。
$clearedSwitches = @('ZSSH_AGENT_E2E', 'ZSSH_LIVE_FULL') | Where-Object { Test-Path -LiteralPath ("Env:" + $_) }
foreach ($name in $clearedSwitches) { Remove-Item -LiteralPath ("Env:" + $name) -ErrorAction SilentlyContinue }
if (@($clearedSwitches).Count -gt 0) {
    Write-Host ('已清除本进程里会拉起真实智能体 / 跑完整联网档的环境变量：' + (@($clearedSwitches) -join '、'))
}

$ownDataDir = $null
if (-not $env:ZSSH_DATA_DIR) {
    $ownDataDir = Join-Path ([System.IO.Path]::GetTempPath()) ('zeeterm-gates-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Force $ownDataDir | Out-Null
    $env:ZSSH_DATA_DIR = $ownDataDir
}

$steps = @(
    @{ Name = '核心库 cargo fmt';     Args = @('fmt', '--all', '--', '--check') },
    @{ Name = '核心库 cargo clippy';  Args = @('clippy', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings') },
    @{ Name = '核心库 cargo test';    Args = @('test', '--workspace', '--locked') },
    @{ Name = '原生 cargo fmt';       Args = @('fmt', '--manifest-path', 'native/Cargo.toml', '--all', '--', '--check') },
    @{ Name = '原生 cargo clippy';    Args = @('clippy', '--manifest-path', 'native/Cargo.toml', '--workspace', '--all-targets', '--locked', '--', '-D', 'warnings') },
    @{ Name = '原生 cargo test';      Args = @('test', '--manifest-path', 'native/Cargo.toml', '--workspace', '--locked') },
    @{ Name = '原生 gate dev_hooks';  DenyWarnings = $true; Args = @('test', '--manifest-path', 'native/Cargo.toml', '-p', 'zssh-gpui', '--profile', 'gate', '--locked', 'dev_hooks') },
    @{ Name = '原生 gate 构建';       DenyWarnings = $true; Args = @('build', '--manifest-path', 'native/Cargo.toml', '--workspace', '--profile', 'gate', '--locked') }
)

# macOS：集成测试和原生测试链接的是普通（非 cfg(test)）的 zssh-core，会去开真实的登录钥匙串。
# 调试构建认 ZSSH_VAULT_MEMORY=1，改用进程内的内存钥匙串，不碰用户自己的 ZeeTerm 密钥条目。
# Windows PowerShell 5.1 里没有 $IsMacOS（按假处理），Windows / Linux 上这一段什么都不做。
$savedVaultMemory = $env:ZSSH_VAULT_MEMORY
if ($IsMacOS) { $env:ZSSH_VAULT_MEMORY = '1' }

$total = [System.Diagnostics.Stopwatch]::StartNew()
$failed = @()
try {
    # 界面文案覆盖：写死的中文只能在 allow 清单里的文件里（按文件 burn-down）。
    Write-Host '==> i18n 覆盖检查: scripts/check-i18n.ps1'
    $i18nWatch = [System.Diagnostics.Stopwatch]::StartNew()
    $hostExe = if (Get-Command pwsh -ErrorAction SilentlyContinue) { 'pwsh' } else { 'powershell' }
    & $hostExe -NoProfile -File (Join-Path $PSScriptRoot 'check-i18n.ps1')
    $i18nCode = $LASTEXITCODE
    $i18nWatch.Stop()
    Write-Host ("[i18n 覆盖检查] 退出码 {0}  耗时 {1} 秒" -f $i18nCode, [int]$i18nWatch.Elapsed.TotalSeconds)
    if ($i18nCode -ne 0) { $failed += 'i18n 覆盖检查' }

    foreach ($step in $steps) {
        Write-Host ("==> {0}: cargo {1}" -f $step.Name, ($step.Args -join ' '))
        $watch = [System.Diagnostics.Stopwatch]::StartNew()
        if ($step.DenyWarnings) {
            $warnings = 0
            & cargo @($step.Args) 2>&1 | ForEach-Object {
                $line = "$_"
                if ($line -match '^warning(\[[^\]]*\])?:') { $warnings++ }
                Write-Host $line
            }
            $code = $LASTEXITCODE
            if ($code -eq 0 -and $warnings -gt 0) {
                Write-Host ("编译输出里有 {0} 条 warning，按失败处理。" -f $warnings) -ForegroundColor Red
                $code = 1
            }
        } else {
            & cargo @($step.Args)
            $code = $LASTEXITCODE
        }
        $watch.Stop()
        Write-Host ("[{0}] 退出码 {1}  耗时 {2} 秒" -f $step.Name, $code, [int]$watch.Elapsed.TotalSeconds)
        if ($code -ne 0) { $failed += $step.Name }
    }
} finally {
    if ($IsMacOS) {
        if ($null -eq $savedVaultMemory) {
            Remove-Item Env:ZSSH_VAULT_MEMORY -ErrorAction SilentlyContinue
        } else {
            $env:ZSSH_VAULT_MEMORY = $savedVaultMemory
        }
    }
    if ($ownDataDir) { Remove-Item -LiteralPath $ownDataDir -Recurse -Force -ErrorAction SilentlyContinue }
}

Write-Host ("总耗时 {0} 秒" -f [int]$total.Elapsed.TotalSeconds)
if ($failed.Count -gt 0) {
    Write-Host ("失败：{0}" -f ($failed -join '、')) -ForegroundColor Red
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
exit 0
