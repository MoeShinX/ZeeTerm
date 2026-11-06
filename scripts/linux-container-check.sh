#!/usr/bin/env bash
# 依赖基线检查：把打好的 .deb 放进干净的容器（默认 debian:12 和 ubuntu:22.04）里装一遍，
# 确认 `ldd` 没有 not found、`zeeterm --version` 能跑、桌面文件通过校验、卸载干净。
# 发布包在 ubuntu-22.04 上编（glibc 2.35 基线），这一步证明老一点的系统也能装能跑。
#
#   scripts/linux-container-check.sh <deb 文件> [镜像...]
#
# 需要 docker（或把 DOCKER 环境变量指到 podman）。镜像要联网拉取，容器里会 apt-get 装依赖。
set -euo pipefail
script_dir=$(cd -P "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$script_dir/.." && pwd)
runtime=${DOCKER:-docker}

deb=${1:?用法: $0 <deb 文件> [镜像...]}
shift
images=("$@")
[ "${#images[@]}" -gt 0 ] || images=(debian:12 ubuntu:22.04)

[ -f "$deb" ] || { echo "找不到 deb：$deb" >&2; exit 2; }
command -v "$runtime" >/dev/null 2>&1 || { echo "条件缺失：没有 $runtime，没法做容器检查" >&2; exit 3; }
deb_dir=$(cd "$(dirname "$deb")" && pwd)
deb_name=$(basename "$deb")
# 包名形如 ZeeTerm-<版本>-linux-x86_64.deb，版本取自文件名。
version=${deb_name#ZeeTerm-}
version=${version%-linux-x86_64.deb}
[ "$version" != "$deb_name" ] || { echo "deb 文件名不是 ZeeTerm-<版本>-linux-x86_64.deb：$deb_name" >&2; exit 2; }

for image in "${images[@]}"; do
    echo "==> 容器检查：$image"
    "$runtime" run --rm \
        -v "$deb_dir/$deb_name:/pkg/$deb_name:ro" \
        -v "$root/native/installer/linux/verify-in-container.sh:/verify.sh:ro" \
        "$image" sh /verify.sh "/pkg/$deb_name" "$version"
done
echo "所有镜像的容器检查通过：${images[*]}"
