# 原生主窗口的截图 / 驱动脚本（调试构建）：带着 `ZSSH_MAIN_SCRIPT` 起程序，程序执行到 `shot:<名字>` 时先做隐私
# 检查再写 <ShotDir>\<名字>.ready，这里用 PrintWindow 截自己启动的那个窗口（不动真实鼠标、不碰桌面），
# 存成 <OutDir>\<名字>.png，再写 <名字>.done 放行。
#
# 用法（数据目录必须是不含用户名的独立目录；联网用例的服务器信息只放在 -ExtraEnv 里，用 | 分隔）：
#   powershell -File scripts/native-shot.ps1 -Script "view:vault;shot:a;quit" -DataDir C:\zssh-v2-data\x\data `
#     -ShotDir C:\zssh-v2-data\x\sig -OutDir C:\zssh-v2-data\x\out -ExtraEnv "ZSSH_SEED_PLACEHOLDERS=1"
# 步骤写法见 native/zssh-gpui/src/main_window/dev_script.rs 和 dev_acts.rs。
param(
  [string]$Exe = '',
  [Parameter(Mandatory = $true)][string]$Script,
  [Parameter(Mandatory = $true)][string]$DataDir,
  [Parameter(Mandatory = $true)][string]$ShotDir,
  [Parameter(Mandatory = $true)][string]$OutDir,
  [string]$LogFile = '',
  [int]$TimeoutSec = 240,
  [string]$ExtraEnv = ''
)
if (-not $Exe) { $Exe = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) '..\native\target\debug\zssh-gpui.exe' }
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class W32 {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr h, System.Text.StringBuilder s, int max);
}
'@

# 找进程 $procId 的主窗口（可见、够大）。$wantTitle 不空时只认这个标题的窗口（独立文件窗口叫 ZeeTerm 文件、更新窗口叫 检查更新，
# 截图名以 files- 开头的就截它）；空的时候跳过文件窗口，免得它盖住主窗口。
function Find-Window([uint32]$procId, [string]$wantTitle = '') {
  $script:found = [IntPtr]::Zero
  $cb = [W32+EnumProc] {
    param($h, $l)
    $p = 0
    [void][W32]::GetWindowThreadProcessId($h, [ref]$p)
    $sb = New-Object System.Text.StringBuilder 256
    [void][W32]::GetWindowText($h, $sb, 256)
    $title = $sb.ToString()
    $titleOk = if ($wantTitle -ne '') { $title -eq $wantTitle } else { $title -ne 'ZeeTerm 文件' -and $title -ne 'ZeeTerm 终端' -and $title -ne '检查更新' }
    if ($p -eq $procId -and $titleOk -and [W32]::IsWindowVisible($h)) {
      $r = New-Object W32+RECT
      [void][W32]::GetWindowRect($h, [ref]$r)
      if (($r.Right - $r.Left) -gt 300 -and ($r.Bottom - $r.Top) -gt 200) { $script:found = $h; return $false }
    }
    return $true
  }
  [void][W32]::EnumWindows($cb, [IntPtr]::Zero)
  return $script:found
}

function Save-Shot([IntPtr]$h, [string]$path) {
  $r = New-Object W32+RECT
  [void][W32]::GetWindowRect($h, [ref]$r)
  $w = $r.Right - $r.Left; $hgt = $r.Bottom - $r.Top
  $bmp = New-Object System.Drawing.Bitmap $w, $hgt
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc()
  [void][W32]::PrintWindow($h, $hdc, 2)
  $g.ReleaseHdc($hdc)
  $g.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
}

New-Item -ItemType Directory -Force -Path $DataDir, $ShotDir, $OutDir | Out-Null
Get-ChildItem $ShotDir -Filter '*.ready' -ErrorAction SilentlyContinue | Remove-Item -Force
Get-ChildItem $ShotDir -Filter '*.done' -ErrorAction SilentlyContinue | Remove-Item -Force

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $Exe
$psi.UseShellExecute = $false
$psi.EnvironmentVariables['ZSSH_DATA_DIR'] = $DataDir
$psi.EnvironmentVariables['ZSSH_MAIN_SCRIPT'] = $Script
$psi.EnvironmentVariables['ZSSH_SHOT_DIR'] = $ShotDir
if ($LogFile -ne '') { $psi.EnvironmentVariables['ZSSH_MAIN_LOG'] = $LogFile }
foreach ($pair in ($ExtraEnv -split '\|')) {
  if ($pair -match '^([^=]+)=(.*)$') { $psi.EnvironmentVariables[$Matches[1]] = $Matches[2] }
}
$proc = [System.Diagnostics.Process]::Start($psi)
Write-Host "started pid $($proc.Id)"
$deadline = (Get-Date).AddSeconds($TimeoutSec)
$handled = @{}
while ((Get-Date) -lt $deadline -and -not $proc.HasExited) {
  foreach ($f in Get-ChildItem $ShotDir -Filter '*.ready' -ErrorAction SilentlyContinue) {
    $name = [IO.Path]::GetFileNameWithoutExtension($f.Name)
    if ($handled.ContainsKey($name)) { continue }
    $wantTitle = if ($name -like 'files-*' -or $name -like 'wmclose-files*') { 'ZeeTerm 文件' } elseif ($name -like 'termwin-*') { 'ZeeTerm 终端' } elseif ($name -like 'update-*') { '检查更新' } else { '' }
    $h = Find-Window ([uint32]$proc.Id) $wantTitle
    if ($h -eq [IntPtr]::Zero) { continue }
    # kill-*：模拟程序异常退出（崩溃 / 被结束进程）：直接结束自己启动的那个进程，不给它任何收尾机会
    # （没有关窗口回调、没有退出清理），用来验证下次启动的恢复提示。只对自己启动的进程。
    if ($name -like 'kill-*') {
      $handled[$name] = $true
      Set-Content -Path (Join-Path $ShotDir "$name.done") -Value 'ok'
      Write-Host "kill $name"
      $proc.Kill()
      break
    }
    # wmclose-*：不截图，给这个窗口发一次 WM_CLOSE（和点标题栏上的关闭按钮同一个系统消息），
    # 让程序自己的"关窗口"逻辑（确认、合并）走真实路径。只对自己启动的进程的窗口发。
    if ($name -like 'wmclose-*') {
      [void][W32]::PostMessage($h, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)
      $handled[$name] = $true
      Set-Content -Path (Join-Path $ShotDir "$name.done") -Value 'ok'
      Write-Host "wm_close $name"
      continue
    }
    Start-Sleep -Milliseconds 400
    Save-Shot $h (Join-Path $OutDir "$name.png")
    $handled[$name] = $true
    Set-Content -Path (Join-Path $ShotDir "$name.done") -Value 'ok'
    Write-Host "shot $name"
  }
  Start-Sleep -Milliseconds 150
}
# 只结束自己启动的那个进程。
if (-not $proc.HasExited) { Write-Host 'timeout; killing own process'; $proc.Kill() }
$proc.WaitForExit()
Write-Host "exit code $($proc.ExitCode)"
exit $proc.ExitCode
