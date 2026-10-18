# PostgreSQL migrations

These migrations are the PostgreSQL compatibility baseline for Trefoil and are
ported from Plexus's `packages/backend/drizzle/migrations_pg` schema history.
They retain the upstream filenames and ordering so table names and column
semantics remain compatible with Plexus data. Keep filenames and ordering
stable. New Trefoil-specific migrations must be appended after migration 0082
and documented here.

The source is the bundled `.reference/plexus` checkout. When synchronizing a
newer upstream revision, copy only the PostgreSQL migration history, never the
SQLite history.

Trefoil migration numbers 0087–0092 now describe Trefoil-specific changes and
do not match Plexus's newer migrations with those same numbers. Do not copy
Plexus migration 0087–0091 over them. Trefoil migration
`0093_add_alias_synthetic_safeguard_approval.sql` appends a boolean alias
configuration column with `false` as the default for existing rows; a fresh
disposable PostgreSQL migration and alias PUT/PATCH/GET round trip are checked
by `parity/alias-safeguard-e2e.sh`. Other upstream schema additions require a
separate compatibility review and append-only Trefoil migrations.
Trefoil migration `0094_plexus_config_views.sql` adds nullable `providers.config` and `model_aliases.config`
(JSONB): the full Plexus repository-shaped provider and alias (headers, extraBody,
per-model pricing, stall/timeout/concurrency overrides, quirks, compaction…).
The API key stays in its encrypted `api_key` column; headers and quota-checker
options are encrypted inside `config` when `ENCRYPTION_KEY` is set. Rows from
before 0094 keep loading from the legacy columns until next written.

`0095_usage_upstream_model_service_tiers.sql` adds the `request_usage` columns from
Plexus pg migrations 0083/0089 (`upstream_model`, requested/actual service tier
and their raw values) used by the engine usage storage.
