#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Bring up a live lightwatch session over lightphotos: the daemon (which is
# also the web UI's server), the hotpath bridge, and lightphotos itself built
# with the profiling features on. Stop it all again with lightwatch-down.sh.
#
# Three processes, because lightphotos emits only half the picture by itself:
#   lightwatch            ingest socket + HTTP API + the UI at :7700
#   lightwatch-hotpath    polls lightphotos' hotpath server on :6770 and
#                         re-emits function calls and timings as protocol frames
#   lightphotos           emits the live-object census through lightwatch-probe
# The daemon joins the two emitters back into one session per program.
#
# Any arguments are passed straight to lightphotos, so a folder opens in Grid
# and `--profile <folder>` runs the headless profiling pass instead of a window.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LIGHTWATCH_REPO="${LIGHTWATCH_REPO:-$ROOT/../lightwatch}"
TMP="${TMPDIR:-/tmp}"
RUN_DIR="${LIGHTPHOTOS_LIGHTWATCH_RUN_DIR:-${TMP%/}/lightphotos-lightwatch}"

# Pinned rather than inherited: the daemon, the bridge and the probe each
# resolve this directory themselves, and a disagreement shows up as an empty
# UI rather than an error.
export LIGHTWATCH_SOCK_DIR="$RUN_DIR/sock"
export LIGHTWATCH_PORT="${LIGHTWATCH_PORT:-7700}"

if [[ ! -f "$LIGHTWATCH_REPO/crates/lightwatch-daemon/Cargo.toml" ]]; then
  echo "error: no lightwatch checkout at $LIGHTWATCH_REPO" >&2
  echo "       set LIGHTWATCH_REPO to it" >&2
  exit 1
fi

mkdir -p "$RUN_DIR" "$LIGHTWATCH_SOCK_DIR"

running() {
  local pidfile="$1"
  [[ -f "$pidfile" ]] && kill -0 "$(cat "$pidfile")" 2>/dev/null
}

# Starting an already-running service is a no-op, so this script is safe to
# re-run to pick up whichever half died.
start_one() {
  local name="$1"
  shift
  local pidfile="$RUN_DIR/$name.pid"
  local log="$RUN_DIR/$name.log"
  if running "$pidfile"; then
    echo "==> $name already running (pid $(cat "$pidfile"))"
    return
  fi
  # Truncated, not appended: the log belongs to the run this call starts, and
  # a failure tail is unreadable behind every earlier session.
  "$@" >"$log" 2>&1 &
  echo $! >"$pidfile"
  echo "==> Started $name (pid $!), logging to $log"
}

echo "==> Ensuring vendored rawler is present"
"$ROOT/scripts/setup-vendor-rawler.sh"

# Shares target/release/lightphotos with a plain release build, so switching
# between this and `cargo build --release` relinks the binary each time.
echo "==> Building lightphotos with hotpath + lightwatch"
cargo build --release --manifest-path "$ROOT/Cargo.toml" --features hotpath,lightwatch

echo "==> Building the lightwatch daemon and hotpath bridge"
cargo build --release --manifest-path "$LIGHTWATCH_REPO/Cargo.toml" \
  -p lightwatch-daemon -p lightwatch-hotpath

start_one 1-daemon "$LIGHTWATCH_REPO/target/release/lightwatch"

echo "==> Waiting for http://127.0.0.1:$LIGHTWATCH_PORT"
for _ in $(seq 1 100); do
  if curl -sf "http://127.0.0.1:$LIGHTWATCH_PORT/api/processes" >/dev/null; then
    break
  fi
  sleep 0.1
done
if ! curl -sf "http://127.0.0.1:$LIGHTWATCH_PORT/api/processes" >/dev/null; then
  echo "error: the daemon never answered on port $LIGHTWATCH_PORT" >&2
  tail -n 20 "$RUN_DIR/1-daemon.log" >&2 || true
  exit 1
fi

# Starts before the app on purpose: it backs off and waits for a target rather
# than exiting, so it is already streaming the moment lightphotos comes up.
start_one 2-hotpath-bridge "$LIGHTWATCH_REPO/target/release/lightwatch-hotpath"

start_one 3-lightphotos "$ROOT/target/release/lightphotos" "$@"

# A backgrounded process that dies immediately still leaves a pidfile, so the
# only honest report is a second look at what is actually alive.
sleep 1
failed=0
for pidfile in "$RUN_DIR"/*.pid; do
  name="$(basename "$pidfile" .pid)"
  if ! running "$pidfile"; then
    echo "error: $name exited on startup" >&2
    tail -n 20 "$RUN_DIR/$name.log" >&2 || true
    failed=1
  fi
done
[[ $failed -eq 0 ]] || exit 1

if command -v open >/dev/null; then
  open "http://127.0.0.1:$LIGHTWATCH_PORT"
fi

echo
echo "Live view:  http://127.0.0.1:$LIGHTWATCH_PORT"
echo "Logs:       $RUN_DIR"
echo "Stop it:    $ROOT/scripts/lightwatch-down.sh"
