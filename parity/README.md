# Differential parity harness

Verifies Trefoil (Rust) responses match a live Plexus (TS) instance for
ported endpoints, byte-for-byte on status code and structural JSON equality.

## Usage

### Unified parity verification

From the repository root, run the complete validation gate:

```sh
./verify-parity.sh
```

It runs, in order, `cargo test --all-targets`, the frontend Vitest suite,
frontend typechecking, the frontend production build, and `parity/run.sh`.
Every lane runs even when an earlier lane fails. The command exits non-zero
using the first failing lane's status, while the remaining results are still
reported. A machine-readable JSON report is written atomically to
`target/parity-report.json`; set `PARITY_REPORT=/path/to/report.json` to change
the destination. Each report lane includes its stable ordinal, command, status,
exit code, duration, and captured stdout/stderr. A report-write failure is also
fatal, but never replaces an existing lane failure. It does not install
dependencies; run `npm ci` in `frontend` and install Bun/Cargo/Docker first.
All environment overrides documented below are passed through to the harness.

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

### Reproducible evidence counts

Regenerate the inventory used by the parity audit with the read-only command:

```sh
python3 parity/counts.py --reference-dir .reference/plexus > target/parity-counts.json
```

The JSON records both repository revisions, SHA-256 hashes for the differential
harness and OpenAPI input, and counts for named differential cases, Trefoil
`.route(...)` declarations, Plexus OpenAPI paths/operations, and migrations.
The command exits non-zero when a required source is missing or malformed; it
does not turn unavailable evidence into zero and never overwrites raw inputs.
The output is intentionally a derived artifact and may be deleted/recreated.

The harness also runs `INFERENCE_CASES`, a small live contract slice for the
OpenAI-compatible inference boundary. These cases intentionally cover
authentication, required fields, streaming-request validation, and unknown
model/provider errors without contacting a real model provider. Provider-backed
success and SSE cases require an explicitly configured disposable upstream and
must not use production credentials. The Rust unit suite provides deterministic
disposable-upstream evidence for inference failover: chat proves ordered
traversal across two transport failures followed by success, while generic
non-chat coverage proves configured target ordering and exhaustion returns the
final error without an extra attempt.

The latest live differential run (Trefoil `a5bc1a6`, Plexus `31b09ce`)
completed with 302/302 executed MCP and custom-checker cases matching, including
the cases added in the final differential pass. The inventory contains 302 named
cases. OAuth provider discovery matches
Plexus's wire shape: Trefoil omits the internal `capabilities` metadata from
the response. Provider-backed success and SSE cases still require an
explicitly configured disposable upstream as described above.

Unsupported modes are intentionally covered by local contract tests rather than
the provider-backed differential lane: external LLM compaction is rejected by
config validation, and manual probes reject `apiType: "oauth"`; OAuth must use
the session flow.

Since that live run, the MCP admin shim has added configuration operations for
MCP enablement, timeout, compaction, exploration/failover/cooldown, tracing,
stall, vision fallthrough, background exploration, and trusted proxies, plus
system-log level and module operations. These milestones are verified by the
Rust unit suite at revision `f2319d9` (`cargo test --all-targets`: 238 passed),
but are not yet represented by a newer live differential-run claim.
