#!/usr/bin/env bash
# Run a command with FIRESTORE_EMULATOR_HOST pointing at a Firestore emulator (ADR-0017).
# Reuses one already set; otherwise starts `gcloud emulators firestore` on a free port and
# stops it afterwards. Without gcloud the command still runs and the emulator tests skip.
set -euo pipefail

if [ -n "${FIRESTORE_EMULATOR_HOST:-}" ]; then exec "$@"; fi

GCLOUD=$(command -v gcloud || echo "$HOME/google-cloud-sdk/bin/gcloud")
if [ ! -x "$GCLOUD" ]; then
  echo "with-firestore-emulator: no gcloud - Firestore tests will skip" >&2
  exec "$@"
fi

port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
log=$(mktemp)
setsid "$GCLOUD" emulators firestore start --host-port="127.0.0.1:$port" >"$log" 2>&1 &
pgid=$!
trap 'kill -- -"$pgid" 2>/dev/null || true; rm -f "$log"' EXIT

for _ in $(seq 1 120); do
  curl -sf "http://127.0.0.1:$port/" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -sf "http://127.0.0.1:$port/" >/dev/null || { echo "emulator did not start:" >&2; tail -20 "$log" >&2; exit 1; }

FIRESTORE_EMULATOR_HOST="127.0.0.1:$port" "$@"
