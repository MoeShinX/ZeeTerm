# V2-G13a: five idle shells + ~100 MB output flood + Ctrl+C, driven in the ZeeTerm window (Windows PowerShell 5.1,
# ASCII only). Needs a build with the debug hooks (release profile + debug assertions, see the report):
# release builds ignore ZSSH_MAIN_SCRIPT by design.
#
# The in-app driver (ZSSH_MAIN_SCRIPT) opens 5 shells on the seeded host (one SSH connection), then runs
# the script below. `shot:<name>` steps are used as barriers only: the app writes <name>.ready, we sample
# for the phase and then write <name>.done. The memory sampler runs for the whole session (500 ms) and
# writes epoch-stamped CSV, so phases are cut out afterwards (analyze.mjs).
#
# Output: <OutDir>\samples.csv (memory/CPU of the process tree), steps.csv (arrival time of each driver
# step in the app log, epoch ms), marks.csv (barrier times). No server address is read or written here:
# the host lives in the isolated data dir's database.
param(
    [Parameter(Mandatory = $true)][string]$Exe,
    [Parameter(Mandatory = $true)][string]$DataDir,
    [Parameter(Mandatory = $true)][string]$OutDir,
    [int]$IdleProbes = 20,
    [int]$FloodProbes = 40,
    [int]$FloodLines = 12400000,
    [int]$IdleSeconds = 30,
    [int]$PostSeconds = 60,
    [int]$WindowW = 1200,
    [int]$WindowH = 800
)
. "$PSScriptRoot\perf-lib.ps1"
Assert-IsolatedDir $DataDir
Assert-IsolatedDir $OutDir
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$shotDir = Join-Path $OutDir 'sig'
Remove-Item -Recurse -Force $shotDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $shotDir | Out-Null
$log = Join-Path $OutDir 'app.log'
Remove-Item -Force $log -ErrorAction SilentlyContinue

function Probes([string]$prefix, [int]$n) {
    $s = ''
    for ($i = 1; $i -le $n; $i++) {
        $tok = '{0}{1:00}' -f $prefix, $i
        $s += "type:$tok;expect:$tok;key:ctrl-u;"
    }
    return $s
}
$open = ''
for ($i = 0; $i -lt 5; $i++) { $open += 'open:1;wait_live;' }
$script = $open + 'assert_tabs:5;' +
    "select:0;type:PS1='ZP> '\r;expect:ZP>;select:1;type:PS1='ZP> '\r;expect:ZP>;" +
    "sleep:12000;sleep:$($IdleSeconds * 1000);" +
    (Probes 'ZQa' $IdleProbes) +
    "select:0;type:seq 1 $FloodLines && echo ZDONE`$((1+1))\r;select:1;" +
    (Probes 'ZQb' $FloodProbes) +
    "sleep:15000;select:0;expect:ZDONE2;sleep:$($PostSeconds * 1000);" +
    "type:seq 1 $FloodLines\r;sleep:3000;key:ctrl-c;expect:ZP>;" +
    'sleep:8000;quit'

$psi = New-Object System.Diagnostics.ProcessStartInfo($Exe)
$psi.UseShellExecute = $false
$psi.WorkingDirectory = (Split-Path $Exe)
$vars = @{
    'ZSSH_DATA_DIR' = $DataDir; 'ZSSH_SSH_TRUST_ONCE' = '1'; 'ZSSH_MAIN_SCRIPT' = $script
    'ZSSH_MAIN_LOG' = $log; 'ZSSH_SHOT_DIR' = $shotDir
}
foreach ($k in $vars.Keys) { $psi.EnvironmentVariables[$k] = $vars[$k] }
$psi.EnvironmentVariables.Remove('ZSSH_TEST_PASSWORD')
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$proc = [System.Diagnostics.Process]::Start($psi)
$start = $proc.StartTime
[PerfNative]::StartSampler($proc.Id, 500, (Join-Path $OutDir 'samples.csv'))

$epoch0 = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$steps = New-Object System.Collections.ArrayList
$marks = New-Object System.Collections.ArrayList
$seen = 0
$handled = @{}
$sized = $false
$deadline = (Get-Date).AddMinutes(12)
$finished = $false
while ((Get-Date) -lt $deadline -and -not $proc.HasExited -and -not $finished) {
    if (-not $sized) {
        $t = ''
        $h = [PerfNative]::FindMainWindow($proc.Id, 300, 200, [ref]$t)
        if ($h -ne [IntPtr]::Zero) { [PerfNative]::MoveSize($h, 40, 20, $WindowW, $WindowH); $sized = $true }
    }
    if (Test-Path $log) {
        $fs = New-Object System.IO.FileStream($log, 'Open', 'Read', 'ReadWrite')
        $sr = New-Object System.IO.StreamReader($fs, [System.Text.Encoding]::UTF8)
        $lines = @()
        while (($ln = $sr.ReadLine()) -ne $null) { $lines += $ln }
        $sr.Close(); $fs.Close()
        $now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        while ($seen -lt $lines.Count) {
            [void]$steps.Add([pscustomobject]@{ idx = $seen; epoch_ms = $now; line = $lines[$seen] })
            if ($lines[$seen] -match 'FAIL' -or $lines[$seen] -match 'script end') { }
            if ($lines[$seen] -match 'Quit') { $finished = $true }
            $seen++
        }
    }
    foreach ($f in (Get-ChildItem $shotDir -Filter '*.ready' -ErrorAction SilentlyContinue)) {
        $name = $f.BaseName
        if ($handled.ContainsKey($name)) { continue }
        $handled[$name] = 1
        [void]$marks.Add([pscustomobject]@{ name = "$name`_start"; epoch_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() })
        $wait = 0
        if ($name -eq 'idle') { $wait = $IdleSeconds } elseif ($name -eq 'post') { $wait = $PostSeconds } elseif ($name -eq 'end') { $wait = 8 }
        Start-Sleep -Seconds $wait
        [void]$marks.Add([pscustomobject]@{ name = "$name`_end"; epoch_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() })
        New-Item -ItemType File -Path (Join-Path $shotDir "$name.done") -Force | Out-Null
    }
    Start-Sleep -Milliseconds 4
}
Start-Sleep -Seconds 1
$peak = [PerfNative]::StopSampler()
$steps | Export-Csv -Path (Join-Path $OutDir 'steps.csv') -NoTypeInformation -Encoding UTF8
$marks | Export-Csv -Path (Join-Path $OutDir 'marks.csv') -NoTypeInformation -Encoding UTF8
$killed = [PerfNative]::KillTree($proc.Id, $start)
Write-Host ("done: finished={0}, steps={1}, peak total {2:N1} MB, killed leftover {3}, elapsed {4:N0}s" -f $finished, $steps.Count, $peak, $killed, $sw.Elapsed.TotalSeconds)
Write-Host ("last log lines: " + (($steps | Select-Object -Last 3 | ForEach-Object { $_.line }) -join ' / '))
