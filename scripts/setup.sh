#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# One-time setup for the desktop build. Run it once per clone, and again when
# the vendored rawler changes (a new version or patch); scripts/build.sh tells
# you when. Safe to re-run: each step is a no-op once done.
#
# Today that is just the patched rawler tree under vendor/, which every cargo
# command needs (see scripts/setup-vendor-rawler.sh). Extra arguments, such as
# --force, go to that script.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

echo "==> Ensuring vendored rawler is present"
"$ROOT/scripts/setup-vendor-rawler.sh" "$@"
