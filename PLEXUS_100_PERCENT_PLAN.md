# Path to a 100% Plexus drop-in replacement

Granular checklist derived from direct source comparison (`backend/src/*.rs`
vs `.reference/plexus/packages/backend/src/**`), not estimates. Each item
lists what exists today, what's missing, and roughly how big the gap is.

Legend: 🔴 not started · 🟡 partial · 🟢 done

---

## 1. Provider OAuth login flow — 🟡 partial

**Session plumbing is implemented; provider exchanges remain partial.** Config schema already has `oauth_provider`/
`oauth_account` fields on `ProviderConfig` (`state.rs`), and
`oauth.rs` exposes static provider *discovery* (`GET /oauth/providers`,
`credentials/status`, `DELETE credentials`), but there is no way to actually
perform the browser-based (or device-code) login that populates those
credentials.

Plexus reference: `services/oauth/` — `oauth-login-session.ts` (369 lines,
session state machine: pending → awaiting_code → exchanging → complete/error),
`oauth-auth-manager.ts` (489 lines, per-provider auth orchestration + token
refresh), `oauth-native-request.ts` (837 lines, provider-specific PKCE/device
flows for Anthropic, OpenAI Codex, GitHub Copilot, xAI, Kimi Code,
OpenRouter), `codex-version-service.ts` (72 lines).

To port:
- [x] `POST /v0/management/oauth/sessions` — start a login session for a
      provider id, return authorize URL (+ PKCE verifier stored server-side).
- [x] `GET /v0/management/oauth/sessions/:id` — poll persisted session status
      from the real session store/state machine.
- [x] `POST /v0/management/oauth/sessions/:id/prompt` — supply user input
      mid-flow (e.g. selecting an org/workspace).
- [x] `POST /v0/management/oauth/sessions/:id/manual-code` — accept a
      manually pasted authorization code (headless/CLI-driven login).
- [x] `POST /v0/management/oauth/sessions/:id/cancel` — abort a pending
      session.
- [x] `GET /v0/management/oauth/models` — list models available under a
      provider's OAuth credential (subscription-tier-gated catalogs).
- [~] Token storage + refresh scheduling per provider (persist
      access/refresh tokens encrypted, auto-refresh before expiry, surface
      expiry/invalid state via `credentials/status`).
- [ ] Per-provider auth implementations: Anthropic (Claude Pro/Max),
      OpenAI Codex (ChatGPT Plus/Pro), GitHub Copilot, xAI, Kimi Code,
      OpenRouter — each has distinct flow quirks (device code vs PKCE vs
      callback server) in `oauth-native-request.ts`.
- [x] Provider discovery exposes and tests the supported login-flow matrix
      (`callback` versus `manualCode`) for all six advertised providers.
- [ ] Request masking/identity spoofing for subscription providers
      (`transformers/oauth/masking/*` — cc-billing, cc-identity, cc-tools,
      cc-headers — 11 files) so requests sent through a Claude-Code/Codex
      OAuth credential look like first-party client traffic. Without this,
      OAuth-authenticated requests may be rejected upstream even after login
      succeeds.

**Impact if skipped:** any provider that requires subscription OAuth instead
of a plain API key cannot be used through Trefoil at all. Pure API-key
providers are unaffected.

---

## 2. Shared inference dispatch service — 🟡 partial

`inference.rs` is a single 3190-line file; each handler
(`chat_completions`, `completions`, `embeddings`, `messages`, `responses`,
`images`, `gemini`) reimplements its own request/retry/fallback/usage-record
logic instead of calling one shared service. A shared `RetryableFailoverPolicy`
now owns ordered candidate iteration and single-target behavior when failover
is disabled for generic JSON and chat status-failover paths; the common
dispatch entry point and uniform per-route semantics remain open.

