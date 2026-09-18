# Trefoil/Plexus Parity Audit (granular re-audit)

> **Current evidence.** Regenerate the source-traceable inventory with
> `python3 parity/counts.py --reference-dir .reference/plexus` (see
> `parity/README.md#reproducible-evidence-counts`). The command records the
> exact Trefoil/reference revisions and input hashes; it fails closed when the
> reference checkout is missing or malformed.

The counts below are historical claims unless they are reproduced by that
command. In particular, route counts from earlier audits are not mechanically
comparable to the current `.route(...)` declaration count: Axum combines
methods and mounts nested routers, while the reference inventory uses OpenAPI
operations.

Re-audited directly against `.reference/plexus` source (not the prior
estimate). Route inventory extracted mechanically from both codebases and
diffed; feature depth verified by reading handler bodies.

## Route inventory (mechanical diff; historical baseline)

- Plexus registers **182** distinct method+path route entries (excluding
  `__tests__`).
- Trefoil registers **156** distinct method+path route entries.
- After normalizing path params (`:id`/`*` → `:P`), **27 plexus routes have
  no Trefoil counterpart** (below). Trefoil has 2 routes plexus lacks
  (`GET /healthz` duplicate form, `POST /oauth/revoke` — plexus's MCP OAuth
  has no revoke endpoint, only Trefoil's mgmt `/v0/management/mcp-oauth/*
  /revoke`, which is a different, already-ported surface).

The original 27-route gap list below is retained as audit history only. The current source now implements the previously listed custom-checker, checker-result, observability, probe, and conversations surfaces.

### Historical routes missing outright in Trefoil (27)

Custom quota checkers (whole feature, 6 routes):
- `GET/PUT/PATCH/DELETE /v0/management/custom-checkers/:id`
- `GET /v0/management/custom-checkers`
- `POST /v0/management/custom-checkers/:id/check`
- `POST /v0/management/custom-checkers/:id/test`

Quota checker results/history (checker-scoped, 3 routes — Trefoil has only
the flat `/v0/management/quotas` and `/v0/management/quota/status/:key`):
- `GET /v0/management/quotas/:checkerId`
- `GET /v0/management/quotas/:checkerId/history`
- `POST /v0/management/quotas/:checkerId/check`

Provider-specific OAuth login routes (5 routes — session lifecycle and model
discovery are now present in Trefoil; provider exchanges remain partial):

Observability/metrics (3 routes):
- `GET /v0/management/metrics` (Prometheus text-format scrape endpoint)
- `GET /v0/management/events` (SSE live usage event stream)
- `GET /v0/system/logs/stream` (SSE live syslog stream; consumed by the admin `/system-logs` page)

Misc management (5 routes):
- `GET /v0/management/mcp-servers/:name/status` (live process status; Trefoil
  only exposes CRUD + `/process-logs`, no dedicated status probe)
- `DELETE /v0/management/logs/reset`
- `POST /v0/management/models/metadata/resolve`
- `POST /v0/management/test` (manual provider/model probe endpoint —
  Trefoil has no `ProbeService` equivalent at all)

Inference (2 routes):
- `GET /v1/conversations/:conversation_id` (Responses API conversation
  retrieval — Trefoil's `responses.rs` handles `/v1/responses/:id` but not
  the separate conversations resource)

Also note: `/raw/:provider/*` **is** implemented in Trefoil (`raw.rs`,
`axum::routing::any`) — my first-pass route scraper mis-flagged it as
missing because plexus registers it with an explicit method array instead of
one call per verb. Not an actual gap.

## Feature-depth findings (read the code, not just route existence)

| Area | Route coverage | Depth vs Plexus | Notes |
|---|---:|---|---|
| Management CRUD/config/auth | ~95% of routes ported | Strong | Field-level config schema (failover, cooldown, compaction, vision-fallthrough, trusted-proxies, MCP servers, OAuth discovery) all present with matching validation error shapes and 302 registered differential cases plus 238 passing Rust unit tests. |
| Custom quota checkers | Implemented, depth partial | Migration, CRUD, check/test routes, validation, and polling are present in `custom_checkers.rs`; sandbox/runtime and exact built-in upstream checker behavior still need parity audit. |
| Quota checker results | Implemented, built-in depth partial | Checker CRUD, sandboxed execution, history, scheduler, and scoped result routes exist; built-in OAuth-backed provider checker behavior remains open. |
| Provider OAuth login flow | Routes and session state implemented; provider exchanges partial | **Partial** | `oauth.rs` implements session lifecycle, PKCE callback/manual-code capability reporting, credential persistence, and model discovery. Provider-specific exchanges and request masking remain incomplete; `backend/src/oauth.rs` tests assert every advertised provider exposes exactly one supported login flow. |
| Inference dispatch/failover | Route-complete | Partial | Chat now snapshots eligible ordered targets and advances through every candidate after transport failures; generic non-chat evidence covers configured ordering, exhaustion without an extra attempt, and provider-not-found fallback traversal (`backend/src/inference.rs` failover tests). The handlers still hand-roll route-specific dispatch/retry/usage-recording logic rather than sharing Plexus's dedicated `services/dispatch/` package, so cross-route behavior remains only partially aligned. |
| Quota enforcement | Functional, documented gap | `enforce_request_quotas` only reports the single most-constrained quota in `blocking_quotas`; Plexus aggregates all simultaneously-blocking quotas. Comment in code acknowledges this. |
| Observability/metrics | Implemented, depth partial | Prometheus metrics, usage SSE, and system-log SSE routes are present; metric dimensions and stream edge cases still need reference comparison. |
| Vision descriptor | Implemented, depth partial | Descriptor extraction/generation, cache, and clear behavior exist; exact provider-backed descriptor semantics remain open. |
| Compaction | Implemented, depth partial | Native and local headroom compaction execute; external LLM-based compaction remains unsupported. |
| Custom probe/test endpoint | Implemented, depth partial | `POST /v0/management/test` is present in `providers.rs`; provider-backed behavior still needs live comparison. |
| MCP proxy (remote) | Implemented | JSON-RPC envelope validation, round-robin key selection, cooldown-on-failure, upstream auth injection, header allowlisting — present in `mcp_proxy.rs`. |
| MCP local process lifecycle | Implemented | Spawn/stop/restart with launcher allowlist, env/arg parsing, process-log capture (`mcp.rs`) — recently added (latest commit), matches Plexus's `mcp-process-manager.ts` at a route level; sandboxing/resource-limit depth not independently verified. |
| MCP admin-shim (`/mcp/plexus`) | Route-present | `mcp_plexus.rs` dispatches configuration (including MCP enablement, timeout, compaction, failover/cooldown, tracing, stall, vision fallthrough, background exploration, and trusted proxies), gateway lifecycle, quota, logging, and system-log operations through management handlers. Broader tool depth remains thinner than Plexus. |
| OAuth (MCP client) | Implemented | Authorization-code + PKCE S256, protected-resource metadata, resource-bound access tokens, `mcp:read`/`mcp:write` enforcement, refresh-token rotation, and DB-backed authorization-code consumption (`oauth_server.rs`, `mcp_proxy.rs`). Legacy unscoped tokens and MCP API-key credentials remain accepted by the proxy. |
| Backup/restore | Implemented, parity-covered | The 13-table `TABLES` list in `backup.rs` matches Plexus's `OPERATIONAL_TABLES`. Config-only JSON and full gzip+tar export are admin-only and size-capped; the differential harness registers eight backup/restore cases covering export auth/modes, restore auth, malformed JSON, missing configuration data, and response content types. Rust tests also verify the reference JSON error envelope. |
| Migrations | Near 1:1 | 84 SQL files in Trefoil vs 83 in Plexus's `migrations_pg` (imported history, +1 Trefoil-specific). |
| Frontend | Partial | Trefoil includes configuration, detailed usage, request Logs, custom quota checkers, system logs, MCP, debug, Errors, Models, MyKey, Playground, and Metrics. The management API adapter now has 37 passing Vitest tests covering auth, OAuth sessions/accounts, MCP, logging/debug/errors, keys/self-service, quotas, custom checkers, metrics, and encoded mutation paths; browser/e2e coverage and exact UI depth remain. |
| Rust test suite | 238/238 passing | Green, fast (<1s), including MCP configuration validation, gateway forwarding, and system-log level/module round trips. |
| Differential harness | 302 named cases in `parity/differential.ts` | Expanded MCP/checker cases are registered; latest verified MCP/checker lane is 302/302. The inventory is generated by `parity/counts.py` and is not itself an execution result. |

## What's actually left (ranked by size/impact)

1. **Frontend** — biggest raw gap by volume. Remaining pages, browser tests,
   and API adapters for newer route surfaces (metrics, events stream, custom
   MCP admin UI, playground, and the distinct logs viewer) remain.
2. **Shared dispatch/failover service** — inference handlers duplicate
   retry/fallback logic per-route; chat now iterates its full ordered candidate
   list, but there is no dedicated `dispatch` module.
3. **Custom quota checkers** — entire feature (schema, CRUD, sandboxed
   execution, scheduler) absent.
4. **Provider OAuth login flow** — provider-specific exchanges and request
   masking remain partial; MCP-client OAuth is implemented separately.
5. **Live observability endpoints** — Prometheus `/metrics`, SSE
   `/v0/management/events`, SSE `/v0/system/logs/stream`, manual
  `/v0/management/test` probe endpoint.
6. **MCP admin-shim tool depth** (`mcp_plexus.rs`) — 202 lines vs ~2000 in
   Plexus; needs a dedicated tool-by-tool audit to size accurately.
7. **Quota aggregation** — multi-quota `blocking_quotas` reporting,
   per-checker quota results/history routes.
8. **Conversations resource** — `GET /v1/conversations/:id` for the
   Responses API.
9. **Compaction execution** — config plumbing exists; whether it's actually
   applied to oversized inference payloads is unverified and should be
   checked next.

## Verified this pass

The current checkout's reproducible count output is the authority for numeric
claims. It must be generated at audit time because the reference checkout is
external and both repositories continue to change. A successful command emits
`target/parity-counts.json` when redirected as shown above; its `inputs` hashes
provide source-to-count traceability.

At this revision (`f2319d9`, reference `31b09ce`), the command produced:

| Input-derived measure | Count |
|---|---:|
| Named differential cases | 302 |
| Trefoil `.route(...)` declarations | 129 |
| Plexus OpenAPI paths | 123 |
| Plexus OpenAPI operations | 177 |
| Trefoil SQL migrations | 86 |

These are inventory measures, not a claim that every route or case is
behaviorally complete. The JSON output is the fixture for this table and
includes hashes and revisions needed to detect drift.

- [x] 238/238 Rust tests pass (`cargo test --all-targets`, <1s), including generic non-chat and speech failover, custom-checker runtime coverage, and MCP system-log round trips.
- [historical] 156 route entries confirmed live in `lib.rs`/module routers.
- [x] Backup table list is an exact 13-table match to Plexus.
- [x] Backup/restore parity cases cover config-only and full export responses,
      admin/limited/missing-credential authorization, restore content-type
      comparison, malformed JSON, and a config envelope missing `data`
      (`parity/differential.ts#BACKUP_CASES`). The Rust backup unit tests also
      cover archive path/type/duplicate/size validation and the JSON error
      envelope, including its message field (`backend/src/backup.rs#tests`).
- [historical] Migration count was 84 (Trefoil) vs 83 (Plexus), consistent history.
- [x] OAuth (MCP-client) authorization-code + PKCE + refresh rotation is
      real, DB-backed, and resource-bound. Protected-resource metadata
      advertises `mcp:read` and `mcp:write`; `tools/call` requires write scope,
      other MCP methods require read scope, and unscoped legacy tokens retain
      compatibility.
- [x] Provider OAuth discovery reports callback/manual-code capabilities for
      all six advertised providers, with a backend test guarding the matrix.
- [x] MCP local-process lifecycle (spawn/stop/restart/logs) is implemented
      with a launcher allowlist.
- [x] MCP configuration operations cover MCP enablement, timeout, compaction,
      exploration/failover/cooldown, tracing, stall, vision fallthrough,
      background exploration, and trusted proxies; local tests cover their
      read/patch/validation contracts.
- [x] MCP system-log operations cover level and module filters, including
      normalization, validation, reset, and management-router round trips.
- [x] `plexus_quota` MCP operations (`status`, `clear`, `recompute`) are wired
      with admin auth, confirmation, attachment, and not-found/error handling.
- [x] Frontend management API adapter parity is covered by wire-level Vitest
      tests (`frontend/src/lib/api.test.ts`): auth, decoded GET responses,
      encoded paths, JSON mutation bodies, text metrics, HTTP failures, and
      transport failures, plus OAuth session lifecycle, unauthenticated/server
      failures, malformed session responses, MCP server/key management,
      logging/debug/error management, key/self-service and quota operations,
      custom-checker operations, and encoded mutation paths (37 tests pass).
- [x] Generic failover coverage verifies non-chat requests preserve configured
      target order and return the final upstream error after exhausting targets
      without an extra attempt (`backend/src/inference.rs`).
- [x] Unified verification gate — backend/frontend lanes pass, and OAuth
      provider discovery matches the reference wire shape.
- [x] OAuth provider discovery shape — Trefoil omits the internal
      `capabilities` metadata that Plexus omits, preserving provider ordering
      and `usesCallbackServer` fields.
- [x] Differential harness MCP/custom-checker lane run end-to-end against live
      Plexus + Trefoil; all 302 cases matched (verified by
      `REFERENCE_DIR=.reference/plexus parity/run.sh`), including MCP,
      custom-checker, and OAuth provider shape.
- [x] MCP and custom-checker differential mismatches are closed; the live
      harness includes their CRUD, validation, auth, and runtime cases.
- [x] Compaction execution is covered by deterministic Rust tests for native
      and headroom compaction, including trigger limits and history retention.
- [x] Unsupported modes fail explicitly before dispatch: external compaction
      is rejected by config validation, and OAuth is rejected by the manual
      provider probe in favor of the OAuth session flow.

Scores are now anchored to route/behavior evidence rather than an
undifferentiated percentage; the categories above without a specific gap
listed should be read as "no gap found in this pass," not "fully audited
line-by-line."
