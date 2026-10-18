# Trefoil

Trefoil is a self-hosted LLM API gateway and router written in Rust (axum,
sqlx, PostgreSQL) with a React admin UI. Clients talk to one endpoint using the
OpenAI, Anthropic or Gemini wire formats; Trefoil authenticates them, resolves
model aliases to provider targets, translates between API formats, fails over
between providers, enforces quotas, and records usage.

Trefoil is a port of [Plexus](https://github.com/mcowger/plexus) by Matt
Cowger. It is API compatible with Plexus — including the `/v0` management API —
and uses a PostgreSQL schema that keeps Plexus's table and column semantics, so
existing Plexus clients, scripts and the Plexus data model carry over.

> **Status:** 0.1.0, first public release. Expect rough edges; see
> [CHANGELOG.md](CHANGELOG.md).

## Features

**Inference API** (authenticated with an API key via `Authorization: Bearer`,
`x-api-key`, or `x-goog-api-key`)

| Endpoint | Format |
|---|---|
| `POST /v1/chat/completions` | OpenAI Chat Completions (streaming and non-streaming) |
| `POST /v1/completions` | OpenAI legacy Completions |
| `POST /v1/messages` | Anthropic Messages |
| `POST /v1/responses`, `GET`/`DELETE /v1/responses/{id}`, `GET /v1/conversations/{id}` | OpenAI Responses |
| `POST /v1beta/models/{model}:generateContent` / `:streamGenerateContent` | Google Gemini |
| `POST /v1/embeddings` | Embeddings |
| `POST /v1/images/generations`, `POST /v1/images/edits` | Image generation and editing |
| `POST /v1/audio/speech`, `POST /v1/audio/transcriptions` | Text to speech, speech to text |
| `POST /v1/decisions` | Plexus decisions endpoint |
| `GET /v1/models`, `GET /v1/metadata/search`, `GET /v1/metadata/lookup` | Model listing and metadata |

Any input format can be routed to a provider speaking a different format;
Trefoil translates requests, responses and SSE streams between them.

**Routing**

- Model aliases with target groups and selectors
  (`in_order`, `random`, `cost`, `latency`, `performance`, `usage`), sticky
  sessions, and aliases as fallback targets.
- Automatic failover and retries across targets, provider cooldowns after
  errors and rate limits, stall detection, and per-provider concurrency limits.
- Per-key access policies (allowed models/providers), key expiry, and user
  quotas; built-in and custom quota checkers for provider subscriptions.
- OAuth-backed providers (see the caveat below) next to ordinary API-key
  providers, plus a model catalog sourced from pi-ai with optional refresh.

**Operations**

- `/v0` management API (keys, providers, aliases, quotas, cooldowns, config,
  backup/restore, logging, debug capture, MCP servers, OAuth sessions), with an
  OpenAPI document at `/.well-known/plexus/openapi.json`.
- MCP: proxy to upstream MCP servers (HTTP or local stdio) at `/mcp/{name}`
  with API-key or OAuth (`mcp:read`/`mcp:write`) access, and a management MCP
  server at `/mcp/plexus`.
- Usage records with token counts, cost, TTFT and throughput; error and debug
  logs; provider performance; Prometheus metrics at `/v0/management/metrics`.
- Admin UI: providers, models, aliases, keys, quotas, MCP, usage analytics
  with saved dashboards, logs, errors, system logs, and a playground.

## Quick start (Docker Compose)

Requirements: Docker with Compose v2 and `openssl`.

```sh
git clone https://github.com/cloverinternational/trefoil.git
cd trefoil
cp .env.example .env && chmod 600 .env
sed -i "s/^ADMIN_KEY=.*/ADMIN_KEY=$(openssl rand -hex 32)/" .env
sed -i "s/^TREFOIL_CREDENTIAL_KEY=.*/TREFOIL_CREDENTIAL_KEY=$(openssl rand -hex 32)/" .env
sed -i "s/^ENCRYPTION_KEY=.*/ENCRYPTION_KEY=$(openssl rand -hex 32)/" .env
sed -i "s/^POSTGRES_PASSWORD=.*/POSTGRES_PASSWORD=$(openssl rand -hex 24)/" .env
docker compose up -d --build
curl -fsS http://127.0.0.1:8080/healthz   # → ok
```

Open <http://127.0.0.1:8080/>, sign in with the `ADMIN_KEY` from `.env`, add a
provider and an API key, then point any OpenAI-compatible client at
`http://127.0.0.1:8080/v1`.

The stack serves plain HTTP. Read [docs/deployment.md](docs/deployment.md)
before exposing it beyond a trusted network.

## Configuration

All configuration other than the variables below (providers, aliases, keys,
quotas, settings) lives in PostgreSQL and is managed through the UI or the
`/v0` API.

| Variable | Required | Default | Description |
|---|---|---|---|
| `DATABASE_URL` | yes | — | PostgreSQL connection string. Compose builds it from the `POSTGRES_*` values. |
| `ADMIN_KEY` | yes | — | Admin credential for the management API and UI (`x-admin-key`). Without it, no admin access is possible. |
| `TREFOIL_CREDENTIAL_KEY` | for OAuth | — | 64 hex characters; AES-256-GCM key for stored OAuth tokens. |
| `ENCRYPTION_KEY` | recommended | unset | Encrypts provider API keys, key secrets and sensitive settings at rest. 64 hex characters are used directly; other strings are derived with scrypt. Unset means plaintext storage (a warning is logged). |
| `PORT` | no | `8080` | Backend listen port (binds `0.0.0.0`). |
| `DATABASE_MAX_CONNECTIONS` | no | `16` | PostgreSQL pool size. |
| `LOG_LEVEL` | no | `info` | `error`, `warn`, `info`, `debug`, `verbose` or `silly`. |
| `DEBUG` | no | — | `true` sets the startup log level to `debug` when `LOG_LEVEL` is unset. |
| `PLEXUS_MODEL_CATALOG_REFRESH` | no | enabled | `false` disables background refresh of the pi-ai model catalog from pi.dev. |
| `TREFOIL_MODELS_STORE_PATH` | no | in memory | JSON file that persists the refreshed model catalog overlay across restarts. |
| `PLEXUS_METRICS_CACHE_TTL_MS` | no | `15000` | Cache lifetime for the Prometheus metrics response. |
| `PLEXUS_PROVIDER_PERFORMANCE_RETENTION_LIMIT` | no | `100` | Performance samples retained per provider/model. |
| `PI_OAUTH_CALLBACK_HOST` | no | `127.0.0.1` | Host for temporary OAuth loopback callback listeners. |
| `TREFOIL_OAUTH_DEVICE_ID` | no | generated | Device identifier sent during the OpenAI (ChatGPT) OAuth login. |
| `KIMI_CODE_OAUTH_HOST`, `KIMI_OAUTH_HOST` | no | `https://auth.kimi.com` | Override the Kimi Code OAuth host. |
| `TREFOIL_UI_DIR` | no | `frontend/dist` | Directory for the Plexus-compatible `/ui/` static route. |

Compose-only variables (`.env`): `POSTGRES_DB` (default `trefoil`),
`POSTGRES_USER` (default `trefoil`), `POSTGRES_PASSWORD` (required),
`TREFOIL_BIND_ADDRESS` (default `0.0.0.0`) and `TREFOIL_PORT` (default `8080`)
for the published UI/API port.

## Migrating from Plexus

`migration/plexus_to_trefoil.py` copies a Plexus SQLite database into a Trefoil
PostgreSQL database: OAuth credentials, providers, models, aliases, API keys,
user quotas, MCP servers and settings, plus optional history
(`--with-history`). It decrypts Plexus-encrypted values with Plexus's
`ENCRYPTION_KEY` and re-encrypts OAuth tokens with `TREFOIL_CREDENTIAL_KEY`.

```sh
pip install cryptography
python3 migration/plexus_to_trefoil.py --db plexus-copy.db --dsn "$DATABASE_URL" --apply
```

Use a copy of `plexus.db` taken while Plexus is stopped, start Trefoil with the
same `ENCRYPTION_KEY` afterwards, and read the script header for all options.
Clients keep working unchanged: the inference and `/v0` paths are the same.

## Architecture

```
backend/            Rust service (crate trefoil-backend)
  src/engine/       inference engine: API translation, routing, dispatch,
                    failover, OAuth dispatch, streaming, usage accounting
  src/*.rs          management API, auth, OAuth logins, MCP, quotas, logs
  migrations/       PostgreSQL schema (Plexus lineage + Trefoil additions)
frontend/           React + Vite admin UI, served by nginx (nginx.conf)
migration/          Plexus → Trefoil data migration
parity/             differential and oracle tests against Plexus
docs/               deployment and parity documentation
```

The backend is a single process. At startup it runs migrations, encrypts any
plaintext provider API keys and API key secrets when `ENCRYPTION_KEY` is set, loads keys, providers,
aliases, quotas, settings and cooldowns into memory, and starts background
schedulers (OAuth token refresh, model catalog and metadata refresh, custom
quota checkers). Writes through the management API update PostgreSQL and the
in-memory state together.

## Development

Requirements: Rust 1.88+, Node.js 22, Docker (for PostgreSQL and the e2e
scripts).

```sh
# PostgreSQL for local development
docker run -d --name trefoil-dev-db -p 127.0.0.1:5432:5432 \
  -e POSTGRES_DB=trefoil -e POSTGRES_USER=trefoil -e POSTGRES_PASSWORD=trefoil \
  postgres:16-alpine

# Backend on :8080
DATABASE_URL=postgres://trefoil:trefoil@127.0.0.1:5432/trefoil \
ADMIN_KEY=dev-admin-key TREFOIL_CREDENTIAL_KEY=$(openssl rand -hex 32) \
  cargo run -p trefoil-backend --bin trefoil-backend

# Frontend on :5173 (Vite proxies /v0 and /v1 to :8080)
npm --prefix frontend ci
npm --prefix frontend run dev
```

Checks run by CI:

```sh
cargo fmt --all --check
cargo clippy --workspace --locked -- -D warnings
cargo test --workspace --locked --lib
npm --prefix frontend run typecheck
npm --prefix frontend test
npm --prefix frontend run build
```

Database-backed tests are opt-in: `parity/test-oauth-storage.sh` and the other
`parity/*-e2e.sh` scripts each start a throwaway PostgreSQL container.
Playwright UI tests live in `frontend/e2e/` (`npm --prefix frontend run test:e2e`).
The `*-live` specs drive a running stack and are skipped unless
`TREFOIL_E2E_BASE_URL` is set (e.g. `http://127.0.0.1:8080`). Also set
`TREFOIL_E2E_MOCK_HOST` (the address the backend
container uses to reach mock upstreams on the test host; default
`host.docker.internal`, on Linux usually the Compose network gateway), and
`TREFOIL_E2E_PLAYGROUND_MODEL` for the playground spec.

## Parity with Plexus

Compatibility is checked by running Trefoil and a pinned Plexus revision side
by side and diffing both their responses and the requests they send upstream.
See [docs/parity.md](docs/parity.md) for the method, setup and known
differences.

## Security

- Report vulnerabilities privately; see [SECURITY.md](SECURITY.md).
- Generate every secret with `openssl rand` and keep `.env` out of version
  control. Set `ENCRYPTION_KEY` so provider credentials are encrypted at rest.
- The bundled stack is HTTP only. Use a TLS-terminating reverse proxy for any
  untrusted network, and never publish PostgreSQL.
- API keys can be restricted to specific models and providers and given
  expiry dates and quotas; prefer narrowly scoped keys over sharing `ADMIN_KEY`.

## OAuth providers and terms of service

Trefoil can use subscription accounts as providers through OAuth, inherited
from Plexus and pi-ai: Anthropic (Claude Pro/Max), OpenAI Codex / ChatGPT,
GitHub Copilot, Kimi Code, xAI (Grok), Meta (Muse) and OpenRouter.

Several of these work by presenting Trefoil as the provider's official client.
For example, Anthropic OAuth requests carry Claude Code's user agent, beta
headers and system prompt; OpenAI Codex requests use the Codex CLI's user agent
and originator; and GitHub Copilot requests use Copilot Chat's editor headers.
The Kimi Code, xAI and Meta logins use OAuth client IDs embedded from pi-ai
rather than a client registered by you. Using a subscription this way may
violate the provider's terms of service and can lead to account suspension.

These integrations are provided as-is for compatibility with Plexus. You are
solely responsible for making sure your use complies with each provider's
terms. If in doubt, use the provider's official API with an API key instead.

## License

Trefoil is released under the [MIT License](LICENSE). It includes code and data
derived from Plexus (MIT, Copyright (c) 2025 Matt Cowger) and pi-ai (MIT); see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
