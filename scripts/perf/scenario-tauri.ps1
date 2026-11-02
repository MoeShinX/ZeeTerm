# V2-G13a: launcher for scenario-tauri.mjs (Windows PowerShell 5.1, ASCII only). Starts the Z-SSH 0.2.14 perf
# copy with a WebView2 remote-debugging port, sizes its window like the ZeeTerm run, samples the whole
# process tree (Z-SSH + every msedgewebview2 child) every 500 ms into samples.csv, and runs the CDP driver.
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$DataDir,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [int]$Port = 9222,
    [int]$WindowW = 1200,
    [int]$WindowH = 800,
    [string[]]$DriverArgs = @()
)
. "$PSScriptRoot\perf-lib.ps1"
Assert-IsolatedDir $DataDir
Assert-IsolatedDir $OutDir
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$stray0 = Stop-PerftestWebView2
if ($stray0 -gt 0) { Write-Host "stopped $stray0 stray perftest WebView2 process(es)" }
$psi = New-Object System.Diagnostics.ProcessStartInfo($Exe)
$psi.UseShellExecute = $false
$psi.WorkingDirectory = (Split-Path $Exe)
$psi.EnvironmentVariables['ZSSH_DATA_DIR'] = $DataDir
$psi.EnvironmentVariables['WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS'] = "--remote-debugging-port=$Port"
$psi.EnvironmentVariables.Remove('ZSSH_TEST_PASSWORD')
$proc = [System.Diagnostics.Process]::Start($psi)
$start = $proc.StartTime
[PerfNative]::StartSampler($proc.Id, 500, (Join-Path $OutDir 'samples.csv'))
$sized = $false
for ($i = 0; $i -lt 100 -and -not $sized; $i++) {
    $t = ''
    $h = [PerfNative]::FindMainWindow($proc.Id, 300, 200, [ref]$t)
    if ($h -ne [IntPtr]::Zero) { [PerfNative]::MoveSize($h, 40, 20, $WindowW, $WindowH); $sized = $true } else { Start-Sleep -Milliseconds 100 }
}
$node = (Get-Command node).Source
$args_ = @((Join-Path $PSScriptRoot 'scenario-tauri.mjs'), '--out', $OutDir, '--port', "$Port") + $DriverArgs
$ErrorActionPreference = 'Continue'
try { & $node @args_ 2>&1 | Where-Object { $_ -notmatch 'ExperimentalWarning|--trace-warnings' } | ForEach-Object { Write-Host $_ } } catch { Write-Host ("driver error: " + $_) }
$code = $LASTEXITCODE
Start-Sleep -Seconds 1
$peak = [PerfNative]::StopSampler()
$killed = [PerfNative]::KillTree($proc.Id, $start)
Start-Sleep -Milliseconds 500
$stray = Stop-PerftestWebView2
if ($stray -gt 0) { Write-Host "stopped $stray stray perftest WebView2 process(es)" }
Write-Host ("driver exit {0}, peak total {1:N1} MB, killed {2}" -f $code, $peak, $killed)
exit $code
