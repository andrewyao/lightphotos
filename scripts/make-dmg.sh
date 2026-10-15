#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Build the release binary for one macOS target, assemble and sign
# LightPhotos.app around it, and wrap that in a drag-install .dmg whose mounted
# volume shows the app icon instead of a blank disk. release.yml runs it once
# per target.
#
#   ./scripts/make-dmg.sh aarch64-apple-darwin lightphotos-macos-arm64.dmg
#   ./scripts/make-dmg.sh x86_64-apple-darwin  lightphotos-macos-x64.dmg
#
# With MACOS_SIGN_IDENTITY set (e.g. "Developer ID Application: Name (TEAMID)",
# from a keychain that holds it), the app and .dmg are Developer ID signed, then
# the .dmg is notarized and stapled, so Gatekeeper opens a downloaded copy with
# no prompt. Notarizing needs an App Store Connect API key: APPLE_API_KEY_PATH
# (the .p8 file), APPLE_API_KEY_ID and APPLE_API_ISSUER. Without an identity the
# app is ad-hoc signed and users must clear the quarantine flag themselves.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${1:?usage: $0 <target-triple> <out.dmg>}"
OUT="${2:?usage: $0 <target-triple> <out.dmg>}"
BIN_NAME="lightphotos"
IDENTITY="${MACOS_SIGN_IDENTITY:-}"

if [[ -n "$IDENTITY" ]]; then
  for var in APPLE_API_KEY_PATH APPLE_API_KEY_ID APPLE_API_ISSUER; do
    if [[ -z "${!var:-}" ]]; then
      echo "error: MACOS_SIGN_IDENTITY is set but $var is not; a signed app that isn't notarized still gets blocked" >&2
      exit 1
    fi
  done
fi

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
cp "$ROOT/LICENSE-MIT" "$ROOT/LICENSE-APACHE" "$ROOT/THIRD-PARTY-LICENSES.txt" "$APP/Contents/Resources/"

# The linker signs only the binary, which leaves the bundle's signature invalid
# and makes Gatekeeper call a downloaded copy "damaged". Notarization requires
# the hardened runtime and a secure timestamp. The ad-hoc fallback downgrades
# "damaged" to an "Open Anyway" prompt.
if [[ -n "$IDENTITY" ]]; then
  echo "==> Signing LightPhotos.app as $IDENTITY"
  codesign --force --options runtime --timestamp --sign "$IDENTITY" "$APP"
else
  echo "==> Ad-hoc signing LightPhotos.app (MACOS_SIGN_IDENTITY unset)"
  codesign --force --sign - "$APP"
fi
codesign --verify --strict --verbose=2 "$APP"

ln -s /Applications "$STAGE/Applications"
cp "$ROOT/assets/icon/AppIcon.icns" "$STAGE/.VolumeIcon.icns"

hdiutil create -quiet -volname LightPhotos -srcfolder "$STAGE" -fs HFS+ -format UDRW -ov "$RW"
MNT="$(hdiutil attach -nobrowse -noautoopen "$RW" | awk -F'\t' '/\/Volumes\// {print $NF}')"
# Finder reads .VolumeIcon.icns only when the volume root carries the custom-icon flag.
SetFile -a C "$MNT"
hdiutil detach -quiet "$MNT"
MNT=""

hdiutil convert -quiet "$RW" -format UDZO -ov -o "$OUT"

# Apple notarizes the .dmg and the app inside it in one submission; stapling
# the ticket to the .dmg lets Gatekeeper check it offline.
if [[ -n "$IDENTITY" ]]; then
  echo "==> Signing and notarizing $OUT"
  codesign --force --timestamp --sign "$IDENTITY" "$OUT"
  result="$(xcrun notarytool submit "$OUT" --wait --output-format json \
    --key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER")"
  status="$(plutil -extract status raw -o - - <<<"$result")"
  if [[ "$status" != "Accepted" ]]; then
    id="$(plutil -extract id raw -o - - <<<"$result")"
    echo "error: notarization finished as '$status'; Apple's log follows" >&2
    xcrun notarytool log "$id" \
      --key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY_ID" --issuer "$APPLE_API_ISSUER" >&2
    exit 1
  fi
  xcrun stapler staple "$OUT"
  spctl --assess --type open --context context:primary-signature --verbose=2 "$OUT"
fi
echo "==> Wrote $OUT"
