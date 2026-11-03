# 重新生成内置 models.dev 快照（crates/zssh-core/src/ai/agent/builtin/catalog/models-dev-snapshot.json）。
# 不在门禁里跑：需要联网，手动执行，生成后把变化的快照提交进仓库。
#
# 用法：powershell -File scripts/models-dev-snapshot.ps1 [-ApiJson <已下载的 api.json>]
# 下载走环境里的 HTTPS_PROXY / 系统代理（Invoke-WebRequest 默认行为）；已有 api.json 就用 -ApiJson 跳过下载。
param([string]$ApiJson)
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')

$out = 'crates/zssh-core/src/ai/agent/builtin/catalog/models-dev-snapshot.json'
$tmp = $null
if (-not $ApiJson) {
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('models-dev-' + [guid]::NewGuid().ToString('N').Substring(0, 8) + '.json')
    Invoke-WebRequest -Uri 'https://models.dev/api.json' -OutFile $tmp -UseBasicParsing
    $ApiJson = $tmp
}
try {
    $date = (Get-Date).ToUniversalTime().ToString('yyyy-MM-dd')
    cargo run -q -p zssh-core --example models_dev_snapshot -- $ApiJson $out $date
    if ($LASTEXITCODE -ne 0) { throw "生成失败，退出码 $LASTEXITCODE" }
    $size = (Get-Item $out).Length
    Write-Host ("快照 {0} 字节（{1:N0} KB），日期 {2}" -f $size, ($size / 1KB), $date)
    if ($size -gt 1MB) { throw '快照超过 1 MB，请收紧 data.rs 里的收录范围' }
} finally {
    if ($tmp) { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }
}