Plexus reference: `services/dispatch/` — `dispatcher.ts` (1086 lines),
`dispatcher-auto-compat.ts` (1320 lines), `media-dispatcher.ts` (1312 lines),
`standard-attempt-request.ts` (685 lines), `request-manager.ts` (372 lines),
`request-payload-builder.ts` (417 lines), `image-model-bridge.ts` (371
lines), `empty-completion.ts` (265 lines) — ~4800 lines total.

To port:
- [ ] Extract a single `dispatch()` entry point that all inference routes
      call, parameterized by API type (chat/completions/embeddings/
      messages/responses/images/speech/transcriptions/gemini).
- [x] Full ordered-target iteration: try every enabled, non-cooling-down
      target in an alias's target group (not just the first), with
      per-attempt retry-policy from `FailoverPolicySchema`
      (`retryableStatusCodes`, `retryableErrors`).
- [x] Share ordered retry/failover iteration through
      `RetryableFailoverPolicy` across generic JSON dispatch and chat status
      failover, including disabled-failover attempt limits.
- [ ] Attempt-history recording across the *entire* candidate chain (not
      just success + one fallback), matching Plexus's per-attempt audit
      trail.
- [ ] Cancellation propagation (client disconnect aborts the upstream
      request) — verify this exists uniformly across all 9 API types, not
      just the ones tested so far.
- [x] Consolidate the 4 near-duplicate "try provider, on retryable status
      try fallback" blocks in `inference.rs` (chat/completions/embeddings/
      messages each hand-roll this) into one shared function.
- [x] Add deterministic shared-vs-direct target-selection parity coverage for
      ordered, direct-provider, missing-model, and provider-policy cases;
      HTTP-path parity and the extracted dispatch entry point remain open.

**Impact if skipped:** works fine for the common case (one healthy fallback),
but route-specific request and usage behavior can still drift across API
types. Multiple simultaneous provider outages are covered by shared target
iteration, while cancellation, attempt-history, and the common dispatch
entry-point remain parity risks.

---

## 3. Quota aggregation — 🟢 done

`enforce_request_quotas` in `inference.rs` returns as soon as it hits the
first exceeded quota and reports only that one in `blocking_quotas` (code
comment self-acknowledges this). Plexus aggregates *all* simultaneously
violated quotas into the array.

To port:
- [x] Evaluate all applicable quotas before responding, not just until the
      first failure.
- [x] Populate `blocking_quotas` with every exceeded quota (name, limit,
      current usage, remaining, resetsAt) matching
      `services/quota/quota-enforcer.ts`'s `buildQuotaExceededBody`.

---

## 4. Custom quota checkers — 🟡 partial

Custom checker persistence, CRUD, validation, sandboxed execution, history, and scheduling are implemented. Remaining parity is concentrated in built-in provider checker upstream calls and exact result semantics.

Plexus reference: `services/quota/checker-registry.ts` (311 lines),
`services/quota/custom-checker-runtime.ts`, `checkers/claude-code-checker.ts`
(301 lines), `checkers/openai-codex-checker.ts` (294 lines),
`routes/management/custom-checkers.ts`, `quota-scheduler.ts` (591 lines).

To port:
- [x] `custom_checkers` table + migration.
- [x] `GET/PUT/PATCH/DELETE /v0/management/custom-checkers/:id`,
      `GET /v0/management/custom-checkers`.
- [x] `POST /v0/management/custom-checkers/:id/check` — run the checker now.
- [x] `POST /v0/management/custom-checkers/:id/test` — dry-run validate
      checker code before saving.
- [x] Sandboxed execution of user-supplied checker code (Plexus runs this in
      an isolated context — decide the Rust equivalent: embedded scripting
      engine, WASM, or subprocess with strict resource limits).
- [ ] Built-in checkers with real upstream calls: `claude-code-checker.ts`
      and `openai-codex-checker.ts` actually query the provider's usage API
      using the stored OAuth credential — this depends on item #1 (provider
      OAuth) being done first.
