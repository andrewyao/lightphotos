#!/bin/sh
# Materializes vendor/lightwatch/ — the three lightwatch crates the census
# instrumentation is built against (lightwatch-proto, lightwatch-probe and
# lightwatch-probe-macros). vendor/ is NOT committed (see .gitignore), so this
# must be run once per fresh clone, before any cargo command, exactly like
# scripts/setup-vendor-rawler.sh.
#
# Why a copy and not a dependency. lightwatch is not published and has no git
# remote, so the only thing to point at is a checkout on this machine. Cargo
# resolves every path dependency whether or not the feature that uses it is
# on — an optional, default-off `path = "../lightwatch"` was measured failing
# the same way — so a sibling path would red every build, and every CI job, on
# any machine that does not happen to have lightwatch beside this repo.
#
# What it rewrites. The three crates inherit version, edition, license,
# repository and their lightwatch-proto/serde/serde_json dependencies from
# lightwatch's own workspace manifest, which does not come along in the copy.
# Each inherited key is replaced below with the value that manifest gives it.
# Those values are duplicated here: bumping one in lightwatch means editing
# this script too, and a mismatch shows up as a cargo error, not as wrong code.
#
# Unlike the rawler script, this one always re-copies instead of no-opping on
# an existing tree. The source is a working checkout that moves rather than a
# pinned crates.io tarball, so a stale copy is the failure worth preventing.
#
# IT NEEDS A CHECKOUT TO COPY FROM, and there is nothing to fetch one from yet.
# A CI runner that checks out lightphotos alone cannot satisfy this until
# lightwatch is published somewhere, and will fail here with the message below.
#
# Usage:
#   scripts/setup-vendor-lightwatch.sh              copy from ../lightwatch
#   scripts/setup-vendor-lightwatch.sh <checkout>   copy from there
#   LIGHTWATCH_SRC=<checkout> scripts/setup-vendor-lightwatch.sh
set -eu

HERE="$(cd "$(dirname "$0")/.." && pwd)"
SRC="${1:-${LIGHTWATCH_SRC:-$HERE/../lightwatch}}"
DEST="$HERE/vendor/lightwatch"
CRATES="lightwatch-proto lightwatch-probe lightwatch-probe-macros"

for crate in $CRATES; do
  if [ ! -f "$SRC/crates/$crate/Cargo.toml" ]; then
    echo "No lightwatch checkout at $SRC (looked for crates/$crate/Cargo.toml)." >&2
    echo "Pass a checkout as an argument, or set LIGHTWATCH_SRC to one." >&2
    exit 1
  fi
done

rm -rf "$DEST"
mkdir -p "$DEST"

for crate in $CRATES; do
  cp -R "$SRC/crates/$crate" "$DEST/$crate"
  chmod -R u+w "$DEST/$crate"
  rm -rf "$DEST/$crate/target"
  # -i.bak, then delete the backup: the bare -i spelling differs between BSD
  # and GNU sed and this runs on macOS, Linux and git-bash alike.
  sed -i.bak \
    -e 's|^version\.workspace = true$|version = "0.1.0"|' \
    -e 's|^edition\.workspace = true$|edition = "2021"|' \
    -e 's|^license\.workspace = true$|license = "MIT"|' \
    -e 's|^repository\.workspace = true$|repository = "https://github.com/andrewyao/lightwatch"|' \
    -e 's|^lightwatch-proto = { workspace = true }$|lightwatch-proto = { version = "0.1.0", path = "../lightwatch-proto" }|' \
    -e 's|^serde = { workspace = true }$|serde = { version = "1", features = ["derive"] }|' \
    -e 's|^serde_json = { workspace = true }$|serde_json = "1"|' \
    "$DEST/$crate/Cargo.toml"
  rm -f "$DEST/$crate/Cargo.toml.bak"
  if grep -q "workspace = true" "$DEST/$crate/Cargo.toml"; then
    echo "$crate still inherits a key from lightwatch's workspace:" >&2
    grep -n "workspace = true" "$DEST/$crate/Cargo.toml" >&2
    echo "Teach the sed block above what that key resolves to." >&2
    exit 1
  fi
done

FROM="$(cd "$SRC" && git rev-parse --short HEAD 2>/dev/null || echo "not a git checkout")"
echo "$SRC @ $FROM" > "$DEST/.lightphotos-vendored"

echo "Done — vendor/lightwatch ready, copied from $SRC @ $FROM."
