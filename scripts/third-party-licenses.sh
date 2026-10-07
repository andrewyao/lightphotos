#!/usr/bin/env bash
# Regenerate THIRD-PARTY-LICENSES.txt, the notices file every download ships
# beside LICENSE-MIT and LICENSE-APACHE. It covers every crate compiled into
# any shipped binary (the release.yml targets plus the wasm build, per
# about.toml), then the bundled assets that are not crates: the Noto Sans SC
# font and the modified rawler.
#
#   scripts/third-party-licenses.sh           rewrite the committed file
#   scripts/third-party-licenses.sh --check   exit 1 if the committed file is stale
#
# Rerun it after any Cargo.lock change; CI runs --check. Needs cargo-about
# (cargo install --locked cargo-about --features cli) at ABOUT_VERSION, since
# another version may format the same input differently. Runs offline once
# `cargo fetch` has the sources, so the output depends only on Cargo.lock and
# the files here, never on a network lookup.
set -euo pipefail

ABOUT_VERSION="0.9.2"

CHECK=0
case "${1:-}" in
  "") ;;
  --check) CHECK=1 ;;
  *) echo "usage: $0 [--check]" >&2; exit 2 ;;
esac

cd "$(dirname "$0")/.."

have="$(cargo about --version 2>/dev/null || true)"
if [[ "$have" != "cargo-about $ABOUT_VERSION" ]]; then
  echo "error: need cargo-about $ABOUT_VERSION, found '${have:-none}'" >&2
  echo "       cargo install --locked cargo-about --version $ABOUT_VERSION --features cli" >&2
  exit 1
fi

./scripts/setup-vendor-rawler.sh >/dev/null
cargo fetch --locked --quiet

PATCHES="$(sed -n 's/^PATCHES="\(.*\)"$/\1/p' scripts/setup-vendor-rawler.sh)"
[[ -n "$PATCHES" ]] || { echo "error: no PATCHES= line in scripts/setup-vendor-rawler.sh" >&2; exit 1; }

generate() {
  cargo about generate --frozen about.hbs
  cat <<EOF
################################################################################
Bundled assets that are not crates
################################################################################

Noto Sans SC font. The app embeds a subset of it for its interface
(assets/fonts/NotoSansSC-ui-subset.otf), and the web build serves the full
font. Licensed under the SIL Open Font License 1.1:

EOF
  cat assets/fonts/NotoSansSC-OFL.txt
  cat <<EOF

--------------------------------------------------------------------------------

rawler 0.7.2 (LGPL-2.1, full text above). LightPhotos ships a modified copy.
scripts/setup-vendor-rawler.sh fetches rawler 0.7.2 from crates.io and applies
these patches from the patches/ directory:

EOF
  for p in $PATCHES; do echo "  patches/$p"; done
  cat <<EOF

The modified source, the patches and the script are available at
https://github.com/andrewyao/lightphotos.
EOF
}

OUT="THIRD-PARTY-LICENSES.txt"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
generate >"$tmp"
if [[ "$CHECK" == 1 ]]; then
  if ! cmp -s "$tmp" "$OUT"; then
    echo "error: $OUT is stale. Run scripts/third-party-licenses.sh and commit the result." >&2
    diff -u "$OUT" "$tmp" | head -40 >&2 || true
    exit 1
  fi
  echo "$OUT is up to date."
else
  mv "$tmp" "$OUT"
  chmod 644 "$OUT"
  echo "Wrote $OUT."
fi