- [x] `QuotaScheduler` — periodic background polling
      (`intervalMinutes` per checker config) that writes results the
      `/v0/management/quotas/:checkerId` and `/history` routes read from.
- [x] `GET /v0/management/quotas/:checkerId`,
      `GET /v0/management/quotas/:checkerId/history`,
      `POST /v0/management/quotas/:checkerId/check`.

**Depends on:** #1 (provider OAuth), since the two built-in checkers need a
live provider credential to check against.

---

## 5. Admin MCP management shim (`/mcp/plexus`) tool depth — 🟡 partial

This is a **bigger gap than the route inventory suggests.** The management shim now implements the listed provider, alias, key, quota, gateway, settings, logs, usage, debug, and operations adapters. Remaining depth is concentrated in exact edge-case parity, checker-specific operations, and exhaustive differential coverage.

Plexus reference: `routes/mcp/plexus/configuration.ts` (440 lines) +
`operations.ts` (395 lines) + `management.ts` implement the full operation
set:

| Tool | Operations in Plexus | Implemented in Trefoil |
|---|---|---|
| `plexus_config` | get, export, status | get, status, export (3/3) |
| `plexus_provider` | list, get, put, update, delete, fetch_models | 6/6 |
| `plexus_model_alias` | list, get, put, update, delete, delete_all | 6/6 |
| `plexus_key` | list, get, put, update, delete, disable | 6/6 |
| `plexus_quota` | list, get, put, update, delete, status, clear, recompute | 8/8 |
| `plexus_quota_checker` | types, list, get | 3/3 |
| `plexus_mcp_gateway` | servers_list, get, put, update, delete | 5/5 |
| `plexus_settings` | get | 1/1 |
| `plexus_system_logs` | recent, level, set_level, reset_level | 4/4 |
| `plexus_usage` | list, summary | 2/2 |
| `plexus_debug` | state | 1/1 |
| `plexus_operations` | backup, restore, refresh_metadata, list_cooldowns, clear_cooldowns | 5/5 |

The listed operation adapters are implemented; remaining work is exact edge-case and exhaustive differential parity. Since every underlying management
route already exists as a REST handler (aliases.rs, keys.rs, providers.rs,
etc.), the fastest path is thin adapters that call the same internal
functions the REST routes call, rather than reimplementing logic — mirrors
how Plexus's `configuration.ts` shim wraps its own REST route handlers.

- [x] `plexus_provider`: wrap `providers.rs` list/get/put/patch/delete/
      fetch-models.
- [x] `plexus_model_alias`: wrap `aliases.rs` list/get/put/patch/delete/
      delete_all.
- [x] `plexus_key`: wrap `keys.rs` list/get/put/patch/delete/disable.
- [x] `plexus_quota`: wrap `user_quotas.rs` + `quota_management.rs`
      (list/get/put/patch/delete/status/clear/recompute).
- [x] `plexus_quota_checker`: wrap `config_routes.rs` checker_types + (new)
      per-checker routes from item #4.
- [x] `plexus_mcp_gateway`: wrap `mcp.rs` server CRUD.
- [x] `plexus_settings`: wrap `system_settings.rs` get.
- [x] `plexus_system_logs`: wrap `system_logs.rs` + `logging.rs`.
- [x] `plexus_usage`: wrap `usage.rs` list/summary.
- [x] `plexus_debug`: wrap `debug.rs` state.
- [x] `plexus_operations`: wrap `backup.rs` (backup/restore),
      `config_routes.rs` (refresh_metadata), `cooldowns.rs`
      (list/clear cooldowns).
- [ ] `destructive: "acknowledged"` gate for high-impact operations (delete,
      restore, clear) — check `security.ts` in Plexus for the exact
      contract.
- [x] Secret redaction on `plexus_key`/`plexus_provider` responses by
      default, with an explicit `redact: false` opt-out — check
      `responses.ts`/`management.ts` (`buildShimHeaders`) for the exact
      redaction rules.

