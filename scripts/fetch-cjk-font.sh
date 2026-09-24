#!/usr/bin/env bash
# Download the full Noto Sans SC to assets/fonts/NotoSansSC-Regular.otf. The
# web build serves it beside the app's wasm and fetches it once a folder has a
# Chinese name the bundled UI subset can't draw. It is 8 MB, so it stays out
# of git. Safe to re-run: a no-op once the file is present and verified.
#
# Needs network access to GitHub on first run.
set -euo pipefail

cd "$(dirname "$0")/.."

URL="https://github.com/notofonts/noto-cjk/raw/Sans2.004/Sans/SubsetOTF/SC/NotoSansSC-Regular.otf"
SHA256="faa6c9df652116dde789d351359f3d7e5d2285a2b2a1f04a2d7244df706d5ea9"
OUT="assets/fonts/NotoSansSC-Regular.otf"

if [[ -f "$OUT" ]] && echo "$SHA256  $OUT" | shasum -a 256 -c - >/dev/null 2>&1; then
  exit 0
fi

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
curl -sSLf -o "$tmp" "$URL"
echo "$SHA256  $tmp" | shasum -a 256 -c - >/dev/null
mkdir -p "$(dirname "$OUT")"
mv "$tmp" "$OUT"
chmod 644 "$OUT"
trap - EXIT
ls -l "$OUT"
