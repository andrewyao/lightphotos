#!/usr/bin/env bash
# Regenerate assets/fonts/NotoSansSC-ui-subset.otf: Noto Sans SC cut down to
# the CJK characters in src/i18n.rs. Rerun after adding or changing Chinese
# text; `cargo test i18n::` fails until you do.
#
# Needs fontTools (`pip3 install fonttools`) and network access to GitHub.
set -euo pipefail

cd "$(dirname "$0")/.."

URL="https://github.com/notofonts/noto-cjk/raw/Sans2.004/Sans/SubsetOTF/SC/NotoSansSC-Regular.otf"
SHA256="faa6c9df652116dde789d351359f3d7e5d2285a2b2a1f04a2d7244df706d5ea9"
OUT="assets/fonts/NotoSansSC-ui-subset.otf"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

curl -sSLf -o "$tmp/full.otf" "$URL"
echo "$SHA256  $tmp/full.otf" | shasum -a 256 -c - >/dev/null

# Same rule as `i18n::tests::is_cjk`: CJK punctuation, Han, and full-width forms.
python3 - "$tmp/chars.txt" <<'EOF'
import sys
text = open("src/i18n.rs", encoding="utf-8").read()
cjk = sorted({c for c in text if 0x2E80 <= ord(c) <= 0x9FFF or 0xFF00 <= ord(c) <= 0xFFEF})
open(sys.argv[1], "w", encoding="utf-8").write("".join(cjk))
print(f"{len(cjk)} CJK characters", file=sys.stderr)
EOF

mkdir -p "$(dirname "$OUT")"
python3 -m fontTools.subset "$tmp/full.otf" \
  --text-file="$tmp/chars.txt" \
  --layout-features='*' \
  --no-hinting \
  --name-IDs='*' \
  --output-file="$OUT"
ls -l "$OUT"
