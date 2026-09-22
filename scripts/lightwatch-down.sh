#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0

# Stop everything lightwatch-up.sh started: lightphotos, the hotpath bridge and
# the lightwatch daemon (which is also the UI's web server), then clear the
# ingest socket. Only processes recorded in the run directory are touched, so a
# lightwatch or lightphotos you started by hand survives this.

set -euo pipefail

TMP="${TMPDIR:-/tmp}"
RUN_DIR="${LIGHTPHOTOS_LIGHTWATCH_RUN_DIR:-${TMP%/}/lightphotos-lightwatch}"
SOCK_DIR="${LIGHTWATCH_SOCK_DIR:-$RUN_DIR/sock}"
PORT="${LIGHTWATCH_PORT:-7700}"

stop_one() {
  local pidfile="$1"
  local name pid
  name="$(basename "$pidfile" .pid)"
  pid="$(cat "$pidfile" 2>/dev/null || true)"
  rm -f "$pidfile"
  if [[ -z "$pid" ]] || ! kill -0 "$pid" 2>/dev/null; then
    echo "==> $name was not running"
    return
  fi
  for signal in TERM KILL; do
    kill -"$signal" "$pid" 2>/dev/null || true
    for _ in $(seq 1 20); do
      if ! kill -0 "$pid" 2>/dev/null; then
        echo "==> Stopped $name (pid $pid) with SIG$signal"
        return
      fi
      sleep 0.1
    done
  done
  echo "warning: $name (pid $pid) survived SIGKILL" >&2
}

shopt -s nullglob
pidfiles=("$RUN_DIR"/*.pid)
shopt -u nullglob

if [[ ${#pidfiles[@]} -eq 0 ]]; then
  echo "==> Nothing recorded in $RUN_DIR"
fi

# Reverse of the order lightwatch-up.sh starts them in: the emitters go before
# the daemon they are writing to.
for (( i = ${#pidfiles[@]} - 1; i >= 0; i-- )); do
  stop_one "${pidfiles[$i]}"
done

# The daemon unlinks its socket only on the SIGINT it handles, and stop_one
# sends TERM, so the file is ours to clear.
rm -f "$SOCK_DIR"/*.sock
rmdir "$SOCK_DIR" 2>/dev/null || true

if curl -sf "http://127.0.0.1:$PORT/api/processes" >/dev/null 2>&1; then
  echo "warning: something still serves http://127.0.0.1:$PORT — not ours" >&2
fi

echo
echo "Logs kept in $RUN_DIR"
