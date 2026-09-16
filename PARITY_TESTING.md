# Plexus parity testing plan

Yes. Plexus has a substantial test suite that can serve as a behavioral
specification, but most tests cannot be copied and expected to compile against
Rust unchanged.

## What exists in the reference

- Backend: at least 270 `*.test.ts` files under `packages/backend`, covering
  routes, authentication, database migrations/repositories, dispatch/routing,
  provider transforms, quotas, OAuth, MCP, SSE, and services.
- Frontend: 8 test files under `packages/frontend`, mostly around API helpers
  and model/options behavior.
- Additional script/shared tests and an OpenAPI route test.
- Vitest projects for unit, SQLite, and Postgres/PGlite-style database tests.

The source of truth for its conventions is `.reference/plexus/docs/TESTING.md`.
The reference test runner uses `bun run test`; raw `bun test` is intentionally
blocked.

## What can be reused directly

1. **Test cases as requirements:** Preserve test names, scenarios, fixtures,
   expected status codes, JSON fields, headers, streaming events, and error
   semantics in a porting matrix.
2. **Protocol fixtures:** Copy compatible JSON/OpenAPI examples and golden
   request/response data, subject to the Plexus MIT license and attribution.
3. **Browser behavior:** Keep the frontend tests that validate generic API
   client behavior, then point them at Trefoil's API adapter.
4. **Contract tests:** Run the same HTTP cases against the Plexus reference and
   the Rust implementation, comparing normalized responses.

## What must be ported or rewritten

- TypeScript/Vitest unit tests that import Plexus modules cannot run against
  Rust internals. Rewrite them as Rust unit tests or black-box HTTP tests.
- Database tests must be redesigned for Trefoil's chosen schema, migrations,
  database, and transaction behavior.
- Provider/OAuth/MCP tests are only relevant if Trefoil implements those
  features.
- Tests asserting Plexus-specific implementation details should be discarded;
  retain observable behavior instead.

## Recommended parity phases

### Phase 1: Inventory

Build a matrix with columns for Plexus test path, capability, protocol surface,
fixture/data dependencies, relevance to Trefoil, and port status. Group by
public surface rather than copying the repository's directory tree.

### Phase 2: Freeze the contract

Extract the relevant OpenAPI schemas and HTTP examples. Define Trefoil's
compatibility target explicitly: exact compatibility, intentionally changed, or
not implemented. Normalize nondeterministic values such as IDs, timestamps,
latencies, and generated tokens before comparing responses.

### Phase 3: Differential runner

Create a Rust-independent test harness that sends identical requests to two
base URLs:

```text
PLEXUS_BASE_URL=http://...
TREFOIL_BASE_URL=http://...
```

Compare status, selected headers, JSON structure/values, and SSE event
sequences. Never send real credentials or production requests to the harness.

### Phase 4: Rust implementation tests

For each parity case, add:

- an Axum handler/integration test for HTTP behavior;
- Rust unit tests for domain logic and transformations;
- database tests only for Trefoil-owned persistence behavior; and
- frontend tests for rendering, loading, error, and empty states.

Keep differential tests separate from the Rust unit suite so Trefoil remains
testable when the Plexus reference is unavailable.

## Important limitation

Parity is not proof that the Rust implementation is correct for Trefoil's
future domain. Plexus's large suite is a strong compatibility baseline, not a
replacement for security, persistence, concurrency, and product-specific
requirements.

