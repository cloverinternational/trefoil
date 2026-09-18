ALTER TABLE providers ADD COLUMN IF NOT EXISTS retry_policy jsonb;
