#!/bin/sh
# Materializes vendor/rawler-0.7.2/ — the patched RAW-decode crate that
# [patch.crates-io] in the root Cargo.toml redirects every use of `rawler`
# to. vendor/ is NOT committed (see .gitignore), so this must be run once
# per fresh clone, before any cargo/trunk command. Safe to re-run: an
# already-set-up tree is left alone and the script exits 0.
#
# What it does: fetches plain rawler 0.7.2 from crates.io (no third-party
# fork) and applies patches/rawler-web-time.patch, which swaps
# std::time::Instant for web_time::Instant at the 4 call sites
# (imgop/sensor/bayer/ppg.rs, decompressors/crx/decoder.rs, dng/writer.rs)
# that otherwise panic unconditionally on bare wasm32-unknown-unknown,
# which has no clock source. Hit for real the first time
# DemosaicMode::Quality (PPGDemosaic) ran inside an actual wasm32 Web
# Worker: "panicked at .../unsupported.rs: time not implemented on this
# platform". See Cargo.toml's own comment on the [patch.crates-io] entry.
# Then it applies patches/rawler-ljpeg-restart.patch, which teaches the
# lossless JPEG decoder restart markers (Samsung Galaxy Linear DNGs).
#
# Needed for ANY cargo invocation that touches this crate, wasm32 or not —
# [patch.crates-io] redirects every use of `rawler` here, globally (Cargo
# has no way to scope a patch to one target). The patch is a no-op on
# native (web_time is a real passthrough to std::time::Instant there), so
# running this everywhere is harmless — just a one-time setup step.
#
# Usage:
#   scripts/setup-vendor-rawler.sh           ensure the tree exists (no-op if it does)
#   scripts/setup-vendor-rawler.sh --force   wipe and regenerate from crates.io
#
# Bumping the pinned version is a deliberate edit here + a re-check that
# every patch in PATCHES still applies. Adding a patch changes the sentinel,
# so trees set up before it are regenerated on the next run.
set -eu

CRATE_VERSION="0.7.2"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$HERE/vendor/rawler-$CRATE_VERSION"
# Written only after the patch applies cleanly; its presence is what marks
# the tree as fully set up (a half-materialized dir has no sentinel).
SENTINEL="$DEST/.lightphotos-patched"
PATCHES="rawler-web-time.patch rawler-ljpeg-restart.patch"
STAMP="rawler $CRATE_VERSION + $PATCHES"

FORCE=0
[ "${1:-}" = "--force" ] && FORCE=1

if [ -d "$DEST" ]; then
  if [ "$FORCE" -eq 1 ]; then
    echo "Removing existing $DEST (--force)..."
    chmod -R u+w "$DEST" 2>/dev/null || true
    rm -rf "$DEST"
  elif [ -f "$SENTINEL" ] && [ "$(cat "$SENTINEL")" = "$STAMP" ]; then
    echo "vendor/rawler-$CRATE_VERSION already set up ($STAMP) — nothing to do."
    exit 0
  elif [ -f "$SENTINEL" ]; then
    echo "vendor/rawler-$CRATE_VERSION was set up with other patches ($(cat "$SENTINEL")) — regenerating."
    chmod -R u+w "$DEST" 2>/dev/null || true
    rm -rf "$DEST"
  else
    echo "vendor/rawler-$CRATE_VERSION exists but looks incomplete (no patch sentinel)." >&2
    echo "Re-run with --force to wipe and regenerate it." >&2
    exit 1
  fi
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# crates.io requires a User-Agent identifying the client.
curl -sSL -A "lightphotos-wasm-build (local dev/CI, not a real client)" \
  "https://crates.io/api/v1/crates/rawler/$CRATE_VERSION/download" \
  -o "$TMP/rawler.crate"

mkdir -p "$HERE/vendor"
tar -xzf "$TMP/rawler.crate" -C "$TMP"
mv "$TMP/rawler-$CRATE_VERSION" "$DEST"
chmod -R u+w "$DEST"

for p in $PATCHES; do
  echo "Applying $p..."
  # -d vendor: the patch's paths are "a/rawler-upstream/..." and
  # "b/rawler-0.7.2/...", -p1 strips the first component, so the remainder
  # needs to resolve relative to vendor/, not to this script's own directory.
  patch -p1 -d "$HERE/vendor" < "$HERE/patches/$p"
done

echo "$STAMP" > "$SENTINEL"

echo "Done — vendor/rawler-$CRATE_VERSION ready with $PATCHES applied."
