# ZeeTerm 安装包的本地静默测试：静默安装 -> 启动并查窗口响应 -> 升级（含程序运行中）-> 静默卸载，
# 每一步核对文件、注册表卸载项、快捷方式和数据目录；全程对比 Tauri 版 Z-SSH 的卸载项 / 快捷方式 /
# 安装目录有没有被动过（只读枚举，不改、不删、不启动它的任何东西）。
#
# 只在 -Root 下的独立目录里装（含空格和中文路径各一次），数据目录也在 -Root 下；不跑正式安装器。
# 输出里的用户目录和用户名一律替换成占位符，可以直接贴进报告。
#
# 用法（两个测试包用 build.ps1 造：-TestVersion 只在测试包里指定版本号）：
#   pwsh native/installer/smoke-test.ps1 -OldSetup <ZeeTerm-Setup-0.2.14.exe> -NewSetup <ZeeTerm-Setup-9.9.9.exe> `
#        -Root C:\zssh-v2-data\V2-G12 -Log C:\zssh-v2-data\V2-G12\smoke.log
param(
    [Parameter(Mandatory = $true)][string]$OldSetup,
    [Parameter(Mandatory = $true)][string]$NewSetup,
    [Parameter(Mandatory = $true)][string]$Root,
    [string]$Log = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$productName = 'ZeeTerm'
$mainExe = "$productName.exe"
$uninstKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\$productName"
$script:failures = 0
$script:checks = 0

# ---------------------------------------------------------------- 输出（脱敏）

function Sanitize([string]$Text) {
    foreach ($pair in @(@($env:USERPROFILE, '%USERPROFILE%'), @($env:USERNAME, '<user>'))) {
        if ($pair[0] -and $pair[0].Length -ge 3) {
            $Text = [regex]::Replace($Text, [regex]::Escape($pair[0]), $pair[1], 'IgnoreCase')
        }
    }
    return $Text
}

function Say([string]$Text) {
    $line = Sanitize $Text
    Write-Host $line
    if ($Log) { Add-Content -LiteralPath $Log -Value $line -Encoding UTF8 }
}

function Check([string]$What, [bool]$Ok, [string]$Detail = '') {
    $script:checks++
    if ($Ok) {
        Say "  [通过] $What"
    } else {
        $script:failures++
        Say "  [失败] $What $Detail"
    }
}

# ---------------------------------------------------------------- 窗口

Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class ZW {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsHungAppWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, StringBuilder s, int max);
}
'@

# 进程 $ProcessId 的主窗口（可见、够大）；返回 @{ Handle; Title }，没有就是 $null。
function Find-MainWindow([uint32]$ProcessId) {
    $script:zwFound = $null
    $callback = [ZW+EnumProc] {
        param($h, $l)
        $owner = 0
        [void][ZW]::GetWindowThreadProcessId($h, [ref]$owner)
        if ($owner -eq $ProcessId -and [ZW]::IsWindowVisible($h)) {
            $r = New-Object ZW+RECT
            [void][ZW]::GetWindowRect($h, [ref]$r)
            if (($r.Right - $r.Left) -gt 300 -and ($r.Bottom - $r.Top) -gt 200) {
                $sb = New-Object System.Text.StringBuilder 256
                [void][ZW]::GetWindowText($h, $sb, 256)
                $script:zwFound = @{ Handle = $h; Title = $sb.ToString() }
                return $false
            }
        }
        return $true
    }
    [void][ZW]::EnumWindows($callback, [IntPtr]::Zero)
    return $script:zwFound
}

# ---------------------------------------------------------------- Tauri 版的快照（只读）

$uninstallRoots = @(
    'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
    'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall',
    'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall'
)

