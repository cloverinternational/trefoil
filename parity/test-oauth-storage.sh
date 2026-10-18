#!/usr/bin/env bash
# Disposable integration gate; never touches DATABASE_URL or existing volumes.
set -Eeuo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
container=
cleanup() { [[ -z "$container" ]] || docker stop "$container" >/dev/null; }
trap cleanup EXIT
# shellcheck source=lib/pg.sh
source "$ROOT/parity/lib/pg.sh"
pg_start --tmpfs /var/lib/postgresql/data \
  -e POSTGRES_PASSWORD=test-only -e POSTGRES_USER=trefoil -e POSTGRES_DB=trefoil
container=$pg_container
port=$pg_port
for attempt in {1..30}; do
  if docker exec "$container" pg_isready -U trefoil >/dev/null 2>&1; then break; fi
  if [[ "$attempt" == 30 ]]; then echo 'Disposable PostgreSQL did not start' >&2; exit 1; fi
  sleep 1
done
TREFOIL_TEST_DATABASE_URL="postgres://trefoil:test-only@127.0.0.1:$port/trefoil" \
TREFOIL_CREDENTIAL_KEY=$(printf '11%.0s' {1..32}) \
  cargo test --manifest-path "$ROOT/Cargo.toml" --test oauth_credentials -- --ignored --test-threads=1
