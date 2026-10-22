#!/usr/bin/env bash
# Disposable PostgreSQL/API E2E for version-free provider JSON and conditional edits.
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
    s.bind(('127.0.0.1',0))
    print(s.getsockname()[1])
PY
)
DATABASE_URL="postgres://trefoil:trefoil@127.0.0.1:$db_port/trefoil" \
  ADMIN_KEY=disposable-provider-etag-e2e PORT="$port" ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
ready=0
for i in {1..45}; do
  if curl -fsS --max-time 2 "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then ready=1; break; fi
  if ! kill -0 "$gateway_pid" 2>/dev/null; then break; fi
  sleep 1
done
if [[ $ready != 1 ]]; then echo 'disposable gateway failed to start' >&2; cat "$log" >&2; exit 1; fi
TREFOIL_E2E_URL="http://127.0.0.1:$port" python3 - <<'PY'
import json, os, urllib.request, urllib.error
base=os.environ['TREFOIL_E2E_URL']; admin='disposable-provider-etag-e2e'
def call(path,method='GET',data=None,headers=None):
    # Like Plexus (Fastify), an empty body with a JSON content-type is a 400,
    # so only label requests that carry a body.
    h={'x-admin-key':admin}
    if data is not None: h['content-type']='application/json'
    if headers: h.update(headers)
    req=urllib.request.Request(base+path,method=method,headers=h,data=None if data is None else json.dumps(data).encode())
    try:
        with urllib.request.urlopen(req,timeout=5) as response:
            return response.status, {k.lower():v for k,v in response.headers.items()}, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, {k.lower():v for k,v in error.headers.items()}, json.load(error)
p='/v0/management/providers/etag-e2e'
assert call(p,'PUT',{'api_base_url':'https://api.example.com','api_key':'synthetic-key','adapter':'openai'})[0]==200
status,headers,body=call(p); assert status==200 and 'version' not in body,body
old=headers.get('etag'); assert old and old.startswith('"'),headers
assert 'version' not in call('/v0/management/providers')[2]['etag-e2e']
status,headers,body=call(p,'PATCH',{'enabled':False},headers={'If-Match':old}); assert status==200,(status,body)
assert 'version' not in body and headers.get('etag')!=old,(headers,body)
assert call(p,'PATCH',{'enabled':True},headers={'If-Match':old})[0]==409
assert call(p)[2]['enabled'] is False
assert call(p,'PATCH',{'enabled':True},headers={'If-Match':'bogus'})[0]==400
assert call(p,'PATCH',{'enabled':True,'expectedVersion':1},headers={'If-Match':headers['etag']})[0]==400
new=headers['etag']; version=int(new.strip('"'))
assert call(p,'PATCH',{'enabled':True,'expectedVersion':version})[0]==200
assert call(p)[2]['enabled'] is True
assert call(p,'PATCH',{'enabled':False,'expectedVersion':version})[0]==409
assert call(p,'DELETE')[0]==200
print('provider ETag: JSON excludes version, stale/invalid/conflicting edits rejected, legacy revision retained')
PY
