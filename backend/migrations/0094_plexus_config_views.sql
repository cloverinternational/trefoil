-- Plexus repository-shaped configs (provider_view / alias_view).
-- NULL for rows written before 0094; those keep loading from legacy columns.
ALTER TABLE providers ADD COLUMN config json;
ALTER TABLE model_aliases ADD COLUMN config json;
ALTER TABLE user_quota_definitions ALTER COLUMN limit_value TYPE double precision;
ALTER TABLE user_quota_definitions ALTER COLUMN warn_at TYPE double precision;
