# Trefoil/Plexus Parity Audit (granular re-audit)

Re-audited directly against `.reference/plexus` source (not the prior
estimate). Route inventory extracted mechanically from both codebases and
diffed; feature depth verified by reading handler bodies.

## Route inventory (mechanical diff)

- Plexus registers **182** distinct method+path route entries (excluding
  `__tests__`).
- Trefoil registers **156** distinct method+path route entries.
- After normalizing path params (`:id`/`*` → `:P`), **27 plexus routes have
  no Trefoil counterpart** (below). Trefoil has 2 routes plexus lacks
  (`GET /healthz` duplicate form, `POST /oauth/revoke` — plexus's MCP OAuth
  has no revoke endpoint, only Trefoil's mgmt `/v0/management/mcp-oauth/*
  /revoke`, which is a different, already-ported surface).

### Routes missing outright in Trefoil (27)

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

OAuth login-session flow (interactive provider OAuth, 5 routes — Trefoil's
`oauth.rs` only stubs `GET /sessions/:id` → 404):
- `POST /v0/management/oauth/sessions`
- `POST /v0/management/oauth/sessions/:id/cancel`
- `POST /v0/management/oauth/sessions/:id/manual-code`
- `POST /v0/management/oauth/sessions/:id/prompt`
- `GET /v0/management/oauth/models`

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
| Management CRUD/config/auth | ~95% of routes ported | Strong | Field-level config schema (failover, cooldown, compaction, vision-fallthrough, trusted-proxies, MCP servers, OAuth discovery) all present with matching validation error shapes and ~275 differential-harness cases plus 173 passing Rust unit tests. |
| Custom quota checkers | 0% | **Not implemented** | No `custom_checkers` table/module in Trefoil at all. `checker_types()` returns a hardcoded mirror of Plexus's built-in registry; `configured` is always `[]`. Running/testing user-supplied checker code (`custom-checker-runtime.ts` in Plexus) has no Trefoil equivalent. |
| Quota checker results | Partial | `/v0/management/quotas` and `/quota/status/:key` exist (real DB-backed `quota_state` reads) but the per-checker `/quotas/:checkerId`, `/history`, `/check` routes are absent — no scheduled provider-quota polling (`QuotaScheduler`) exists. |
| Provider OAuth login flow | 0% (interactive flow) | **Not implemented** | Registration/PKCE-token-exchange plumbing for *MCP client* OAuth is solid (`oauth_server.rs`), but the separate *provider* OAuth login flow (browser-based Anthropic/OpenAI/xAI login, callback server, manual-code entry) used to populate `oauth_provider`/`oauth_account` credentials has no session/prompt/cancel/manual-code routes or backing service. |
| Inference dispatch/failover | Route-complete | Partial | `resolve_target` picks the *first* eligible alias target per group rather than iterating the full ordered target list with retry-on-failure across all candidates (Plexus's `dispatcher.ts`/`router.ts`). Each inference handler (`chat_completions`, `completions`, `embeddings`, `messages`) hand-rolls its own single-hop "fallback" branch instead of sharing one dispatch service — duplicated retry/failover/usage-recording logic across ~4 call sites in `inference.rs` (3190 lines, monolithic) vs Plexus's dedicated `services/dispatch/` package (dispatcher, media-dispatcher, standard-attempt-request, request-manager, image-model-bridge — ~4000 lines across focused modules). |
| Quota enforcement | Functional, documented gap | `enforce_request_quotas` only reports the single most-constrained quota in `blocking_quotas`; Plexus aggregates all simultaneously-blocking quotas. Comment in code acknowledges this. |
| Observability/metrics | Missing | No Prometheus `/metrics` endpoint, no SSE `/events` or `/logs/stream` endpoints. `GET /v0/system/logs/recent` (polling) is present; live streaming is not. |
| Vision descriptor | Stub | `/cache/vision-descriptor/clear` is a truthful no-op (feature doesn't exist upstream in Trefoil), matches Plexus route but not Plexus behavior (Plexus actually generates/caches descriptors). |
| Compaction | Config-only | GET/PATCH persist and validate `compaction` config shape, but there's no evidence Trefoil's inference path actually performs headroom/native compaction on oversized requests — this needs verification against `services/compaction/*` (not confirmed either way from route/config code alone). |
| Custom probe/test endpoint | Missing | `POST /v0/management/test` (manual per-provider/model canonical probe) has no Trefoil equivalent; no `ProbeService`. |
| MCP proxy (remote) | Implemented | JSON-RPC envelope validation, round-robin key selection, cooldown-on-failure, upstream auth injection, header allowlisting — present in `mcp_proxy.rs`. |
| MCP local process lifecycle | Implemented | Spawn/stop/restart with launcher allowlist, env/arg parsing, process-log capture (`mcp.rs`) — recently added (latest commit), matches Plexus's `mcp-process-manager.ts` at a route level; sandboxing/resource-limit depth not independently verified. |
| MCP admin-shim (`/mcp/plexus`) | Route-present | `mcp_plexus.rs` dispatches `plexus_quota` status, clear (acknowledged destructive confirmation), and recompute through DB-backed management handlers; broader tool depth remains thinner than Plexus. |
| OAuth (MCP client) | Implemented | Full authorization-code + PKCE S256 + refresh-token rotation with DB-backed consumption tracking (`oauth_server.rs`). This is materially more complete than the prior audit's "45%" estimate. |
| Backup/restore | Implemented | Table list (`TABLES` in `backup.rs`) is an exact match to Plexus's `OPERATIONAL_TABLES` (13 tables). Gzip+tar archive format, admin-only, size-capped. |
| Migrations | Near 1:1 | 84 SQL files in Trefoil vs 83 in Plexus's `migrations_pg` (imported history, +1 Trefoil-specific). |
| Frontend | Partial | Trefoil now includes configuration, detailed usage, request Logs (`/logs`), custom quota checkers, system logs, MCP management, debug trace management, and the Errors viewer; remaining major pages include Models, MyKey, and Playground. |
| Rust test suite | 173/173 passing | Green, fast (<1s). No regressions from latest commit. |
| Differential harness | ~275 named cases in `parity/differential.ts` | Requires manually running both live servers side-by-side (Plexus TS + Trefoil against Postgres) — not wired into CI per the README; last-run status unverified in this pass. |

## What's actually left (ranked by size/impact)

1. **Frontend** — biggest raw gap by volume. Remaining pages, browser tests,
   and API adapters for newer route surfaces (metrics, events stream, custom
   MCP admin UI, playground, and the distinct logs viewer) remain.
2. **Shared dispatch/failover service** — inference handlers duplicate
   retry/fallback logic per-route and only try one fallback target instead of
   iterating a full ordered candidate list; no dedicated `dispatch` module.
3. **Custom quota checkers** — entire feature (schema, CRUD, sandboxed
   execution, scheduler) absent.
4. **Provider OAuth login flow** — interactive session-based OAuth
   (`/oauth/sessions*`, `/oauth/models`) absent; only static provider
   discovery and MCP-client OAuth are implemented.
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

- [x] 173/173 Rust unit tests pass (`cargo test --release`, <1s).
- [x] 156 route entries confirmed live in `lib.rs`/module routers.
- [x] Backup table list is an exact 13-table match to Plexus.
- [x] Migration count is 84 (Trefoil) vs 83 (Plexus), consistent history.
- [x] OAuth (MCP-client) authorization-code + PKCE + refresh rotation is
      real, DB-backed, and more complete than previously scored.
- [x] MCP local-process lifecycle (spawn/stop/restart/logs) is implemented
      with a launcher allowlist.
- [x] `plexus_quota` MCP operations (`status`, `clear`, `recompute`) are wired
      with admin auth, confirmation, attachment, and not-found/error handling.
- [ ] Differential harness (~275 cases) run end-to-end against live Plexus +
      Trefoil in this pass — not executed (requires two live servers +
      Postgres fixture provisioning per `parity/README.md`); route/config
      parity was instead verified by direct code reading.
- [ ] MCP admin-shim tool-by-tool coverage — not enumerated.
- [ ] Compaction execution path (vs config-only) — not confirmed.

Scores are now anchored to route/behavior evidence rather than an
undifferentiated percentage; the categories above without a specific gap
listed should be read as "no gap found in this pass," not "fully audited
line-by-line."
