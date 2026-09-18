# Differential parity harness

Verifies Trefoil (Rust) responses match a live Plexus (TS) instance for
ported endpoints, byte-for-byte on status code and structural JSON equality.

## Usage

### One-shot runner (recommended)

With Docker, Bun, and Cargo installed, run the complete check from the
repository root:

```sh
REFERENCE_DIR=.reference/plexus parity/run.sh
```

The runner resets and tears down its disposable PostgreSQL volume, builds both backends, starts
temporary Plexus and Trefoil instances, waits for their health endpoints, and invokes the
differential harness. Logs are printed if either server fails to become ready.
The `.reference/plexus` checkout is intentionally external to this repository.
Set `SKIP_REFERENCE_BUILD=1` or `SKIP_TREFOIL_BUILD=1` when using already-built
artifacts. `REFERENCE_DIR`, `PLEXUS_PORT`, `TREFOIL_PORT`, `ADMIN_KEY`, and
`DATABASE_URL` may also be overridden.

1. Start Plexus (from `.reference/plexus`, after `bun run build:frontend` and
   `generate:openapi:asset`):
   ```
   ADMIN_KEY=correct-admin-key DATABASE_URL="sqlite:///tmp/plexus-data/plexus.db" \
     PORT=4100 bun run packages/backend/src/index.ts
   ```
   Then provision the fixture key used by the harness:
   ```
   curl -X PUT http://127.0.0.1:4100/v0/management/keys/test-key \
     -H 'x-admin-key: correct-admin-key' -H 'Content-Type: application/json' \
     -d '{"secret":"sk-test-secret"}'
   ```
2. Start Trefoil:
   ```
   DATABASE_URL=postgres://trefoil:trefoil@127.0.0.1:5432/trefoil \
   ADMIN_KEY=correct-admin-key ./target/debug/trefoil-backend
   ```
   Trefoil loads its API keys, providers, aliases, quotas, settings, and
   cooldowns from PostgreSQL. Provision the fixture key in Trefoil's database
   before running the harness, or use the management PUT endpoint with the
   admin key.
3. Run the harness:
   ```
   PLEXUS_BASE_URL=http://127.0.0.1:4100 TREFOIL_BASE_URL=http://127.0.0.1:8080 \
     bun run parity/differential.ts
   ```

Add new `Case` entries to `differential.ts` as more endpoints are ported.

The harness also runs `INFERENCE_CASES`, a small live contract slice for the
OpenAI-compatible inference boundary. These cases intentionally cover
authentication, required fields, streaming-request validation, and unknown
model/provider errors without contacting a real model provider. Provider-backed
success and SSE cases require an explicitly configured disposable upstream and
must not use production credentials.
