#!/usr/bin/env bash
# Disposable gateway E2E for the built-in quota checker registry metadata.
set -Eeuo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
command -v docker >/dev/null
cargo build --locked -q --bin trefoil-backend
container=$(docker run --rm -d --tmpfs /var/lib/postgresql/data \
  -e POSTGRES_DB=trefoil -e POSTGRES_USER=trefoil -e POSTGRES_PASSWORD=trefoil \
  -p 127.0.0.1::5432 postgres:16-alpine)
gateway_pid=
log=$(mktemp)
cleanup() {
  if [[ -n "$gateway_pid" ]]; then kill "$gateway_pid" 2>/dev/null || :; wait "$gateway_pid" 2>/dev/null || :; fi
  docker stop "$container" >/dev/null 2>&1 || :
  rm -f "$log"
}
trap cleanup EXIT
db_port=$(docker port "$container" 5432/tcp | sed -E 's/.*:([0-9]+)$/\1/')
for i in {1..40}; do docker exec "$container" pg_isready -U trefoil -d trefoil >/dev/null 2>&1 && break; sleep 1; done
port=$(python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(('127.0.0.1', 0))
    print(s.getsockname()[1])
PY
)
DATABASE_URL="postgres://trefoil:trefoil@127.0.0.1:$db_port/trefoil" \
  ADMIN_KEY=disposable-quota-registry-e2e PORT="$port" ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
ready=0
for i in {1..45}; do
  if curl -fsS --max-time 2 "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then ready=1; break; fi
  if ! kill -0 "$gateway_pid" 2>/dev/null; then break; fi
  sleep 1
done
if [[ $ready != 1 ]]; then echo 'disposable gateway failed to start' >&2; cat "$log" >&2; exit 1; fi
TREFOIL_E2E_URL="http://127.0.0.1:$port" python3 - <<'PY'
import json, os, urllib.request
base = os.environ['TREFOIL_E2E_URL']
expected = ('naga', 'synthetic', 'nanogpt', 'zai', 'moonshot', 'novita',
    'minimax', 'minimax-coding', 'openrouter', 'kilo', 'openai-codex',
    'kimi-code', 'muse-code', 'claude-code', 'copilot', 'wisdomgate', 'apertis', 'poe',
    'routing-run', 'ollama', 'neuralwatt', 'zenmux', 'devpass',
    'opencode-go', 'crof', 'exedev', 'hyper', 'deepseek', 'sakana', 'cline')
def get(path):
    request = urllib.request.Request(base + path, headers={'x-admin-key':'disposable-quota-registry-e2e'})
    with urllib.request.urlopen(request, timeout=5) as response:
        assert response.status == 200
        return json.load(response)
types = get('/v0/management/quota-checker-types')
assert tuple(item['type'] for item in types['types']) == expected, types
assert types['count'] == len(expected) == 30, types['count']
assert next(item for item in types['types'] if item['type'] == 'muse-code')['displayName'] == 'Muse Code'
assert all(item['type'] != 'wafer' for item in types['types'])
checkers = get('/v0/management/quota-checkers')
known = checkers['knownTypes']
assert tuple(item['type'] for item in known) == expected, known
assert len(known) == 30 and all(item['custom'] is False for item in known)
assert next(item for item in known if item['type'] == 'muse-code')['displayName'] == 'Muse Code'
assert all(item['type'] != 'wafer' for item in known)
print('quota checker registry parity: both routes expose muse-code (Muse Code), omit wafer, and retain 30 built-ins')
PY
