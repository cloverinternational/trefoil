# Trefoil LAN Deployment Plan

## Goal

Provide a secure, repeatable first deployment for a trusted LAN using Docker
Compose, with PostgreSQL private to the Compose network and Trefoil/frontend
reachable through one host port. Initial deployment intentionally uses LAN HTTP;
HTTPS/Caddy remains an optional follow-up profile.

## Choreography

1. **Define the runtime contract.** Confirm the Rust backend's required
   environment, automatic migrations, `/healthz`, frontend relative `/v0` and
   `/v1` routing, and PostgreSQL persistence. This step determines container
   commands and health checks; no implementation depends on guesses.
2. **Build the backend image.** Add a multi-stage Dockerfile that compiles the
   Rust binary in a builder stage and runs it in a minimal non-root runtime.
   The runtime receives only `DATABASE_URL`, `ADMIN_KEY`,
   `TREFOIL_CREDENTIAL_KEY`, and bounded database settings.
3. **Build and serve the frontend.** Add a frontend build stage and a small
   unprivileged static server image. Proxy `/v0`, `/v1`, and `/healthz` to the
   backend so the browser uses same-origin requests and SSE remains usable.
4. **Compose the stack.** Extend the local Compose deployment with private
   PostgreSQL, backend and frontend services, named database storage, health
   dependencies, restart policies, no PostgreSQL host port, and an explicit
   LAN frontend port. Use an env file for secrets; never commit generated
   credentials.
5. **Add operational documentation.** Document secret generation, first login,
   provider/API-key setup, OAuth callback limitations on LAN HTTP, backups,
   upgrades, rollback, and how to bind the frontend to a specific LAN address.
6. **Verify progressively.** Run image builds, start the disposable stack,
   wait for health, verify frontend and backend routes, exercise persistence
   across restart, run backend/frontend tests, and report any environment gaps.

## Lowest-level data flow

Browser → frontend static server → `/v0`/`/v1` reverse proxy → backend →
private PostgreSQL. Backend startup connects to PostgreSQL, runs migrations,
loads state, starts refresh/checker schedulers, then listens on port 8080.
OAuth credential encryption uses `TREFOIL_CREDENTIAL_KEY`; admin management
uses `ADMIN_KEY`.

## Breaking points and mitigations

- Weak or missing secrets: fail startup or document a hard preflight check; use
  generated local `.env` with restrictive permissions.
- Database race: Compose healthcheck plus backend retry/start ordering; verify
  migrations and restart persistence.
- Public database exposure: do not publish PostgreSQL's port.
- Root container compromise: run backend/static server as non-root and use a
  read-only/static runtime where practical.
- Frontend proxy/SSE mismatch: test management requests, inference routes, and
  `/v0/system/logs/stream` through the frontend origin.
- LAN HTTP credential exposure: explicitly warn that admin/API/OAuth secrets are
  plaintext on the LAN; recommend Caddy HTTPS before untrusted networks.
- OAuth callback reachability: LAN HTTP requires `TREFOIL_PUBLIC_URL` to point
  at a reachable LAN address; document manual/device-code flows as safer for
  headless clients.
- Upgrade failure: take a database backup before image replacement; preserve
  the prior image tag and named volume for rollback.

## Acceptance gates

- `docker compose config` succeeds with a generated local env file.
- Backend image builds and runs as non-root.
- PostgreSQL is not reachable through a host-published port.
- `/healthz` returns `ok`; frontend loads from the LAN port.
- Admin login and one management read succeed through the frontend origin.
- A database-backed fixture survives backend/container restart.
- Existing backend/frontend test suites remain green.
- Documentation includes first-run, OAuth, backup, upgrade, and rollback steps.

## Non-goals for the first deployment

- Internet exposure or automatic HTTPS.
- Kubernetes, NixOS packaging, or multi-node scaling.
- Production-grade external backup storage.
- Claiming provider OAuth works without live provider credentials.
