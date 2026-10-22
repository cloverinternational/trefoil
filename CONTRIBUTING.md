# Contributing to Trefoil

Thanks for helping improve Trefoil. This guide covers how to set up, what we
check, and what a good pull request looks like. By participating you agree to
the [Code of Conduct](CODE_OF_CONDUCT.md).

## Ways to contribute

- **Report a bug** — open an issue with the bug report template. Include the
  Trefoil version or commit, how you run it, and steps to reproduce.
- **Suggest a feature** — open an issue first for anything non-trivial so the
  approach can be agreed before you invest time.
- **Fix or build something** — pick an open issue, or send a focused pull
  request. Small, single-purpose changes are reviewed fastest.
- **Report a vulnerability** — do **not** open a public issue; follow
  [SECURITY.md](SECURITY.md).

## Development setup

Requirements: Rust 1.88+, Node.js 22, Docker (PostgreSQL and the e2e scripts).
The commands are in the [Development section of the README](README.md#development).

```sh
git clone https://github.com/cloverinternational/trefoil.git && cd trefoil
npm --prefix frontend ci
```

## Before you open a pull request

Run the same checks CI runs:

```sh
cargo fmt --all --check
cargo clippy --workspace --locked -- -D warnings
cargo test --workspace --locked --lib
npm --prefix frontend run typecheck
npm --prefix frontend test
npm --prefix frontend run build
```

Also run what your change touches:

- **Behaviour that must match Plexus** — see [docs/parity.md](docs/parity.md)
  and the `parity/*-e2e.sh` scripts. Intentional differences must be documented
  there.
- **UI changes** — `npm --prefix frontend run test:e2e` (the service-free specs
  need a built frontend). Look at your change in the browser, in both light and
  dark themes.
- **Database changes** — add a new numbered migration in
  `backend/migrations/`; never edit an applied one.

## Conventions

- **Rust** — `cargo fmt`, no clippy warnings, no panics on request paths; return
  errors in the Plexus-compatible shape.
- **Frontend** — TypeScript strict, Tailwind utility classes, components from
  `frontend/src/components/ui`. The UI is deliberately dense and square-edged:
  no rounded corners or shadows, compact controls (`h-7`, `text-xs`), and
  keyboard access for anything an operator does often.
- **Secrets** — never commit credentials, `.env`, or production data dumps.
- **Commits** — [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat(ui): …`, `fix(backend): …`, `docs: …`, `test: …`). One logical change
  per commit.
- **Changelog** — add a line under `## [Unreleased]` in
  [CHANGELOG.md](CHANGELOG.md) for user-visible changes.

## Pull request checklist

- [ ] Focused change with a clear description of *what* and *why*
- [ ] Tests added or updated; all checks above pass
- [ ] Docs and changelog updated where behaviour changed
- [ ] No secrets, generated files or unrelated formatting churn

## Licensing

Trefoil is [MIT licensed](LICENSE). By submitting a contribution you agree it
may be distributed under that license. Code derived from third-party projects
must be MIT-compatible and recorded in
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
