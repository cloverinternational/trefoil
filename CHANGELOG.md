# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Brand: new identity. The mark is three equal lobes, each bitten by its neighbour,
  whose channels meet at one point (one endpoint, routed to many providers), built
  from circles as single-path SVGs with a separate 16-24px cut; lowercase Geist
  wordmark; jade green on one OKLCH hue. See `docs/brand/`. New favicon.ico,
  adaptive favicon.svg, apple-touch-icon and social preview.
- Admin UI colors: primary, success and accent use the brand greens; destructive is
  darker in light mode and lighter in dark mode. All text and button-label pairs
  now pass WCAG AA (light-mode primary text and destructive labels failed before).
  Success and error badges show an icon.
- Reference Plexus moved to `a4cd39b0` (46 commits). Ported: Anthropic OAuth
  reasoning translation and system-prompt preservation, mid-conversation system
  entries and per-turn effort, `responses:lite` reasoning context, Muse Code
  subscription compatibility, automatic prompt caching for translated requests,
  completion-only Responses tool arguments, provider-stream capture before
  trace flush, MCP admin-key bearer auth, persistent local MCP data
  directories, `tools/list` TTL clamping, `sslmode` for PostgreSQL URLs, hiding
  the legacy OpenAI Codex login, the ChatGPT sign-in device id, and the Cheaper
  Inference and Runcrate provider presets. No upstream PostgreSQL migrations
  were added in this range.
- `migration/plexus_to_trefoil.py` now migrates a running Plexus through the
  management backup/restore API instead of reading its SQLite file.

- Admin UI: compact, square-edged redesign across every page (no rounded
  corners or shadows, 13px base type, denser tables, cards and forms), a
  brighter destructive color in dark mode, and a full-width layout.
- Admin UI: catalog filter toolbar (providers, models, aliases, API keys) is a
  single row with inline group and tag controls.

### Added

- Admin UI keyboard shortcuts: command palette (Ctrl/⌘+K), `g` + letter
  navigation, `/` to focus search, `[` to collapse the sidebar, `?` for help.
- Brand assets (`docs/assets/banner.svg`, `docs/assets/logo.svg`) and a UI
  favicon.
- Community health files: `CONTRIBUTING.md`, `CODE_OF_CONDUCT.md`, issue and
  pull request templates, Dependabot configuration and `.editorconfig`.
- Cargo and npm package metadata (`rust-version`, repository, description).

### Fixed

- Backup restore ignored user quotas, settings, MCP servers and keys, and OAuth
  credentials, rejected real Plexus alias and key exports, and could not
  restore full archives (including Trefoil's own). Config backups now also
  export MCP servers and keys and OAuth credentials.

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
