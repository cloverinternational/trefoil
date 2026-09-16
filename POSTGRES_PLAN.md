# PostgreSQL persistence foundation plan

## Scope

Move the currently implemented Trefoil management slice from production
in-memory storage to PostgreSQL while preserving the existing HTTP contracts.
This milestone covers keys, providers, aliases, user quotas, system settings,
and persisted cooldowns. It does not implement the still-missing inference,
MCP, OAuth, usage, or operational APIs.

## Decisions

- PostgreSQL is the only production database.
- `DATABASE_URL` is required for the real binary; startup fails clearly when
  it is absent or unreachable.
- Use `sqlx` with the Tokio PostgreSQL runtime and a bounded connection pool.
- Use checked-in SQL migrations executed at startup.
- Keep an explicit in-memory constructor for fast unit tests only.
- Use Docker Compose for a reproducible local PostgreSQL instance and test
  documentation.
- Match Plexus table names, column names, types, defaults, indexes, and JSONB
  shapes wherever the feature is shared. This keeps future data migration and
  cross-checking against Plexus possible.
- Use transactions for mutations that validate references and then change
  state, especially quota deletion and alias updates.

## Implementation sequence

1. Add `sqlx` and required Tokio features; create the database module and pool
   configuration.
2. Reuse Plexus's PostgreSQL schema and `migrations_pg` history as the
   compatibility baseline instead of inventing a parallel schema. Import the
   relevant SQL/migration files with attribution and record the upstream
   revision used.
3. Add Trefoil-owned follow-up migrations only where Rust needs a compatibility
   fix or a new feature; never silently fork table names or column semantics.
4. Add repository/query functions for each entity and map database rows to the
   existing domain structs.
5. Refactor route handlers to use repositories while retaining an explicit
   in-memory test backend where needed for current unit tests.
6. Make `main.rs` load `DATABASE_URL`, connect, run migrations, and construct
   the PostgreSQL-backed application state. Remove unconditional production
   seeding of `test-key`; provide fixtures only through test setup/documented
   development provisioning.
7. Add `docker-compose.yml`, environment documentation, and update parity
   instructions to provision both databases safely.
8. Add integration tests for migrations, CRUD persistence across state reload,
   concurrent mutations, transaction rollback, reference protection, and
   startup configuration failures.
9. Run formatting, compile/build, all Rust tests, the Plexus suite, and the
   79-case differential parity harness against fresh PostgreSQL-backed
   instances.

## Acceptance criteria

- `DATABASE_URL=... cargo run -p trefoil-backend` starts against PostgreSQL.
- No production route reads or writes the old HashMap stores.
- Restarting Trefoil preserves all management mutations.
- Reference checks and alias-cycle checks are transaction-safe.
- No secrets appear in logs or error bodies.
- Existing parity cases remain 79/79 passing.
- Rust tests and PostgreSQL integration tests pass from a clean database.
- Documentation includes setup, migration, reset, backup, and test commands.

## Risks and mitigations

- Preserve response serializers and add golden/contract assertions before
  changing storage internals.
- Keep test-only in-memory construction until repository-backed tests cover all
  handlers, preventing a large unverified rewrite.
- Treat database errors as controlled 5xx responses; never silently fall back
  to memory.
- Use transaction-scoped reads for delete/reference operations.
- Use a dedicated test database/schema and cleanup strategy to avoid tests
  contaminating parity runs.

## Verification commands

```sh
cargo fmt --check
cargo test --all-targets
cargo build
# with PostgreSQL and both servers running:
PLEXUS_BASE_URL=http://127.0.0.1:4100 \
TREFOIL_BASE_URL=http://127.0.0.1:8080 \
bun run parity/differential.ts
```
