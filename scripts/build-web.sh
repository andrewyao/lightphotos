#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Build the wasm (browser) app into dist/ via trunk, doing every per-clone
# setup step first: the wasm target, the vendored rawler tree and the full
# Chinese font. Each step is a no-op once done, so this is safe to re-run.
# Extra arguments go to `trunk build`.
#
# RUSTFLAGS is set here rather than in Trunk.toml because trunk 0.21.14
# doesn't pass that file's `rustflags` key through to cargo, and without the
# cfg flag the build leaves out the WebGPU and File System Access bindings.
# Always --release: a debug wasm build is 10-30x slower at RAW decode.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if ! command -v trunk >/dev/null 2>&1; then
  echo "error: trunk not found; install it once per machine:" >&2
  echo "       brew install trunk   # or: cargo install --locked trunk" >&2
  exit 1
fi

# Targets belong to a toolchain, so add it from the repo root, where
# rust-toolchain.toml picks the toolchain the build will use.
echo "==> Ensuring the wasm32-unknown-unknown target is installed"
rustup target add wasm32-unknown-unknown

echo "==> Ensuring vendored rawler is present"
"$ROOT/scripts/setup-vendor-rawler.sh"

echo "==> Ensuring the full Chinese font is present"
"$ROOT/scripts/fetch-cjk-font.sh"

echo "==> Building (trunk build --release)"
RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config "$ROOT/Trunk.toml" "$@"
