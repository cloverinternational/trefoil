# Trefoil frontend reuse analysis

## Current state

Trefoil is currently an empty repository: there is no application source, Rust
crate, frontend, manifest, or build configuration yet.

The Plexus reference is available locally at `.reference/plexus/` and is ignored
by Git. It was cloned from `https://github.com/mcowger/plexus` at revision
`31b09cebac35338ce4ff2a07035a3ae2062d3920`.

## Plexus architecture observed

- The project is a Bun/TypeScript application (`package.json`, `bun.lock`,
  `tsconfig.json`, `bunfig.toml`).
- The repository contains separate `packages/frontend` and
  `packages/backend` workspaces. The frontend is React/TypeScript with
  `react-router-dom`, Tailwind, and a component tree under
  `packages/frontend/src/components`; the application entry points are
  `App.tsx` and `main.tsx`.
- API contracts are represented through OpenAPI-related scripts and generated
  types (`scripts/openapi-types.ts`, `scripts/sync-openapi.ts`, and
  `redocly.yaml`).
- The reference includes Docker and development orchestration, but those
  scripts should not be copied wholesale into Trefoil.
- Licensing must be reviewed before copying source or assets; the reference
  includes a `LICENSE` file.

## Reuse boundaries

The most portable material is presentational React code under
`packages/frontend/src/components`, layout code, and selected styling/assets.
The least portable material is `packages/frontend/src/lib/api`, page hooks, auth
context, and anything importing `@plexus/shared`; these encode Plexus-specific
routes, schemas, and authentication. The frontend already uses relative `/v0`
and `/v1` requests plus SSE, so a Rust replacement must preserve or deliberately
version-map those contracts rather than only matching page names.

## Recommendation

Reuse the Plexus frontend selectively, not as a whole repository. Since Trefoil
has no existing frontend, first establish a clean Rust API boundary and then
port or copy only the UI components that are useful. Keep the reference clone
read-only and ignored.

Recommended initial boundary:

1. Create a Rust HTTP service (Axum is a suitable default) with versioned JSON
   endpoints under `/api/v1`.
2. Create a separate frontend package using the reference's UI framework only
   after confirming the exact framework entry points and component license.
3. Put all browser calls behind a small typed client module. Do not let copied
   UI components call Plexus-specific server modules directly.
4. Start with one vertical slice: health/status plus one read-only domain page.
   Define request/response JSON in Rust and generate or hand-maintain matching
   frontend types from the contract.
5. Configure a development proxy or same-origin deployment so the browser uses
   `/api`, avoiding production CORS as the default integration path.

## Important unknowns before implementation

- Which Plexus screens and components Trefoil actually needs.
- The exact frontend framework and source entry points used by the current
  Plexus revision.
- Which Plexus backend behaviors are required and therefore need Rust
  equivalents.
- Authentication, persistence, streaming/WebSocket requirements, and domain
  schemas for Trefoil.

These should be resolved by tracing the reference's `src/` imports, routes,
API clients, and runtime scripts. No product source changes were made because
Trefoil does not yet define a target domain or a safe first vertical slice.

