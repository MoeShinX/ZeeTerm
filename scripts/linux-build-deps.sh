#!/bin/sh
# Linux 上编译、测试、打包 ZeeTerm 要装的系统包（Debian 12+ / Ubuntu 22.04+）。
# CI（.github/workflows/）和 README 都引用这份清单，改依赖只改这里。
#
#   scripts/linux-build-deps.sh [--print] [build] [test] [package]
#
#   不带组名 = 三组全装。--print 只打印包名（一行一个），不安装。
#   build    编译需要的开发包（GPUI 的窗口 / 键盘 / 字体，TLS，SQLite，托盘依赖的 GTK）
#   test     跑测试需要的：X11 转发用真 Xvfb（xvfb、xauth），钥匙串测试用一次性 D-Bus 会话 + gnome-keyring，
#            串口测试用 socat 造伪终端
#   package  打包 / 检查需要的：dpkg-shlibdeps（dpkg-dev）、desktop-file-validate、appstreamcli、lintian
#
# 还需要 Rust 工具链（按仓库的 rust-toolchain.toml：`rustup toolchain install`），这里不装。
# 非 root 时用 sudo 调 apt-get。
set -eu

build_pkgs="build-essential pkg-config
libssl-dev libsqlite3-dev libzstd-dev
libdbus-1-dev
libfontconfig-dev libfreetype-dev
libx11-dev libx11-xcb-dev libxcb1-dev libxcb-xkb-dev
libxkbcommon-dev libxkbcommon-x11-dev
libwayland-dev libvulkan-dev
libgtk-3-dev"

test_pkgs="xvfb xauth dbus dbus-x11 gnome-keyring socat"

package_pkgs="dpkg-dev binutils desktop-file-utils appstream lintian xz-utils gzip"

print_only=0
groups=""
for arg in "$@"; do
    case "$arg" in
        --print) print_only=1 ;;
        build|test|package) groups="$groups $arg" ;;
        -h|--help)
            sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "不认识的参数: $arg（可用: --print build test package）" >&2
            exit 2
            ;;
    esac
done
[ -n "$groups" ] || groups="build test package"

list=""
for group in $groups; do
    case "$group" in
        build) list="$list $build_pkgs" ;;
        test) list="$list $test_pkgs" ;;
        package) list="$list $package_pkgs" ;;
    esac
done
# 去重，一行一个。
list=$(printf '%s\n' $list | sort -u)

if [ "$print_only" = 1 ]; then
    printf '%s\n' "$list"
    exit 0
fi

if [ "$(id -u)" = 0 ]; then
    sudo_cmd=""
else
    sudo_cmd="sudo"
fi
export DEBIAN_FRONTEND=noninteractive
# shellcheck disable=SC2086
$sudo_cmd apt-get update
# shellcheck disable=SC2086
$sudo_cmd apt-get install -y --no-install-recommends $list
