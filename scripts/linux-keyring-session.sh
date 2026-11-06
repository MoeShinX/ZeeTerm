#!/bin/sh
# 在一次性的 D-Bus 会话 + 临时解锁的 GNOME Keyring 里运行一条命令，结束即丢弃：
#   scripts/linux-keyring-session.sh cargo test --workspace --locked
#
# 为什么：Linux 版把本机秘密的密钥放进 Secret Service（见 crates/zssh-core/src/store/vault.rs），
# 测试要走真实的 Secret Service，又不能碰用户桌面的钥匙串。所以每次跑一个私有的会话总线，
# 起一个用固定测试口令解锁的 gnome-keyring-daemon，命令结束后整个会话连同它的数据一起删掉。
# 本机的 /opt/zeeterm/vm-gates.sh 和 CI 用的是同一种做法。需要 dbus-run-session 和 gnome-keyring
# （scripts/linux-build-deps.sh test 会装）。命令的退出码原样返回。
set -eu

if [ $# -eq 0 ]; then
    echo "用法: $0 <命令> [参数...]" >&2
    exit 2
fi
for tool in dbus-run-session gnome-keyring-daemon; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "缺少 $tool（scripts/linux-build-deps.sh test 会装）" >&2
        exit 127
    fi
done

session_dir=$(mktemp -d "${TMPDIR:-/tmp}/zt-keyring-XXXXXX")
trap 'rm -rf "$session_dir"' EXIT INT TERM

status=0
dbus-run-session -- sh -c '
    export XDG_DATA_HOME="$1" XDG_RUNTIME_DIR="$1"
    shift
    printf "%s" zt-test | gnome-keyring-daemon --unlock --components=secrets >/dev/null 2>&1
    "$@"
' _ "$session_dir" "$@" || status=$?
exit "$status"