---

## 6. Live observability endpoints — 🟡 partial

- [x] `GET /v0/management/metrics` — Prometheus text-exposition endpoint.
      Reference: `routes/management/metrics.ts` (1048 lines). Computes
      counters/gauges from `request_usage` (cumulative + per-dimension +
      today's totals + in-flight concurrency), `CooldownManager` (active
      cooldowns), and `provider_performance` (TTFT/throughput). All the
      underlying data already exists in Trefoil's Postgres tables
      (`request_usage`, `provider_cooldowns`, `provider_performance`) — this
      is a pure read/format task, no new data model needed.
- [x] `GET /v0/management/events` — SSE stream of live usage events as they
      land, using `eventsource-encoder`-equivalent framing (`text/
      event-stream`, `data:`/`event:`/`id:` lines). Reference:
      `routes/management/usage.ts:831`.
- [x] `GET /v0/system/logs/stream` — SSE stream of live syslog entries.
      Reference: `routes/management/system-logs.ts:20`. Requires an
      in-process log-event emitter Trefoil doesn't currently have (Plexus's
      `logEmitter` in `utils/logger.ts`); Trefoil would need an equivalent
      broadcast channel (e.g. `tokio::sync::broadcast`) fed by the existing
      logging macros.
- [x] `DELETE /v0/management/logs/reset` — reset accumulated log state.
      Reference: `routes/management/backup.ts`.

---

## 7. Manual probe endpoint — 🟢 implemented

- [x] `POST /v0/management/test` — send a canonical probe request through
      the *real* dispatch path for a given provider/model/apiType, without
      needing an inference key. Reference: `routes/management/test.ts` (69
      lines) + `services/probes/probe-service.ts` (328 lines) +
      `services/probes/stream-probe.ts` (408 lines). Straightforward once
      item #2 (shared dispatch) exists, since it just calls dispatch with a
      canned payload per `apiType` and reports success/duration/error.
      Depends on #2 for a clean integration point.

---

## 8. Responses API — conversations resource — 🟢 done

- [x] `GET /v1/conversations/:conversation_id` — Trefoil's `responses.rs`
      only exposes `/v1/responses/:id`; the separate `conversations` table
      (already backed up per `backup.rs`'s `TABLES` list, so the schema
      exists) has no read route. Reference:
      `routes/inference/responses.ts` (conversation retrieval branch) +
      `services/responses/responses-storage.ts` (408 lines).

---

## 9. Vision descriptor generation — 🟡 partial

`/v0/management/cache/vision-descriptor/clear` exists but clears nothing,
because vision descriptors are never generated in the first place. Reference:
`services/vision/vision-descriptor-service.ts` (310 lines) — generates
textual descriptions of image inputs for providers/models that need
image-to-text fallback (`vision_fallthrough` config, which Trefoil *does*
already store/validate).

- [~] Implement descriptor generation triggered by `vision_fallthrough`
      config on non-vision-capable target models.
- [x] Wire the cache clear endpoint to actually clear it.

---

## 10. Compaction execution — 🟡 partial

Config CRUD (`GET`/`PATCH /v0/management/config/compaction`) is implemented
and validated in `config_routes.rs`, but whether the inference path actually
*applies* compaction (headroom or native strategy) to oversized requests was
not confirmed in this audit — needs a direct code check against
`services/compaction/compaction-service.ts` (265 lines) +
`headroom-compactor.ts` (320 lines) equivalence.

- [x] Implement native and local headroom compaction application before dispatch.
- [~] Configured `strategy: "headroom"` now calls the actual `/v1/compress`
      service protocol with auth, timeout, protected instructions/recent messages,
      validated output and fail-open behavior. Mock service tests pass. Complete
      model-context/inheritance and native-message parity remain open.
      `strategy: "external"` remains invalid (the reference also only defines
      `native` and `headroom`).

---

