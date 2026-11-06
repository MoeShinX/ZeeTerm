#!/usr/bin/env bash
# 打 ZeeTerm 的 Linux 安装包：ZeeTerm-<版本>-linux-x86_64.deb 和 ZeeTerm-<版本>-linux-x86_64.tar.gz。
# 本机和 CI（ci.yml 的打包步骤、release.yml 的 Linux 任务）都调它，对应 Windows 的 native/installer/build.ps1。
#
#   scripts/package-linux.sh [--out-dir DIR] [--profile release|gate] [--skip-build] [--version X.Y.Z]
#
#   --out-dir DIR    产物目录，默认 <仓库>/dist/<版本>/
#   --profile P      cargo 配置：release（默认，正式包用）或 gate（本机试打包，省编译时间；产物不能发布）
#   --skip-build     不调 cargo，直接用已经编好的二进制（zssh-gpui、zssh-icons 在 native 工作区，zssh-mcp 在根工作区；
#                    位置按 CARGO_TARGET_DIR，没设就是 native/target 和 target）。编译机上要经 vm-gates.sh 编，就用它
#   --version V      只改包名和包里的版本号（演练 / 升级测试用）；程序里的版本仍是编译进去的那个，
#                    所以 --version 与仓库版本不一致时跳过 `zeeterm --version` 的核对
#
# 环境变量：
#   ZEETERM_PKG_REQUIRE_CHECKS=1   lintian / appstreamcli / desktop-file-validate 缺了按失败处理，
#                                  glibc 符号超过 2.35 也按失败处理（CI 里设；本机默认只提示）
#   SOURCE_DATE_EPOCH              固定包里的时间戳（默认取最近一次提交的时间）
#
# 做的事：编译 -> hicolor 图标 -> 暂存 deb 目录树 -> dpkg-shlibdeps 算 Depends -> dpkg-deb 打包 -> 暂存并打 tar.gz ->
# 自检（文件布局、元数据、desktop-file-validate、appstreamcli、lintian、ldd 无 not found、zeeterm --version、
# tar.gz 里的 install.sh / uninstall.sh 在临时前缀里走一遍）。不需要 root，不装任何系统包。
set -euo pipefail

script_dir=$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$script_dir/.." && pwd)
cd "$root"

APP_ID="app.zeeterm.ZeeTerm"
ARCH_NAME="linux-x86_64"
GLIBC_BASELINE="2.35"
MAINTAINER="ZeeTerm contributors <noreply@users.noreply.github.com>"
HOMEPAGE="https://github.com/MoeShinX/ZeeTerm"
linux_src="$root/native/installer/linux"
brand="$root/native/zssh-gpui/assets/brand"

die() { echo "错误：$*" >&2; exit 1; }
step() { echo; echo "==> $*"; }
require_checks=${ZEETERM_PKG_REQUIRE_CHECKS:-0}

# ---------- 参数 ----------
out_dir=""
profile=release
skip_build=0
version_override=""
while [ $# -gt 0 ]; do
    case "$1" in
        --out-dir) [ $# -ge 2 ] || die "--out-dir 需要一个目录"; out_dir=$2; shift 2 ;;
        --profile) [ $# -ge 2 ] || die "--profile 需要 release 或 gate"; profile=$2; shift 2 ;;
        --skip-build) skip_build=1; shift ;;
        --version) [ $# -ge 2 ] || die "--version 需要一个版本号"; version_override=$2; shift 2 ;;
        -h|--help) sed -n '2,24p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "不认识的参数：$1" ;;
    esac
done
case "$profile" in release|gate) ;; *) die "--profile 只能是 release 或 gate：$profile" ;; esac

