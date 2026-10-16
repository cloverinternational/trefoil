ALTER TABLE providers ADD COLUMN IF NOT EXISTS api_base_urls jsonb NOT NULL DEFAULT '{}'::jsonb;
ALTER TABLE providers ADD COLUMN IF NOT EXISTS models jsonb NOT NULL DEFAULT '{}'::jsonb;
