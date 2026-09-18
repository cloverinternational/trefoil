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

The current inventory contains 313 named cases. The latest recorded live
differential attempt (Trefoil `701ddb4`, Plexus `31b09ce`) executed 283 cases:
282 matched and one diverged on the then-documented OAuth provider-discovery
shape difference. That historical run is not evidence that the current 313
registered cases passed. Provider-backed success and SSE cases still require
an explicitly configured disposable upstream as described above.

### Backup and restore parity coverage

`BACKUP_CASES` in `parity/differential.ts` is the authoritative live parity
inventory for `/v0/management/backup` and `/v0/management/restore` (eight
registered cases). It covers config-only JSON export and full archive export,
including admin-only access, limited-key rejection, and missing-credential
rejection. Restore cases cover the same authorization failures, malformed JSON,
and a JSON envelope without `data`; the runner compares status and response
content type for both backup and restore requests. These cases intentionally
stop before a successful restore, so they do not prove database round-trip or
operational-table restoration.

The implementation accepts a config JSON envelope or a full gzip/tar archive.
Config restore replaces providers, models, and keys; archive restore validates
the manifest and entries, replaces the listed operational tables, then applies
the config. Both paths require an admin credential and enforce the request-size
limits in `backend/src/backup.rs`. Deterministic Rust tests in
`backend/src/backup.rs#tests` additionally cover archive traversal/link,
duplicate, and size rejection plus the JSON error envelope and its message
field. The differential runner also compares restore response content types.
They do not replace an end-to-end PostgreSQL backup/restore round-trip test.

### Frontend management API parity coverage

`frontend/src/lib/api.test.ts` contains 38 passing wire-level Vitest tests for
the management adapter. Coverage includes authentication and decoded reads,
OAuth session/account flows, MCP server/key management, logging, debug and
inference-error management, key/self-service and quota operations,
custom-checker operations, metrics text responses, encoded path parameters,
JSON mutation bodies, HTTP failures, transport failures, and malformed OAuth
responses. This verifies frontend-to-backend request/response contracts; it
does not claim browser/e2e or complete UI parity.

OAuth PKCE persistence, replay safety, and cancellation cleanup have deterministic backend evidence:
`oauth::tests::pkce_verifier_is_persisted_but_not_serialized` verifies the
verifier survives session persistence without entering the API response, and
`oauth::tests::callback_claim_removes_in_memory_verifier` verifies that a
callback consumes the process-local verifier. The cancellation test
`oauth::tests::cancelling_session_cleans_up_pkce_verifier_and_is_terminal`
verifies successful cancellation removes the one-shot verifier and makes the
session terminal, including conflict on repeated cancellation. Migration `0086`
and the startup session loader cover recovery of the persisted verifier after a
restart.

Unsupported modes are intentionally covered by local contract tests rather than
the provider-backed differential lane: external LLM compaction is rejected by
config validation, and manual probes reject `apiType: "oauth"`; OAuth must use
the session flow.

### MCP OAuth scope and resource parity

MCP OAuth exposes `/.well-known/oauth-protected-resource/mcp/:name` with the
resource `/mcp/:name` and supported scopes `mcp:read` and `mcp:write`. The
authorization-code flow requires a valid resource indicator and S256 PKCE;
requested scopes are checked against the client's registered scope before a
code is stored. Access and refresh tokens preserve both the resource and scope.

The MCP proxy binds a bearer token to the requested `/mcp/:name` resource (a
matching absolute URL is accepted by path), checks expiry and revocation, and
requires `mcp:write` for `tools/call`. Other MCP methods require `mcp:read`,
while `mcp:write` also grants read access. Tokens without a scope retain the
legacy unrestricted behavior, and configured MCP API-key credentials continue
to use the legacy path. A scope failure returns HTTP 403; an invalid,
expired, revoked, or wrong-resource token returns HTTP 401.

This scope/resource behavior is covered by the deterministic
`mcp_proxy.rs` unit test `enforces_oauth_read_write_scopes_and_legacy_unscoped_tokens`.
The live differential inventory does not claim coverage for these OAuth
authorization and token-resource cases; provider-specific OAuth exchanges and
request masking remain documented implementation gaps.

Since that live run, the MCP admin shim has added configuration operations for
MCP enablement, timeout, compaction, exploration/failover/cooldown, tracing,
stall, vision fallthrough, background exploration, and trusted proxies, plus
system-log level and module operations. These milestones are verified by the
Rust unit suite at revision `895bbf7` (`cargo test --all-targets`: 257 passed),
but are not yet represented by a newer live differential-run claim.

Media failover has deterministic backend evidence in
`backend/src/inference.rs`: image generation, image edit, speech, and
transcription preserve configured fallback order, while
`media_failover_disabled_does_not_try_fallback_target` proves that disabling
failover limits each media request to its primary target.
