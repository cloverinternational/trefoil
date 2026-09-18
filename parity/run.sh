#!/usr/bin/env bash
set -Eeuo pipefail

# Run the complete differential check from a clean checkout.  The reference
# checkout is deliberately configurable because it is not part of this repo.
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
REFERENCE_DIR=${REFERENCE_DIR:-"$ROOT/.reference/plexus"}
PLEXUS_PORT=${PLEXUS_PORT:-4100}
TREFOIL_PORT=${TREFOIL_PORT:-8080}
POSTGRES_PORT=${POSTGRES_PORT:-5432}
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
  docker compose -p "$COMPOSE_PROJECT_NAME" -f "$ROOT/docker-compose.yml" down -v >/dev/null 2>&1
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
# The parity database is disposable; remove the named volume so every run
# starts from the same migration state.
docker compose -p "$COMPOSE_PROJECT_NAME" -f "$ROOT/docker-compose.yml" down -v >/dev/null 2>&1
postgres_id=$(docker compose -p "$COMPOSE_PROJECT_NAME" -f "$ROOT/docker-compose.yml" run -d --service-ports -p "$POSTGRES_PORT:5432" postgres)

for attempt in {1..60}; do
  if docker exec "$postgres_id" pg_isready -U trefoil -d trefoil >/dev/null 2>&1; then break; fi
  [[ "$attempt" == 60 ]] && die "PostgreSQL did not become ready"
  sleep 1
done

if [[ "$SKIP_REFERENCE_BUILD" != 1 ]]; then
  echo "Building Plexus reference..."
  (cd "$REFERENCE_DIR" && bun run build:frontend && bun run generate:openapi:asset)
fi
if [[ "$SKIP_TREFOIL_BUILD" != 1 ]]; then
  echo "Building Trefoil..."
  (cd "$ROOT" && cargo build --bin trefoil-backend)
fi

echo "Starting Plexus and Trefoil..."
(
  cd "$REFERENCE_DIR"
  ADMIN_KEY="$ADMIN_KEY" DATABASE_URL="sqlite://$tmp_dir/plexus.db" PORT="$PLEXUS_PORT" \
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
