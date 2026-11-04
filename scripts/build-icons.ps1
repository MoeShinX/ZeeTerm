# 重新出 ZeeTerm 的图标：从 SVG 渲染全部尺寸，打包 zeeterm.ico，更新 zeeterm-logo-32.png / -64.png，
# 并把同一份 ico 复制到安装包用的 native/installer/assets/icon.ico（有测试要求两份逐字节相同）。
#
# 为什么用 Rust 小工具（native/tools/icons）而不是纯 PowerShell：.NET 的 System.Drawing 画不了 SVG，
# 而 resvg / tiny-skia 已经在依赖树里（gpui 用它画 SVG），不引入新依赖；本脚本只是包一层，免得手敲路径。
#
# 源文件（都在 native/zssh-gpui/assets/brand/）：
#   zeeterm-logo.svg                    48 / 64 / 128 / 256 以及 zeeterm-logo-64.png
#   small/zeeterm-icon-{16,24,32}.svg   小尺寸，按像素网格画的；zeeterm-logo-32.png 也用 32 这张
# 用法：
#   powershell -NoProfile -File scripts/build-icons.ps1            # 重新生成并写入
#   powershell -NoProfile -File scripts/build-icons.ps1 -Check     # 只比对，源 SVG 和已提交的图标不一致就失败
# 渲染不读系统字体、不带时间戳，重复运行产物逐字节相同。
param([switch]$Check)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
$manifest = Join-Path $root 'native\Cargo.toml'
$brand = Join-Path $root 'native\zssh-gpui\assets\brand'
$installerIcon = Join-Path $root 'native\installer\assets\icon.ico'
$toolArgs = @('run', '--quiet', '--locked', '--manifest-path', $manifest, '-p', 'zssh-icons', '--', $brand, $installerIcon)
if ($Check) { $toolArgs += '--check' }
& cargo @toolArgs
exit $LASTEXITCODE
