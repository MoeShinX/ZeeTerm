#!/bin/sh
# 在干净的 Debian / Ubuntu 容器里运行（由 scripts/linux-container-check.sh 挂进去执行）：
# 装上 .deb（连带 Depends）-> 动态库没有 not found -> `zeeterm --version` 能跑 -> 桌面文件校验 -> 卸载干净。
#   verify-in-container.sh /pkg/ZeeTerm-<版本>-linux-x86_64.deb <期望的版本>
# 会 apt-get 装包，所以只能在一次性的容器里跑，不要在日常机器上直接运行。
set -eu

deb=${1:?用法: $0 <deb 文件> <期望版本>}
expected_version=${2:?用法: $0 <deb 文件> <期望版本>}
app_id="app.zeeterm.ZeeTerm"

fail() { echo "容器检查失败：$*" >&2; exit 1; }
. /etc/os-release
echo "==> 镜像 ${PRETTY_NAME:-unknown}，glibc $(ldd --version | head -n1 | awk '{ print $NF }')"

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
# 不装 Recommends：验证 Depends 本身就够程序加载起来。
apt-get install -y -qq --no-install-recommends "$deb" desktop-file-utils >/dev/null

for exe in /usr/bin/zeeterm /usr/bin/zssh-mcp; do
    [ -x "$exe" ] || fail "$exe 不存在或不可执行"
    missing=$(ldd "$exe" 2>&1 | grep 'not found' || true)
    [ -z "$missing" ] || fail "$exe 有缺失的动态库: $missing"
done
echo "ldd：没有 not found"

# 没有显示环境、没有家目录：--version 在任何初始化之前处理。
got=$(env -i PATH=/usr/bin:/bin /usr/bin/zeeterm --version) || fail "zeeterm --version 运行失败"
[ "$got" = "ZeeTerm $expected_version" ] || fail "zeeterm --version 输出 '$got'，期望 'ZeeTerm $expected_version'"
echo "zeeterm --version: $got"

for path in \
    "/usr/share/applications/$app_id.desktop" \
    "/usr/share/metainfo/$app_id.metainfo.xml" \
    "/usr/share/icons/hicolor/16x16/apps/$app_id.png" \
    "/usr/share/icons/hicolor/256x256/apps/$app_id.png" \
    "/usr/share/icons/hicolor/512x512/apps/$app_id.png" \
    /usr/share/doc/zeeterm/copyright \
    /usr/share/man/man1/zeeterm.1.gz; do
    [ -f "$path" ] || fail "缺少 $path"
done
desktop-file-validate "/usr/share/applications/$app_id.desktop" || fail "desktop-file-validate 没通过"
echo "桌面文件校验通过"

dpkg -s zeeterm | sed -n '1,12p'
apt-get remove -y -qq zeeterm >/dev/null
for gone in /usr/bin/zeeterm /usr/bin/zssh-mcp "/usr/share/applications/$app_id.desktop" "/usr/share/icons/hicolor/256x256/apps/$app_id.png"; do
    [ ! -e "$gone" ] || fail "卸载后还留着 $gone"
done
echo "卸载干净"
echo "容器检查通过。"
