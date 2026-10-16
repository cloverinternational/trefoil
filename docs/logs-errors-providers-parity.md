# Logs, Errors and Providers parity work

## Comparison

Observed authenticated admin Plexus `http://100.121.114.16:4000/ui/logs`, `/ui/errors`, `/ui/providers` versus Trefoil `http://127.0.0.1:8080/logs`, `/errors`, `/providers`, at roughly 1256px desktop on October 1, 2026. Reference screenshots `/tmp/plexus-{logs,errors,providers}-desktop.png`; local baselines `/tmp/trefoil-{logs,errors,providers}-desktop.png`. The live Plexus revision is unknown. Trefoil retains its brand and CSS colors.

- Plexus **Logs**: page header subtitle, SSE connection badge, Delete All, key/model/provider/date searches, per-page 20/50/100/200, dense wide table with date/key/API/model/tokens/cost/perf/meta/status, page controls, row details. Trefoil has a 25-row paginated table, local-only free-text filter, date/model/provider/status/tokens, raw JSON dialog, refresh and single delete. Backend `GET /v0/management/usage` currently supports only limit/offset/api_key (`backend/src/usage.rs:112-117,184-235`), so model/provider/date filters cannot honestly claim server-wide search without a backend change. `/v0/management/events` is a distinct usage SSE route, not `/v0/system/logs/stream` (System Logs); keep them separate.
- Plexus **Errors**: header with Delete All/Refresh, split recent-error list and details area. Observed empty production state says “No errors found” and “Select an error to inspect details”. Trefoil has a compact card/table with local text filter, Inspect modal and pagination. Backend `GET /v0/management/errors` scopes limited keys (`backend/src/errors.rs:51-111`); no list error observed in production reference, so populated details must be grounded in source and local data, not invented screenshot.
- Plexus **Providers**: header Add OAuth account/Add provider, compact table ID/name, status toggle, model count, quota/balance, actions; provider editing is a rich modal. Trefoil shows grid cards, combined Add provider wizard, refresh/custom checkers and OAuth cards. Provider CRUD is `backend/src/providers.rs:29-46,625-740`; avoid changing stored provider IDs, secrets or enabled state merely for visual comparison. Reference provider table counts are from the Plexus config; Trefoil data differs.

## Contracts

- `frontend/src/App.tsx:38-65` registers protected routes. `frontend/src/lib/api.ts:151-163,201-217,219-252` defines usage, errors and provider adapters; `frontend/src/pages/Logs.tsx`, `Errors.tsx`, `Providers.tsx` own page state. Limited users may read scoped Logs/Errors, but only admins may delete; Providers is admin-only at the API. Keep all secrets redacted in screenshots and notes.
- No unit tests may be added. E2E verification should use a real local backend or existing browser flows; do not mutate Plexus production. The existing System Logs SSE behavior is not the request Logs route and must not be conflated.

## Logs

`frontend/src/pages/Logs.tsx` now uses a compact eight-column table, page-size 20/50/100/200, key/model/provider/date inputs, explicit Search, page controls, row details and admin-only single deletion. It preserves API paging and scoped-key reads. **Filters apply only to the fetched page**, clearly labeled in the UI; backend has no model/provider/date query, so cross-page filtering remains a parity gap. API direction, advanced token/perf/meta fields, SSE badge and bulk delete are not asserted as implemented. Loading, empty-filter, fetch error and retry are distinct. No invalid/malformed SSE is processed by this page.

## Errors

`frontend/src/pages/Errors.tsx` now renders a Recent Errors pane and persistent detail pane, with admin-only Delete All/individual delete and a Refresh button. Backend pagination is 50 rows/page with next/previous controls; scoped reads are unchanged. The “Grouped by signature · last 24h” subtitle was deliberately replaced with “Recent inference errors” because Trefoil's endpoint does not group or enforce a 24h window. Empty data leaves the details prompt visible and bulk delete disabled. Request failures show alert/retry; no invented result rows. JSON details parse defensively. A malformed successful response currently normalizes to an empty list in `api.ts`, so it cannot distinguish malformed payload from genuine empty; this remains a gap.

## Logs and Errors E2E

`frontend/e2e/logs-errors-live.spec.ts` ran against live local Compose at `127.0.0.1:8080` with installed ARM Chromium: 3/3 passed. Verified nonempty Logs, per-page 50, page-local model no-match/reset, details dialog; empty Errors split panes, disabled Delete All, Refresh; mobile control reachability. `npm run build` and `git diff --check` passed. No deletion was executed; nonempty error details and admin-vs-limited authorization not exercised in this run. Initial Playwright launch failed because this checkout's pinned browser 1148 is not installed; supplied the existing `chromium-1243/chrome-linux-arm64/chrome` executable. A failed test iteration used button role for the Radix combobox; corrected to combobox and reran green.

## Providers

`frontend/src/pages/Providers.tsx` uses the compact, horizontally scrollable table with ID/name, enabled switch, models, quota/balance and edit/delete actions. Models and quota/balance show an honest dash: the current provider-list contract has no model count or quota fields. Add OAuth account and Add provider now open distinct existing OAuth chooser/API-key flows. OAuth cards, refresh and custom checkers remain accessible. Enabled switches use backend's admin-only merging `PATCH /v0/management/providers/{slug}` with only `{enabled}`, not destructive PUT; the row updates only after HTTP success, failures toast. Existing provider edits still use PUT and retain their pre-existing limitations: complex fields may not be round-tripped by the simple edit form, so avoid editing complex production providers until this is addressed. Empty list, load error, invalid form and OAuth failure states are preserved. Trefoil identity/colors stay unchanged.

## Providers E2E

`frontend/e2e/providers-live.spec.ts` passed 2/2 in installed ARM Chromium against local Compose: table and separate OAuth/API-key entry, create disposable provider, toggle enabled and re-read backend to confirm the URL is unchanged, reopen edit, delete only that disposable provider in `finally`; narrow 390px table and controls remain accessible without document overflow. No existing provider was mutated. Real OAuth authorization, provider deletion confirmation and unavailable-backend failure were not exercised. Frontend build and `git diff --check` passed. Root audit reported pre-existing `.swarmpi`, `MAP.md`, `node_modules`, `package-lock.json`, `package.json` at `/home/swarm/clover`; no root entries were created or moved by this task.
