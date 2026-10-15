#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Ship everything: build the web app into the lightphotos.app site repo
# (deploy-web.sh), cut a desktop release (release.sh), then commit the
# site's public/ changes. Does not push the site repo.
#
#   ./scripts/ship.sh           # next patch version
#   ./scripts/ship.sh v0.2.0    # an explicit version, passed to release.sh

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SITE_DIR="${LIGHTPHOTOS_SITE_DIR:-$ROOT/../lightphotos-app}"

"$ROOT/scripts/deploy-web.sh"
"$ROOT/scripts/release.sh" "$@"

version="$(git -C "$ROOT" tag --points-at HEAD 'v*' | sort -V | tail -1)"

echo "==> Committing public/ in $SITE_DIR"
cd "$SITE_DIR"
git add -A -- public ':!**/.DS_Store'
if git diff --cached --quiet -- public; then
  echo "==> No changes under public/ to commit"
else
  git commit -m "Release ${version:-web app}" -- public
  echo "==> Committed. Push with: cd $SITE_DIR && git push"
fi
