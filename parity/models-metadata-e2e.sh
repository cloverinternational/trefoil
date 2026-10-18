#!/usr/bin/env bash
# Disposable PostgreSQL/API E2E for alias-backed /v1/models metadata.
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
  ADMIN_KEY=disposable-models-e2e PORT="$port" ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
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
admin = 'disposable-models-e2e'
def request(path, key=None, method='GET', data=None):
    headers = {}
    if key is not None:
        headers['x-admin-key' if key == admin else 'x-api-key'] = key
    if data is not None: headers['content-type'] = 'application/json'
    req = urllib.request.Request(base + path, method=method, headers=headers,
                                 data=None if data is None else json.dumps(data).encode())
    return urllib.request.urlopen(req, timeout=5)
# Plexus serves GET /v1/models publicly (no key filtering).
with request('/v1/models') as response: assert response.status == 200
request('/v0/management/keys/models-e2e', admin, 'PUT', {
    'secret':'disposable-models-limited', 'allowedModels':['chat-alias','embed-alias']
}).close()
request('/v0/management/aliases/chat-alias', admin, 'PUT', {
    'type':'text', 'preferred_api':['responses','chat_completions'],
    'target_groups':[{'name':'main','selector':'in_order','targets':[{'provider':'fake','model':'provider-model'}]}]
}).close()
request('/v0/management/aliases/embed-alias', admin, 'PUT', {'type':'embeddings'}).close()
with request('/v1/models', 'disposable-models-limited') as response:
    assert response.status == 200
    payload = json.load(response)
by_id = {model['id']: model for model in payload['data']}
assert {'chat-alias','embed-alias'} <= set(by_id), by_id
assert by_id['chat-alias']['type'] == 'text'
assert by_id['chat-alias']['preferred_api'] == ['responses','chat_completions']
assert by_id['chat-alias']['object'] == 'model' and by_id['chat-alias']['owned_by'] == 'plexus'
assert by_id['embed-alias']['type'] == 'embeddings'
assert 'preferred_api' not in by_id['embed-alias']
for unsupported in ('reasoning_options','service_tiers','tier','reasoning'):
    assert unsupported not in by_id['chat-alias'], (unsupported, by_id['chat-alias'])
print('models metadata: alias type and preferred_api exposed; IDs and legacy fields preserved')
print('auth: public listing served; configured limited API key can list models')

# short_name (Plexus PR #999): explicit override is trimmed and persisted,
# otherwise derived at resolve time and never stored.
request('/v0/management/keys/models-e2e-all', admin, 'PUT', {'secret':'disposable-models-all'}).close()
request('/v0/management/aliases/short-alias', admin, 'PUT', {
    'metadata':{'source':'custom','overrides':{'name':'Anthropic: Claude Sonnet 5.5','short_name':'  Custom Short  '}},
}).close()
request('/v0/management/aliases/auto-alias', admin, 'PUT', {
    'metadata':{'source':'custom','overrides':{'name':'DeepSeek: DeepSeek V4.1 Flash'}},
}).close()
for bad in ('   ', 'x' * 65, 7):
    try: request('/v0/management/aliases/bad-alias', admin, 'PUT', {'metadata':{'source':'custom','overrides':{'name':'X','short_name':bad}}}).close()
    except urllib.error.HTTPError as e: assert e.code == 400, (bad, e.code)
    else: raise AssertionError(f'short_name {bad!r} must be rejected')
with request('/v0/management/aliases/short-alias', admin) as response:
    assert json.load(response)['metadata']['overrides'] == {'name':'Anthropic: Claude Sonnet 5.5','short_name':'Custom Short'}
with request('/v0/management/aliases/auto-alias', admin) as response:
    assert 'short_name' not in json.load(response)['metadata']['overrides']
with request('/v1/models', 'disposable-models-all') as response:
    by_id = {model['id']: model for model in json.load(response)['data']}
assert by_id['short-alias']['short_name'] == 'Custom Short', by_id['short-alias']
assert by_id['auto-alias']['short_name'] == 'DS 4.1 F', by_id['auto-alias']
assert list(by_id['auto-alias']).index('short_name') == list(by_id['auto-alias']).index('name') + 1
request('/v0/management/aliases/short-alias', admin, 'PUT', {
    'metadata':{'source':'custom','overrides':{'name':'Anthropic: Claude Sonnet 5.5'}},
}).close()
with request('/v0/management/aliases/short-alias', admin) as response:
    assert 'short_name' not in json.load(response)['metadata']['overrides']
with request('/v1/models', 'disposable-models-all') as response:
    by_id = {model['id']: model for model in json.load(response)['data']}
assert by_id['short-alias']['short_name'] == 'Sonnet 5.5', by_id['short-alias']
print('short_name: override trimmed and round-tripped; invalid values rejected; derived default served, never stored')
PY
column=$(docker exec "$container" psql -U trefoil -d trefoil -tAc \
  "SELECT data_type || ':' || is_nullable FROM information_schema.columns WHERE table_name = 'alias_metadata_overrides' AND column_name = 'short_name'")
[[ $column == 'text:YES' ]] || { echo "alias_metadata_overrides.short_name missing or wrong type: '$column'" >&2; exit 1; }
applied=$(docker exec "$container" psql -U trefoil -d trefoil -tAc \
  "SELECT success FROM _sqlx_migrations WHERE version = 96")
[[ $applied == 't' ]] || { echo "migration 0096 not applied: '$applied'" >&2; exit 1; }
echo 'migration 0096: alias_metadata_overrides.short_name (nullable text) applied on a fresh database'
