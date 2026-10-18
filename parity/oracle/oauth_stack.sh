#!/usr/bin/env bash
# Isolated Plexus ($PLEXUS_PORT, sqlite) + Trefoil ($TREFOIL_PORT, host-network
# PG $PG_PORT) pair for OAuth dispatch probing. OAuth upstreams are redirected
# to the recording mocks ($MOCK1 Plexus, $MOCK2 Trefoil) through a pi catalog overlay
# (models-store.json) and identical fake credentials are stored in both DBs.
# No real provider is contacted (PLEXUS_MODEL_CATALOG_REFRESH=false).
#   oauth_stack.sh up | down
# Trefoil binary: $TREFOIL_BIN (default target/debug/trefoil-backend).
set -euo pipefail
T=$(cd "$(dirname "$0")/../.." && pwd)
O=$(cd "$(dirname "$0")" && pwd)
S=${ORACLE_STATE:-/tmp/trefoil-oauth-oracle}
BIN=${TREFOIL_BIN:-$T/target/debug/trefoil-backend}
KEY=$(printf '42%.0s' $(seq 1 32))
PG_PORT=${PG_PORT:-55494}; PLEXUS_PORT=${PLEXUS_PORT:-14244}; TREFOIL_PORT=${TREFOIL_PORT:-18244}; MOCK1=${MOCK1:-19044}; MOCK2=${MOCK2:-19045}
mkdir -p "$S"
cmd=${1:-up}

overlay() { # $1 mock port
  python3 - "$1" <<'EOF'
import json, sys
base = f"http://127.0.0.1:{sys.argv[1]}"
def m(provider, id, api):
    return {"id": id, "name": id, "api": api, "provider": provider, "baseUrl": base, "reasoning": False, "input": ["text"],
            "cost": {"input": 1, "output": 2, "cacheRead": 0, "cacheWrite": 0}, "contextWindow": 200000, "maxTokens": 8192, "type": "chat"}
store = {
  "anthropic": {"models": [m("anthropic", "claude-mock", "anthropic-messages"), m("anthropic", "claude-mock-echotool", "anthropic-messages")]},
  "openai-codex": {"models": [m("openai-codex", "gpt-mock-codex", "openai-responses"), m("openai-codex", "gpt-mock-codex-echotool", "openai-responses"), m("openai-codex", "gpt-image-mock", "openai-responses")]},
  "meta": {"models": [m("meta", "muse-spark", "openai-responses")]},
  "xai": {"models": [m("xai", "grok-mock", "openai-completions")]},
  "kimi-coding": {"models": [m("kimi-coding", "kimi-mock", "openai-completions")]},
  "openrouter": {"models": [m("openrouter", "or-mock", "openai-completions")]},
}
for v in store.values(): v["checkedAt"] = 4102444800000
print(json.dumps(store))
EOF
}

