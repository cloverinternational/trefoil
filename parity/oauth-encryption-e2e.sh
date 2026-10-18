#!/usr/bin/env bash
# Disposable PostgreSQL/API E2E for pinned OAuth descriptors, Anthropic
# copy-code and OpenAI ChatGPT pasted-callback validation, and ENCRYPTION_KEY
# at-rest secrets. No provider host is contacted: every exchange is rejected
# locally before network use.
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
  ADMIN_KEY=disposable-oauth-e2e PORT="$port" ENCRYPTION_KEY="disposable passphrase" \
  TREFOIL_OAUTH_DEVICE_ID=123e4567-e89b-12d3-a456-426614174000 \
  TREFOIL_CREDENTIAL_KEY=$(printf 'ab%.0s' {1..32}) ./target/debug/trefoil-backend >"$log" 2>&1 & gateway_pid=$!
ready=0
for i in {1..45}; do
  if curl -fsS --max-time 2 "http://127.0.0.1:$port/healthz" >/dev/null 2>&1; then ready=1; break; fi
  if ! kill -0 "$gateway_pid" 2>/dev/null; then break; fi
  sleep 1
done
if [[ $ready != 1 ]]; then echo 'disposable gateway failed to start' >&2; cat "$log" >&2; exit 1; fi
TREFOIL_E2E_URL="http://127.0.0.1:$port" python3 - <<'PY'
import json, os, time, urllib.request, urllib.error, urllib.parse
base=os.environ['TREFOIL_E2E_URL']; admin='disposable-oauth-e2e'
def call(path,method='GET',data=None):
    # Like Plexus (Fastify), an empty body labelled JSON is a 400: only send
    # the content-type with a body.
    headers={'x-admin-key':admin,**({} if data is None else {'content-type':'application/json'})}
    req=urllib.request.Request(base+path,method=method,headers=headers,data=None if data is None else json.dumps(data).encode())
    try:
        with urllib.request.urlopen(req,timeout=5) as response: return response.status, json.load(response)
    except urllib.error.HTTPError as error: return error.code, json.load(error)
def started(provider):
    # As in Plexus, POST returns the session before the login flow has
    # produced authInfo and started waiting for the pasted code (it runs
    # asynchronously); poll until it is waiting.
    status,body=call('/v0/management/oauth/sessions','POST',{'providerId':provider,'accountId':'e2e'}); assert status==200,body
    session=body['data']
    for _ in range(50):
        if 'authInfo' in session and session['status']=='awaiting_manual_code': return session
        time.sleep(0.1); session=call(f"/v0/management/oauth/sessions/{session['id']}")[1]['data']
    raise AssertionError(session)
status,body=call('/v0/management/oauth/providers')
assert status==200 and body['total']==8,body
flags={p['id']:p['usesCallbackServer'] for p in body['data']}
assert flags=={'anthropic':False,'github-copilot':False,'kimi-coding':False,'meta':False,'openai':False,'openai-codex':True,'openrouter':False,'xai':False},flags

def rejected(provider,value):
    # Plexus parity: the pasted value is handed to the running login flow
    # (submit answers 200), which validates it and fails the session.
    session=started(provider); sid=session['id']
    status,body=call(f'/v0/management/oauth/sessions/{sid}/manual-code','POST',{'value':value(sid) if callable(value) else value}); assert status==200,body
    for _ in range(50):
        session=call(f'/v0/management/oauth/sessions/{sid}')[1]['data']
        if session['status'] not in ('in_progress','awaiting_manual_code'): return session
        time.sleep(0.1)
    raise AssertionError(session)

session=started('anthropic'); q=dict(urllib.parse.parse_qsl(urllib.parse.urlparse(session['authInfo']['url']).query))
assert q['redirect_uri']=='https://platform.claude.com/oauth/code/callback' and q['code']=='true' and q['state'],q
sid=session['id']
# Plexus exposes no per-session callback route.
assert call(f'/v0/management/oauth/sessions/{sid}/callback?state={sid}&code=x')[0]==404
assert call(f'/v0/management/oauth/sessions/{sid}/cancel','POST')[1]['data']['status']=='cancelled'
session=rejected('anthropic','code#wrong-state')
assert (session['status'],session.get('error'))==('error','OAuth state mismatch'),session

session=started('openai'); q=dict(urllib.parse.parse_qsl(urllib.parse.urlparse(session['authInfo']['url']).query))
assert q['client_id']=='dynamic_agent_client' and q['redirect_uri']=='http://127.0.0.1:1455/auth/callback' and q['state'],q
assert 'chatgpt.tokens.use.direct' in q['scope'] and q['ext_agent_host_id']=='urn:uuid:123e4567-e89b-12d3-a456-426614174000',q
assert call(f"/v0/management/oauth/sessions/{session['id']}/cancel",'POST')[1]['data']['status']=='cancelled'
def state_of(sid):
    url=call(f'/v0/management/oauth/sessions/{sid}')[1]['data']['authInfo']['url']
    return dict(urllib.parse.parse_qsl(urllib.parse.urlparse(url).query))['state']
for value,expected in [('bare-code','Paste the full callback URL from the browser'),
                       (lambda sid:f'https://evil.example/auth/callback?code=c&state={state_of(sid)}&client_id=i','The pasted callback URL must start with http://127.0.0.1:1455/auth/callback'),
                       ('http://127.0.0.1:1455/auth/callback?code=c&state=other&client_id=i','OAuth state mismatch'),
                       (lambda sid:f'http://127.0.0.1:1455/auth/callback?code=c&state={state_of(sid)}','OpenAI OAuth registration callback did not contain an issued client ID')]:
    session=rejected('openai',value)
    assert (session['status'],session.get('error'))==('error',expected),(expected,session)

p='/v0/management/providers/enc-e2e'
assert call(p,'PUT',{'api_base_url':'https://api.example.com','api_key':'synthetic-provider-key','adapter':'openai'})[0]==200
assert call('/v0/management/keys/enc-e2e','PUT',{'secret':'sk-synthetic-e2e'})[0]==200
print('oauth: 8 pinned descriptors, cancel terminal, invalid copy-code/pasted-callback input fails the login (Plexus parity)')
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
