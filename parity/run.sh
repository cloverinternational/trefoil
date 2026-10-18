#!/usr/bin/env bash
set -Eeuo pipefail

# Run the complete differential check from a clean checkout.  The reference
# checkout is deliberately configurable because it is not part of this repo.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REFERENCE_DIR=${REFERENCE_DIR:-"$ROOT/.reference/plexus"}
PLEXUS_PORT=${PLEXUS_PORT:-4100}
TREFOIL_PORT=${TREFOIL_PORT:-8080}
POSTGRES_PORT=${POSTGRES_PORT:-0}
COMPOSE_PROJECT_NAME=${COMPOSE_PROJECT_NAME:-trefoil-parity-${RANDOM}${RANDOM}}
ADMIN_KEY=${ADMIN_KEY:-correct-admin-key}
DATABASE_URL=${DATABASE_URL:-postgres://trefoil:trefoil@127.0.0.1:${POSTGRES_PORT}/trefoil}
SKIP_REFERENCE_BUILD=${SKIP_REFERENCE_BUILD:-0}
SKIP_TREFOIL_BUILD=${SKIP_TREFOIL_BUILD:-0}

tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/trefoil-parity.XXXXXX")
plexus_log="$tmp_dir/plexus.log"
trefoil_log="$tmp_dir/trefoil.log"
plexus_pid=
trefoil_pid=

cleanup() {
  set +e
  [[ -n "$plexus_pid" ]] && kill "$plexus_pid" 2>/dev/null
  [[ -n "$trefoil_pid" ]] && kill "$trefoil_pid" 2>/dev/null
  [[ -n "$plexus_pid" ]] && wait "$plexus_pid" 2>/dev/null
  [[ -n "$trefoil_pid" ]] && wait "$trefoil_pid" 2>/dev/null
  [[ -n "${postgres_id:-}" ]] && docker stop "$postgres_id" >/dev/null 2>&1
  rm -rf "$tmp_dir"
}
trap cleanup EXIT INT TERM

die() { echo "parity runner: $*" >&2; exit 1; }
command -v bun >/dev/null || die "bun is required"
command -v cargo >/dev/null || die "cargo is required"
command -v curl >/dev/null || die "curl is required"
command -v docker >/dev/null || die "docker is required"
[[ -d "$REFERENCE_DIR" ]] || die "reference checkout not found: $REFERENCE_DIR"

echo "Starting PostgreSQL..."
# Run an isolated disposable database. Do not use compose --service-ports:
# its service mapping and -p override collide when both publish 5432.
# POSTGRES_NETWORK=host is an opt-in for hosts whose Docker bridge is
# unreachable; it needs an explicit free POSTGRES_PORT.
if [[ "${POSTGRES_NETWORK:-}" == host ]]; then
  [[ "$POSTGRES_PORT" != 0 ]] || die "POSTGRES_NETWORK=host requires POSTGRES_PORT"
  postgres_id=$(docker run --rm -d --network host --tmpfs /var/lib/postgresql/data \
    -e PGPORT="$POSTGRES_PORT" -e POSTGRES_DB=trefoil -e POSTGRES_USER=trefoil -e POSTGRES_PASSWORD=trefoil \
    postgres:16-alpine)
else
  postgres_id=$(docker run --rm -d --tmpfs /var/lib/postgresql/data \
    -e POSTGRES_DB=trefoil -e POSTGRES_USER=trefoil -e POSTGRES_PASSWORD=trefoil \
    -p "127.0.0.1:${POSTGRES_PORT}:5432" postgres:16-alpine)
fi
if [[ "$POSTGRES_PORT" == 0 ]]; then
  POSTGRES_PORT=$(docker port "$postgres_id" 5432/tcp | sed -E 's/.*:([0-9]+)$/\1/')
  [[ -n "$POSTGRES_PORT" ]] || die "failed to determine PostgreSQL host port"
  DATABASE_URL="postgres://trefoil:trefoil@127.0.0.1:${POSTGRES_PORT}/trefoil"
fi

for attempt in {1..60}; do
  if docker exec "$postgres_id" pg_isready -U trefoil -d trefoil >/dev/null 2>&1; then break; fi
  [[ "$attempt" == 60 ]] && die "PostgreSQL did not become ready"
  sleep 1
done

# PLEXUS_STORAGE=postgres runs the reference on its own database in the same
# PostgreSQL server (default: SQLite file), so both sides share storage semantics.
PLEXUS_DATABASE_URL="sqlite://$tmp_dir/plexus.db"
if [[ "${PLEXUS_STORAGE:-sqlite}" == postgres ]]; then
  docker exec "$postgres_id" psql -U trefoil -d trefoil -c "CREATE DATABASE plexus" >/dev/null
  PLEXUS_DATABASE_URL="postgres://trefoil:trefoil@127.0.0.1:${POSTGRES_PORT}/plexus"
fi

if [[ "$SKIP_REFERENCE_BUILD" != 1 ]]; then
  echo "Building Plexus reference..."
  (cd "$REFERENCE_DIR" && bun install --frozen-lockfile --ignore-scripts && bun run build:frontend && bun run generate:openapi:asset)
fi
if [[ "$SKIP_TREFOIL_BUILD" != 1 ]]; then
  echo "Building Trefoil..."
  (cd "$ROOT" && cargo build --bin trefoil-backend)
fi

echo "Starting Plexus and Trefoil..."
(
  cd "$REFERENCE_DIR"
  ADMIN_KEY="$ADMIN_KEY" DATABASE_URL="$PLEXUS_DATABASE_URL" PORT="$PLEXUS_PORT" \
    bun run packages/backend/src/index.ts
) >"$plexus_log" 2>&1 & plexus_pid=$!
(
  cd "$ROOT"
  DATABASE_URL="$DATABASE_URL" ADMIN_KEY="$ADMIN_KEY" PORT="$TREFOIL_PORT" \
    ./target/debug/trefoil-backend
) >"$trefoil_log" 2>&1 & trefoil_pid=$!

wait_for() {
  local url=$1 pid=$2
  for attempt in {1..60}; do
    curl --silent --fail "$url" >/dev/null 2>&1 && return 0
    kill -0 "$pid" 2>/dev/null || return 1
    sleep 1
  done
  return 1
}
wait_for "http://127.0.0.1:$PLEXUS_PORT/health" "$plexus_pid" || {
  cat "$plexus_log" >&2; die "Plexus did not become ready"
}
wait_for "http://127.0.0.1:$TREFOIL_PORT/healthz" "$trefoil_pid" || {
  cat "$trefoil_log" >&2; die "Trefoil did not become ready"
}

echo "Running differential harness..."
(cd "$ROOT" && PLEXUS_BASE_URL="http://127.0.0.1:$PLEXUS_PORT" \
  TREFOIL_BASE_URL="http://127.0.0.1:$TREFOIL_PORT" bun run parity/differential.ts)
