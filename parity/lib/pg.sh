# Disposable PostgreSQL launcher shared by the parity E2E scripts.
#
#   pg_start [docker run options...]
#
# Starts postgres:16-alpine detached (--rm) with the caller's options (env,
# tmpfs, name) and sets `pg_container` (container id) and `pg_port` (the host
# port to connect to on 127.0.0.1). By default 5432 is published on an
# ephemeral loopback port (or POSTGRES_PORT when set).
#
# POSTGRES_NETWORK=host is an opt-in for hosts whose Docker bridge networking
# is unreachable (as in parity/run.sh): the server joins the host network and
# listens on POSTGRES_PORT, which must then be an explicit free port. PGPORT
# is set in the container, so `docker exec pg_isready/psql` follow it.
pg_start() {
  if [[ "${POSTGRES_NETWORK:-}" == host ]]; then
    if [[ -z "${POSTGRES_PORT:-}" || "$POSTGRES_PORT" == 0 ]]; then
      echo "POSTGRES_NETWORK=host requires POSTGRES_PORT" >&2
      return 1
    fi
    pg_container=$(docker run --rm -d --network host -e PGPORT="$POSTGRES_PORT" "$@" postgres:16-alpine)
    pg_port=$POSTGRES_PORT
  else
    pg_container=$(docker run --rm -d "$@" -p "127.0.0.1:${POSTGRES_PORT:-}:5432" postgres:16-alpine)
    pg_port=$(docker port "$pg_container" 5432/tcp | sed -E 's/.*:([0-9]+)$/\1/')
  fi
}
