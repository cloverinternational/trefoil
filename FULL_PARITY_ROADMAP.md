# Full Plexus parity roadmap

## Current baseline

- Plexus backend registers approximately 169 route handlers across inference,
  management, MCP, OAuth, raw passthrough, and OpenAPI.
- Trefoil has approximately 75 route declarations and 159 Rust tests.
- PostgreSQL uses the imported Plexus `migrations_pg` history.
- The implemented management/config/observability slice is tested locally;
  full differential execution is still blocked by reference-server lifecycle
  setup and must be repaired before claiming endpoint parity.

## Choreography

1. **Stabilize the verification harness.** Start Plexus and Trefoil in stable
   long-lived processes, provision isolated fixtures, add deterministic cases
   for every implemented route, and record status/body/header differences.
   This is the gate for all later work.
2. **Finish provider execution.** Extract a shared Rust dispatch service used by
   chat, completions, embeddings, messages, responses, audio, and images.
   Implement provider/model target iteration, retryable-status/network policy,
   cooldown writes, attempt history, request cancellation, SSE terminal
   handling, and consistent usage/error records.
3. **Complete inference contracts.** Add Gemini, response retrieval/deletion,
   raw passthrough, model metadata/search/lookup, OpenRouter/Muse model routes,
   provider-specific transformations, token/cost accounting, and all relevant
   differential cases.
4. **Complete quotas and usage.** Implement token/cost limits, scoped quotas,
   warning/reset headers, quota checker management, usage event streaming,
   exact summary breakdowns, and atomic reservation/accounting tests.
5. **Complete MCP.** Implement server/key CRUD parity, MCP proxy forwarding,
   JSON-RPC tool surface, streamable HTTP GET/POST/DELETE behavior, upstream
   authentication, MCP usage/debug logs, key cooldowns, and safe local-process
   lifecycle with explicit sandbox/allowlist controls.
6. **Complete OAuth.** Implement authorization-code storage, redirect and
   client validation, PKCE S256 verification, API-key consent, access/refresh
   token issuance and rotation, expiration/revocation, scope enforcement,
   metadata, and management session flows.
7. **Complete operations and backup.** Add full archive backup/restore,
   system logs/streaming, restart, provider model fetch, custom checkers,
   remaining config routes, and exact error semantics.
8. **Frontend parity.** Port the eight frontend tests, add API adapters, then
   implement loading/error/empty/persistence behavior and browser tests. The System Logs page is now implemented at `/system-logs` with admin SSE, runtime level controls, module filtering, pause/resume, clear, and error handling; browser coverage remains outstanding.
9. **Security/performance hardening.** Add SSRF defenses, secret redaction,
   request limits, connection-pool tuning, race/concurrency tests, migration
   compatibility checks, and production-like load verification.
10. **Final parity gate.** Run Plexus's full suite, Trefoil's full suite, all
    deterministic differential cases, and a documented unsupported-feature
    report with no placeholder routes counted as complete.

## Breaking points to control

- Do not count a route returning `501`, an empty placeholder, or a passthrough
  without the Plexus transformation as implemented parity.
- Keep shared dispatch logic centralized; duplicating route-specific HTTP
  clients will cause inconsistent auth, retries, streaming, and accounting.
- Never expose provider API keys or OAuth tokens in config exports, logs, or
  error responses without an explicit encrypted/portable backup contract.
- MCP subprocesses must not accept arbitrary executable/argument/environment
  input without an allowlist and resource limits.
- Differential tests need isolated databases and fresh fixture names; cleanup
  must be transactional and safe after partial failure.

## Immediate next milestone

Repair the differential runner lifecycle and implement the shared provider
dispatch service. This gives the highest parity return because inference is the
largest missing public surface and all provider, quota, cooldown, usage, and
failover behavior depends on it.

## Acceptance criteria

- Every currently implemented route has at least one deterministic differential
  case or an explicit reason it cannot be compared.
- Chat dispatch works against a local mock OpenAI-compatible provider for both
  success and retryable failure/failover.
- Usage, cooldown, and attempt-history records agree with the request outcome.
- Existing Rust tests remain green throughout each milestone.
- Final completion requires zero unexplained route/test differences, not merely
  a growing route count.