# ---------- 版本 ----------
# 只从 native 工作区的 [workspace.package] 段读（zssh-gpui 继承它），和 build.ps1 一样；读不到就失败。
repo_version=$(awk '
    /^\[workspace\.package\]/ { in_section = 1; next }
    /^\[/ { in_section = 0 }
    in_section && /^[[:space:]]*version[[:space:]]*=/ { gsub(/^[^"]*"|"[^"]*$/, ""); print; exit }
' native/Cargo.toml)
[ -n "$repo_version" ] || die "读不到 native/Cargo.toml 的 workspace.package version，没法确定包的版本。"
version=${version_override:-$repo_version}
semver_re='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z][0-9A-Za-z.]*)?$'
[[ $version =~ $semver_re ]] || die "版本号要写成 X.Y.Z（可带 -rc.1 这类后缀）：$version"
# Debian 版本里预发布要用 ~，这样 0.4.0~rc.1 排在 0.4.0 之前；包文件名仍用 semver 原样（和 latest.json 的约定一致）。
deb_version=${version/-/\~}
base_name="ZeeTerm-${version}-${ARCH_NAME}"
[ -n "$out_dir" ] || out_dir="$root/dist/$version"
mkdir -p "$out_dir"
out_dir=$(cd "$out_dir" && pwd)
deb_file="$out_dir/$base_name.deb"
tar_file="$out_dir/$base_name.tar.gz"

if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
    epoch=$SOURCE_DATE_EPOCH
else
    epoch=$(git -C "$root" log -1 --format=%ct 2>/dev/null || true)
    [ -n "$epoch" ] || epoch=$(date +%s)
fi
export SOURCE_DATE_EPOCH=$epoch
release_date=$(date -u -d "@$epoch" +%Y-%m-%d)
changelog_date=$(LC_ALL=C date -u -d "@$epoch" '+%a, %d %b %Y %H:%M:%S +0000')

echo "ZeeTerm Linux 打包：版本 $version（deb 版本 $deb_version），配置 $profile，输出 $out_dir"
[ "$(uname -s)" = Linux ] || die "只能在 Linux 上打包。"
[ "$(uname -m)" = x86_64 ] || die "只能在 x86_64 上打包（当前 $(uname -m)）。"

# ---------- 编译 ----------
native_target=${CARGO_TARGET_DIR:-$root/native/target}
root_target=${CARGO_TARGET_DIR:-$root/target}
gpui_bin="$native_target/$profile/zssh-gpui"
icons_bin="$native_target/$profile/zssh-icons"
mcp_bin="$root_target/$profile/zssh-mcp"

if [ "$skip_build" = 0 ]; then
    command -v cargo >/dev/null 2>&1 || die "找不到 cargo，请先安装 Rust 工具链（rustup toolchain install）。"
    # 和 build.ps1、CI 的 "cargo build --release" 同一条命令（整个 native 工作区）：特性合并结果一致，已有的编译缓存直接命中。
    step "cargo build --profile $profile（native 工作区：zssh-gpui、zssh-icons）"
    cargo build --profile "$profile" --locked --manifest-path native/Cargo.toml --workspace
    # zssh-mcp 属于根工作区，AI 工具桥靠它：必须和主程序放在同一个目录，所以每次都现编。
    step "cargo build --profile $profile（根工作区：zssh-mcp）"
    cargo build --profile "$profile" --locked --manifest-path Cargo.toml -p zssh-mcp
fi
for bin in "$gpui_bin" "$icons_bin" "$mcp_bin"; do
    if [ ! -f "$bin" ] || [ ! -x "$bin" ]; then
        die "缺少编译产物：$bin（用了 --skip-build 的话要先自己编好）"
    fi
done
for bin in "$gpui_bin" "$mcp_bin"; do
    machine=$(readelf -h "$bin" | awk -F: '/Machine/ { gsub(/^[ \t]+/, "", $2); print $2 }')
    case "$machine" in *X86-64*) ;; *) die "$bin 不是 x86_64 的可执行文件（$machine）" ;; esac
done

# ---------- 暂存 ----------
work=$(mktemp -d "${TMPDIR:-/tmp}/zeeterm-pkg-XXXXXXXX")
trap 'rm -rf "$work"' EXIT
step "暂存目录 $work"

# 图标：hicolor 各尺寸，从品牌 SVG 渲染（同一套出图工具，逐字节可复现）。
"$icons_bin" --hicolor "$brand" "$work/hicolor" >/dev/null
icon_count=$(find "$work/hicolor" -name "$APP_ID.png" | wc -l)
[ "$icon_count" -ge 8 ] || die "图标只生成了 $icon_count 个，期望 8 个。"

