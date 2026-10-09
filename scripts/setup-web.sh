#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# One-time setup for the web build. Run it once per clone, and again when the
# vendored rawler or the pinned nightly in scripts/web-env.sh changes;
# scripts/deploy-web.sh tells you when. Safe to re-run: each step is a no-op
# once done.
#
# Runs the desktop setup, then installs the pinned nightly with rust-src and
# the wasm target. trunk itself is installed once per machine, not here.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
source "$ROOT/scripts/web-env.sh"

if ! command -v trunk >/dev/null 2>&1; then
  echo "error: trunk not found; install it once per machine:" >&2
  echo "       brew install trunk   # or: cargo install --locked trunk" >&2
  exit 1
fi

"$ROOT/scripts/setup.sh"

echo "==> Ensuring $WEB_TOOLCHAIN with rust-src, clippy and wasm32-unknown-unknown"
rustup toolchain install "$WEB_TOOLCHAIN" --profile minimal \
  --component rust-src --component clippy --target wasm32-unknown-unknown