function Get-KeyHash($Key) {
    $lines = foreach ($name in ($Key.GetValueNames() | Sort-Object)) {
        $value = $Key.GetValue($name, $null, 'DoNotExpandEnvironmentNames')
        $kind = $Key.GetValueKind($name)
        "$name|$kind|$value"
    }
    $bytes = [System.Text.Encoding]::UTF8.GetBytes(($lines -join "`n"))
    return ([System.BitConverter]::ToString([System.Security.Cryptography.SHA256]::HashData($bytes))).Replace('-', '')
}

# 所有卸载键名，加上"和 Z-SSH 有关"的那几个键的全部值的哈希。
function Get-RegistrySnapshot {
    $all = New-Object System.Collections.Generic.List[string]
    $related = @{}
    foreach ($root in $uninstallRoots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        foreach ($sub in Get-ChildItem -LiteralPath $root) {
            $all.Add("$root|$($sub.PSChildName)")
            $display = [string]$sub.GetValue('DisplayName', '')
            if ($sub.PSChildName -match 'Z-?SSH' -or $display -match 'Z-?SSH') {
                $related["$root|$($sub.PSChildName)"] = Get-KeyHash $sub
            }
        }
    }
    return @{ All = ($all | Sort-Object); Related = $related }
}

function Get-ShortcutSnapshot {
    $result = @{}
    $folders = @(
        @('开始菜单', [Environment]::GetFolderPath('Programs')),
        @('桌面', [Environment]::GetFolderPath('Desktop')),
        @('公共开始菜单', [Environment]::GetFolderPath('CommonPrograms')),
        @('公共桌面', [Environment]::GetFolderPath('CommonDesktopDirectory'))
    )
    foreach ($entry in $folders) {
        if (-not $entry[1] -or -not (Test-Path -LiteralPath $entry[1])) { continue }
        foreach ($file in Get-ChildItem -LiteralPath $entry[1] -Filter '*.lnk' -File -ErrorAction SilentlyContinue) {
            if ($file.Name -match 'Z-?SSH') {
                $result["$($entry[0])|$($file.Name)"] = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash
            }
        }
    }
    return $result
}

# Tauri 版安装目录（取它自己登记的 InstallLocation）：文件数、总大小、最新修改时间。路径本身不输出。
function Get-TauriDirSnapshot {
    foreach ($root in $uninstallRoots) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        foreach ($sub in Get-ChildItem -LiteralPath $root) {
            $display = [string]$sub.GetValue('DisplayName', '')
            if ($sub.PSChildName -notmatch 'Z-?SSH' -and $display -notmatch 'Z-?SSH') { continue }
            $location = ([string]$sub.GetValue('InstallLocation', '')).Trim('"')
            if ($location -and (Test-Path -LiteralPath $location)) {
                $files = @(Get-ChildItem -LiteralPath $location -Recurse -File -ErrorAction SilentlyContinue)
                $latest = ($files | Measure-Object -Property LastWriteTimeUtc -Maximum).Maximum
                $size = ($files | Measure-Object -Property Length -Sum).Sum
                return "文件 $($files.Count) 个，共 $size 字节，最新修改 $($latest.ToString('o'))"
            }
        }
    }
    return '（没有找到 Tauri 版的安装目录）'
}

function Take-Snapshot {
    return @{
        Registry = Get-RegistrySnapshot
        Shortcuts = Get-ShortcutSnapshot
        TauriDir = Get-TauriDirSnapshot
    }
}

function Show-Related($Snapshot, [string]$Title) {
    Say "$Title"
    $related = $Snapshot.Registry.Related
    if ($related.Count -eq 0) { Say '  卸载项（和 Z-SSH 有关）：无' }
    foreach ($name in ($related.Keys | Sort-Object)) {
        $short = $name.Replace('HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall', 'HKCU\...\Uninstall')
        $short = $short.Replace('HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall', 'HKLM\...\Uninstall')
        $short = $short.Replace('HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall', 'HKLM\...\WOW6432Node\...\Uninstall')
        Say "  卸载键 $short  值哈希 $($related[$name].Substring(0, 16))"
    }
    foreach ($name in ($Snapshot.Shortcuts.Keys | Sort-Object)) {
        Say "  快捷方式 $name  哈希 $($Snapshot.Shortcuts[$name].Substring(0, 16))"
    }
    Say "  Tauri 版安装目录：$($Snapshot.TauriDir)"
}

