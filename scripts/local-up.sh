#!/usr/bin/env bash
# Starts two mock model servers and the adapter on localhost. Stop with scripts/local-down.sh.
# Ports: adapter 9480, small mock 9481, large mock 9482 (override with ADAPTER_PORT etc. only
# together with a matching config).
set -euo pipefail
cd "$(dirname "$0")/.."
BIN=${BIN:-target/debug}
RUN=${RUN_DIR:-.run}
CONFIG=${CONFIG:-config/adapter.local.toml}
mkdir -p "$RUN"

start() { # name, then env assignments, then command
  local name=$1; shift
  env "$@" >"$RUN/$name.log" 2>&1 &
  echo $! >"$RUN/$name.pid"
}

start small MOCK_PORT=9481 MOCK_MODEL=mock-small MOCK_PROFILE=small MOCK_POD=small-pod-0 \
  MOCK_JUDGE_ERROR_RATE="${JUDGE_ERROR_RATE:-0.1}" MOCK_SLOTS="${SMALL_SLOTS:-16}" "$BIN/mock-llm"
start large MOCK_PORT=9482 MOCK_MODEL=mock-large MOCK_PROFILE=large MOCK_POD=large-pod-0 \
  MOCK_SLOTS="${LARGE_SLOTS:-8}" "$BIN/mock-llm"
start adapter RUST_LOG="${RUST_LOG:-info}" "$BIN/bnk-switchyard-adapter" "$CONFIG"

for url in http://127.0.0.1:9481/health http://127.0.0.1:9482/health http://127.0.0.1:9480/readyz; do
  for _ in $(seq 1 50); do
    curl -fsS "$url" >/dev/null 2>&1 && break
    sleep 0.1
  done
  curl -fsS "$url" >/dev/null || { echo "not ready: $url (see $RUN/*.log)"; exit 1; }
done
echo "adapter on http://127.0.0.1:9480 (logs in $RUN/)"
