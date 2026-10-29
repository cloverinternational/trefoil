#!/usr/bin/env bash
# E2E for migration/plexus_to_trefoil.py against the real Plexus reference.
#
# Boots a disposable PostgreSQL, the Plexus checkout in .reference/plexus
# (SQLite, with ENCRYPTION_KEY so secrets are encrypted at rest) and Trefoil,
# seeds Plexus, migrates, then checks that Trefoil's config export equals
# Plexus's exactly, both right after the migration and after a Trefoil restart
# (which reloads everything from PostgreSQL). Also checks secrets are encrypted
# at rest in Trefoil and that re-running with --yes is idempotent.
#
# Needs docker, bun, cargo, curl. Overrides: REFERENCE_DIR, POSTGRES_NETWORK=host
# with POSTGRES_PORT (see lib/pg.sh).
set -Eeuo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REFERENCE_DIR=${REFERENCE_DIR:-$ROOT/.reference/plexus}
cd "$ROOT"
for tool in docker bun cargo curl python3; do command -v "$tool" >/dev/null || { echo "$tool is required" >&2; exit 1; }; done
[[ -d "$REFERENCE_DIR" ]] || { echo "reference checkout not found: $REFERENCE_DIR" >&2; exit 1; }
cargo build --locked -q --bin trefoil-backend
# shellcheck source=lib/pg.sh
source "$ROOT/parity/lib/pg.sh"
pg_start --tmpfs /var/lib/postgresql/data \
  -e POSTGRES_DB=trefoil -e POSTGRES_USER=trefoil -e POSTGRES_PASSWORD=trefoil
container=$pg_container
tmp=$(mktemp -d)
plexus_pid= trefoil_pid=
stop_trefoil() { [[ -n "$trefoil_pid" ]] && { kill "$trefoil_pid" 2>/dev/null || :; wait "$trefoil_pid" 2>/dev/null || :; trefoil_pid=; }; }
cleanup() {
  stop_trefoil
  [[ -n "$plexus_pid" ]] && { kill "$plexus_pid" 2>/dev/null || :; wait "$plexus_pid" 2>/dev/null || :; }
  docker stop "$container" >/dev/null 2>&1 || :
  rm -rf "$tmp"
}
trap cleanup EXIT
for i in {1..60}; do docker exec "$container" pg_isready -U trefoil -d trefoil >/dev/null 2>&1 && break; sleep 1; done

free_port() { python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])'; }
plexus_port=$(free_port); trefoil_port=$(free_port)
enc=$(python3 -c 'print("a"*64)'); cred=$(python3 -c 'print("b"*64)')
export PLEXUS_ADMIN_KEY=e2e-plexus-admin TREFOIL_ADMIN_KEY=e2e-trefoil-admin
db="postgres://trefoil:trefoil@127.0.0.1:$pg_port/trefoil"

wait_for() {
  for i in {1..90}; do curl -fsS --max-time 2 "$1" >/dev/null 2>&1 && return 0; sleep 1; done
  return 1
}
start_trefoil() {
  DATABASE_URL="$db" ADMIN_KEY="$TREFOIL_ADMIN_KEY" ENCRYPTION_KEY="$enc" TREFOIL_CREDENTIAL_KEY="$cred" \
    PORT="$trefoil_port" ./target/debug/trefoil-backend >"$tmp/trefoil.log" 2>&1 & trefoil_pid=$!
  wait_for "http://127.0.0.1:$trefoil_port/healthz" || { cat "$tmp/trefoil.log" >&2; echo 'Trefoil failed to start' >&2; exit 1; }
}

(cd "$REFERENCE_DIR" && ADMIN_KEY="$PLEXUS_ADMIN_KEY" ENCRYPTION_KEY="$enc" \
  DATABASE_URL="sqlite://$tmp/plexus.db" PORT="$plexus_port" \
  bun run packages/backend/src/index.ts) >"$tmp/plexus.log" 2>&1 & plexus_pid=$!
wait_for "http://127.0.0.1:$plexus_port/health" || { cat "$tmp/plexus.log" >&2; echo 'Plexus failed to start' >&2; exit 1; }
start_trefoil

