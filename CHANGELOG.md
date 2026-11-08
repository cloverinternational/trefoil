# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- NixOS module: installs and starts **before** credentials exist. `credentials`
  defaults to `/etc/trefoil/env` and `/etc/trefoil/database-url`, and a missing file
  is a warning rather than an evaluation failure, so the files can be added after the
  service is installed and a restart brings it up.
- NixOS module: secrets are declared by name through `services.trefoil.credentials.<name>.file`
  (staged with `LoadCredential`), so any secret manager that produces files works.
  `environmentFile` and `databaseUrlFile` remain as shorthands.
- Admin UI is now served at `/`; `/ui` and `/ui/*` redirect to the root
  equivalent, because the client router has no basename.
- Admin UI: compact, square-edged redesign across every page (no rounded
  corners or shadows, 13px base type, denser tables, cards and forms), a
  brighter destructive color in dark mode, and a full-width layout.
- Admin UI: catalog filter toolbar (providers, models, aliases, API keys) is a
  single row with inline group and tag controls.

### Added

- Plain systemd units and an entrypoint under `deploy/` for hosts that are not
  NixOS; both bind loopback and read credentials through `LoadCredential`.
- Nix packaging: `flake.nix` with `packages.default` (backend plus built admin UI)
  and `nixosModules.default` exposing `services.trefoil`, running the service as a
  dedicated system user with systemd credentials and hardening.
- Generated interaction catalog (`docs/interactions.md`) covering every management
  route and environment variable, verified in CI by
  `scripts/gen-interactions.py --check`.
- Admin UI keyboard shortcuts: command palette (Ctrl/⌘+K), `g` + letter
  navigation, `/` to focus search, `[` to collapse the sidebar, `?` for help.
- Brand assets (`docs/assets/banner.svg`, `docs/assets/logo.svg`) and a UI
  favicon.
- Community health files: `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, issue and
  pull request templates, Dependabot configuration and `.editorconfig`.
- Cargo and npm package metadata (`rust-version`, repository, description).

### Fixed

- `TREFOIL_BIND_ADDRESS` is honoured by the listener. It was documented and read by
  Compose, but the server always bound `0.0.0.0`.
- Admin UI static assets were served as the HTML shell (or 404) instead of the
  bundle, so the interface failed to load.
- `Button` ignored `asChild`, so link buttons rendered as a `<button>` wrapping
  an anchor (the Models "Add model" button wrapped onto two lines).
- Overview used CSS classes that were not defined in this app, leaving an
  unreadable banner.

## [0.1.0] - 2026-10-18

First public release. Trefoil is a Rust port of
[Plexus](https://github.com/mcowger/plexus), verified against Plexus revision
`17b7e60804e3401acbe2bc2beb6b9b9042b31cce`.

### Added

- Inference endpoints compatible with Plexus: OpenAI Chat Completions,
  legacy Completions and Responses; Anthropic Messages; Google Gemini
  (`/v1beta`); embeddings; image generation and edits; speech and
  transcription; decisions; model listing and metadata.
- Inference engine with cross-format request/response/SSE translation, model
  aliases and routing selectors, failover and retries, provider cooldowns,
  stall detection, concurrency limits, sticky sessions and usage/cost
  accounting.
- `/v0` management API compatible with Plexus: API keys, providers, aliases,
  user quotas and quota checkers, cooldowns, configuration, backup and
  restore, logging, debug capture, errors, performance, metrics, MCP servers
  and OAuth sessions.
- OAuth-backed providers (Anthropic, OpenAI Codex/ChatGPT, GitHub Copilot,
  Kimi Code, xAI, Meta, OpenRouter) with encrypted token storage and
  background refresh.
- MCP proxy for HTTP and local stdio servers with API-key and OAuth access,
  and the management MCP server at `/mcp/plexus`.
- PostgreSQL storage with Plexus-compatible schema history and automatic
  migrations; at-rest encryption via `ENCRYPTION_KEY` and
  `TREFOIL_CREDENTIAL_KEY`.
- React admin UI, including usage analytics with saved dashboards and catalog
  organization (groups and tags) for providers, models, aliases and keys.
- `migration/plexus_to_trefoil.py` for moving a Plexus SQLite database to
  Trefoil.
- Docker Compose deployment (PostgreSQL, backend, nginx frontend) with health
  checks.
- System log entries are written to stdout in Plexus's console format
  (`timestamp [level] [module]: message`), so `docker compose logs` shows them.
- Parity tooling: differential harness, oracle suites with recording mock
  upstreams, and focused end-to-end scripts.
- CI for formatting, linting, unit tests, frontend build and Docker images;
  manually triggered differential parity workflow.

[Unreleased]: https://github.com/cloverinternational/trefoil/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/cloverinternational/trefoil/releases/tag/v0.1.0
