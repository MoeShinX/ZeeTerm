#!/bin/sh
# ZeeTerm 免 root 安装脚本（tar.gz 包里的 install.sh）。
#
#   ./install.sh                 装到 ~/.local（lib/zeeterm、bin、share/applications、share/icons、share/metainfo）
#   ./install.sh --prefix DIR    装到 DIR（DIR/lib/zeeterm、DIR/bin、DIR/share/...）
#
# 布局和 deb 一致：程序放在 <prefix>/lib/zeeterm/{zeeterm,zssh-mcp}（zssh-mcp 是内部辅助程序，必须和主程序同目录），
# <prefix>/bin/zeeterm 是指向 ../lib/zeeterm/zeeterm 的相对软链接，只有它占用户 PATH 里的命令名。
# 应用内更新认这个布局：程序在 <prefix>/lib/zeeterm 下，且 <prefix>/share/zeeterm/install-manifest 在。
#
# 装了什么记在 <prefix>/share/zeeterm/install-manifest，卸载用同目录下的 uninstall.sh。
# 不需要 root，不碰你的数据目录（~/.local/share/ZeeTerm）。可以重复运行（覆盖旧版）。
set -eu

APP_ID="app.zeeterm.ZeeTerm"
here=$(cd -P "$(dirname "$0")" && pwd)

usage() {
    cat <<USAGE
用法: $0 [--prefix DIR]
Usage: $0 [--prefix DIR]

  --prefix DIR   安装位置 / install location (default: \$HOME/.local)
  -h, --help     显示本说明 / show this help
USAGE
}

prefix=""
while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)
            [ $# -ge 2 ] || { echo "错误 / error: --prefix 需要一个目录 / needs a directory" >&2; exit 2; }
            prefix=$2
            shift 2
            ;;
        --prefix=*)
            prefix=${1#--prefix=}
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "错误 / error: 不认识的参数 / unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "$prefix" ]; then
    if [ -z "${HOME:-}" ]; then
        echo "错误 / error: 没有设置 HOME，请用 --prefix 指定安装位置 / HOME is not set, pass --prefix" >&2
        exit 1
    fi
    prefix="$HOME/.local"
fi
case "$prefix" in
    /*) ;;
    *) prefix="$(pwd)/$prefix" ;;
esac
# 去掉末尾多余的斜杠（根目录除外）。
while [ "$prefix" != "/" ] && [ "${prefix%/}" != "$prefix" ]; do prefix=${prefix%/}; done

for required in zeeterm zssh-mcp "$APP_ID.desktop" icons/hicolor; do
    if [ ! -e "$here/$required" ]; then
        echo "错误 / error: 安装包不完整，缺少 / incomplete package, missing: $required" >&2
        exit 1
    fi
done

bin_dir="$prefix/bin"
lib_dir="$prefix/lib/zeeterm"
app_dir="$prefix/share/applications"
icon_root="$prefix/share/icons/hicolor"
meta_dir="$prefix/share/metainfo"
state_dir="$prefix/share/zeeterm"
manifest="$state_dir/install-manifest"

# 先在 state_dir 里写新的清单草稿，装完再换上；中途失败不会留下指向不存在文件的清单。
mkdir -p "$bin_dir" "$lib_dir" "$app_dir" "$state_dir"
draft="$state_dir/.install-manifest.$$"
trap 'rm -f "$draft"' EXIT INT TERM
: > "$draft"

put() { # put <模式> <源文件> <目标文件>
    mkdir -p "$(dirname "$3")"
    # 先写临时名再 mv：正在运行的 zeeterm 被覆盖时不会出现 "Text file busy"。
    tmp="$3.new.$$"
    install -m "$1" "$2" "$tmp"
    mv -f "$tmp" "$3"
    printf '%s\n' "$3" >> "$draft"
}

put 755 "$here/zeeterm" "$lib_dir/zeeterm"
put 755 "$here/zssh-mcp" "$lib_dir/zssh-mcp"

# bin/zeeterm：相对软链接。先建临时名再 mv：已有旧版（软链接或文件）时原子替换，不会有一瞬间没有这个命令。
link_tmp="$bin_dir/zeeterm.new.$$"
rm -f "$link_tmp"
ln -s ../lib/zeeterm/zeeterm "$link_tmp"
mv -f "$link_tmp" "$bin_dir/zeeterm"
printf '%s\n' "$bin_dir/zeeterm" >> "$draft"

# Exec / TryExec 里写绝对路径：桌面会话的 PATH 里不一定有 ~/.local/bin。Exec 的路径里有空格等字符时按桌面文件规范
# 加引号转义；TryExec 是不带引号的普通路径。
exe="$bin_dir/zeeterm"
case "$exe" in
    *[!A-Za-z0-9/._+-]*)
        esc=$(printf '%s' "$exe" | sed -e 's/\\/\\\\\\\\/g' -e 's/"/\\\\"/g' -e 's/`/\\\\`/g' -e 's/\$/\\\\$/g' -e 's/%/%%/g')
        exec_value="\"$esc\""
        ;;
    *)
        exec_value=$exe
        ;;
esac
desktop_tmp="$state_dir/.desktop.$$"
# 用环境变量传值：awk -v 会解释反斜杠转义，破坏上面转义好的路径。
EXEC_VALUE=$exec_value TRYEXEC_VALUE=$exe awk '/^Exec=/ { print "Exec=" ENVIRON["EXEC_VALUE"]; next } /^TryExec=/ { print "TryExec=" ENVIRON["TRYEXEC_VALUE"]; next } { print }' \
    "$here/$APP_ID.desktop" > "$desktop_tmp"
put 644 "$desktop_tmp" "$app_dir/$APP_ID.desktop"
rm -f "$desktop_tmp"

for png in "$here"/icons/hicolor/*/apps/"$APP_ID".png; do
    [ -f "$png" ] || continue
    size_dir=$(basename "$(dirname "$(dirname "$png")")")
    put 644 "$png" "$icon_root/$size_dir/apps/$APP_ID.png"
done

if [ -f "$here/$APP_ID.metainfo.xml" ]; then
    put 644 "$here/$APP_ID.metainfo.xml" "$meta_dir/$APP_ID.metainfo.xml"
fi

put 755 "$here/uninstall.sh" "$state_dir/uninstall.sh"
printf '%s\n' "$manifest" >> "$draft"
mv -f "$draft" "$manifest"
trap - EXIT INT TERM

# 刷新缓存让菜单和图标马上出现；没有这些工具、或者目录不是桌面会话认的位置都不影响使用。
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$app_dir" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q -t -f "$icon_root" 2>/dev/null || true
fi

echo "已安装 / Installed: $bin_dir/zeeterm"
echo "卸载 / Uninstall: $state_dir/uninstall.sh"
case ":${PATH:-}:" in
    *":$bin_dir:"*) ;;
    *) echo "提示 / Note: $bin_dir 不在 PATH 里；应用菜单不受影响，命令行请用完整路径 / is not in PATH (the app menu entry still works)" ;;
esac
