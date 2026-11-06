#!/bin/sh
# ZeeTerm 卸载脚本：删除 install.sh 装上的文件（按 <prefix>/share/zeeterm/install-manifest 清单）。
#
#   ~/.local/share/zeeterm/uninstall.sh             卸载装在 ~/.local 的
#   ./uninstall.sh --prefix DIR                    卸载装在 DIR 的
#
# 不会删除你的数据（主机、设置、凭据库等在 ~/.local/share/ZeeTerm，或 ZSSH_DATA_DIR 指定的目录）。
set -eu

APP_ID="app.zeeterm.ZeeTerm"
here=$(cd -P "$(dirname "$0")" && pwd)

usage() {
    cat <<USAGE
用法: $0 [--prefix DIR]
Usage: $0 [--prefix DIR]

  --prefix DIR   安装位置 / install location (default: where this script was installed, else \$HOME/.local)
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
    # 从 <prefix>/share/zeeterm/ 里运行时，前缀就是上两级。
    if [ "$(basename "$here")" = "zeeterm" ] && [ "$(basename "$(dirname "$here")")" = "share" ]; then
        prefix=$(dirname "$(dirname "$here")")
    elif [ -n "${HOME:-}" ]; then
        prefix="$HOME/.local"
    else
        echo "错误 / error: 没有设置 HOME，请用 --prefix 指定位置 / HOME is not set, pass --prefix" >&2
        exit 1
    fi
fi
case "$prefix" in
    /*) ;;
    *) prefix="$(pwd)/$prefix" ;;
esac
while [ "$prefix" != "/" ] && [ "${prefix%/}" != "$prefix" ]; do prefix=${prefix%/}; done

state_dir="$prefix/share/zeeterm"
manifest="$state_dir/install-manifest"
if [ ! -f "$manifest" ]; then
    echo "错误 / error: $prefix 下没有找到 ZeeTerm 的安装记录 / no ZeeTerm install record under $prefix" >&2
    echo "（用 --prefix 指到安装时用的目录 / pass the --prefix used at install time）" >&2
    exit 1
fi

removed=0
while IFS= read -r path; do
    [ -n "$path" ] || continue
    # 只删清单里 <prefix> 之下的路径：清单被改坏也不会越界。
    case "$path" in
        "$prefix"/*) ;;
        *) echo "跳过 / skip（不在安装位置下 / outside prefix）: $path" >&2; continue ;;
    esac
    if [ -e "$path" ] || [ -L "$path" ]; then
        rm -f "$path"
        removed=$((removed + 1))
    fi
done < "$manifest"
rm -f "$manifest"

# 只清理我们自己建的空目录；hicolor、applications、bin 这类共用目录不动，rmdir 遇到非空目录会失败，忽略。
for dir in "$prefix"/share/icons/hicolor/*/apps; do
    [ -d "$dir" ] || continue
    rmdir "$dir" 2>/dev/null || true
    rmdir "$(dirname "$dir")" 2>/dev/null || true
done
rmdir "$state_dir" 2>/dev/null || true

if command -v update-desktop-database >/dev/null 2>&1 && [ -d "$prefix/share/applications" ]; then
    update-desktop-database -q "$prefix/share/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -d "$prefix/share/icons/hicolor" ]; then
    gtk-update-icon-cache -q -t -f "$prefix/share/icons/hicolor" 2>/dev/null || true
fi

data_dir="${XDG_DATA_HOME:-${HOME:-~}/.local/share}/ZeeTerm"
echo "已卸载 / Uninstalled ($removed files) from $prefix"
echo "数据没有删除 / Your data was left in place: $data_dir"
