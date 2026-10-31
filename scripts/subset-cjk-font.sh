#!/usr/bin/env bash
# Regenerate the two UI font subsets in assets/fonts/ from the text in
# src/i18n.rs and src/i18n/*.rs:
#   NotoSansSC-ui-subset.otf  Han, kana, CJK punctuation and full-width forms
#                             (Simplified and Traditional Chinese, Japanese)
#   NotoSansKR-ui-subset.otf  Hangul (Korean)
# Rerun after adding or changing CJK text; `cargo test i18n::` fails until you
# do. Noto Sans SC has no Hangul, hence the second font.
#
# Needs fontTools (`pip3 install fonttools`) and network access to GitHub.
set -euo pipefail

cd "$(dirname "$0")/.."

BASE="https://github.com/notofonts/noto-cjk/raw/Sans2.004/Sans/SubsetOTF"
SC_SHA256="faa6c9df652116dde789d351359f3d7e5d2285a2b2a1f04a2d7244df706d5ea9"
KR_SHA256="69975a0ac8472717870aefeab0a4d52739308d90856b9955313b2ad5e0148d68"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Same ranges as `i18n::tests::is_han_or_kana` and `is_hangul`.
python3 - "$tmp" <<'PY'
import glob, sys
text = "".join(open(p, encoding="utf-8").read() for p in ["src/i18n.rs", *sorted(glob.glob("src/i18n/*.rs"))])
def hangul(c):
    o = ord(c)
    return 0x1100 <= o <= 0x11FF or 0xAC00 <= o <= 0xD7AF
def han_or_kana(c):
    o = ord(c)
    return (0x2E80 <= o <= 0x9FFF or 0xFF00 <= o <= 0xFFEF) and not hangul(c)
for name, keep in [("sc", han_or_kana), ("kr", hangul)]:
    chars = sorted({c for c in text if keep(c)})
    open(f"{sys.argv[1]}/{name}.txt", "w", encoding="utf-8").write("".join(chars))
    print(f"{name}: {len(chars)} characters", file=sys.stderr)
PY

subset() { # <SC|KR> <sha256> <chars file> <out>
  curl -sSLf -o "$tmp/$1.otf" "$BASE/$1/NotoSans$1-Regular.otf"
  echo "$2  $tmp/$1.otf" | shasum -a 256 -c - >/dev/null
  python3 -m fontTools.subset "$tmp/$1.otf" \
    --text-file="$3" \
    --layout-features='*' \
    --no-hinting \
    --name-IDs='*' \
    --output-file="$4"
  ls -l "$4"
}

mkdir -p assets/fonts
subset SC "$SC_SHA256" "$tmp/sc.txt" assets/fonts/NotoSansSC-ui-subset.otf
subset KR "$KR_SHA256" "$tmp/kr.txt" assets/fonts/NotoSansKR-ui-subset.otf