case $cmd in
  down)
    for f in plexus trefoil mock1 mock2; do [[ -f $S/$f.pid ]] && kill "$(cat "$S/$f.pid")" 2>/dev/null || true; rm -f "$S/$f.pid"; done
    [[ -f $S/pg.id ]] && docker stop "$(cat "$S/pg.id")" >/dev/null 2>&1 || true; rm -f "$S/pg.id" ;;
  up)
    if ss -ltn | grep -q ":$PG_PORT "; then echo "port $PG_PORT busy" >&2; exit 1; fi
    docker run --rm -d --network host --tmpfs /var/lib/postgresql/data -e PGPORT=$PG_PORT -e POSTGRES_DB=trefoil -e POSTGRES_USER=trefoil -e POSTGRES_PASSWORD=trefoil postgres:16-alpine > "$S/pg.id"
    for i in $(seq 1 30); do docker exec "$(cat "$S/pg.id")" pg_isready -U trefoil -d trefoil >/dev/null 2>&1 && break; sleep 1; done; sleep 2
    rm -f "$S"/plexus.db*
    overlay $MOCK1 > "$S/models-store.json"
    overlay $MOCK2 > "$S/trefoil-models-store.json"
    start_plexus() { (cd "$T/.reference/plexus" && PLEXUS_MODEL_CATALOG_REFRESH=false ADMIN_KEY=admin DATABASE_URL=sqlite://$S/plexus.db PORT=$PLEXUS_PORT exec nohup bun run packages/backend/src/index.ts > "$S/plexus.log" 2>&1 & echo $! > "$S/plexus.pid"); }
    start_plexus
    (cd "$T" && PLEXUS_MODEL_CATALOG_REFRESH=false TREFOIL_MODELS_STORE_PATH=$S/trefoil-models-store.json TREFOIL_CREDENTIAL_KEY=$KEY DATABASE_URL=postgres://trefoil:trefoil@127.0.0.1:$PG_PORT/trefoil ADMIN_KEY=admin PORT=$TREFOIL_PORT exec nohup "$BIN" > "$S/trefoil.log" 2>&1 & echo $! > "$S/trefoil.pid")
    (exec nohup python3 "$O/mock_upstream.py" $MOCK1 > "$S/mock1.log" 2>&1 & echo $! > "$S/mock1.pid")
    (exec nohup python3 "$O/mock_upstream.py" $MOCK2 > "$S/mock2.log" 2>&1 & echo $! > "$S/mock2.pid")
    for i in $(seq 1 60); do curl -fs localhost:$PLEXUS_PORT/health >/dev/null && curl -fs localhost:$TREFOIL_PORT/healthz >/dev/null && break; sleep 1; done
    # Fake credentials: Plexus sqlite (loaded at startup → restart), Trefoil PG (encrypted, read per request).
    CODEX_JWT=$(python3 -c 'import base64,json;print("h."+base64.urlsafe_b64encode(json.dumps({"https://api.openai.com/auth":{"chatgpt_account_id":"acct-123"}}).encode()).decode().rstrip("=")+".s")')
    CREDS="anthropic:acct:sk-ant-oat01-fake openai-codex:acct:$CODEX_JWT meta:acct:meta-key xai:acct:xai-key kimi-coding:acct:kimi-key"
    python3 - "$S/plexus.db" $CREDS <<'EOF'
import sqlite3, sys, time
db = sqlite3.connect(sys.argv[1]); now = int(time.time() * 1000)
for c in sys.argv[2:]:
    p, a, tok = c.split(":", 2)
    db.execute("INSERT INTO oauth_credentials (oauth_provider_type, account_id, access_token, refresh_token, expires_at, created_at, updated_at) VALUES (?,?,?,?,?,?,?)", (p, a, tok, "refresh-" + p, 4102444800000, now, now))
db.commit()
EOF
    python3 - "$KEY" $CREDS <<'EOF' | docker exec -i "$(cat "$S/pg.id")" psql -q -U trefoil -d trefoil
import base64, os, struct, sys, time
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
key = bytes.fromhex(sys.argv[1]); now = int(time.time() * 1000)
def enc(p, a, field, text):
    aad = b"trefoil/oauth/v1" + b"".join(struct.pack(">Q", len(x.encode())) + x.encode() for x in (p, a, field))
    nonce = os.urandom(12)
    return "v1:" + base64.b64encode(nonce + AESGCM(key).encrypt(nonce, text.encode(), aad)).decode()
for c in sys.argv[2:]:
    p, a, tok = c.split(":", 2)
    print(f"INSERT INTO oauth_credentials (oauth_provider_type,account_id,access_token,refresh_token,expires_at,created_at,updated_at) VALUES ('{p}','{a}','{enc(p,a,'access_token',tok)}','{enc(p,a,'refresh_token','refresh-'+p)}',4102444800000,{now},{now});")
EOF
    kill "$(cat "$S/plexus.pid")"; sleep 1; start_plexus
    for i in $(seq 1 60); do curl -fs localhost:$PLEXUS_PORT/health >/dev/null && break; sleep 1; done
    echo up ;;
esac
