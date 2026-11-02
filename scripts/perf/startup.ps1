# V2-G13a: repeated start-up measurement of one or more exes, interleaved (Windows PowerShell 5.1, ASCII only).
#
# For each round and each app: start the exe with an isolated ZSSH_DATA_DIR, wait for (a) first visible
# window, (b) window answering WM_NULL, (c) first PrintWindow capture with many distinct colours (= the real
# UI is drawn), then kill ONLY the process tree we started. One CSV row per run. Apps are interleaved
# round by round so that machine noise hits both the same way.
#
# -FreshEach: delete the data dir (and -CleanDirs, e.g. the WebView2 profile of the perftest copy) before
# every run = "first launch" runs. Not an OS-cold start: the OS file cache is warm from earlier use.
#
#   powershell -File scripts/perf/startup.ps1 -Exe a.exe,b.exe -DataDir D1,D2 -Label A,B -Runs 20 -Out out.csv
param(
    [Parameter(Mandatory = $true)][string[]]$Exe,
    [Parameter(Mandatory = $true)][string[]]$DataDir,
    [Parameter(Mandatory = $true)][string[]]$Label,
    [Parameter(Mandatory = $true)][string]$Out,
    [int]$Runs = 20,
    [int]$MinColours = 24,
    [int]$TimeoutMs = 30000,
    [string[]]$ExtraEnv = @(),
    [switch]$FreshEach,
    [string[]]$CleanDirs = @(),
    [string]$ShotDir = ''
)
. "$PSScriptRoot\perf-lib.ps1"
foreach ($d in $DataDir) { Assert-IsolatedDir $d }
$stray0 = Stop-PerftestWebView2
if ($stray0 -gt 0) { Write-Host "stopped $stray0 stray perftest WebView2 process(es) before the first run" }
$null = [PerfNative]::SampleTree($PID)  # warm up the performance-counter read (slow first call)
$rows = @()
for ($i = 1; $i -le $Runs; $i++) {
    for ($a = 0; $a -lt $Exe.Count; $a++) {
        $dir = $DataDir[$a]
        if ($FreshEach) {
            if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
            New-Item -ItemType Directory -Force -Path $dir | Out-Null
            foreach ($cd in @($CleanDirs[$a])) { if ($cd) { Assert-IsolatedOrKnown $cd; if (Test-Path $cd) { Remove-Item -Recurse -Force $cd } } }
        }
        $env_ = @("ZSSH_DATA_DIR=$dir")
        if ($ExtraEnv.Count -gt $a -and $ExtraEnv[$a]) { $env_ += ($ExtraEnv[$a] -split '\|') }
        $png = ''
        if ($ShotDir -and $i -eq 2) { $png = Join-Path $ShotDir ($Label[$a] + '-startup.png') }
        $r = [PerfNative]::MeasureStartup($Exe[$a], (Split-Path $Exe[$a]), $env_, $MinColours, $TimeoutMs, $png)
        Start-Sleep -Milliseconds 800
        $mem = [PerfNative]::SampleTree($r.Pid)
        $killed = [PerfNative]::KillTree($r.Pid, $r.StartTime)
        Start-Sleep -Milliseconds 300
        $stray = Stop-PerftestWebView2
        if ($stray -gt 0) { Write-Host "  stopped $stray stray perftest WebView2 process(es)" }
        $rows += [pscustomobject]@{
            label = $Label[$a]; run = $i
            first_launch = [int][bool]$FreshEach
            visible_ms = [math]::Round($r.VisibleMs, 1)
            responding_ms = [math]::Round($r.RespondingMs, 1)
            content_ms = [math]::Round($r.ContentMs, 1)
            colours = $r.Colours
            window = $r.WindowSize
            private_mb_at_ready_plus_0_8s = [math]::Round($mem.TotalMB, 1)
            procs = $mem.Procs
        }
        Write-Host ("{0} run {1}: visible {2} ms, responding {3} ms, content {4} ms (colours {5}), procs {6}, killed {7}" -f $Label[$a], $i, [math]::Round($r.VisibleMs), [math]::Round($r.RespondingMs), [math]::Round($r.ContentMs), $r.Colours, $mem.Procs, $killed)
        $rows | Export-Csv -Path $Out -NoTypeInformation -Encoding UTF8
        Start-Sleep -Milliseconds 1800
    }
}
$rows | Export-Csv -Path $Out -NoTypeInformation -Encoding UTF8
foreach ($l in ($Label | Select-Object -Unique)) {
    $c = @($rows | Where-Object { $_.label -eq $l -and $_.content_ms -gt 0 } | ForEach-Object { [double]$_.content_ms })
    Write-Host ("{0}: runs with content {1}/{2}, median {3} ms, worst {4} ms" -f $l, $c.Count, $Runs, (Get-Median $c), ($c | Measure-Object -Maximum).Maximum)
}
