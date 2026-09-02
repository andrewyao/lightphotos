#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Build the wasm app via trunk and sync the output into the lightphotos.app
# site repo's public/ (its Astro build copies public/ straight into dist/),
# automating the manual steps documented in that repo's public/app.html
# comment: build, copy the four asset files into public/app/ (deleting
# stale hashed ones), update the two hashed URLs in public/app.html. Does
# NOT commit or push in the site repo — review the diff there and do that
# yourself.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SITE_DIR="${LIGHTPHOTOS_SITE_DIR:-$ROOT/../lightphotos.app}"
SITE_PUBLIC="$SITE_DIR/public"

if [[ ! -f "$SITE_PUBLIC/app.html" ]]; then
  echo "error: no public/app.html found in $SITE_DIR" >&2
  echo "       set LIGHTPHOTOS_SITE_DIR to the lightphotos.app checkout if it's elsewhere" >&2
  exit 1
fi

echo "==> Ensuring vendored rawler is present"
"$ROOT/scripts/setup-vendor-rawler.sh"

echo "==> Building (trunk build --release)"
RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config "$ROOT/Trunk.toml"

DIST="$ROOT/dist"
NEW_JS="$(find "$DIST" -maxdepth 1 -name 'lightphotos-*.js' -not -name '*_bg.wasm' | head -1)"
if [[ -z "$NEW_JS" ]]; then
  echo "error: no lightphotos-*.js found in $DIST after build" >&2
  exit 1
fi
NEW_HASH="$(basename "$NEW_JS" .js | sed 's/^lightphotos-//')"
NEW_WASM="$DIST/lightphotos-${NEW_HASH}_bg.wasm"
[[ -f "$NEW_WASM" ]] || { echo "error: expected $NEW_WASM, not found" >&2; exit 1; }

OLD_HASH="$(grep -o 'lightphotos-[0-9a-f]*' "$SITE_PUBLIC/app.html" | head -1 | sed 's/^lightphotos-//')"
echo "==> Old hash: $OLD_HASH"
echo "==> New hash: $NEW_HASH"

mkdir -p "$SITE_PUBLIC/app"

echo "==> Removing stale hashed files from $SITE_PUBLIC/app"
rm -f "$SITE_PUBLIC"/app/lightphotos-*.js "$SITE_PUBLIC"/app/lightphotos-*_bg.wasm

echo "==> Copying build output into $SITE_PUBLIC/app"
cp "$NEW_JS" "$NEW_WASM" "$SITE_PUBLIC/app/"
cp "$DIST/wasm_worker.js" "$DIST/wasm_worker_bg.wasm" "$SITE_PUBLIC/app/"

if [[ -n "$OLD_HASH" && "$OLD_HASH" != "$NEW_HASH" ]]; then
  echo "==> Updating hashed URLs in public/app.html"
  sed -i '' "s/lightphotos-${OLD_HASH}/lightphotos-${NEW_HASH}/g" "$SITE_PUBLIC/app.html"
fi

echo "==> Done. Review the diff in $SITE_DIR and commit/push there yourself:"
echo "    cd $SITE_DIR && git status"
