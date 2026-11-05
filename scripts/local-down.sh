#!/usr/bin/env bash
# Stops what scripts/local-up.sh started, and waits for each process to exit (the adapter
# drains for a few seconds after SIGTERM before it closes its listener).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
RUN=${RUN_DIR:-.run}
pids=()
for name in adapter small large; do
  if [[ -f "$RUN/$name.pid" ]]; then
    pid=$(cat "$RUN/$name.pid")
    kill "$pid" 2>/dev/null && pids+=("$pid")
    rm -f "$RUN/$name.pid"
  fi
done
for pid in "${pids[@]}"; do
  for _ in $(seq 1 100); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.1
  done
done
