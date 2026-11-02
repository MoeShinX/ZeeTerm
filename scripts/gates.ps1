# ZeeTerm 全部检查：核心库（根工作区）和原生应用（native 工作区）各自的 fmt / clippy / test，
# 再加一次原生应用的 release 构建。逐条报告退出码和耗时，任何一条失败整体退出码就是 1。
#
# 用法（任意目录都行）：powershell -File scripts/gates.ps1
#
# 联网用例不在这里跑：没设 ZSSH_TEST_* 环境变量时它们会自己跳过并打印原因。
# 自动化一律不碰用户的真实数据：没设 ZSSH_DATA_DIR 时，本脚本给它指一个临时目录，跑完删掉。
$ErrorActionPreference = 'Continue'
[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
Set-Location (Join-Path $PSScriptRoot '..')

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
    @{ Name = '原生 release 构建';    Args = @('build', '--manifest-path', 'native/Cargo.toml', '--workspace', '--release', '--locked') }
)

$total = [System.Diagnostics.Stopwatch]::StartNew()
$failed = @()
try {
    foreach ($step in $steps) {
        Write-Host ("==> {0}: cargo {1}" -f $step.Name, ($step.Args -join ' '))
        $watch = [System.Diagnostics.Stopwatch]::StartNew()
        & cargo @($step.Args)
        $code = $LASTEXITCODE
        $watch.Stop()
        Write-Host ("[{0}] 退出码 {1}  耗时 {2} 秒" -f $step.Name, $code, [int]$watch.Elapsed.TotalSeconds)
        if ($code -ne 0) { $failed += $step.Name }
    }
} finally {
    if ($ownDataDir) { Remove-Item -LiteralPath $ownDataDir -Recurse -Force -ErrorAction SilentlyContinue }
}

Write-Host ("总耗时 {0} 秒" -f [int]$total.Elapsed.TotalSeconds)
if ($failed.Count -gt 0) {
    Write-Host ("失败：{0}" -f ($failed -join '、')) -ForegroundColor Red
    exit 1
}
Write-Host '全部通过。' -ForegroundColor Green
exit 0
