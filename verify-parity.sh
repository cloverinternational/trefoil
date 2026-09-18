#!/usr/bin/env bash
set -Eeuo pipefail

# Unified parity gate. Each lane is intentionally run in a separate working
# directory and the gate stops at the first failure.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

run_lane() {
  local name=$1
  shift
  printf '\n==> %s\n' "$name"
  "$@"
  printf '<== %s: PASS\n' "$name"
}

run_lane "backend (cargo test --all-targets)" \
  cargo test --all-targets --manifest-path "$ROOT/Cargo.toml"

run_lane "frontend (npm test)" \
  npm --prefix "$ROOT/frontend" test
run_lane "frontend (npm run typecheck)" \
  npm --prefix "$ROOT/frontend" run typecheck
run_lane "frontend (npm run build)" \
  npm --prefix "$ROOT/frontend" run build

# parity/run.sh owns the disposable database, server lifecycle, and harness.
# Pass through its documented environment overrides unchanged.
run_lane "differential harness (parity/run.sh)" \
  "$ROOT/parity/run.sh"

printf '\nParity verification passed: backend, frontend, and differential harness.\n'
