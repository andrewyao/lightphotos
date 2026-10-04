#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Build the desktop app. Run scripts/setup.sh first.
#
#   ./scripts/build.sh debug     # target/debug/lightphotos
#   ./scripts/build.sh release   # target/release/lightphotos
#
# Extra arguments go to `cargo build`.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

mode="${1:-}"
case "$mode" in
  debug) profile=() ;;
  release) profile=(--release) ;;
  *)
    echo "usage: $0 debug|release [cargo build args...]" >&2
    exit 1
    ;;
esac
shift

if ! "$ROOT/scripts/setup-vendor-rawler.sh" --check; then
  echo "error: vendored rawler is missing or out of date; run ./scripts/setup.sh" >&2
  exit 1
fi

cargo build ${profile[@]+"${profile[@]}"} "$@"
