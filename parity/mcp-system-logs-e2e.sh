#!/usr/bin/env bash
# Exercise the MCP system-log wrapper against an isolated Trefoil process.
set -Eeuo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
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
  ADMIN_KEY=disposable-mcp-log-e2e PORT="$port" ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
ready=0
for i in {1..45}; do
  if curl -fsS --max-time 2 "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then ready=1; break; fi
  if ! kill -0 "$gateway_pid" 2>/dev/null; then break; fi
  sleep 1
done
if [[ $ready != 1 ]]; then echo 'disposable gateway failed to start' >&2; cat "$log" >&2; exit 1; fi
TREFOIL_E2E_URL="http://127.0.0.1:$port" python3 - <<'PY'
import json, os, urllib.request, urllib.error
base = os.environ['TREFOIL_E2E_URL']
key = 'disposable-mcp-log-e2e'
def get(path, admin=key):
    req = urllib.request.Request(base+path, headers={'x-admin-key':admin} if admin else {})
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode()
def call(limit=1, offset=0, admin=key):
    data={'jsonrpc':'2.0','id':'recent-e2e','method':'tools/call',
          'params':{'name':'plexus_system_logs','arguments':{'operation':'recent','limit':limit,'offset':offset}}}
    req=urllib.request.Request(base+'/mcp/plexus',data=json.dumps(data).encode(),
        headers={'x-admin-key':admin,'content-type':'application/json'})
    try:
        with urllib.request.urlopen(req, timeout=5) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, error.read().decode()
status, page=get('/v0/system/logs/recent?limit=1&offset=0')
assert status==200 and set(page)=={'data','total','limit','offset'}, (status,page)
status, body=call()
assert status==200 and body['result']['isError'] is False, (status,body)
result=body['result']; content=result['structuredContent']
assert content['data']['limit']==1 and content['data']['offset']==0, content
assert len(content['data']['data'])<=1 and content['data']['total']>=len(content['data']['data'])
assert json.loads(result['content'][0]['text'])==content, result
assert '\n' in result['content'][0]['text'], result['content']
status, body=call(limit=1,offset=1)
assert status==200 and body['result']['structuredContent']['data']['offset']==1, (status,body)
assert get('/v0/system/logs/recent',admin=None)[0]==401
assert call(admin='invalid-key')[0] in (401,403), 'MCP must reject invalid admin key'
print('MCP recent logs: pretty text matches structured data; bounded paging and auth checked')
PY
