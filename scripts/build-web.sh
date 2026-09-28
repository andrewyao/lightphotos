#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Build the wasm (browser) app into dist/ via trunk, doing every per-clone
# setup step first: the pinned nightly with rust-src and the wasm target, the
# vendored rawler tree and the full Chinese font. Each step is a no-op once
# done, so this is safe to re-run. Extra arguments go to `trunk build`.
#
# The toolchain and flags come from scripts/web-env.sh, which says why the
# browser build needs a nightly. Always --release: a debug wasm build is
# 10-30x slower at RAW decode.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"
source "$ROOT/scripts/web-env.sh"

if ! command -v trunk >/dev/null 2>&1; then
  echo "error: trunk not found; install it once per machine:" >&2
  echo "       brew install trunk   # or: cargo install --locked trunk" >&2
  exit 1
fi

echo "==> Ensuring $WEB_TOOLCHAIN with rust-src and wasm32-unknown-unknown"
rustup toolchain install "$WEB_TOOLCHAIN" --profile minimal \
  --component rust-src --target wasm32-unknown-unknown

echo "==> Ensuring vendored rawler is present"
"$ROOT/scripts/setup-vendor-rawler.sh"

echo "==> Ensuring the full Chinese font is present"
"$ROOT/scripts/fetch-cjk-font.sh"

echo "==> Building (trunk build --release)"
trunk build --release --config "$ROOT/Trunk.toml" "$@"
