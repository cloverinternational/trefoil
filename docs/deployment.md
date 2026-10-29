# Deployment

The supported deployment is Docker Compose: a private PostgreSQL 16 database,
the Rust backend, and an nginx container that serves the admin UI and proxies
the API paths to the backend from a single origin.

```
client ──HTTP──▶ frontend (nginx :8080) ──▶ backend (:8080) ──▶ PostgreSQL
                 /            → admin UI (static)
                 /v0/ /v1/ /v1beta/ /mcp/ /oauth/ /.well-known/ /raw/ /healthz → backend
```

Only the frontend port is published. PostgreSQL and the backend are reachable
only on the Compose network.

## First run

```sh
cp .env.example .env
chmod 600 .env
# Fill in the secrets (or edit .env by hand):
sed -i "s/^ADMIN_KEY=.*/ADMIN_KEY=$(openssl rand -hex 32)/" .env
sed -i "s/^TREFOIL_CREDENTIAL_KEY=.*/TREFOIL_CREDENTIAL_KEY=$(openssl rand -hex 32)/" .env
sed -i "s/^ENCRYPTION_KEY=.*/ENCRYPTION_KEY=$(openssl rand -hex 32)/" .env
sed -i "s/^POSTGRES_PASSWORD=.*/POSTGRES_PASSWORD=$(openssl rand -hex 24)/" .env

docker compose up -d --build
docker compose ps
curl -fsS http://127.0.0.1:8080/healthz   # → ok
```

Open `http://<host>:8080/` and sign in with `ADMIN_KEY`. The backend applies
database migrations automatically at startup.

## Secrets

| Variable | Protects | Notes |
|---|---|---|
| `ADMIN_KEY` | Full administrative access (`x-admin-key` header, UI login) | Required. Long random value. |
| `TREFOIL_CREDENTIAL_KEY` | OAuth access and refresh tokens (AES-256-GCM, bound to provider/account/field) | Required to read or write OAuth credentials. Exactly 64 hex characters. |
| `ENCRYPTION_KEY` | Provider API keys, API key secrets and other sensitive settings (Plexus-compatible `enc:v1:` envelope) | Optional but recommended. 64 hex characters are used directly; any other value is stretched with scrypt. When unset, values are stored in plaintext and a warning is logged. When set, existing plaintext provider API keys and API key secrets are encrypted at startup. |
| `POSTGRES_PASSWORD` | The database | Required by Compose. |
| `MCP_SERVERS_DATA_DIR` / `DATA_DIR` | Persistent per-server `MCP_DATA_DIR` for local MCP servers (`<root>/<sha256 of server name>`, mode 0700; removed when the server is deleted) | Optional. Defaults to `<DATA_DIR or /app/data>/mcp-servers`; Compose mounts a `trefoil-data` volume at `/app/data`. |

Back up `TREFOIL_CREDENTIAL_KEY` and `ENCRYPTION_KEY` separately from the
database. Changing either value does not re-encrypt existing rows: data
encrypted with the old key becomes unreadable (OAuth credentials must then be
re-authorized). There is no built-in key rotation.

## Network exposure and TLS

The bundled nginx speaks plain HTTP. Admin keys, API keys and OAuth material
travel in headers, so:

- On a trusted network, restrict who can reach the port (firewall, or
  `TREFOIL_BIND_ADDRESS=127.0.0.1` to publish on loopback only).
- Anywhere else, put a TLS-terminating reverse proxy (Caddy, Traefik, nginx,
  a cloud load balancer) in front of the frontend port. Disable response
  buffering for `/v1/`, `/v1beta/` and `/mcp/` so streaming (SSE) responses are
  delivered incrementally, and allow long read timeouts.
- Never publish PostgreSQL.

## OAuth provider logins

OAuth logins are started from the admin UI. Flows differ by provider: some use
a device code, some ask you to paste a code, and the OpenAI Codex flow starts a
temporary loopback callback listener inside the backend (port 1455, host from
`PI_OAUTH_CALLBACK_HOST`, default `127.0.0.1`). In a container that listener is
not reachable from your browser; finish the login by pasting the final redirect
URL (or code) into the UI when prompted. See the OAuth caveat in the README
before using these providers.

## Backups and upgrades

- `GET /v0/management/backup` exports configuration (or a full archive);
  `POST /v0/management/restore` restores it. Both require the admin key.
- The named volume `trefoil-postgres` holds the live database.
  `docker compose down` keeps it; `docker compose down -v` deletes it.
- Before upgrading, take a backup and copy it off the host. Then:

  ```sh
  git pull
  docker compose up -d --build
  ```

  Migrations only move forward. To roll back, restore the previous source and
  a database backup taken before the upgrade.

## Operations

```sh
docker compose logs -f backend
docker compose restart backend
docker compose down
```

The backend log level is set with `LOG_LEVEL` (or `DEBUG=true`) and can be
changed at runtime from the admin UI.

## Running without Docker

```sh
cargo build --release --locked
DATABASE_URL=postgres://trefoil:<password>@127.0.0.1:5432/trefoil \
ADMIN_KEY=... TREFOIL_CREDENTIAL_KEY=... ENCRYPTION_KEY=... \
  ./target/release/trefoil-backend
```

The admin UI is a static build (`npm --prefix frontend ci && npm --prefix
frontend run build` produces `frontend/dist`). Serve it from any web server
that falls back to `index.html` for unknown paths and proxies the backend
paths listed above; `frontend/nginx.conf` is a working example. See the README
for the full environment variable reference.
