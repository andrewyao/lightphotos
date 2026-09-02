#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Build a release binary and assemble LightPhotos.app, then register it with
# Launch Services so Finder double-click / "Open With" route image files to us.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="$ROOT/LightPhotos.app"
BIN_NAME="lightphotos"

echo "==> Ensuring vendored rawler is present"
"$ROOT/scripts/setup-vendor-rawler.sh"

echo "==> Building release binary"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$ROOT/Info.plist" "$APP/Contents/Info.plist"
cp "$ROOT/target/release/$BIN_NAME" "$APP/Contents/MacOS/$BIN_NAME"
chmod +x "$APP/Contents/MacOS/$BIN_NAME"

# Optional icon (drop an AppIcon.icns into Resources and uncomment in Info.plist).

echo "==> Registering with Launch Services"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister"
"$LSREGISTER" -f "$APP"

echo "==> Done: $APP"
echo "    Open a file:  open -a \"$APP\" /path/to/photo.jpg"
echo "    Or in Finder: right-click an image -> Open With -> LightPhotos"