# 和测试前比：Z-SSH 相关键的名字和值哈希、快捷方式、安装目录必须一样；整体卸载键名只允许多 / 少 ZeeTerm。
function Compare-WithBaseline($Baseline, [string]$Where) {
    $now = Take-Snapshot
    $sameRelated = (($Baseline.Registry.Related.Keys | Sort-Object) -join ';') -eq (($now.Registry.Related.Keys | Sort-Object) -join ';')
    if ($sameRelated) {
        foreach ($name in $Baseline.Registry.Related.Keys) {
            if ($Baseline.Registry.Related[$name] -ne $now.Registry.Related[$name]) { $sameRelated = $false }
        }
    }
    Check "$Where：Z-SSH 相关的卸载键（名字和全部值）没变" $sameRelated
    $sameLinks = (($Baseline.Shortcuts.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ';') -eq `
        (($now.Shortcuts.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name)=$($_.Value)" }) -join ';')
    Check "$Where：Z-SSH 的快捷方式没变" $sameLinks
    Check "$Where：Tauri 版安装目录没变" ($Baseline.TauriDir -eq $now.TauriDir) "前 $($Baseline.TauriDir) 后 $($now.TauriDir)"
    $extra = @(Compare-Object ($Baseline.Registry.All) ($now.Registry.All) | ForEach-Object { "$($_.SideIndicator) $($_.InputObject)" })
    $onlyZeeTerm = $true
    foreach ($line in $extra) { if ($line -notmatch "\\Uninstall\|$productName$") { $onlyZeeTerm = $false } }
    Check "$Where：卸载键整体只多 / 少过 $productName 一项" $onlyZeeTerm ($extra -join '; ')
}

# ---------------------------------------------------------------- 安装 / 卸载 / 启动

function Invoke-Setup([string]$Setup, [string]$ArgLine, [int]$TimeoutSec = 120) {
    $p = Start-Process -FilePath $Setup -ArgumentList $ArgLine -PassThru
    if (-not $p.WaitForExit($TimeoutSec * 1000)) {
        Say "  安装器超过 $TimeoutSec 秒没结束，结束这次自己启动的安装器进程"
        $p.Kill()
        return -1
    }
    return $p.ExitCode
}

function Start-App([string]$Exe, [string]$DataDir) {
    New-Item -ItemType Directory -Force -Path $DataDir | Out-Null
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Exe
    $psi.WorkingDirectory = Split-Path -Parent $Exe
    $psi.UseShellExecute = $false
    $psi.EnvironmentVariables['ZSSH_DATA_DIR'] = $DataDir
    return [System.Diagnostics.Process]::Start($psi)
}

function Stop-Own($Process) {
    if ($Process -and -not $Process.HasExited) {
        $Process.Kill()
        [void]$Process.WaitForExit(10000)
    }
}

function Test-Registration([string]$Dir, [string]$Version, [string]$Where) {
    $ok = Test-Path -LiteralPath $uninstKey
    Check "$Where：卸载键 $productName 存在" $ok
    if (-not $ok) { return }
    $key = Get-Item -LiteralPath $uninstKey
    Check "$Where：DisplayName 是 $productName" ($key.GetValue('DisplayName') -eq $productName)
    Check "$Where：DisplayVersion 是 $Version" ($key.GetValue('DisplayVersion') -eq $Version) "实际 $($key.GetValue('DisplayVersion'))"
    Check "$Where：InstallLocation 是安装目录" ($key.GetValue('InstallLocation') -eq $Dir)
    Check "$Where：卸载命令指向安装目录里的 uninstall.exe" ($key.GetValue('UninstallString') -eq "`"$Dir\uninstall.exe`"")
    Check "$Where：静默卸载命令带 /S" ($key.GetValue('QuietUninstallString') -eq "`"$Dir\uninstall.exe`" /S")
    Check "$Where：NoModify / NoRepair = 1" (($key.GetValue('NoModify') -eq 1) -and ($key.GetValue('NoRepair') -eq 1))
    Check "$Where：EstimatedSize 大于 0" ([int]$key.GetValue('EstimatedSize') -gt 0)
}

function Test-InstalledFiles([string]$Dir, [string]$Where) {
    $expected = @($mainExe, 'zssh-mcp.exe', 'LICENSE', 'NOTICE', 'icon.ico', 'uninstall.exe')
    $actual = @(Get-ChildItem -LiteralPath $Dir -File -Force | ForEach-Object { $_.Name })
    $missing = @($expected | Where-Object { $actual -notcontains $_ })
    $extra = @($actual | Where-Object { $expected -notcontains $_ })
    Check "$Where：安装目录里正好是约定的 6 个文件" (($missing.Count -eq 0) -and ($extra.Count -eq 0)) "缺 [$($missing -join ',')] 多 [$($extra -join ',')]"
    $noticePath = Join-Path $Dir 'NOTICE'
    if (Test-Path -LiteralPath $noticePath -PathType Leaf) {
        $notice = [System.IO.File]::ReadAllText($noticePath, (New-Object System.Text.UTF8Encoding($false)))
        # 根 NOTICE（Netcatty 一节）和 native/NOTICE（GPUI 一节）都在，产品名已换掉。
        Check "$Where：NOTICE 含根 NOTICE 和 native/NOTICE 两份内容" ($notice.Contains('Netcatty') -and $notice.Contains('GPUI, gpui-component'))
        Check "$Where：NOTICE 以 ZeeTerm 开头" ($notice.StartsWith('ZeeTerm'))
    }
    Check "$Where：没有子目录（没带 WebView2 之类）" (@(Get-ChildItem -LiteralPath $Dir -Directory -Force).Count -eq 0)
}

function Test-Launch([string]$Exe, [string]$DataDir, [string]$Where) {
    $app = Start-App $Exe $DataDir
    $window = $null
    for ($i = 0; $i -lt 60 -and -not $app.HasExited; $i++) {
        $window = Find-MainWindow ([uint32]$app.Id)
        if ($window) { break }
        Start-Sleep -Milliseconds 500
    }
    if ($app.HasExited) {
        Check "$Where：程序启动后没有立刻退出" $false "退出码 $($app.ExitCode)（如果有别的 ZeeTerm 开着，单实例检查会让新进程直接退出）"
        return $null
    }
    Check "$Where：程序启动后 30 秒内出现主窗口" ($null -ne $window)
    if ($window) {
        Check "$Where：窗口标题是 $productName" ($window.Title -eq $productName) "实际 $($window.Title)"
        $hung = $false
        for ($i = 0; $i -lt 8; $i++) {
            $app.Refresh()
            if (-not $app.Responding -or [ZW]::IsHungAppWindow($window.Handle)) { $hung = $true }
            Start-Sleep -Milliseconds 500
        }
        Check "$Where：窗口持续响应（Responding，8 次采样）" (-not $hung)
    }
    # 启动后的自动检查更新要等一段时间（update_auto 的启动延迟）才发请求，这段采样期内程序不该发起任何对外连接。
    $remote = @(Get-NetTCPConnection -OwningProcess $app.Id -ErrorAction SilentlyContinue |
        Where-Object { $_.RemoteAddress -notin @('127.0.0.1', '::1', '0.0.0.0', '::') -and $_.State -ne 'Listen' })
    Check "$Where：运行期间没有任何对外的 TCP 连接" ($remote.Count -eq 0) "有 $($remote.Count) 条"
    return $app
}

# ---------------------------------------------------------------- 开始

New-Item -ItemType Directory -Force -Path $Root | Out-Null
if ($Log) { Set-Content -LiteralPath $Log -Value '' -Encoding UTF8 }
foreach ($setup in @($OldSetup, $NewSetup)) {
    if (-not (Test-Path -LiteralPath $setup -PathType Leaf)) { throw "找不到安装包：$setup" }
}
$oldVersion = [regex]::Match((Split-Path -Leaf $OldSetup), 'Setup-(.+)\.exe$').Groups[1].Value
$newVersion = [regex]::Match((Split-Path -Leaf $NewSetup), 'Setup-(.+)\.exe$').Groups[1].Value
Say "旧版安装包 $oldVersion，新版安装包 $newVersion（版本号只在测试包里指定）"

# 已经有 ZeeTerm 在运行（比如别的测试没关）时，单实例检查会干扰；先确认没有。
$running = @(Get-Process -Name $productName -ErrorAction SilentlyContinue)
if ($running.Count -gt 0) { throw "已经有 $($running.Count) 个 $productName 进程在运行，先让它们退出再测。" }
if (Test-Path -LiteralPath $uninstKey) { throw "这台机器上已经登记了 $productName 的卸载项，测试不覆盖它。" }
$appDataDefault = Join-Path $env:APPDATA $productName
$appDataExisted = Test-Path -LiteralPath $appDataDefault

$baseline = Take-Snapshot
Show-Related $baseline '== 测试前：Z-SSH（Tauri 版）相关的卸载项 / 快捷方式 / 安装目录 =='

$cases = @(
    @{ Name = '空格路径'; Dir = (Join-Path $Root 'install dir with space') },
    @{ Name = '中文路径'; Dir = (Join-Path $Root '安装目录') }
)
$caseNumber = 0
foreach ($case in $cases) {
    $caseNumber++
    $dir = $case.Dir
    $dataDir = Join-Path $Root "smoke-data-$caseNumber"
    $marker = Join-Path $dataDir 'marker-keep-me.txt'
    Say ''
    Say "== 用例 $caseNumber：$($case.Name)（安装到 $dir）=="
    if (Test-Path -LiteralPath $dir) { Remove-Item -LiteralPath $dir -Recurse -Force }
    if (Test-Path -LiteralPath $dataDir) { Remove-Item -LiteralPath $dataDir -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
    Set-Content -LiteralPath $marker -Value 'user data must survive uninstall'

    # 1. 静默安装旧版
    Say "-- 1 静默安装 $oldVersion：/S /D=…"
    $exit = Invoke-Setup $OldSetup "/S /D=$dir"
    Check '安装器退出码 0' ($exit -eq 0) "实际 $exit"
    Test-InstalledFiles $dir '安装后'
    Test-Registration $dir $oldVersion '安装后'
    $startLink = Join-Path ([Environment]::GetFolderPath('Programs')) "$productName.lnk"
    $deskLink = Join-Path ([Environment]::GetFolderPath('Desktop')) "$productName.lnk"
    Check "安装后：开始菜单有 $productName.lnk" (Test-Path -LiteralPath $startLink)
    Check "安装后：桌面有 $productName.lnk（静默安装默认建）" (Test-Path -LiteralPath $deskLink)
    if (Test-Path -LiteralPath $startLink) {
        $shell = New-Object -ComObject WScript.Shell
        $target = $shell.CreateShortcut($startLink).TargetPath
        Check '安装后：开始菜单快捷方式指向安装目录里的主程序' ($target -eq (Join-Path $dir $mainExe)) "目标 $target"
    }
    Compare-WithBaseline $baseline '安装后'

    # 2. 启动、查窗口
    Say '-- 2 启动安装出来的程序（数据目录用测试目录）'
    $app = Test-Launch (Join-Path $dir $mainExe) $dataDir '运行中'

    # 3. 程序运行中静默升级：安装器应该等，超时后失败（退出码 5），不强杀、不覆盖
    if ($app -and -not $app.HasExited -and $caseNumber -eq 1) {
        Say "-- 3 程序还在运行时静默升级到 $newVersion：应等待约 15 秒后失败（退出码 5），不强杀程序"
        $watch = [System.Diagnostics.Stopwatch]::StartNew()
        $exit = Invoke-Setup $NewSetup "/S /UPDATE /D=$dir"
        Check '安装器退出码 5（程序仍在运行，已取消）' ($exit -eq 5) "实际 $exit"
        Check '等待时间约 15 秒（12 到 40 秒之间）' (($watch.Elapsed.TotalSeconds -gt 12) -and ($watch.Elapsed.TotalSeconds -lt 40)) "实际 $([int]$watch.Elapsed.TotalSeconds) 秒"
        $app.Refresh()
        Check '没有强杀：程序仍在运行' (-not $app.HasExited)
        Test-Registration $dir $oldVersion '取消后（仍是旧版）'
    }

    # 4. 升级：先让安装器开始等待，几秒后结束自己启动的那个程序，安装器应当接着装完
    if ($app -and -not $app.HasExited) {
        Say "-- 4 静默升级到 $newVersion（/UPDATE）：安装器等待期间结束自己启动的程序，之后应装完"
        # 模拟被误删的文件：升级后应该被装回来。
        $license = Join-Path $dir 'LICENSE'
        $deleteDesktop = ($caseNumber -eq 2)
        if ($deleteDesktop) { Remove-Item -LiteralPath $deskLink -Force }
        Remove-Item -LiteralPath $license -Force
        $installer = Start-Process -FilePath $NewSetup -ArgumentList "/S /UPDATE /D=$dir" -PassThru
        Start-Sleep -Seconds 3
        Check '等待中：安装器还没结束' (-not $installer.HasExited)
        Stop-Own $app
        if (-not $installer.WaitForExit(90000)) { $installer.Kill(); Check '安装器在程序退出后 90 秒内结束' $false }
        else { Check '安装器退出码 0（程序退出后接着装完）' ($installer.ExitCode -eq 0) "实际 $($installer.ExitCode)" }
    } else {
        Say "-- 4 静默升级到 $newVersion（程序没在运行）"
        $exit = Invoke-Setup $NewSetup "/S /UPDATE /D=$dir"
        Check '安装器退出码 0' ($exit -eq 0) "实际 $exit"
    }
    Test-InstalledFiles $dir '升级后'
    Test-Registration $dir $newVersion '升级后'
    Check '升级后：被删掉的 LICENSE 装回来了' (Test-Path -LiteralPath (Join-Path $dir 'LICENSE'))
    Check '升级后：开始菜单快捷方式在' (Test-Path -LiteralPath $startLink)
    if ($caseNumber -eq 2) {
        Check '升级（/UPDATE）后：原来没有的桌面快捷方式没有被重建' (-not (Test-Path -LiteralPath $deskLink))
    } else {
        Check '升级后：原来有的桌面快捷方式还在' (Test-Path -LiteralPath $deskLink)
    }
    Check '升级后：数据目录里的文件还在' (Test-Path -LiteralPath $marker)
    Compare-WithBaseline $baseline '升级后'

    # 5. 升级后重启：/R /ARGS（装完用同样的参数重启程序）
    Say '-- 5 同版本静默重装 + /R：装完应自动启动程序'
    $env:ZSSH_DATA_DIR = $dataDir
    try {
        $exit = Invoke-Setup $NewSetup "/S /UPDATE /R /ARGS --smoke-arg /D=$dir"
    } finally {
        Remove-Item Env:\ZSSH_DATA_DIR -ErrorAction SilentlyContinue
    }
    Check '安装器退出码 0' ($exit -eq 0) "实际 $exit"
    $relaunched = $null
    for ($i = 0; $i -lt 30 -and -not $relaunched; $i++) {
        $relaunched = Get-Process -Name $productName -ErrorAction SilentlyContinue |
            Where-Object { $_.Path -eq (Join-Path $dir $mainExe) } | Select-Object -First 1
        if (-not $relaunched) { Start-Sleep -Milliseconds 500 }
    }
    Check '/R：安装器装完后启动了安装目录里的程序' ($null -ne $relaunched)
    if ($relaunched) { Stop-Own $relaunched }

    # 6. 静默卸载（和「应用和功能」里的 QuietUninstallString 同一条命令）
    Say '-- 6 静默卸载：uninstall.exe /S（它会复制自己到临时目录再运行，所以轮询等它做完）'
    $quiet = (Get-Item -LiteralPath $uninstKey).GetValue('QuietUninstallString')
    Check '卸载命令来自注册表' ($quiet -eq "`"$dir\uninstall.exe`" /S")
    $un = Start-Process -FilePath (Join-Path $dir 'uninstall.exe') -ArgumentList '/S' -PassThru
    [void]$un.WaitForExit(30000)
    $gone = $false
    for ($i = 0; $i -lt 120; $i++) {
        if (-not (Test-Path -LiteralPath $uninstKey) -and -not (Test-Path -LiteralPath $dir)) { $gone = $true; break }
        Start-Sleep -Milliseconds 500
    }
    Check '卸载后：卸载键没了、安装目录没了（60 秒内）' $gone
    Check "卸载后：开始菜单快捷方式没了" (-not (Test-Path -LiteralPath $startLink))
    Check "卸载后：桌面快捷方式没了" (-not (Test-Path -LiteralPath $deskLink))
    Check '卸载后：数据目录和里面的文件保留' (Test-Path -LiteralPath $marker)
    Check '卸载后：没有在默认位置新建 ZeeTerm 数据目录（测试一直用独立数据目录）' ((-not (Test-Path -LiteralPath $appDataDefault)) -or $appDataExisted)
    Compare-WithBaseline $baseline '卸载后'
}

# 默认数据目录（%APPDATA%\ZeeTerm）里放一个标记文件，装 -> 卸，卸载不能动它。
Say ''
Say '== 用例 3：卸载不动默认数据目录（%APPDATA%\ZeeTerm）=='
$createdDefault = $false
if (-not (Test-Path -LiteralPath $appDataDefault)) {
    New-Item -ItemType Directory -Force -Path $appDataDefault | Out-Null
    $createdDefault = $true
}
$defaultMarker = Join-Path $appDataDefault 'g12-smoke-marker.txt'
Set-Content -LiteralPath $defaultMarker -Value 'smoke marker'
$dir3 = Join-Path $Root 'install-plain'
if (Test-Path -LiteralPath $dir3) { Remove-Item -LiteralPath $dir3 -Recurse -Force }
$exit = Invoke-Setup $OldSetup "/S /D=$dir3"
Check '安装器退出码 0' ($exit -eq 0) "实际 $exit"
Test-Registration $dir3 $oldVersion '安装后'
$un = Start-Process -FilePath (Join-Path $dir3 'uninstall.exe') -ArgumentList '/S' -PassThru
[void]$un.WaitForExit(30000)
for ($i = 0; $i -lt 120 -and (Test-Path -LiteralPath $uninstKey); $i++) { Start-Sleep -Milliseconds 500 }
Check '卸载后：卸载键没了' (-not (Test-Path -LiteralPath $uninstKey))
Check '卸载后：默认数据目录里的标记文件还在' (Test-Path -LiteralPath $defaultMarker)
Remove-Item -LiteralPath $defaultMarker -Force
if ($createdDefault) { Remove-Item -LiteralPath $appDataDefault -Force }
Compare-WithBaseline $baseline '用例 3 结束'

Say ''
$final = Take-Snapshot
Show-Related $final '== 测试后：Z-SSH（Tauri 版）相关的卸载项 / 快捷方式 / 安装目录 =='
Say ''
Say "共 $($script:checks) 项检查，失败 $($script:failures) 项。"
if ($script:failures -gt 0) { exit 1 }
exit 0
