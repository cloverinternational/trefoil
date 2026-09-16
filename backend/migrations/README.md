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
