#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Build the release binary for one macOS target, assemble and ad-hoc sign
# LightPhotos.app around it, and wrap that in a drag-install .dmg whose mounted
# volume shows the app icon instead of a blank disk. release.yml runs it once
# per target.
#
#   ./scripts/make-dmg.sh aarch64-apple-darwin lightphotos-macos-arm64.dmg
#   ./scripts/make-dmg.sh x86_64-apple-darwin  lightphotos-macos-x64.dmg
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${1:?usage: $0 <target-triple> <out.dmg>}"
OUT="${2:?usage: $0 <target-triple> <out.dmg>}"
BIN_NAME="lightphotos"

STAGE="$(mktemp -d)"
RW="$(mktemp -d)/rw.dmg"
MNT=""
cleanup() {
  [[ -n "$MNT" ]] && hdiutil detach -quiet "$MNT" || true
  rm -rf "$STAGE" "$(dirname "$RW")"
}
trap cleanup EXIT

"$ROOT/scripts/setup.sh"

# Rust's x86_64 default is 10.12; match Info.plist's LSMinimumSystemVersion.
export MACOSX_DEPLOYMENT_TARGET=11.0
rustup target add "$TARGET"
echo "==> Building release binary for $TARGET"
cargo build --release --bin "$BIN_NAME" --target "$TARGET" --manifest-path "$ROOT/Cargo.toml"

APP="$STAGE/LightPhotos.app"
echo "==> Assembling LightPhotos.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/target/$TARGET/release/$BIN_NAME" "$APP/Contents/MacOS/$BIN_NAME"
chmod +x "$APP/Contents/MacOS/$BIN_NAME"

# Rendered from assets/icon/lightphotos.svg by scripts/make-icons.sh.
cp "$ROOT/assets/icon/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
cp "$ROOT/LICENSE" "$ROOT/THIRD-PARTY-LICENSES.txt" "$APP/Contents/Resources/"

# The linker signs only the binary, which leaves the bundle's signature invalid
# and makes Gatekeeper call a downloaded copy "damaged". An ad-hoc signature
# over the whole bundle is free and downgrades that to an "Open Anyway" prompt.
echo "==> Ad-hoc signing LightPhotos.app"
codesign --force --sign - "$APP"

ln -s /Applications "$STAGE/Applications"
cp "$ROOT/assets/icon/AppIcon.icns" "$STAGE/.VolumeIcon.icns"

hdiutil create -quiet -volname LightPhotos -srcfolder "$STAGE" -fs HFS+ -format UDRW -ov "$RW"
MNT="$(hdiutil attach -nobrowse -noautoopen "$RW" | awk -F'\t' '/\/Volumes\// {print $NF}')"
# Finder reads .VolumeIcon.icns only when the volume root carries the custom-icon flag.
SetFile -a C "$MNT"
hdiutil detach -quiet "$MNT"
MNT=""

hdiutil convert -quiet "$RW" -format UDZO -ov -o "$OUT"
echo "==> Wrote $OUT"
