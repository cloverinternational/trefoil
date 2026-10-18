#!/usr/bin/env bash
# Disposable PostgreSQL/API E2E for alias safeguard approval round-trip/default.
set -Eeuo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
command -v docker >/dev/null
cargo build --locked -q --bin trefoil-backend
# shellcheck source=lib/pg.sh
source "$ROOT/parity/lib/pg.sh"
pg_start --tmpfs /var/lib/postgresql/data \
  -e POSTGRES_DB=trefoil -e POSTGRES_USER=trefoil -e POSTGRES_PASSWORD=trefoil
container=$pg_container
gateway_pid=
log=$(mktemp)
cleanup() {
  if [[ -n "$gateway_pid" ]]; then kill "$gateway_pid" 2>/dev/null || :; wait "$gateway_pid" 2>/dev/null || :; fi
  docker stop "$container" >/dev/null 2>&1 || :
  rm -f "$log"
}
trap cleanup EXIT
db_port=$pg_port
for i in {1..40}; do docker exec "$container" pg_isready -U trefoil -d trefoil >/dev/null 2>&1 && break; sleep 1; done
port=$(python3 - <<'PY'
import socket
with socket.socket() as s:
    s.bind(('127.0.0.1', 0))
    print(s.getsockname()[1])
PY
)
DATABASE_URL="postgres://trefoil:trefoil@127.0.0.1:$db_port/trefoil" \
  ADMIN_KEY=disposable-alias-e2e PORT="$port" ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
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
admin = 'disposable-alias-e2e'
def request(slug, method='GET', data=None):
    headers = {'x-admin-key': admin}
    if data is not None: headers['content-type'] = 'application/json'
    req = urllib.request.Request(base + '/v0/management/aliases/' + slug, method=method,
                                 headers=headers,
                                 data=None if data is None else json.dumps(data).encode())
    with urllib.request.urlopen(req, timeout=5) as response:
        return json.load(response)
created = request('approval-explicit', 'PUT', {'synthetic_safeguard_approval': True})
assert created['success'] is True
assert request('approval-explicit')['synthetic_safeguard_approval'] is True
request('approval-after-patch', 'PUT', {'synthetic_safeguard_approval': True})
request('approval-after-patch', 'PATCH', {'sticky_session': False})
assert request('approval-after-patch')['synthetic_safeguard_approval'] is True
request('approval-default', 'PUT', {})
assert request('approval-default')['synthetic_safeguard_approval'] is False
print('alias safeguard: explicit true survives GET and unrelated PATCH; omitted field GET defaults false')
PY
