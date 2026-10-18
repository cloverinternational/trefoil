#!/usr/bin/env bash
# Long-lived Plexus (14200, sqlite) + Trefoil (18200, host-network PG 55450)
# pair plus two recording mock upstreams (19001 for Plexus, 19002 for
# Trefoil) for differential probing. State lives in $ORACLE_STATE.
#   stack.sh up | down | restart-trefoil
set -euo pipefail
T=$(cd "$(dirname "$0")/../.." && pwd)
O=$(cd "$(dirname "$0")" && pwd)
S=${ORACLE_STATE:-/tmp/trefoil-oracle}
mkdir -p "$S"
cmd=${1:-up}
start_trefoil() {
  (cd "$T" && DATABASE_URL=postgres://trefoil:trefoil@127.0.0.1:55450/trefoil ADMIN_KEY=admin PORT=18200 exec nohup ./target/debug/trefoil-backend > "$S/trefoil.log" 2>&1 & echo $! > "$S/trefoil.pid")
}
wait_up() {
  for i in $(seq 1 60); do curl -fs localhost:14200/health >/dev/null && curl -fs localhost:18200/healthz >/dev/null && return 0; sleep 1; done
  echo "stack did not become ready" >&2; return 1
}
case $cmd in
  down)
    for f in plexus trefoil mock1 mock2; do [[ -f $S/$f.pid ]] && kill "$(cat "$S/$f.pid")" 2>/dev/null || true; rm -f "$S/$f.pid"; done
    [[ -f $S/pg.id ]] && docker stop "$(cat "$S/pg.id")" >/dev/null 2>&1 || true; rm -f "$S/pg.id" ;;
  restart-trefoil)
    [[ -f $S/trefoil.pid ]] && kill "$(cat "$S/trefoil.pid")" 2>/dev/null || true; sleep 1; start_trefoil; wait_up ;;
  up)
    docker run --rm -d --network host --tmpfs /var/lib/postgresql/data -e PGPORT=55450 -e POSTGRES_DB=trefoil -e POSTGRES_USER=trefoil -e POSTGRES_PASSWORD=trefoil postgres:16-alpine > "$S/pg.id"
    for i in $(seq 1 30); do docker exec "$(cat "$S/pg.id")" pg_isready -U trefoil -d trefoil >/dev/null 2>&1 && break; sleep 1; done; sleep 2
    rm -f "$S"/plexus.db*
    (cd "$T/.reference/plexus" && ADMIN_KEY=admin DATABASE_URL=sqlite://$S/plexus.db PORT=14200 exec nohup bun run packages/backend/src/index.ts > "$S/plexus.log" 2>&1 & echo $! > "$S/plexus.pid")
    start_trefoil
    (exec nohup python3 "$O/mock_upstream.py" 19001 > "$S/mock1.log" 2>&1 & echo $! > "$S/mock1.pid")
    (exec nohup python3 "$O/mock_upstream.py" 19002 > "$S/mock2.log" 2>&1 & echo $! > "$S/mock2.pid")
    wait_up
    for b in 14200 18200; do curl -s -XPUT localhost:$b/v0/management/keys/k -H 'x-admin-key: admin' -H 'content-type: application/json' -d '{"secret":"sk-k"}' >/dev/null; done
    echo up ;;
esac
