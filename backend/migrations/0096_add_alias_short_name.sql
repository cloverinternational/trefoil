-- Plexus pg migration 0092_add_alias_short_name (PR #999): nullable alias
-- short_name override. No backfill: derived short names are computed at
-- resolve time and never stored.
ALTER TABLE "alias_metadata_overrides" ADD COLUMN IF NOT EXISTS "short_name" text;
