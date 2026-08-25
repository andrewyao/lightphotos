#!/bin/sh
# Regenerates the tracked vendor/rawler-0.7.2/ tree.
#
# Fetches plain rawler 0.7.2 from crates.io (no third-party fork), then
# applies patches/rawler-web-time.patch: swaps std::time::Instant for
# web_time::Instant at the 4 call sites (imgop/sensor/bayer/ppg.rs,
# decompressors/crx/decoder.rs, dng/writer.rs) that otherwise panic
# unconditionally on bare wasm32-unknown-unknown, which has no clock source.
# We hit this for real the first time `DemosaicMode::Quality` (PPGDemosaic)
# ran inside an actual wasm32 Web Worker: "panicked at .../unsupported.rs:
# time not implemented on this platform". See Cargo.toml's own comment on the
# `[target.'cfg(target_arch = "wasm32")'.dependencies]` `rawler` entry for
# more.
#
# The patch itself predates this script — it was first worked out on a
# throwaway spike branch before any of this was wired into the real app,
# where it never actually got exercised in production. It's brought over
# unmodified now that `DemosaicMode::Quality` actually runs on wasm32.
#
# Needed for ANY cargo invocation that touches this crate, wasm32 or not —
# `[patch.crates-io]` in the root Cargo.toml redirects every use of `rawler`
# here, globally (Cargo has no way to scope a patch to one target). The patch
# itself is a no-op on native (web_time is a real passthrough to
# std::time::Instant there), so this is safe — just an extra one-time setup
# step. The generated crate is committed, so this script is only needed when
# intentionally refreshing the vendored source from crates.io.
set -eu

CRATE_VERSION="0.7.2"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$HERE/vendor/rawler-$CRATE_VERSION"

if [ -d "$DEST" ]; then
  echo "vendor/rawler-$CRATE_VERSION already exists — remove it first if you want to regenerate." >&2
  exit 1
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

echo "Applying rawler-web-time.patch..."
# -d vendor: the patch's paths are "a/rawler-upstream/..." and
# "b/rawler-0.7.2/...", -p1 strips the first component, so the remainder
# needs to resolve relative to vendor/, not to this script's own directory.
patch -p1 -d "$HERE/vendor" < "$HERE/patches/rawler-web-time.patch"

echo "Done — vendor/rawler-$CRATE_VERSION ready with the wasm time fix applied."
echo "Review and commit the regenerated vendor tree if you refreshed it."
