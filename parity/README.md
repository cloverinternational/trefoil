# Parity harnesses

Tools that check Trefoil (Rust) against a live Plexus (TypeScript) reference.
See [`docs/parity.md`](../docs/parity.md) for the overall approach, the pinned
reference revision, and how to set up `.reference/plexus`.

| Path | Purpose |
|---|---|
| `run.sh` | One-shot differential run: disposable PostgreSQL, both gateways, `differential.ts`. |
| `differential.ts` | HTTP differential harness (status code + structural JSON equality per named case). |
| `differential.test.ts` | Bun tests for the harness's report and inventory reconciliation. |
| `counts.py` | Read-only inventory: harness cases, Trefoil routes, Plexus OpenAPI operations, migrations. |
| `oracle/` | Long-lived Plexus/Trefoil pair with recording mock upstreams and scenario suites. |
| `*-e2e.sh`, `test-oauth-storage.sh` | Focused Trefoil-only end-to-end checks against a disposable PostgreSQL container. |

## Differential runner

With Docker, Bun and Cargo installed, from the repository root:

```sh
REFERENCE_DIR=.reference/plexus parity/run.sh
```

The runner starts a disposable PostgreSQL container on tmpfs, builds both
gateways (unless `SKIP_REFERENCE_BUILD=1` / `SKIP_TREFOIL_BUILD=1`), starts
temporary Plexus and Trefoil instances, waits for their health endpoints, runs
the harness, and tears everything down. Server logs are printed if either side
fails to become ready.

Overrides: `REFERENCE_DIR`, `PLEXUS_PORT` (4100), `TREFOIL_PORT` (8080),
`POSTGRES_PORT` (0 = pick a free port), `POSTGRES_NETWORK=host` (requires an
explicit `POSTGRES_PORT`), `PLEXUS_STORAGE=postgres` (run the reference on
PostgreSQL instead of SQLite), `ADMIN_KEY`, `DATABASE_URL`. The harness writes a
JSON report to `target/differential-report.json` (`PARITY_REPORT` overrides).

### Running the harness by hand

1. Start Plexus from the reference checkout (after `bun run build:frontend` and
   `bun run generate:openapi:asset`):
   ```sh
   ADMIN_KEY=correct-admin-key DATABASE_URL="sqlite://$PWD/plexus-parity.db" \
     PORT=4100 bun run packages/backend/src/index.ts
   ```
2. Start Trefoil against a disposable database:
   ```sh
   DATABASE_URL=postgres://trefoil:trefoil@127.0.0.1:5432/trefoil \
     ADMIN_KEY=correct-admin-key ./target/debug/trefoil-backend
   ```
3. Provision the fixture key on both gateways, then run the harness:
   ```sh
   for port in 4100 8080; do
     curl -X PUT http://127.0.0.1:$port/v0/management/keys/test-key \
       -H 'x-admin-key: correct-admin-key' -H 'Content-Type: application/json' \
       -d '{"secret":"sk-test-secret"}'
   done
   PLEXUS_BASE_URL=http://127.0.0.1:4100 TREFOIL_BASE_URL=http://127.0.0.1:8080 \
     bun run parity/differential.ts
   ```

Add new `Case` entries to `differential.ts` as endpoints are ported. The harness
reconciles its registered case inventory before declaring a run complete, so a
run that skips cases cannot be reported as complete.

Never point the harness at production gateways or use real provider
credentials. Provider-backed success and SSE cases require an explicitly
configured disposable upstream.

## Inventory counts

```sh
python3 parity/counts.py --reference-dir .reference/plexus > target/parity-counts.json
```

The output records both repository revisions, SHA-256 hashes of the harness
and OpenAPI input, and counts of named differential cases, Trefoil routes,
Plexus OpenAPI paths/operations, and migrations. It exits non-zero when a
required input is missing or malformed.

## Oracle suites

`oracle/stack.sh up` starts a long-lived Plexus (SQLite) and Trefoil
(PostgreSQL) pair plus two recording mock upstreams; `oracle/oracle.py
oracle/scenarios/<suite>.json --setup` replays each scenario against both
gateways and diffs both the client responses and the upstream requests each
gateway made. `oracle/oauth_stack.sh` starts an isolated pair for OAuth
dispatch scenarios using fake credentials and a mock-redirected catalog
overlay. Known, accepted differences are tracked in `oracle/GAPS.md`.

## Focused end-to-end scripts

Each script builds `trefoil-backend`, starts it against a throwaway PostgreSQL
container, exercises one area over HTTP, and cleans up. None of them contact a
real provider or touch `DATABASE_URL`.

| Script | Area |
|---|---|
| `alias-safeguard-e2e.sh` | Alias safeguard-approval column round trip and default |
| `logs-filter-e2e.sh` | Server-side usage-log filters (`/v0/management/usage`) |
| `mcp-system-logs-e2e.sh` | MCP system-log wrapper |
| `models-metadata-e2e.sh` | Alias-backed `/v1/models` metadata |
| `oauth-encryption-e2e.sh` | OAuth descriptors, pasted-callback validation, `ENCRYPTION_KEY` at rest |
| `pi-catalog-e2e.sh` | pi-ai provider catalog endpoints (`/v0/management/pi/*`) |
| `provider-contract-e2e.sh` | Provider response shape and OAuth validation |
| `provider-etag-e2e.sh` | Provider JSON and conditional (ETag) edits |
| `quota-registry-e2e.sh` | Built-in quota checker registry metadata |
| `test-oauth-storage.sh` | OAuth credential encryption, account binding, exchange/claim safety |
