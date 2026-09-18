#!/usr/bin/env bash
set -Eeuo pipefail

# Unified parity gate. Each lane is intentionally run in a separate working
# directory. All lanes run even when one fails, so the report describes the
# complete verification rather than only the first failure.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPORT_FILE=${PARITY_REPORT:-"$ROOT/target/parity-report.json"}
REPORT_DIR=$(mktemp -d "${TMPDIR:-/tmp}/trefoil-parity-gate.XXXXXX")
declare -a LANE_RECORDS=()
first_failure=0

cleanup() {
  rm -rf "$REPORT_DIR"
}
trap cleanup EXIT

run_lane() {
  local name=$1
  local id=${#LANE_RECORDS[@]}
  local stdout_file="$REPORT_DIR/$id.stdout"
  local stderr_file="$REPORT_DIR/$id.stderr"
  local started ended duration_ms rc command
  shift
  command=$(printf '%q ' "$@")
  printf '\n==> %s\n' "$name"
  started=$(date +%s%N)
  if "$@" >"$stdout_file" 2>"$stderr_file"; then
    rc=0
  else
    rc=$?
  fi
  ended=$(date +%s%N)
  duration_ms=$(( (ended - started) / 1000000 ))
  cat "$stdout_file"
  cat "$stderr_file" >&2
  if (( rc == 0 )); then
    printf '<== %s: PASS (%sms)\n' "$name" "$duration_ms"
  else
    printf '<== %s: FAIL (exit %s, %sms)\n' "$name" "$rc" "$duration_ms" >&2
    if (( first_failure == 0 )); then
      first_failure=$rc
    fi
  fi
  LANE_RECORDS+=("$id|$name|$rc|$duration_ms|$command|$stdout_file|$stderr_file")
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

mkdir -p "$(dirname "$REPORT_FILE")" 2>/dev/null || true
report_status=0
if ! REPORT_FILE="$REPORT_FILE" python3 - "${LANE_RECORDS[@]}" <<'PY'
import json
import os
import sys
from pathlib import Path

records = []
for raw in sys.argv[1:]:
    lane_id, name, rc, duration_ms, command, stdout_path, stderr_path = raw.split("|", 6)
    def read(path):
        try:
            return Path(path).read_text()
        except OSError as exc:
            return f"<unable to read lane output: {exc}>"
    records.append({
        "id": int(lane_id),
        "name": name,
        "command": command.strip(),
        "status": "pass" if int(rc) == 0 else "fail",
        "exit_code": int(rc),
        "duration_ms": int(duration_ms),
        "stdout": read(stdout_path),
        "stderr": read(stderr_path),
    })
payload = {
    "schema_version": 1,
    "status": "pass" if all(item["status"] == "pass" for item in records) else "fail",
    "lane_count": len(records),
    "passed": sum(item["status"] == "pass" for item in records),
    "failed": sum(item["status"] == "fail" for item in records),
    "lanes": records,
}
destination = Path(os.environ["REPORT_FILE"])
temporary = destination.with_name(destination.name + ".tmp")
temporary.write_text(json.dumps(payload, indent=2) + "\n")
temporary.replace(destination)
PY
then
  report_status=1
  printf 'parity gate: unable to write report: %s\n' "$REPORT_FILE" >&2
fi

if (( first_failure != 0 )); then
  printf '\nParity verification failed (first failing exit code: %s). Report: %s\n' \
    "$first_failure" "$REPORT_FILE" >&2
  exit "$first_failure"
fi
if (( report_status != 0 )); then
  exit 1
fi
printf '\nParity verification passed: all %s lanes. Report: %s\n' \
  "${#LANE_RECORDS[@]}" "$REPORT_FILE"
