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
- [x] Token storage + refresh scheduling per provider (persist
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
logic instead of calling one shared service. `resolve_target` only returns
the **first** eligible alias target per group — no iteration through the
full ordered candidate list on failure.

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

**Impact if skipped:** works fine for the common case (one healthy fallback);
degrades ungracefully with multiple simultaneous provider outages, and the
duplicated logic is a maintenance/bug-parity risk (a failover fix has to be
applied 4 times).

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

No `custom_checkers` table, no CRUD, no execution engine. Trefoil's
`quota_checkers` endpoint returns a hardcoded mirror of Plexus's *built-in*
checker types and an always-empty `configured: []`.

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

To port: **~48 remaining operations.** Since every underlying management
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

- [x] Implement descriptor generation triggered by `vision_fallthrough`
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
- [ ] External LLM-based headroom compaction remains unsupported and explicit.

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
      helpers remain.
- [ ] Browser/e2e test parity — the old `PARITY_TESTING.md` roadmap called
      for this; confirm scope once pages above exist.

---

## 12. Differential harness — 🟡 partial

`parity/differential.ts` has ~275 named cases but this audit did not confirm
they currently pass end-to-end (requires two live servers + Postgres
fixtures per `parity/README.md`, not wired into CI).

- [ ] Wire a CI job (or at minimum a documented one-shot script) that boots
      Plexus + Trefoil + Postgres, provisions fixtures, and runs
      `bun run parity/differential.ts`, failing the build on any diff.
- [~] Add differential cases for every route ported under items #1–#9 above
      as they land — the harness is the acceptance gate, not a checklist to
      backfill later.
- [ ] Add differential cases for the admin MCP shim (`tools/call` for each
      of the ~48 operations in item #5) — currently no case type for JSON-RPC
      tool calls exists in the harness (worth checking).

---

## Current parity estimate (recalculated 2026-09-17)

Using the 12 plan areas with equal weight, and scoring partial areas by
checked capability items rather than route count, the current estimate is
**approximately 89%**. This is a progress metric, not a claim of drop-in
compatibility. Remaining risk is concentrated in provider-specific OAuth
exchanges, complete dispatch cancellation/attempt-history semantics,
custom-checker runtime parity, MCP edge cases, frontend browser coverage,
and repeatable differential CI.

## Current verified status

- **Backend tests:** 203 passing (`cargo test --all-targets`).
- **Frontend:** typecheck, build, and 23 Vitest tests pass, including OAuth
  session API lifecycle, authentication/error, and malformed-response coverage
  (`cd frontend && npm test`, `npm run typecheck`, `npm run build`).
- **Latest media dispatch:** image edit and audio transcription use shared target dispatch; speech remains to be verified.
- **Differential harness:** 265 named cases are registered in
  `parity/differential.ts`; no live Plexus/Trefoil run was performed in this
  refresh, so no end-to-end pass count is claimed.
- **Frontend:** management pages through metrics, debug, logs, MCP, system logs, models, My Key, playground, and custom checkers build and typecheck successfully.
- **MCP:** remote HTTP proxy, local subprocess lifecycle/stdio transport, OAuth token validation, and management-shim operations are implemented across the listed tools.
- **OAuth:** MCP-client authorization-code/PKCE, refresh rotation/revocation, and provider login-session persistence/PKCE URL generation are implemented; provider-specific exchanges and masking remain partial.
- **Observability:** Prometheus metrics, usage SSE, and system-log SSE routes are implemented.

The checklist is capability-based: a checked route means the route exists and
has real behavior, not that every Plexus edge case or frontend test has been
ported. Registered differential cases are coverage inventory, not evidence of
an end-to-end pass until the live runner is executed.

## Latest progress

The completed work is recorded above; unchecked items remain intentionally open until directly verified by tests or source comparison. The current strongest remaining work items are manual probe, provider OAuth-specific flows/masking, custom built-in checkers, MCP shim edge cases, frontend depth/tests, and differential CI.

## Suggested order of attack

1. **Provider-specific OAuth/masking (#1)** — the largest remaining backend capability gap.
2. **Compaction and vision execution (#9/#10)** — currently config/route-level or no-op behavior.
3. **Exact MCP shim edge cases and checker-specific operations (#4/#5).
4. **Frontend test coverage and remaining pages/depth (#11).
5. **Differential CI and inference-focused differential cases (#12).
6. **Provider OAuth login flow (#1)** — largest single effort (~1800+ lines
   of reference logic across 6+ provider-specific flows), but unlocks
   custom quota checkers (#4) and is the only item that blocks a class of
   providers entirely.
7. **Custom quota checkers (#4)** — after #1.
8. **Compaction verification (#10)** — quick to check, unknown effort to fix
   if actually missing.
9. **Frontend (#11)** — largest total surface area; can proceed in parallel
   with backend work since most pages are independent, but several (Debug,
   Mcp UI, SystemLogs, Playground, CustomQuotaCheckers) are gated on the
   backend items above landing first.
10. **Differential CI gate (#12)** — should be tightened continuously
    alongside every item above, not deferred to the end.