# 声明：根 NOTICE + 空一行 + native/NOTICE（和 build.ps1 / product.rs 的拼法一致：LF，根 NOTICE 去掉末尾空白）。
merged_notice="$work/NOTICE"
{
    sed -e 's/\r$//' NOTICE | awk '{ lines[NR] = $0 } END { n = NR; while (n > 0 && lines[n] ~ /^[[:space:]]*$/) n--; for (i = 1; i <= n; i++) print lines[i] }'
    echo
    sed -e 's/\r$//' native/NOTICE
} > "$merged_notice"
sed -e 's/\r$//' LICENSE > "$work/LICENSE"

# metainfo：模板里的版本和日期换成这次发布的值。
if [[ $version == *-* ]]; then release_type=development; else release_type=stable; fi
sed -e "s|@VERSION@|$deb_version|g" -e "s|@TAG_VERSION@|$version|g" -e "s|@DATE@|$release_date|g" \
    -e "s|@RELEASE_TYPE@|type=\"$release_type\"|g" \
    "$linux_src/$APP_ID.metainfo.xml.in" > "$work/$APP_ID.metainfo.xml"
if grep -q '@[A-Z_]*@' "$work/$APP_ID.metainfo.xml"; then die "metainfo 里还有没替换的占位符。"; fi

# ---------- deb ----------
step "deb：暂存目录树"
stage="$work/deb"
bin_dir="$stage/usr/bin"
install -d -m 755 "$bin_dir" "$stage/usr/share/applications" "$stage/usr/share/metainfo" \
    "$stage/usr/share/doc/zeeterm" "$stage/usr/share/man/man1" "$stage/DEBIAN"
install -m 755 "$gpui_bin" "$bin_dir/zeeterm"
install -m 755 "$mcp_bin" "$bin_dir/zssh-mcp"
install -m 644 "$linux_src/$APP_ID.desktop" "$stage/usr/share/applications/$APP_ID.desktop"
install -m 644 "$work/$APP_ID.metainfo.xml" "$stage/usr/share/metainfo/$APP_ID.metainfo.xml"
(cd "$work" && find hicolor -name "$APP_ID.png" -print0 | sort -z) | while IFS= read -r -d '' png; do
    install -D -m 644 "$work/$png" "$stage/usr/share/icons/$png"
done
gzip -9n -c "$linux_src/zeeterm.1" > "$stage/usr/share/man/man1/zeeterm.1.gz"
chmod 644 "$stage/usr/share/man/man1/zeeterm.1.gz"

# copyright：Debian 机器可读格式；第三方声明（根 NOTICE + native/NOTICE）整段放在头部的 Comment 里。
{
    echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
    echo "Upstream-Name: ZeeTerm"
    echo "Upstream-Contact: $HOMEPAGE/issues"
    echo "Source: $HOMEPAGE"
    echo "Comment:"
    awk '{ if ($0 ~ /^[[:space:]]*$/) print " ."; else print " " $0 }' "$merged_notice"
    echo
    echo "Files: *"
    echo "Copyright: 2026 ZeeTerm contributors"
    echo "License: GPL-3+"
    echo
    echo "License: GPL-3+"
    echo " This program is free software: you can redistribute it and/or modify"
    echo " it under the terms of the GNU General Public License as published by"
    echo " the Free Software Foundation, either version 3 of the License, or"
    echo " (at your option) any later version."
    echo " ."
    echo " This program is distributed in the hope that it will be useful,"
    echo " but WITHOUT ANY WARRANTY; without even the implied warranty of"
    echo " MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the"
    echo " GNU General Public License for more details."
    echo " ."
    echo " On Debian systems, the complete text of the GNU General Public"
    printf '%s\n' " License version 3 can be found in \`/usr/share/common-licenses/GPL-3'."
} > "$stage/usr/share/doc/zeeterm/copyright"
chmod 644 "$stage/usr/share/doc/zeeterm/copyright"

