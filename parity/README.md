# Differential parity harness

Verifies Trefoil (Rust) responses match a live Plexus (TS) instance for
ported endpoints, byte-for-byte on status code and structural JSON equality.

## Usage

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
   ADMIN_KEY=correct-admin-key ./target/debug/trefoil-backend
   ```
   (its `main.rs` already seeds an in-memory "test-key"/"sk-test-secret" key
   matching the fixture above).
3. Run the harness:
   ```
   PLEXUS_BASE_URL=http://127.0.0.1:4100 TREFOIL_BASE_URL=http://127.0.0.1:8080 \
     bun run parity/differential.ts
   ```

Add new `Case` entries to `differential.ts` as more endpoints are ported.
