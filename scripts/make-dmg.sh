#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Wrap LightPhotos.app (from bundle.sh) in a drag-install .dmg whose mounted
# volume shows the app icon instead of a blank disk.
#
#   ./scripts/make-dmg.sh lightphotos-macos-arm64.dmg
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:?usage: $0 <out.dmg>}"
APP="$ROOT/LightPhotos.app"
[[ -d "$APP" ]] || { echo "error: $APP not found; run scripts/bundle.sh first" >&2; exit 1; }

STAGE="$(mktemp -d)"
RW="$(mktemp -d)/rw.dmg"
MNT=""
cleanup() {
  [[ -n "$MNT" ]] && hdiutil detach -quiet "$MNT" || true
  rm -rf "$STAGE" "$(dirname "$RW")"
}
trap cleanup EXIT

cp -R "$APP" "$STAGE/"
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
