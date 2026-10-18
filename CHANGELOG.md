# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
- Parity tooling: differential harness, oracle suites with recording mock
  upstreams, and focused end-to-end scripts.
- CI for formatting, linting, unit tests, frontend build and Docker images;
  manually triggered differential parity workflow.

[Unreleased]: https://github.com/cloverinternational/trefoil/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/cloverinternational/trefoil/releases/tag/v0.1.0