## 11. Frontend — 🟡 partial

Trefoil now has dashboard, providers, aliases, keys, quotas, usage, configuration, MCP, system logs, debug, errors, logs, models, My Key, playground, metrics, and custom-checker pages. Core utility tests exist; browser tests remain.
Plexus: 16+ top-level pages + decomposed sub-pages, 251 source files, 8 test
files.

Missing pages (each needs its own API adapter + component + tests):
- [x] `Config` — full config editor (failover/cooldown/compaction/timeout/
      stall/trusted-proxies/vision-fallthrough/mcp-enabled).
- [x] `CustomQuotaCheckers` — CRUD and test UI is implemented.
- [x] `Debug` — debug trace viewer/toggle with trace list, detail, and deletion.
- [x] `DetailedUsage` + 6 sub-modules (`Breakdown`, `Charts`, `Controls`,
      `Header`, `RequestLog`, `Stats`) — the main usage analytics dashboard.
- [x] `Errors` — inference error log viewer at `/errors` with pagination, detail inspection, per-record deletion, and admin-only clear-all.
- [x] `Logs` — debug logs viewer (distinct from system logs).
- [x] `Mcp` — MCP server management UI (CRUD + process control + logs),
      depends on backend items already done (mcp.rs) but the UI doesn't
      exist.
- [x] `Models` — model catalog browser at `/models` with authenticated loading,
      search, empty/error/loading states, target visibility, refresh, and
      delete actions. Metadata editing remains available through `/aliases`.
- [x] `MyKey` — self-service key page (self_routes.rs backend exists;
      no frontend).
- [x] `Playground` — interactive request tester, depends on backend item #7.
- [x] `SystemLogs` — live syslog viewer, depends on backend item #6.
- [x] Existing pages (`Keys`, `Providers`, `Quotas`) in Plexus are richer
      (modals for editing, quota status views, key lists decomposed into
      `keys/*` — 7 files) than Trefoil's single-file equivalents; audit
      each for missing fields/actions, not just missing pages.
- [~] Port the 8 Plexus frontend test areas (`format`, `currency`, `api`,
      `modelOptions`, `modelList`, `normalize`, `detailed-usage/helpers`):
      core `format`, `modelOptions`, and `normalize` helpers now have Vitest
      coverage, and the management API adapter now covers OAuth session
      lifecycle and failure responses; currency, model-list, and detailed-usage
      helper tests have now been added (44 total tests).
- [ ] Browser/e2e test parity — the old `PARITY_TESTING.md` roadmap called
      for this; confirm scope once pages above exist.

---

## 12. Differential harness — 🟡 partial

`parity/differential.ts` currently has 313 named cases. OAuth provider
discovery now matches Plexus's response shape: Trefoil omits its internal
`capabilities` metadata while preserving the provider descriptors.

- [x] Add a documented one-shot script that boots Plexus + Trefoil + Postgres,
      provisions fixtures, and runs `bun run parity/differential.ts`, failing
      the build on any diff; repeatable CI wiring remains open.
- [~] Add differential cases for every route ported under items #1–#9 above
      as they land — the harness is the acceptance gate, not a checklist to
      backfill later.
