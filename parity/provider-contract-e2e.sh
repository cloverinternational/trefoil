#!/usr/bin/env bash
# Disposable PostgreSQL/API E2E for provider response and OAuth validation.
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
  ADMIN_KEY=disposable-provider-contract-e2e PORT="$port" ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
ready=0
for i in {1..45}; do
  if curl -fsS --max-time 2 "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then ready=1; break; fi
  if ! kill -0 "$gateway_pid" 2>/dev/null; then break; fi
  sleep 1
done
if [[ $ready != 1 ]]; then echo 'disposable gateway failed to start' >&2; cat "$log" >&2; exit 1; fi
TREFOIL_E2E_URL="http://127.0.0.1:$port" python3 - <<'PY'
import json, os, urllib.error, urllib.request
base = os.environ['TREFOIL_E2E_URL']
admin = 'disposable-provider-contract-e2e'
def request(path, method, data):
    req = urllib.request.Request(base + path, method=method,
        headers={'x-admin-key': admin, 'content-type': 'application/json'},
        data=json.dumps(data).encode())
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, json.load(error)

# PATCH an absent slug reports the slug in the 404 error.
status, payload = request('/v0/management/providers/provider-contract-missing', 'PATCH', {})
assert status == 404, (status, payload)
assert payload.get('error') == "Provider 'provider-contract-missing' not found", payload

# Plexus only requires oauth_provider: oauth_account is derived from the slug.
status, payload = request('/v0/management/providers/provider-contract-oauth', 'PUT', {
    'api_base_url': 'oauth://', 'oauth_provider': 'unsupported-provider'
})
assert status == 400, (status, payload)
assert [issue.get('message') for issue in payload.get('details', [])] == [
    'oauth_provider must be a supported OAuth provider id'
], payload
print('provider contract: PATCH missing-slug 404 includes slug')
print('provider contract: oauth:// missing account adds no account-required validation issue')
PY
