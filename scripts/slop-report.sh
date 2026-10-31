#!/usr/bin/env bash
# Review lists for trimming comment, helper and test bulk. Prints counts, then the
# worst offenders; pass --all to print every hit. Read-only.
set -euo pipefail
cd "$(dirname "$0")/.."

all=0
[ "${1:-}" = "--all" ] && all=1
files=$(git ls-files 'src/*.rs' 'src/**/*.rs')

show() {
  if [ "$all" = 1 ]; then cat; else head -n "${1:-15}"; fi
}

echo "== 1. Long doc blocks: //! >= 10 lines, /// >= 6 lines (len file:line) =="
# shellcheck disable=SC2086
awk '
  function flush() { if (n >= min) printf "%d %s:%d\n", n, file, start; n = 0 }
  FNR == 1 { flush(); file = FILENAME }
  /^[ \t]*\/\/!/ { if (kind != "i") flush(); kind = "i"; min = 10; if (!n) start = FNR; n++; next }
  /^[ \t]*\/\/\/[^\/]/ || /^[ \t]*\/\/\/$/ { if (kind != "o") flush(); kind = "o"; min = 6; if (!n) start = FNR; n++; next }
  { flush(); kind = "" }
  END { flush() }
' $files | sort -rn > "${TMPDIR:-/tmp}/slop-long.txt"
wc -l < "${TMPDIR:-/tmp}/slop-long.txt" | xargs echo "count:"
show 15 < "${TMPDIR:-/tmp}/slop-long.txt"

echo
echo "== 2. One-line /// of <= 8 words above a field or enum variant =="
# shellcheck disable=SC2086
awk '
  FNR == 1 { prev = ""; pprev = "" }
  {
    if (prev ~ /^[ \t]+\/\/\/ / && pprev !~ /^[ \t]*\/\// && $0 ~ /^[ \t]+(pub(\([a-z]+\))? )?[A-Za-z_][A-Za-z0-9_]*(:.*|\(.*\))?[,{]?[ \t]*$/ && $0 !~ /fn |let |=>|return/) {
      t = prev; sub(/^[ \t]+\/\/\/ /, "", t)
      if (split(t, w, /[ \t]+/) <= 8) printf "%s:%d: %s\n", FILENAME, FNR - 1, t
    }
    pprev = prev; prev = $0
  }
' $files > "${TMPDIR:-/tmp}/slop-short.txt"
wc -l < "${TMPDIR:-/tmp}/slop-short.txt" | xargs echo "count:"
show 15 < "${TMPDIR:-/tmp}/slop-short.txt"

echo
echo "== 3. Comments naming callers or history =="
# shellcheck disable=SC2086
grep -nE '^\s*//[/!]?.*(`[A-Z][A-Za-z0-9]*::[a-z_]+`|the way |mirrors |called by|[Uu]sed by|for now|hidden for now|[Oo]nly wasm32|legacy)' $files > "${TMPDIR:-/tmp}/slop-refs.txt" || true
wc -l < "${TMPDIR:-/tmp}/slop-refs.txt" | xargs echo "count:"
show 15 < "${TMPDIR:-/tmp}/slop-refs.txt"

echo
echo "== 4. Fn names defined in more than one file (candidates to unify) =="
# shellcheck disable=SC2086
for f in $files; do
  { grep -oE '^\s*(pub(\([a-z]+\))? )?fn [a-z_0-9]+' "$f" || true; } | sed -E 's/.*fn //' | sort -u | sed "s|\$| $f|"
done | awk '
  $1 ~ /^(new|default|fmt|from|into|drop|clone|eq|hash|cmp|partial_cmp|next|deref|build|run|update|show|ui|name|id|get|set|len|is_empty|main|tests?|setup|fixture|image|key)$/ { next }
  { c[$1]++; w[$1] = w[$1] " " $2 }
  END { for (n in c) if (c[n] > 1 && w[n] !~ /\/tests?\.rs/) printf "%d %s:%s\n", c[n], n, w[n] }
' | sort -rn > "${TMPDIR:-/tmp}/slop-dups.txt"
wc -l < "${TMPDIR:-/tmp}/slop-dups.txt" | xargs echo "count:"
show 20 < "${TMPDIR:-/tmp}/slop-dups.txt"

echo
echo "== 5. Test families: >= 3 tests sharing a two-word name prefix in one file =="
# shellcheck disable=SC2086
for f in $files; do
  { grep -A1 -E '^\s*#\[test\]' "$f" | grep -oE 'fn [a-z_0-9]+' || true; } | sed -E 's/fn //' |
    awk -F_ -v f="$f" '{ k = $1 "_" $2; c[k]++ } END { for (k in c) if (c[k] >= 3) printf "%d %s %s_*\n", c[k], f, k }'
done | sort -rn > "${TMPDIR:-/tmp}/slop-tests.txt"
wc -l < "${TMPDIR:-/tmp}/slop-tests.txt" | xargs echo "count:"
show 20 < "${TMPDIR:-/tmp}/slop-tests.txt"
