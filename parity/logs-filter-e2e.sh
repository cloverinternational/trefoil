#!/usr/bin/env bash
# Isolated gateway-level E2E: no existing database or provider credentials.
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
  ADMIN_KEY=disposable-logs-e2e PORT="$port" ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
ready=0
for i in {1..45}; do
  if curl -fsS --max-time 2 "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then ready=1; break; fi
  if ! kill -0 "$gateway_pid" 2>/dev/null; then break; fi
  sleep 1
done
if [[ $ready != 1 ]]; then echo 'disposable gateway failed to start' >&2; exit 1; fi
first=$(date -u -d '2026-10-16 12:00:00' +%s000)
older=$(date -u -d '2026-10-15 12:00:00' +%s000)
docker exec "$container" psql -U trefoil -d trefoil -v ON_ERROR_STOP=1 -q -c \
  "INSERT INTO request_usage(request_id,date,api_key,provider,incoming_model_alias,start_time,created_at) VALUES ('log-a','2026-10-16','key-a','alpha','model-a',$first,$first),('log-b','2026-10-15','key-b','beta','model-b',$older,$older),('log-c','2026-10-16','key-a','alpha','model-a',$((first+1000)),$((first+1000)));" >/dev/null
TREFOIL_E2E_URL="http://127.0.0.1:$port" python3 - <<'PY'
import json, os, urllib.error, urllib.request
base = os.environ['TREFOIL_E2E_URL'] + '/v0/management/usage'
key_url = os.environ['TREFOIL_E2E_URL'] + '/v0/management/keys/key-a'
def get(query, key='disposable-logs-e2e'):
    request = urllib.request.Request(base + query, headers={'x-admin-key': key})
    return json.load(urllib.request.urlopen(request, timeout=4))
create = urllib.request.Request(key_url, method='PUT', headers={'x-admin-key':'disposable-logs-e2e','content-type':'application/json'}, data=b'{"secret":"disposable-limited-key"}')
with urllib.request.urlopen(create, timeout=4) as response: assert response.status in (200, 204)
# Rows sort by `date` DESC (as in Plexus). log-a and log-c share the same
# date, so their relative order is unspecified: assert page membership.
for query, total, request_ids in [
    ('?apiKey=key-a&limit=1&offset=0', 2, {'log-a', 'log-c'}),
    ('?apiKey=key-a&limit=1&offset=1', 2, {'log-a', 'log-c'}),
    ('?provider=beta&incomingModelAlias=model-b', 1, {'log-b'}),
    ('?startDate=2026-10-16&endDate=2026-10-16', 2, {'log-a', 'log-c'}),
    ('?provider=missing', 0, None),
]:
    result = get(query)
    assert result['total'] == total and (request_ids is None or result['data'][0]['requestId'] in request_ids), (query, result)
    print(f'{query}: total={total}, returned={len(result["data"])}')
pages = [get(f'?apiKey=key-a&limit=1&offset={offset}')['data'][0]['requestId'] for offset in (0, 1)]
assert sorted(pages) == ['log-a', 'log-c'], pages
print('apiKey pagination: two distinct pages')
# Plexus parity: the list route passes date bounds straight to the
# (string) date filter without validating them, so these match nothing.
for query in ['?startDate=invalid', '?startDate=2026-10-17&endDate=2026-10-16']:
    assert get(query)['total'] == 0, query
print('invalid and reversed date ranges: 200, no rows')
own = get('?apiKey=key-b&limit=1&offset=0', 'disposable-limited-key')
assert own['total'] == 2 and own['data'][0]['apiKey'] == 'key-a', own
assert get('?provider=beta', 'disposable-limited-key')['total'] == 0
print('limited key: forced exact scope, forged key ignored, other provider hidden')
delete = urllib.request.Request(base + '/log-a', method='DELETE', headers={'x-admin-key':'disposable-limited-key'})
try: urllib.request.urlopen(delete, timeout=4)
except urllib.error.HTTPError as error: assert error.code == 403, error.code
else: raise AssertionError('limited key deleted request log')
assert get('?apiKey=key-a')['total'] == 2
print('limited deletion: 403, records unchanged')
PY
