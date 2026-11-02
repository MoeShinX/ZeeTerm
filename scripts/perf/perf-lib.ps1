# V2-G13a: shared loader for the measurement helpers (Windows PowerShell 5.1, ASCII only).
# Dot-source it: . "$PSScriptRoot\perf-lib.ps1"
$ErrorActionPreference = 'Stop'
if (-not ('PerfNative' -as [type])) {
    $src = Join-Path $PSScriptRoot 'PerfNative.cs'
    Add-Type -Path $src -ReferencedAssemblies 'System.Drawing', 'System.Management'
}

function Get-Median([double[]]$v) {
    if ($v.Count -eq 0) { return [double]::NaN }
    $s = $v | Sort-Object
    $n = $s.Count
    if ($n % 2 -eq 1) { return $s[($n - 1) / 2] }
    return ($s[$n / 2 - 1] + $s[$n / 2]) / 2
}

function Get-Percentile([double[]]$v, [double]$p) {
    if ($v.Count -eq 0) { return [double]::NaN }
    $s = @($v | Sort-Object)
    $i = [math]::Min($s.Count - 1, [math]::Max(0, [int][math]::Ceiling($p * $s.Count) - 1))
    return $s[$i]
}

# Refuse to run when the data dir is not an isolated one (never the real app data).
function Assert-IsolatedDir([string]$dir) {
    if (-not $dir) { throw 'data dir is empty' }
    $full = [System.IO.Path]::GetFullPath($dir)
    if ($full -notlike 'C:\zssh-v2-data\*') { throw "data dir must be under C:\zssh-v2-data (got a different location)" }
}

# Directories removed between "first launch" runs must be ours: the isolated data root or the WebView2
# profile folder of the renamed (perftest) Tauri copy. Never the real app folders.
function Assert-IsolatedOrKnown([string]$dir) {
    $full = [System.IO.Path]::GetFullPath($dir)
    if ($full -like 'C:\zssh-v2-data\*') { return }
    if ($full -like '*\AppData\Local\com.zssh.app.perftest') { return }
    throw 'refusing to delete a directory that is not ours'
}

# WebView2 children survive when their parent app is killed ungracefully. They belong to our own profile
# folder (identifier com.zssh.app.perftest), which no other program uses, so the command line identifies
# them exactly. Returns how many were stopped. Never matches the user's own Z-SSH profile.
function Stop-PerftestWebView2 {
    $n = 0
    Get-CimInstance Win32_Process -Filter "Name = 'msedgewebview2.exe'" | Where-Object { $_.CommandLine -like '*com.zssh.app.perftest*' } | ForEach-Object {
        try { Stop-Process -Id $_.ProcessId -Force -ErrorAction Stop; $n++ } catch { }
    }
    return $n
}
