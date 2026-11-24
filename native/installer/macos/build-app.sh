#!/bin/bash
# 打 macOS 的 .app，再压成发版用的 zip。
#
# 支持两种架构：
#   arm64  aarch64-apple-darwin（默认；ZeeTerm-<版本>-macos-arm64.zip 的名字和位置不变）
#   x64    x86_64-apple-darwin（Intel；ZeeTerm-<版本>-macos-x64.zip）
# 也可以 --arch all 一次打两个。每个架构的 .app 放在 <dist>/<arch>/，zip 放在 <dist>/；
# 清理旧 zip 时只清自己架构的，不会把另一份删掉。
#
#   native/installer/macos/build-app.sh [--arch arm64|x64|all] [--print-plan]
#
# 产物在 native/target/dist/（或 ZEETERM_MAC_DIST_DIR）：
#   ZeeTerm-<版本>-macos-arm64.zip   现有发版 / Homebrew 用的名字，保持不变
#   ZeeTerm-<版本>-macos-x64.zip     Intel 预览
#   只有 ad-hoc 签名（没有 Apple 开发者证书）、未公证。
set -euo pipefail

usage() {
  sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

die() { echo "错误：$*" >&2; exit 1; }

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
NATIVE="$ROOT/native"
BRAND="$NATIVE/zeeterm-gpui/assets/brand"
DIST="${ZEETERM_MAC_DIST_DIR:-$NATIVE/target/dist}"
# 和 Linux 的应用标识（app.zeeterm.ZeeTerm.desktop / window_frame::APP_ID）一致：发出去之后再改，
# 钥匙串条目的访问授权、隐私授权（TCC）和 defaults 域都会对不上。
BUNDLE_ID="app.zeeterm.ZeeTerm"
# 版本号取 native/Cargo.toml 里 [workspace.package] 的 version（和程序自己显示的是同一个）。
VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$NATIVE/Cargo.toml" | head -1)"
# CFBundleShortVersionString 只能是点分的数字（带预发布后缀时系统会拒绝或排序出错）。
if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+(\.[0-9]+){1,2}$'; then
  die "从 native/Cargo.toml 读到的版本号不能直接用作 macOS 版本号：'$VERSION'"
fi
# 首轮按 macOS 14.0 验证；面向 Intel 正式发版前再评估能否下调（GPUI/Metal/依赖决定）。
MIN_MACOS="14.0"

arch=arm64
print_plan=0
while [ $# -gt 0 ]; do
  case "$1" in
    --arch)
      [ $# -ge 2 ] || die "--arch 需要 arm64、x64 或 all"
      arch=$2
      shift 2
      ;;
    --print-plan)
      print_plan=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "不认识的参数：$1"
      ;;
  esac
done

case "$arch" in
  arm64|aarch64) arch=arm64 ;;
  x64|x86_64|intel) arch=x64 ;;
  all) ;;
  *) die "--arch 只能是 arm64、x64 或 all：$arch" ;;
esac

if [ "$arch" = "all" ]; then
  archs=(arm64 x64)
else
  archs=("$arch")
fi

# 目标架构、ZIP 名和 .app 目录都从同一项映射取，避免主程序、helper 和包名对不上。
map_arch() {
  case "$1" in
    arm64)
      RUST_TARGET=aarch64-apple-darwin
      ZIP_ARCH=arm64
      LIPO_ARCH=arm64
      ;;
    x64)
      RUST_TARGET=x86_64-apple-darwin
      ZIP_ARCH=x64
      LIPO_ARCH=x86_64
      ;;
    *) die "内部错误：未知架构 $1" ;;
  esac
  APP="$DIST/$ZIP_ARCH/ZeeTerm.app"
  ZIP_NAME="ZeeTerm-${VERSION}-macos-${ZIP_ARCH}.zip"
  ZIP="$DIST/$ZIP_NAME"
}

if [ "$print_plan" = 1 ]; then
  for item in "${archs[@]}"; do
    map_arch "$item"
    printf 'arch=%s\nrust_target=%s\napp=%s\nzip=%s\n' \
      "$item" "$RUST_TARGET" "$APP" "$ZIP"
  done
  exit 0
fi

export MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS"

echo "== 生成图标 =="
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
# 用 SVG 的 viewBox 放大到 1024，避免从 432 拉伸糊掉。
sed 's#<svg xmlns="http://www.w3.org/2000/svg" viewBox="33 33 432 432">#<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="33 33 432 432">#' \
  "$BRAND/zeeterm-logo.svg" > "$WORK/logo.svg"
sips -s format png "$WORK/logo.svg" --out "$WORK/logo1024.png" >/dev/null
mkdir -p "$WORK/icon.iconset"
for spec in "16 16" "32 32" "64 64" "128 128" "256 256" "512 512"; do
  size="${spec%% *}"
  sips -z "$size" "$size" "$WORK/logo1024.png" --out "$WORK/icon.iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$WORK/logo1024.png" --out "$WORK/icon.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$WORK/icon.iconset" -o "$WORK/ZeeTerm.icns"

