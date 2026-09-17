# Path to a 100% Plexus drop-in replacement

Granular checklist derived from direct source comparison (`backend/src/*.rs`
vs `.reference/plexus/packages/backend/src/**`), not estimates. Each item
lists what exists today, what's missing, and roughly how big the gap is.

Legend: 🔴 not started · 🟡 partial · 🟢 done

---

## 1. Provider OAuth login flow — 🟡 partial

**Missing entirely.** Config schema already has `oauth_provider`/
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
- [x] `GET /v0/management/oauth/sessions/:id` — poll session status
      (currently a hardcoded 404 stub in `oauth.rs`; needs a real session
      store + state machine).
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

This is a **bigger gap than the route inventory suggests.** All 12 tool
names exist in `mcp_plexus.rs` and are correctly listed via `tools/list`,
but `tools/call` only implements 2 of ~35 operations
(`plexus_config` → `get`/`status`). Every other operation across every other
tool returns `{"ok":false,"error":{"message":"Operation is not implemented
yet"}}`.

Plexus reference: `routes/mcp/plexus/configuration.ts` (440 lines) +
`operations.ts` (395 lines) + `management.ts` implement the full operation
set:

| Tool | Operations in Plexus | Implemented in Trefoil |
|---|---|---|
| `plexus_config` | get, export, status | get, status (2/3) |
| `plexus_provider` | list, get, put, update, delete, fetch_models | 0/6 |
| `plexus_model_alias` | list, get, put, update, delete, delete_all | 0/6 |
| `plexus_key` | list, get, put, update, delete, disable | 0/6 |
| `plexus_quota` | list, get, put, update, delete, status, clear, recompute | 0/8 |
| `plexus_quota_checker` | types, list, get | 0/3 |
| `plexus_mcp_gateway` | servers_list, get, put, update, delete | 0/5 |
| `plexus_settings` | get | 0/1 |
| `plexus_system_logs` | recent, level, set_level, reset_level | 0/4 |
| `plexus_usage` | list, summary | 0/2 |
| `plexus_debug` | state | 0/1 |
| `plexus_operations` | backup, restore, refresh_metadata, list_cooldowns, clear_cooldowns | 0/5 |

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
- [ ] Secret redaction on `plexus_key`/`plexus_provider` responses by
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

## 7. Manual probe endpoint — 🔴 not started

- [ ] `POST /v0/management/test` — send a canonical probe request through
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

## 9. Vision descriptor generation — 🔴 not started (currently a truthful no-op)

`/v0/management/cache/vision-descriptor/clear` exists but clears nothing,
because vision descriptors are never generated in the first place. Reference:
`services/vision/vision-descriptor-service.ts` (310 lines) — generates
textual descriptions of image inputs for providers/models that need
image-to-text fallback (`vision_fallthrough` config, which Trefoil *does*
already store/validate).

- [ ] Implement descriptor generation triggered by `vision_fallthrough`
      config on non-vision-capable target models.
- [ ] Wire the cache clear endpoint to actually clear it.

---

## 10. Compaction execution — 🟡 unverified

Config CRUD (`GET`/`PATCH /v0/management/config/compaction`) is implemented
and validated in `config_routes.rs`, but whether the inference path actually
*applies* compaction (headroom or native strategy) to oversized requests was
not confirmed in this audit — needs a direct code check against
`services/compaction/compaction-service.ts` (265 lines) +
`headroom-compactor.ts` (320 lines) equivalence.

- [ ] Verify/implement compaction application in the request pipeline before
      dispatch, for both `native` (provider-side truncation) and `headroom`
      (external compaction service call) strategies.

---

## 11. Frontend — 🟡 partial

Trefoil: 6 pages (`Dashboard`, `Login`, `ApiKeys`, `ModelAliases`,
`Providers`, `Quotas`), ~650 LOC, 0 tests.
Plexus: 16+ top-level pages + decomposed sub-pages, 251 source files, 8 test
files.

Missing pages (each needs its own API adapter + component + tests):
- [x] `Config` — full config editor (failover/cooldown/compaction/timeout/
      stall/trusted-proxies/vision-fallthrough/mcp-enabled).