plexus="http://127.0.0.1:$plexus_port" trefoil="http://127.0.0.1:$trefoil_port"
python3 parity/lib/seed_plexus.py "$plexus" "$PLEXUS_ADMIN_KEY"
python3 migration/plexus_to_trefoil.py --plexus-url "$plexus" --trefoil-url "$trefoil"
echo '--- re-run without --yes must refuse, with --yes must succeed (idempotent)'
if python3 migration/plexus_to_trefoil.py --plexus-url "$plexus" --trefoil-url "$trefoil" >/dev/null 2>&1; then
  echo 'expected refusal against a populated Trefoil' >&2; exit 1
fi
python3 migration/plexus_to_trefoil.py --plexus-url "$plexus" --trefoil-url "$trefoil" --yes >/dev/null

compare() {
  python3 - "$plexus" "$trefoil" "$1" <<'PY'
import json, sys, urllib.request, os
def export(url, key):
    req = urllib.request.Request(url + "/v0/management/backup", headers={"x-admin-key": key})
    return json.load(urllib.request.urlopen(req, timeout=30))["data"]
a = export(sys.argv[1], os.environ["PLEXUS_ADMIN_KEY"])
b = export(sys.argv[2], os.environ["TREFOIL_ADMIN_KEY"])
# Trefoil materialises the `shared` column default when reloading from PostgreSQL.
for quota in b.get("user_quotas", {}).values():
    if quota.get("shared") is False:
        del quota["shared"]
bad = [k for k in a if a[k] != b.get(k)]
if bad:
    for k in bad:
        print(f"MISMATCH {k}\n  plexus : {json.dumps(a[k], sort_keys=True)[:400]}\n  trefoil: {json.dumps(b.get(k), sort_keys=True)[:400]}")
    sys.exit(1)
print(f"{sys.argv[3]}: Trefoil config export equals Plexus ({len(a)} sections)")
PY
}
compare "after migration"

echo '--- secrets must be encrypted at rest'
q() { docker exec "$container" psql -U trefoil -d trefoil -Atc "$1"; }
[[ $(q "select count(*) from providers where api_key like 'enc:v1:%'") == 2 ]] || { echo 'provider api_key not encrypted' >&2; exit 1; }
[[ $(q "select count(*) from api_keys where secret like 'enc:v1:%'") == 1 ]] || { echo 'api key secret not encrypted' >&2; exit 1; }
[[ $(q "select count(*) from oauth_credentials where access_token like 'v1:%' and refresh_token like 'v1:%'") == 1 ]] || { echo 'oauth tokens not encrypted' >&2; exit 1; }

echo '--- --with-history: generate history in Plexus (requests fail upstream), migrate the full archive'
key=$(curl -fsS -H "x-admin-key: $PLEXUS_ADMIN_KEY" "$plexus/v0/management/keys" |
  python3 -c 'import sys,json;print(json.load(sys.stdin)["team-a"]["secret"])')
for i in 1 2 3; do
  curl -s -o /dev/null -H "Authorization: Bearer $key" -H 'content-type: application/json' \
    "$plexus/v1/chat/completions" -d '{"model":"fast","messages":[{"role":"user","content":"hi"}]}' || :
done
sleep 3
expected_usage=$(python3 - "$plexus" <<'PY'
import csv, gzip, io, json, os, sys, tarfile, urllib.request
req = urllib.request.Request(sys.argv[1] + "/v0/management/backup?full=true",
                             headers={"x-admin-key": os.environ["PLEXUS_ADMIN_KEY"]})
tar = tarfile.open(fileobj=io.BytesIO(urllib.request.urlopen(req, timeout=60).read()), mode="r:gz")
text = gzip.decompress(tar.extractfile("request_usage.csv.gz").read()).decode()
print(sum(1 for _ in csv.DictReader(io.StringIO(text))))
PY
)
[[ "$expected_usage" -ge 1 ]] || { echo 'Plexus recorded no usage rows to migrate' >&2; exit 1; }
python3 migration/plexus_to_trefoil.py --plexus-url "$plexus" --trefoil-url "$trefoil" --with-history --yes
[[ $(q "select count(*) from request_usage") == "$expected_usage" ]] ||
  { echo "request_usage rows differ (expected $expected_usage)" >&2; exit 1; }
compare "after history migration"

stop_trefoil
start_trefoil
compare "after Trefoil restart"
echo 'plexus-migration-e2e: OK'
