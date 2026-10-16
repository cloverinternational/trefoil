# Catalog organization

## Contract

- Protected routes in `frontend/src/App.tsx`: `/providers`, `/models`, `/aliases`, `/keys`. Providers use `GET /v0/management/providers`, Models and Aliases share `GET /v0/management/aliases` (a model catalog entry is an alias slug, not an independent model resource), API Keys use `GET /v0/management/keys` (`frontend/src/lib/api.ts`). Admin-only writes are enforced by the existing backend management routes. List GET errors are shown on their pages; empty states must remain distinct from no filter matches.
- `frontend/src/pages/Providers.tsx` already uses a table; Models and ModelAliases use tables; ApiKeys uses cards and currently prints `key.secret` in its list. Key list must never render secret data. Alias `target_groups` are routing strategy, **not** organizational groups. API Key `permissionGroup` affects access/analytics and must **not** be confused with organizational groups.

## Semantics

User selected **organization only**: one group per item, multiple tags, shared group vocabulary across Providers, Models, Aliases and API Keys. Names and tags are administrator-editable metadata persisted server-side across browsers; never alter inference routing, alias target groups, provider status or key permissions. Models and Aliases reference the same alias slug, but can be organized separately by resource type. Unassigned is the default. A group or tag filter intersects case-insensitive search and any page-specific status/provider filter; clearing filters restores alphabetic order. Filters are per-page ephemeral UI state; no URL or browser-local secret metadata. Missing/deleted resources must not show stale memberships. No-match should differ from an empty backend catalog.

## Shared

`backend/migrations/0089_catalog_organization.sql` stores shared groups with `ON DELETE SET NULL` and item metadata keyed by kind/resource ID. `backend/src/catalog_organization.rs` offers admin-only GET, group PUT/DELETE and item PUT/DELETE; unavailable DB 503, unknown group 400, duplicate group 409. The API stores no credentials or routing configuration. `frontend/src/lib/catalogOrganization.ts` and `frontend/src/components/CatalogOrganizer.tsx` implement combined case-insensitive search/group/tag filters, group creation/deletion, per-row group and tags, clear and refresh. Writes refresh only after success and show errors. Group and tag labels use lowercase slug syntax (letters/numbers/single hyphens; max 64 bytes server-side). Absent/deleted catalog rows do not show stale organization metadata, though orphan metadata can remain in the backing table until explicitly deleted. Concurrent edits are last-write-wins.

## Providers

`frontend/src/pages/Providers.tsx` retains the provider table, status switch, edit/delete and OAuth workflows. Its Organization column and shared controls filter provider slug/display name; routing state is separate from metadata.

## Models

`frontend/src/pages/Models.tsx` lists aliases-as-models with target context and independent `model` organization metadata. Existing alias routing and delete remain unchanged.

## Aliases

`frontend/src/pages/ModelAliases.tsx` lists aliases with target and target-group context and independent `alias` organization metadata. An alias routing target group is not an organizational group.

## API Keys

`frontend/src/pages/ApiKeys.tsx` is now a compact table with key name, comment, permission group, scopes, status and organization controls. It no longer renders key secrets or searches them. Existing add/edit/disable/delete remain; metadata never changes `permissionGroup`.

## E2E

Against local Compose `http://127.0.0.1:8080` with authenticated admin and installed ARM Chromium, `frontend/e2e/catalog-organization-live.spec.ts` passed 2/2. It created a disposable shared group, assigned one existing row in each of Providers/Models/Aliases/API Keys with a unique tag, combined group+tag+nonmatching search, cleared filters, reloaded and confirmed persisted group via browser and sanitized metadata API. A second check confirmed the rendered key table contains none of the returned credential values (without printing them). Unknown group returned 400; unauthenticated metadata GET returned 401. Fixture memberships/group were removed in `finally`; subsequent SQL counts for `org-e2e-%` and the tagged rows were both zero. Combined catalog and existing provider regression suite passed 4/4. No existing routing/key permissions were changed. Full OAuth, concurrent editors, stale orphan cleanup, limited-principal denial, offline/error retry and responsive mobile for all four pages were not exercised here. Frontend build, backend check, database migration and `git diff --check` passed; Vite emitted only a large-chunk warning. Root housekeeping audit found pre-existing `.swarmpi`, `MAP.md`, `node_modules`, `package-lock.json`, `package.json` outside this repo; nothing was moved or deleted.
