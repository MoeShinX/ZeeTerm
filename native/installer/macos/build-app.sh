#!/bin/bash
# 打一个 macOS Apple Silicon 的 .app，再压成发版用的 zip。
#
# 只做 Apple Silicon（aarch64-apple-darwin）：不生成 Intel 或 Universal 2。
# 产物在 native/target/dist/（或 ZEETERM_MAC_DIST_DIR）：
#   ZeeTerm.app                          只有 ad-hoc 签名（没有 Apple 开发者证书）、未公证；
#   ZeeTerm-<版本>-macos-arm64.zip       CI 上传、发版时放进 GitHub Release 的就是这个文件，
#                                        文件名和 scripts/update-homebrew-cask.ps1、发版工作流核对的名字一致。
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
NATIVE="$ROOT/native"
BRAND="$NATIVE/zeeterm-gpui/assets/brand"
DIST="${ZEETERM_MAC_DIST_DIR:-$NATIVE/target/dist}"
APP="$DIST/ZeeTerm.app"
# 和 Linux 的应用标识（app.zeeterm.ZeeTerm.desktop / window_frame::APP_ID）一致：发出去之后再改，
# 钥匙串条目的访问授权、隐私授权（TCC）和 defaults 域都会对不上。
BUNDLE_ID="app.zeeterm.ZeeTerm"
# 版本号取 native/Cargo.toml 里 [workspace.package] 的 version（和程序自己显示的是同一个）。
VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' "$NATIVE/Cargo.toml" | head -1)"
# CFBundleShortVersionString 只能是点分的数字（带预发布后缀时系统会拒绝或排序出错）。
if ! printf '%s' "$VERSION" | grep -Eq '^[0-9]+(\.[0-9]+){1,2}$'; then
  echo "从 native/Cargo.toml 读到的版本号不能直接用作 macOS 版本号：'$VERSION'" >&2
  exit 1
fi
MIN_MACOS="14.0"
export MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS"

echo "== 构建 release =="
cd "$NATIVE"
cargo build --locked --release --target aarch64-apple-darwin --target-dir "$NATIVE/target" -p zeeterm-gpui

echo "== 构建 helper（zeeterm-mcp） =="
# helper 属于根工作区（crates/），使用相同的目标架构独立构建。
cd "$ROOT"
cargo build --locked --release --target aarch64-apple-darwin --target-dir "$ROOT/target" -p zeeterm-mcp
cd "$NATIVE"

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

echo "== 组装 .app =="
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$NATIVE/target/aarch64-apple-darwin/release/zeeterm-gpui" "$APP/Contents/MacOS/ZeeTerm"
cp "$WORK/ZeeTerm.icns" "$APP/Contents/Resources/ZeeTerm.icns"
cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
cp "$ROOT/NOTICE" "$APP/Contents/Resources/NOTICE"
cp "$NATIVE/NOTICE" "$APP/Contents/Resources/NOTICE-native"
cp "$ROOT/target/aarch64-apple-darwin/release/zeeterm-mcp" "$APP/Contents/MacOS/zeeterm-mcp"
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

# ad-hoc 签名让系统至少能校验包完整性；这不是 Developer ID 签名，也没有公证。
codesign --force --sign - --timestamp=none "$APP/Contents/MacOS/zeeterm-mcp"
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --deep --strict "$APP"
plutil -lint "$APP/Contents/Info.plist"

echo "== 压缩成 zip =="
ZIP_NAME="ZeeTerm-${VERSION}-macos-arm64.zip"
ZIP="$DIST/$ZIP_NAME"
# 先清掉别的版本留下的 zip：构建缓存或上一次本机打包可能留着它们，而 CI 按通配符上传，会把旧版本一起带走。
rm -f "$DIST"/ZeeTerm-*-macos-arm64.zip
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
# 解到临时目录再验一遍签名：证明压缩没有破坏符号链接、权限和签名（用户拿到的是解压出来的那份）。
CHECK="$WORK/zip-check"
mkdir -p "$CHECK"
ditto -x -k "$ZIP" "$CHECK"
codesign --verify --deep --strict "$CHECK/ZeeTerm.app"

echo "== 完成 =="
echo "$APP"
echo "$ZIP"
echo "版本 ${VERSION}，Bundle ID ${BUNDLE_ID}，最低系统 ${MIN_MACOS}"
codesign -dv "$APP" 2>&1 | head -5 || true