# changelog：Debian 格式（原生包叫 changelog.gz），只指向发布页，详细内容在 CHANGELOG.md / Release。
{
    echo "zeeterm ($deb_version) unstable; urgency=medium"
    echo
    echo "  * Release $version. See $HOMEPAGE/releases/tag/v$version"
    echo
    echo " -- $MAINTAINER  $changelog_date"
} | gzip -9n > "$stage/usr/share/doc/zeeterm/changelog.gz"
chmod 644 "$stage/usr/share/doc/zeeterm/changelog.gz"

install -m 755 "$linux_src/deb/postinst" "$stage/DEBIAN/postinst"
install -m 755 "$linux_src/deb/postrm" "$stage/DEBIAN/postrm"

step "deb：dpkg-shlibdeps 算依赖"
command -v dpkg-shlibdeps >/dev/null 2>&1 || die "找不到 dpkg-shlibdeps（装 dpkg-dev：scripts/linux-build-deps.sh package）。"
shlibs_dir="$work/shlibs"
mkdir -p "$shlibs_dir/debian"
printf 'Source: zeeterm\n\nPackage: zeeterm\nArchitecture: amd64\n' > "$shlibs_dir/debian/control"
shlibs_line=$(cd "$shlibs_dir" && dpkg-shlibdeps -O -e"$bin_dir/zeeterm" -e"$bin_dir/zssh-mcp")
shlibs=${shlibs_line#shlibs:Depends=}
if [ -z "$shlibs" ] || [ "$shlibs" = "$shlibs_line" ]; then die "dpkg-shlibdeps 没有给出依赖：$shlibs_line"; fi
echo "shlibs:Depends=$shlibs"

# dpkg-shlibdeps 看不出来的运行时依赖：这些库是程序运行时 dlopen 的，不在 ELF 的 NEEDED 里。
#   libvulkan1（渲染，Vulkan 加载器）、libwayland-client0（Wayland 窗口）、libegl1（没有 Vulkan 驱动时的 OpenGL 后备）。
extra_depends="libvulkan1, libwayland-client0, libegl1"
depends="$shlibs, $extra_depends"
# 去重（保持顺序）：同一个包 shlibdeps 已经给过就不再重复。
depends=$(printf '%s\n' "$depends" | awk -F', ' '{ for (i = 1; i <= NF; i++) { name = $i; sub(/ .*/, "", name); if (!(name in seen)) { seen[name] = 1; out = out (out ? ", " : "") $i } } print out }')

installed_size=$(du -sk --exclude=DEBIAN "$stage" | cut -f1)
cat > "$stage/DEBIAN/control" <<CONTROL
Package: zeeterm
Version: $deb_version
Architecture: amd64
Maintainer: $MAINTAINER
Installed-Size: $installed_size
Depends: $depends
Recommends: mesa-vulkan-drivers, gnome-keyring | keepassxc | kwalletmanager, xdg-utils
Section: net
Priority: optional
Homepage: $HOMEPAGE
Description: SSH, SFTP and serial terminal client
 ZeeTerm is a native graphical SSH client. It offers a tabbed and split
 terminal workspace, SFTP file management, remote editing, port forwarding,
 X11 forwarding, and Telnet and serial connections.
 .
 Saved credentials are protected by the desktop keyring (Secret Service),
 and synchronization and backups are encrypted. An optional AI assistant
 works in the terminal with the permissions you choose.
CONTROL
chmod 644 "$stage/DEBIAN/control"
(cd "$stage" && find usr -type f -print0 | sort -z | xargs -0 md5sum) > "$stage/DEBIAN/md5sums"
chmod 644 "$stage/DEBIAN/md5sums"

# 权限和时间戳统一：目录 755、文件 644（可执行的另行设置）、时间戳固定。
find "$stage" -type d -exec chmod 755 {} +
find "$stage" -exec touch -h -d "@$epoch" {} +

step "deb：dpkg-deb --build"
rm -f "$deb_file"
dpkg-deb --root-owner-group -Zxz -z6 --build "$stage" "$deb_file"

# ---------- tar.gz ----------
step "tar.gz：暂存并打包"
tar_root="$work/tar/$base_name"
install -d -m 755 "$tar_root"
install -m 755 "$gpui_bin" "$tar_root/zeeterm"
install -m 755 "$mcp_bin" "$tar_root/zssh-mcp"
install -m 755 "$linux_src/tar/install.sh" "$tar_root/install.sh"
install -m 755 "$linux_src/tar/uninstall.sh" "$tar_root/uninstall.sh"
install -m 644 "$linux_src/tar/README.txt" "$tar_root/README"
install -m 644 "$work/LICENSE" "$tar_root/LICENSE"
install -m 644 "$merged_notice" "$tar_root/NOTICE"
install -m 644 "$linux_src/$APP_ID.desktop" "$tar_root/$APP_ID.desktop"
install -m 644 "$work/$APP_ID.metainfo.xml" "$tar_root/$APP_ID.metainfo.xml"
(cd "$work" && find hicolor -name "$APP_ID.png" -print0 | sort -z) | while IFS= read -r -d '' png; do
    install -D -m 644 "$work/$png" "$tar_root/icons/$png"
done
find "$work/tar" -type d -exec chmod 755 {} +
find "$work/tar" -exec touch -h -d "@$epoch" {} +
rm -f "$tar_file"
tar --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$epoch" -C "$work/tar" -cf - "$base_name" | gzip -9n > "$tar_file"

# ---------- 自检 ----------
check_failed=0
fail_check() { echo "自检失败：$*" >&2; check_failed=1; }
skip_or_fail() { # 工具缺失：本机提示，CI 里按失败处理
    if [ "$require_checks" = 1 ]; then fail_check "条件缺失：$1"; else echo "条件缺失，跳过：$1"; fi
}

step "自检：deb 元数据和文件布局"
dpkg-deb -I "$deb_file" | sed -n '1,40p'
deb_listing=$(dpkg-deb -c "$deb_file")
for expected in \
    "./usr/bin/zeeterm" "./usr/bin/zssh-mcp" \
    "./usr/share/applications/$APP_ID.desktop" \
    "./usr/share/metainfo/$APP_ID.metainfo.xml" \
    "./usr/share/doc/zeeterm/copyright" \
    "./usr/share/icons/hicolor/16x16/apps/$APP_ID.png" \
    "./usr/share/icons/hicolor/512x512/apps/$APP_ID.png"; do
    printf '%s\n' "$deb_listing" | grep -qF " $expected" || fail_check "deb 里没有 $expected"
done
# 全部属主 root/root；可执行文件 755。
if printf '%s\n' "$deb_listing" | awk '{ print $2 }' | grep -vqx 'root/root'; then fail_check "deb 里有不是 root/root 的文件"; fi
if printf '%s\n' "$deb_listing" | grep -E ' \./usr/bin/(zeeterm|zssh-mcp)$' | grep -qv '^-rwxr-xr-x'; then
    fail_check "usr/bin 下的程序权限不是 755"
fi
[ "$(dpkg-deb -f "$deb_file" Package)" = zeeterm ] || fail_check "包名不对"
[ "$(dpkg-deb -f "$deb_file" Version)" = "$deb_version" ] || fail_check "包版本不对"

step "自检：desktop-file-validate / appstreamcli / lintian"
if command -v desktop-file-validate >/dev/null 2>&1; then
    if desktop-file-validate "$linux_src/$APP_ID.desktop"; then echo "desktop-file-validate 通过"; else fail_check "desktop-file-validate 没通过"; fi
else
    skip_or_fail "没有 desktop-file-validate（desktop-file-utils）"
fi
if command -v appstreamcli >/dev/null 2>&1; then
    appstreamcli validate --no-net --explain "$work/$APP_ID.metainfo.xml" || fail_check "appstreamcli validate 没通过"
else
    skip_or_fail "没有 appstreamcli（appstream）"
fi
if command -v lintian >/dev/null 2>&1; then
    lintian --fail-on error --tag-display-limit 0 "$deb_file" || fail_check "lintian 报告了 error"
else
    skip_or_fail "没有 lintian"
fi

step "自检：动态库依赖和 glibc 基线"
for bin in "$bin_dir/zeeterm" "$bin_dir/zssh-mcp"; do
    missing=$(ldd "$bin" 2>&1 | grep -F 'not found' || true)
    [ -z "$missing" ] || fail_check "$(basename "$bin") 有缺失的动态库：$missing"
done
max_glibc=$(objdump -T "$bin_dir/zeeterm" "$bin_dir/zssh-mcp" 2>/dev/null | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1 || true)
echo "二进制用到的最高 glibc 符号版本：${max_glibc:-无}（基线 $GLIBC_BASELINE）"
if [ -n "$max_glibc" ] && [ "$(printf '%s\n%s\n' "$max_glibc" "$GLIBC_BASELINE" | sort -V | tail -1)" != "$GLIBC_BASELINE" ]; then
    if [ "$require_checks" = 1 ]; then
        fail_check "最高 glibc 符号 $max_glibc 超过基线 $GLIBC_BASELINE（要在 ubuntu-22.04 上编）"
    else
        echo "提示：这台机器的 glibc 比基线新，编出来的包不能用于发布（发布包在 ubuntu-22.04 上编）。"
    fi
fi

step "自检：zeeterm --version"
if [ "$version" = "$repo_version" ]; then
    # 没有显示环境、没有家目录也要能跑：参数在任何初始化之前处理。
    got=$(env -i PATH=/usr/bin:/bin "$bin_dir/zeeterm" --version) || fail_check "zeeterm --version 运行失败"
    [ "$got" = "ZeeTerm $repo_version" ] || fail_check "zeeterm --version 输出 '$got'，期望 'ZeeTerm $repo_version'"
    echo "$got"
else
    echo "--version 指定的版本（$version）与仓库版本（$repo_version）不同，跳过核对。"
fi

step "自检：tar.gz 里的 install.sh / uninstall.sh（临时前缀）"
verify_dir="$work/verify"
mkdir -p "$verify_dir/extract"
tar -xzf "$tar_file" -C "$verify_dir/extract"
if (
    set -e
    cd "$verify_dir/extract/$base_name"
    prefix="$verify_dir/prefix with space"
    ./install.sh --prefix "$prefix" >/dev/null
    for f in bin/zeeterm bin/zssh-mcp "share/applications/$APP_ID.desktop" \
        "share/icons/hicolor/256x256/apps/$APP_ID.png" "share/metainfo/$APP_ID.metainfo.xml" \
        share/zeeterm/install-manifest share/zeeterm/uninstall.sh; do
        [ -e "$prefix/$f" ] || { echo "安装后缺少 $f" >&2; exit 1; }
    done
    # Exec 指向装好的程序（路径里有空格，所以必须带引号），且装好的 .desktop 能通过校验。
    exec_line=$(grep '^Exec=' "$prefix/share/applications/$APP_ID.desktop")
    [ "$exec_line" = "Exec=\"$prefix/bin/zeeterm\"" ] || { echo "Exec 行不对：$exec_line" >&2; exit 1; }
    if command -v desktop-file-validate >/dev/null 2>&1; then
        desktop-file-validate "$prefix/share/applications/$APP_ID.desktop"
    fi
    "$prefix/share/zeeterm/uninstall.sh" >/dev/null
    # update-desktop-database / gtk-update-icon-cache 生成的缓存是共用的，不算我们的文件。
    leftover=$(find "$prefix" -type f ! -name mimeinfo.cache ! -name icon-theme.cache)
    [ -z "$leftover" ] || { echo "卸载后还剩文件：$leftover" >&2; exit 1; }
); then
    echo "install.sh / uninstall.sh 自检通过"
else
    fail_check "tar.gz 的 install.sh / uninstall.sh 自检没通过"
fi

if [ "$check_failed" != 0 ]; then
    die "自检没通过，产物不能使用：$out_dir"
fi

step "完成"
for f in "$deb_file" "$tar_file"; do
    printf '%s  %s\n' "$(sha256sum "$f" | cut -d' ' -f1)" "$(basename "$f")"
done
ls -l "$deb_file" "$tar_file"
