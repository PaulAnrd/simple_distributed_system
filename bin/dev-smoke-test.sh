#!/usr/bin/env bash
# Local, single-machine smoke test for Phase 1: spins up one head and two
# workers as separate processes on localhost, confirms `clusterd status`
# shows a live combined view, then kills a worker and confirms it flips to
# DOWN after the heartbeat timeout. Not a substitute for real multi-machine
# validation (see doc/testing-vms.md) — this only proves the logic, not
# real networking/TLS-between-hosts/systemd integration.
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DATA_ROOT="$ROOT_DIR/data/smoke-test"
BIN="$ROOT_DIR/target/debug/clusterd"
HEAD_ADDR="127.0.0.1:17700"

PIDS=()
cleanup() {
  echo "--- cleaning up agent processes ---"
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  wait 2>/dev/null || true
}
trap cleanup EXIT

echo "--- building ---"
(cd "$ROOT_DIR" && cargo build --quiet)

rm -rf "$DATA_ROOT"
mkdir -p "$DATA_ROOT"

echo "--- init head ---"
"$BIN" --data-dir "$DATA_ROOT/head" init --node-name head-1 --role head --listen "$HEAD_ADDR"

for w in worker-1 worker-2; do
  echo "--- issuing cert + init config for $w ---"
  "$BIN" --data-dir "$DATA_ROOT/head" cert issue --name "$w" --out "$DATA_ROOT/$w-certs"
  "$BIN" --data-dir "$DATA_ROOT/$w" init --node-name "$w" --role worker --head "$HEAD_ADDR"
  cp "$DATA_ROOT/$w-certs/ca.crt" "$DATA_ROOT/$w-certs/node.crt" "$DATA_ROOT/$w-certs/node.key" "$DATA_ROOT/$w/"
done

echo "--- starting agents ---"
RUST_LOG=warn "$BIN" --data-dir "$DATA_ROOT/head" agent run &
PIDS+=($!)
sleep 1
RUST_LOG=warn "$BIN" --data-dir "$DATA_ROOT/worker-1" agent run &
PIDS+=($!)
WORKER2_ARGS=(--data-dir "$DATA_ROOT/worker-2" agent run)
RUST_LOG=warn "$BIN" "${WORKER2_ARGS[@]}" &
WORKER2_PID=$!
PIDS+=("$WORKER2_PID")

echo "--- waiting for heartbeats to arrive ---"
sleep 5

echo "--- status (expect head-1, worker-1, worker-2 all up) ---"
"$BIN" --data-dir "$DATA_ROOT/head" status

echo "--- killing worker-2, waiting past the 15s heartbeat timeout ---"
kill "$WORKER2_PID"
sleep 17

echo "--- status (expect worker-2 DOWN, others still up) ---"
"$BIN" --data-dir "$DATA_ROOT/head" status

echo "--- smoke test complete ---"
