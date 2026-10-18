#!/usr/bin/env bash
# Disposable PostgreSQL/API E2E for pinned OAuth descriptors, Anthropic
# copy-code and OpenAI ChatGPT pasted-callback validation, and ENCRYPTION_KEY
# at-rest secrets. No provider host is contacted: every exchange is rejected
# locally before network use.
set -Eeuo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
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
    s.bind(('127.0.0.1',0))
    print(s.getsockname()[1])
PY
)
DATABASE_URL="postgres://trefoil:trefoil@127.0.0.1:$db_port/trefoil" \
  ADMIN_KEY=disposable-oauth-e2e PORT="$port" ENCRYPTION_KEY="disposable passphrase" \
  TREFOIL_CREDENTIAL_KEY=$(printf 'ab%.0s' {1..32}) ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
ready=0
for i in {1..45}; do
  if curl -fsS --max-time 2 "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then ready=1; break; fi
  if ! kill -0 "$gateway_pid" 2>/dev/null; then break; fi
  sleep 1
done
if [[ $ready != 1 ]]; then echo 'disposable gateway failed to start' >&2; cat "$log" >&2; exit 1; fi
TREFOIL_E2E_URL="http://127.0.0.1:$port" python3 - <<'PY'
import json, os, urllib.request, urllib.error, urllib.parse
base=os.environ['TREFOIL_E2E_URL']; admin='disposable-oauth-e2e'
def call(path,method='GET',data=None):
    req=urllib.request.Request(base+path,method=method,headers={'x-admin-key':admin,'content-type':'application/json'},data=None if data is None else json.dumps(data).encode())
    try:
        with urllib.request.urlopen(req,timeout=5) as response: return response.status, json.load(response)
    except urllib.error.HTTPError as error: return error.code, json.load(error)
status,body=call('/v0/management/oauth/providers')
assert status==200 and body['total']==8,body
flags={p['id']:p['usesCallbackServer'] for p in body['data']}
assert flags=={'anthropic':False,'github-copilot':False,'kimi-coding':False,'meta':False,'openai':False,'openai-codex':True,'openrouter':False,'xai':False},flags

status,body=call('/v0/management/oauth/sessions','POST',{'providerId':'anthropic','accountId':'e2e'}); assert status==200,body
session=body['data']; q=dict(urllib.parse.parse_qsl(urllib.parse.urlparse(session['authInfo']['url']).query))
assert q['redirect_uri']=='https://platform.claude.com/oauth/code/callback' and q['code']=='true' and q['state'],q
sid=session['id']
status,body=call(f'/v0/management/oauth/sessions/{sid}/manual-code','POST',{'value':'code#wrong-state'})
assert status==400 and body['error']=='OAuth state mismatch',body
assert call(f'/v0/management/oauth/sessions/{sid}')[1]['data']['status']=='waiting'
status,body=call(f'/v0/management/oauth/sessions/{sid}/callback?state={sid}&code=x'); assert status==400,body
assert call(f'/v0/management/oauth/sessions/{sid}/cancel','POST')[1]['data']['status']=='cancelled'

status,body=call('/v0/management/oauth/sessions','POST',{'providerId':'openai','accountId':'e2e'}); assert status==200,body
session=body['data']; q=dict(urllib.parse.parse_qsl(urllib.parse.urlparse(session['authInfo']['url']).query))
assert q['client_id']=='dynamic_agent_client' and q['redirect_uri']=='http://127.0.0.1:1455/auth/callback' and q['state']==session['id'],q
assert 'chatgpt.tokens.use.direct' in q['scope'] and q['ext_agent_host_id'].startswith('urn:uuid:'),q
sid=session['id']
for value,expected in [('bare-code','Paste the full callback URL from the browser'),
                       (f'https://evil.example/auth/callback?code=c&state={sid}&client_id=i','The pasted callback URL must start with http://127.0.0.1:1455/auth/callback'),
                       ('http://127.0.0.1:1455/auth/callback?code=c&state=other&client_id=i','OAuth state mismatch'),
                       (f'http://127.0.0.1:1455/auth/callback?code=c&state={sid}','OpenAI OAuth registration callback did not contain an issued client ID')]:
    status,body=call(f'/v0/management/oauth/sessions/{sid}/manual-code','POST',{'value':value}); assert (status,body['error'])==(400,expected),(value,status,body)
assert call(f'/v0/management/oauth/sessions/{sid}/cancel','POST')[1]['data']['status']=='cancelled'

p='/v0/management/providers/enc-e2e'
assert call(p,'PUT',{'api_base_url':'https://api.example.com','api_key':'synthetic-provider-key','adapter':'openai'})[0]==200
assert call('/v0/management/keys/enc-e2e','PUT',{'secret':'sk-synthetic-e2e'})[0]==200
print('oauth: 8 pinned descriptors, copy-code/pasted-callback validation non-terminal, cancel terminal')
PY
rows=$(docker exec "$container" psql -U trefoil -d trefoil -tAc "SELECT (SELECT api_key FROM providers WHERE slug='enc-e2e') || '|' || (SELECT secret FROM api_keys WHERE name='enc-e2e')")
[[ "$rows" == enc:v1:*'|'enc:v1:* ]] || { echo "secrets not encrypted at rest: ${rows:0:12}..." >&2; exit 1; }
[[ "$rows" != *synthetic* ]] || { echo 'plaintext secret found in database' >&2; exit 1; }
# Restart with the same key: encrypted rows must load and authenticate.
kill "$gateway_pid"; wait "$gateway_pid" 2>/dev/null || :
DATABASE_URL="postgres://trefoil:trefoil@127.0.0.1:$db_port/trefoil" \
  ADMIN_KEY=disposable-oauth-e2e PORT="$port" ENCRYPTION_KEY="disposable passphrase" \
  ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
for i in {1..45}; do curl -fsS --max-time 2 "http://127.0.0.1:$port/healthz" >/dev/null 2>&1 && break; sleep 1; done
curl -fsS -H 'x-admin-key: sk-synthetic-e2e' "http://127.0.0.1:$port/v0/management/auth/verify" >/dev/null
echo 'encryption: provider key and key secret stored as enc:v1, reload decrypts and authenticates'
