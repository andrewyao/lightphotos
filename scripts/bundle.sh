#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Build a release binary and assemble LightPhotos.app, then register it with
# Launch Services so Finder double-click / "Open With" route image files to us.
#
#   ./scripts/bundle.sh                               # this Mac's architecture
#   ./scripts/bundle.sh --target x86_64-apple-darwin  # cross-build the Intel app
set -euo pipefail

TARGET=""
case "${1:-}" in
  "") ;;
  --target) TARGET="${2:?--target needs a triple}" ;;
  *) echo "usage: $0 [--target <triple>]" >&2; exit 2 ;;
esac

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/LightPhotos.app"
BIN_NAME="lightphotos"

echo "==> Ensuring vendored rawler is present"
"$ROOT/scripts/setup-vendor-rawler.sh"

if [[ -n "$TARGET" ]]; then
  # Rust's x86_64 default is 10.12; match Info.plist's LSMinimumSystemVersion.
  export MACOSX_DEPLOYMENT_TARGET=11.0
  rustup target add "$TARGET"
  echo "==> Building release binary for $TARGET"
  cargo build --release --bin "$BIN_NAME" --target "$TARGET" --manifest-path "$ROOT/Cargo.toml"
  BIN="$ROOT/target/$TARGET/release/$BIN_NAME"
else
  echo "==> Building release binary"
  cargo build --release --manifest-path "$ROOT/Cargo.toml"
  BIN="$ROOT/target/release/$BIN_NAME"
fi

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/Info.plist" "$APP/Contents/Info.plist"
cp "$BIN" "$APP/Contents/MacOS/$BIN_NAME"
chmod +x "$APP/Contents/MacOS/$BIN_NAME"

# Rendered from assets/icon/lightphotos.svg by scripts/make-icons.sh.
cp "$ROOT/assets/icon/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"

echo "==> Registering with Launch Services"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
"$LSREGISTER" -f "$APP"

echo "==> Done: $APP"
echo "    Open a file:  open -a \"$APP\" /path/to/photo.jpg"
echo "    Or in Finder: right-click an image -> Open With -> LightPhotos"