build_arch() {
  map_arch "$1"

  echo
  echo "== 构建 $ZIP_ARCH（$RUST_TARGET） =="
  (
    cd "$NATIVE"
    cargo build --locked --release --target "$RUST_TARGET" --target-dir "$NATIVE/target" -p zeeterm-gpui
  )
  echo "== 构建 $ZIP_ARCH 的 helper（zeeterm-mcp） =="
  # helper 属于根工作区（crates/），使用相同的目标架构独立构建。
  (
    cd "$ROOT"
    cargo build --locked --release --target "$RUST_TARGET" --target-dir "$ROOT/target" -p zeeterm-mcp
  )

  echo "== 组装 $ZIP_ARCH 的 .app =="
  rm -rf "$APP"
  mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
  cp "$NATIVE/target/$RUST_TARGET/release/zeeterm-gpui" "$APP/Contents/MacOS/ZeeTerm"
  chmod +x "$APP/Contents/MacOS/ZeeTerm"
  cp "$WORK/ZeeTerm.icns" "$APP/Contents/Resources/ZeeTerm.icns"
  cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
  cp "$ROOT/NOTICE" "$APP/Contents/Resources/NOTICE"
  cp "$NATIVE/NOTICE" "$APP/Contents/Resources/NOTICE-native"
  cp "$ROOT/target/$RUST_TARGET/release/zeeterm-mcp" "$APP/Contents/MacOS/zeeterm-mcp"
  chmod +x "$APP/Contents/MacOS/zeeterm-mcp"

  cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>ZeeTerm</string>
    <key>CFBundleDisplayName</key><string>ZeeTerm</string>
    <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key><string>$VERSION</string>
    <key>CFBundleShortVersionString</key><string>$VERSION</string>
    <key>CFBundleExecutable</key><string>ZeeTerm</string>
    <key>CFBundleIconFile</key><string>ZeeTerm</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleDevelopmentRegion</key><string>en</string>
    <key>CFBundleLocalizations</key>
    <array>
        <string>en</string>
        <string>zh-Hans</string>
    </array>
    <key>LSMinimumSystemVersion</key><string>$MIN_MACOS</string>
    <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
PLIST

  # 主程序和 helper 必须是目标架构；Intel 交叉编译 / 误用缓存都会在这里挡下。
  # 用 lipo -archs 精确比对：Universal 2（arm64 x86_64）不能冒充单架构包。
  for bin in "$APP/Contents/MacOS/ZeeTerm" "$APP/Contents/MacOS/zeeterm-mcp"; do
    archs=$(lipo -archs "$bin" 2>/dev/null || true)
    [ "$archs" = "$LIPO_ARCH" ] || die "$bin 的架构是 '$archs'，期望 '$LIPO_ARCH'"
  done

  # ad-hoc 签名让系统至少能校验包完整性；这不是 Developer ID 签名，也没有公证。
  codesign --force --sign - --timestamp=none "$APP/Contents/MacOS/zeeterm-mcp"
  codesign --force --sign - --timestamp=none "$APP"
  codesign --verify --deep --strict "$APP"
  plutil -lint "$APP/Contents/Info.plist"

  echo "== 压缩 $ZIP_ARCH 的 zip =="
  # 先清掉这个架构别的版本留下的 zip：CI 按通配符上传，旧版本不能一起带走。
  rm -f "$DIST"/ZeeTerm-*-macos-"$ZIP_ARCH".zip
  # 必须用 ditto：zip -r 会把符号链接展开、丢掉扩展属性，破坏 .app 里的签名。
  # --keepParent 让压缩包顶层就是 ZeeTerm.app（解压即得到 .app）。
  ditto -c -k --keepParent "$APP" "$ZIP"
  # 压缩包顶层只能有 ZeeTerm.app（不能带 __MACOSX 之类的东西）。
  STRAY="$(unzip -Z1 "$ZIP" | grep -v '^ZeeTerm\.app/' || true)"
  if [ -n "$STRAY" ]; then
    echo "zip 顶层有 ZeeTerm.app 以外的内容：" >&2
    echo "$STRAY" >&2
    exit 1
  fi
  # 解到临时目录再验一遍签名和架构：用户拿到的是解压出来的那份，不能只看压缩前。
  CHECK="$WORK/zip-check-$ZIP_ARCH"
  mkdir -p "$CHECK"
  ditto -x -k "$ZIP" "$CHECK"
  codesign --verify --deep --strict "$CHECK/ZeeTerm.app"
  for bin in "$CHECK/ZeeTerm.app/Contents/MacOS/ZeeTerm" "$CHECK/ZeeTerm.app/Contents/MacOS/zeeterm-mcp"; do
    archs=$(lipo -archs "$bin" 2>/dev/null || true)
    [ "$archs" = "$LIPO_ARCH" ] || die "解压后的 $bin 架构是 '$archs'，期望 '$LIPO_ARCH'"
  done

  echo "完成 $ZIP_ARCH：$ZIP"
}

for item in "${archs[@]}"; do
  build_arch "$item"
done

echo
echo "== 完成 =="
for item in "${archs[@]}"; do
  map_arch "$item"
  echo "$ZIP"
done
echo "版本 ${VERSION}，Bundle ID ${BUNDLE_ID}，最低系统 ${MIN_MACOS}"