- [x] `CustomQuotaCheckers` — CRUD and test UI is implemented.
- [x] `Debug` — debug trace viewer/toggle with trace list, detail, and deletion.
- [x] `DetailedUsage` + 6 sub-modules (`Breakdown`, `Charts`, `Controls`,
      `Header`, `RequestLog`, `Stats`) — the main usage analytics dashboard.
- [ ] `Errors` — inference error log viewer.
- [ ] `Logs` — debug logs viewer (distinct from system logs).
- [ ] `Mcp` — MCP server management UI (CRUD + process control + logs),
      depends on backend items already done (mcp.rs) but the UI doesn't
      exist.
- [ ] `Models` — model metadata/catalog browser.
- [ ] `MyKey` — self-service key page (self_routes.rs backend exists;
      no frontend).
- [ ] `Playground` — interactive request tester, depends on backend item #7.
- [ ] `SystemLogs` — live syslog viewer, depends on backend item #6.
- [x] Existing pages (`Keys`, `Providers`, `Quotas`) in Plexus are richer
      (modals for editing, quota status views, key lists decomposed into
      `keys/*` — 7 files) than Trefoil's single-file equivalents; audit
      each for missing fields/actions, not just missing pages.
- [ ] Port the 8 Plexus frontend test files (`format`, `currency`, `api`,
      `modelOptions`, `modelList`, `normalize`, `detailed-usage/helpers`)
      and add equivalents for any new Trefoil-specific logic.
- [ ] Browser/e2e test parity — the old `PARITY_TESTING.md` roadmap called
      for this; confirm scope once pages above exist.

---

## 12. Differential harness — run it, don't just extend it

`parity/differential.ts` has ~275 named cases but this audit did not confirm
they currently pass end-to-end (requires two live servers + Postgres
fixtures per `parity/README.md`, not wired into CI).

- [ ] Wire a CI job (or at minimum a documented one-shot script) that boots
      Plexus + Trefoil + Postgres, provisions fixtures, and runs
      `bun run parity/differential.ts`, failing the build on any diff.
- [ ] Add differential cases for every route ported under items #1–#9 above
      as they land — the harness is the acceptance gate, not a checklist to
      backfill later.
- [ ] Add differential cases for the admin MCP shim (`tools/call` for each
      of the ~48 operations in item #5) — currently no case type for JSON-RPC
      tool calls exists in the harness (worth checking).

---

## Current verified status

- **Backend tests:** 174 passing.
- **Latest media dispatch:** image edit and audio transcription use shared target dispatch; speech remains to be verified.
- **Frontend:** dashboard, providers, aliases, keys, quotas, usage, and configuration pages build and typecheck successfully.
- **MCP:** remote HTTP proxy, local subprocess lifecycle/stdio transport, OAuth token validation, and management-shim operations are implemented across the listed tools.
- **OAuth:** MCP-client authorization-code/PKCE, refresh rotation/revocation, and provider login-session persistence/PKCE URL generation are implemented; provider-specific exchanges and masking remain partial.
- **Observability:** Prometheus metrics, usage SSE, and system-log SSE routes are implemented.

The checklist is capability-based: a checked route means the route exists and has real behavior, not that every Plexus edge case or frontend test has been ported.

## Latest progress

The completed work is recorded above; unchecked items remain intentionally open until directly verified by tests or source comparison. The current strongest remaining work items are manual probe, provider OAuth-specific flows/masking, custom built-in checkers, MCP shim edge cases, frontend depth/tests, and differential CI.

## Suggested order of attack

1. **Shared dispatch service (#2)** — unblocks accurate failover parity and
   is a prerequisite for the manual probe endpoint (#7) and cleaner future
   work; also reduces the 4x logic duplication that's a correctness risk
   today.
2. **Quota aggregation (#3)** — small, self-contained, already flagged in
   code comments.
3. **Admin MCP shim operations (#5)** — mostly thin wrappers around routes
   that already exist; high leverage-to-effort ratio (~48 operations, but
   each is a few lines once the REST handler is reused).
4. **Observability endpoints (#6)** — metrics is pure read/format over
   existing tables; SSE streams need a broadcast channel but are
   self-contained.
5. **Conversations resource (#8)** and **vision descriptor (#9)** — small,
   isolated.
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
