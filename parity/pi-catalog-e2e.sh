#!/usr/bin/env bash
set -euo pipefail

# Starts Trefoil against a disposable PostgreSQL database on tmpfs; no credentials used.
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
command -v docker >/dev/null || { echo 'docker is required' >&2; exit 2; }
TMP="$(mktemp -d /dev/shm/trefoil-pi-catalog.XXXXXX)"
NAME="trefoil-pi-catalog-$$"
cleanup() { docker rm -f "$NAME" >/dev/null 2>&1 || true; rm -rf "$TMP"; }
trap cleanup EXIT
# shellcheck source=lib/pg.sh
source "$ROOT/parity/lib/pg.sh"
pg_start --name "$NAME" --tmpfs /var/lib/postgresql/data:rw,size=256m -e POSTGRES_PASSWORD=localtest -e POSTGRES_DB=trefoil >/dev/null
for _ in $(seq 1 60); do docker exec "$NAME" pg_isready -U postgres -d trefoil >/dev/null 2>&1 && break; sleep 1; done
db_port=$pg_port
export DATABASE_URL="postgres://postgres:localtest@127.0.0.1:$db_port/trefoil"
port=$(python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(('127.0.0.1',0))
    print(s.getsockname()[1])
PY
)
cargo build --locked -q --bin trefoil-backend
ADMIN_KEY=pi-catalog-test PORT="$port" DATABASE_URL="$DATABASE_URL" "$ROOT/target/debug/trefoil-backend" >"$TMP/server.log" 2>&1 &
SERVER=$!
trap 'kill "$SERVER" >/dev/null 2>&1 || true; cleanup' EXIT
ready=0
for _ in $(seq 1 60); do
  if curl -sf "http://127.0.0.1:$port/v0/management/pi/providers" -H 'x-admin-key: pi-catalog-test' >/dev/null; then ready=1; break; fi
  kill -0 "$SERVER" 2>/dev/null || break
  sleep 1
done
if [[ $ready != 1 ]]; then cat "$TMP/server.log" >&2; exit 1; fi
BASE="http://127.0.0.1:$port/v0/management/pi"
curl -sf "$BASE/providers" -H 'x-admin-key: pi-catalog-test' | grep -q 'openai'
curl -sf "$BASE/models?provider=openai" -H 'x-admin-key: pi-catalog-test' | grep -q 'gpt-4o'
curl -sf "$BASE/models?provider=anthropic&q=Sonnet%204.5" -H 'x-admin-key: pi-catalog-test' | grep -q 'claude-sonnet-4-5'
curl -sf "$BASE/models?provider=openai&q=gpt-4o" -H 'x-admin-key: pi-catalog-test' | grep -q 'gpt-4o'
UNKNOWN=$(curl -sf "$BASE/models?provider=not-a-provider" -H 'x-admin-key: pi-catalog-test')
[[ "$UNKNOWN" == '{"data":[]}' ]]
CODE=$(curl -s -o "$TMP/unauthorized" -w '%{http_code}' "$BASE/providers")
[[ "$CODE" == 401 ]]
echo 'Pi catalog E2E passed.'