- [ ] Add differential cases for the admin MCP shim (`tools/call` for each
      of the ~48 operations in item #5) — 86 read/validation operation cases now exist and use initialized MCP transport;
      successful mutation coverage and exact result parity remain open.

---

## Historical parity estimate (superseded by live verification below)

Using the 12 plan areas with equal weight, and scoring partial areas by
checked capability items rather than route count, the current estimate is
**approximately 82%**. This is a progress metric, not a claim of drop-in
compatibility. Remaining risk is concentrated in provider-specific OAuth
exchanges, complete dispatch extraction/cancellation/attempt-history semantics,
custom-checker runtime parity, MCP edge cases, frontend browser coverage,
and repeatable differential CI.

## Current verified status (completion pass, 2026-09-18)

**Not complete.** A full local seven-lane gate ran using disposable PostgreSQL,
Plexus `31b09cebac35338ce4ff2a07035a3ae2062d3920`, and the current Trefoil tree.

| Gate | Result |
|---|---|
| `cargo test --all-targets` | 295 unit + 4 integration tests pass |
| OAuth storage / dispatch integration | 4 disposable PostgreSQL tests pass |
| Frontend Vitest | 45 tests across 8 files pass |
| Frontend typecheck | pass |
| Frontend production build | pass |
| Playwright Chromium | 3 browser scenarios pass |
| Differential harness unit tests | 12 pass |
| Live differential | **406 pass, 1 fail, 407 observed, no missing/duplicate cases** |

Reports: `target/parity-report.json` and `target/differential-report.json`.
These are regenerable local artifacts, not committed acceptance evidence.
Run `./verify-parity.sh`; install frontend dependencies and Playwright Chromium
first. An existing browser can be selected with
`PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH`. CI now includes these lanes with a pinned
reference checkout, and the runner owns only its disposable database container.

### Verified improvements

- Recent system logs retain the newest 1,000 entries; live SSE has event IDs,
  subscription cleanup, and runtime severity filtering.
- Limited-key usage streams reject both foreign-key and unscoped activity.
- Metrics include live cooldown gauges, provider performance and daily cost;
  timezone handling uses actual local midnight.
- Built-in checker upstream calls are wired into manual checks and polling;
  mock HTTP tests cover auth headers, allowance windows and upstream failures.
- MCP missing-key errors and stored process-log responses are corrected;
  recursive redaction preserves object shape instead of inventing fields.
- OAuth transport has injectable tests, provider client IDs, validated expiry,
  UTF-8-safe diagnostic masking and query-preserving URL redaction. This does
  **not** complete provider-specific login or first-party request masking.
- Frontend browser tests exercise authentication/navigation, usage/error states,
  provider creation and ordered system-log events. Remaining helper areas now
  have tests. This is three fixture-backed scenarios, not exhaustive page parity.
- MCP differential calls initialize the protocol, negotiate JSON/SSE, and parse
  responses structurally. The 86 operation cases largely exercise reads and
  missing-input validation, **not** successful mutation coverage for every tool.

### Corrections to earlier completion claims

- Vision now calls the configured descriptor model from opted-in chat and
  messages dispatch, supports custom prompts/OpenAI and Anthropic image blocks,
  and reuses successful cached descriptions. HTTP tests cover these paths,
  failures and opt-out. Descriptor usage accounting, full OAuth/native-provider
  support and all API-family transforms remain open, so section 9 stays partial.
- OAuth credentials are currently persisted as raw token columns; encryption
  at rest claimed above is **not implemented**. Device-flow and provider-specific
  authorization/exchange semantics remain incomplete.
- The shared dispatch entry point, complete attempt chain and uniform
  cancellation verification are still open. An attempted external compaction
  implementation introduced regressions and was removed; local compaction
  remains working, external/headroom-service parity remains open.
- Metrics still lack reference dimensions and exact exposition parity.
- Built-in checkers still need full scoped-window/reset/account/refresh semantics;
  scheduler per-checker interval handling is not equivalent to the reference.
- The live MCP suite exposes numerous result/error-envelope differences,
  lifecycle differences and absent management-tool audit logging. Fixture state
  changes also require isolation; failures are not normalized away.

### Latest progress

The full gate is intentionally red until the remaining live differences and remaining
capability gaps are resolved. The old 82% estimate was not an acceptance result
and should not be used to claim drop-in compatibility. Newly found gaps above
supersede stale checked-item descriptions elsewhere in this document.

## Suggested order of attack

1. **Provider-specific OAuth/masking (#1)** — largest remaining backend capability gap.
2. **Shared dispatch extraction and cancellation (#2)** — route-specific loops remain.
3. **Built-in checker behavior and MCP edge cases (#4/#5).**
4. **Frontend browser/e2e coverage and remaining depth (#11).**
5. **Differential CI and broader live inference/MCP cases (#12).**


## Follow-up completion pass

Earlier follow-up gate: **383/407 live cases matched; 24 differed**, with all six other
lanes passing. Backend test count is now 272 unit + 4 integration: obsolete
MCP tests for operations unsupported by the pinned reference were replaced with
reference-contract validation tests, not treated as successful supported APIs.
MCP tool errors, collection shapes, destructive confirmation and exact allowed
operation lists are substantially improved. Shared logging test locking removes
an observed race. The harness re-provisions authentication after destructive
restore probes, exposing actual authorization differences rather than testing
against a deleted reference key. Remaining live failures include full-backup
comparison, metrics completeness, management authorization, MCP lifecycle,
configuration/usage shapes, checker discovery, metadata refresh and audit logs.

## Latest verified completion pass

The full seven-lane gate now observes **404 passing / 3 failing live cases**,
with 272 backend unit tests, 4 backend integration tests, 44 frontend unit tests,
3 browser tests and 11 harness unit tests passing; frontend build/typecheck pass.
Remaining live cases are metrics exposition, recent system logs, and model
metadata refresh. This does **not** close untested capability gaps in sections
1/2/4/9/10 or make the 407 cases exhaustive.

This pass fixed limited-principal 403 responses, MCP lifecycle/settings/operation
shapes, custom checker discovery, reference-shaped usage summaries, management
MCP audit persistence and peer identity, reset of MCP audit records, and empty
backup table files. Binary backups are now decompressed and compared by entry
rather than decoded as UTF-8. Audit UUIDs/timings and backup timestamps/dialect
are validated before normalization; identity, counts, tool, status and payload
remain compared. Malformed restore probes run last because the reference
partially mutates configuration even when rejecting them; Trefoil does not copy
that unsafe behavior. Usage grouped breakdowns/exclusions remain incomplete.

## Latest implementation pass: runtime accounting and external services

Full gate: 281 backend unit + 4 integration, 45 frontend unit, 3 browser and
12 harness unit tests pass; build/typecheck pass. Latest live result is
**405/407 matching**, with recent logs differing and metadata refresh differing
only because Plexus coalesced the manual request into an in-flight `startup`
refresh. An earlier run matched 406/407. Counts, source identity, success and
errors remain strict; the trigger race was not normalized away.

Implemented real attempt concurrency counts, streaming ownership and synchronous
release when the body is dropped; metrics now expose these counts and match the
reference exposition. Scheduler intervals now run independently in minutes.
Metadata refresh fetches and retains catalogs, including auxiliary OpenRouter
embeddings/video sources, with tested partial-source failure handling.
Configured Headroom service requests preserve protected context and fail open;
local fallback remains when no service base URL is explicitly configured.
Dashboard summary adaptation now handles the new reference-shaped backend data.

OpenRouter token transport now supports its JSON key exchange and nonexpiring
key response, but end-to-end login remains open: an unverified authorization/
manual-code handler change was removed rather than marked complete. Encryption,
other provider flows, shared dispatch extraction, comprehensive cancellation,
metadata consumption/scheduling and exhaustive checker semantics remain open.

## Latest security and OAuth implementation pass

Verified gate: **406/407 live cases match** (recent system-log histories differ).
288 backend unit tests + 4 stdio integration tests, 45 frontend tests, 3 browser
tests, and 12 harness tests pass. The ignored-by-default credential integration
test was **separately executed and passed** against disposable PostgreSQL 16.

OAuth access/refresh storage now uses AES-256-GCM with random nonces and
length-prefixed provider/account/field associated data. `TREFOIL_CREDENTIAL_KEY`
(32-byte hex) is mandatory for writes/decryption; missing/wrong keys, tampered
rows and legacy plaintext fail closed. The credential upsert's missing timestamp
binding was fixed. Quota checkers use decrypted accessors. See
`docs/credentials.md`; existing plaintext rows/key rotation need explicit
migration, not silent acceptance.

OpenRouter authorization uses its actual `callback_url`/PKCE contract, supports
admin-submitted redirect URLs/codes, and persists nonexpiring keys via the JSON
exchange. Local HTTP tests prove the exchange wire format, and state tests prove
one-time verifier claiming. Live provider login was not performed. Other provider
onboarding remains incomplete. Cancelled verifiers are removed from persisted
sessions, prompts cannot reopen an exchange, and refresh re-reads credentials
under its per-account gate. Cross-process exchange/cancellation races still need
transactional treatment. No claim of all six providers working is made.

## Latest transactional OAuth pass

The verification gate now has eight lanes: **seven pass**, while live
comparison remains **406/407** (recent system-log history). Backend coverage is
289 unit + 4 stdio integration tests; the separate storage lane executes all
3 normally ignored PostgreSQL credential tests. Frontend/harness counts remain
45 / 3 browser / 12 harness tests.

Durable OAuth claims now use a conditional database update; two simultaneous
claimers have exactly one winner. Credential insertion and session completion
are transactional, so cancelled/expired/already-completed sessions cannot write
a new credential. Terminal session records cannot be overwritten by stale
upserts, and completion clears verifier/prompt and records completion progress.
These properties are exercised against disposable PostgreSQL and enforced in CI
by `parity/test-oauth-storage.sh`, which never uses production `DATABASE_URL`.

Anthropic now uses the pinned reference client ID, authorization/token hosts,
JSON refresh/exchange bodies and required state; payload tests cover those
contracts. This does not finish its fixed-port callback-server flow. Copilot's
client-ID transcription was also corrected. Remaining provider device flows,
callback listeners and live provider acceptance remain unverified/open.

## Latest dispatch correctness pass

Chat transport and HTTP-status failover now use the same candidate-chain helper.
Streaming requests retry retryable HTTP failures and preserve the fallback SSE
body instead of parsing it as JSON. Disabled failover does not contact fallback
targets, and configured nonretryable transport errors stop iteration. Reqwest
connection/timeout errors are mapped to the policy's error codes.

Attempt numbering is contiguous; a real PostgreSQL test dispatches through two
503 providers then success and asserts `[1,2,3]` in `inference_attempts`.
Streaming tests assert SSE terminal preservation and no leaked concurrency
permits. Live concurrency records now include reference timestamps.

Latest full gate: 291 backend unit + 4 stdio integration, **4 executed disposable
PostgreSQL integration tests**, 45 frontend unit, 3 browser, 12 harness tests
pass; live comparison remains **406/407**. Shared entry-point extraction and
uniform semantics for all nine APIs are still open; this is consolidation and
verified bug-fixing, not completion of section 2.

## Latest media dispatch and metadata-consumer pass

Speech responses now stream opaque bytes with the upstream content type instead
of JSON-decoding audio. Tests exercise non-UTF-8 MPEG bytes and concurrency
release. Native streaming only starts on successful status, allowing retryable
errors to follow normal failover. Malformed multipart input is rejected before
transcription dispatch rather than silently accepting the parsed prefix.

Model authorization precedes external Headroom calls: tests verify denied input
never reaches the compactor. Metadata search/lookup now use the loaded catalog,
not local aliases, with real normalized metadata exposed and tested.

The latest full eight-lane gate passed seven lanes and matched 406/407 live
cases; a final multipart fix additionally passed all backend tests, bringing
backend coverage to **295 unit + 4 stdio integration**. The separate disposable
PostgreSQL lane continues to execute 4 storage/dispatch integration tests.
The overall plan is still incomplete; the remaining live comparison is not a
substitute for provider-flow, shared-dispatch and exhaustive parity work.
