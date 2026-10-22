ALTER TABLE request_usage ADD COLUMN IF NOT EXISTS upstream_model text;
ALTER TABLE request_usage ADD COLUMN IF NOT EXISTS requested_service_tier text;
ALTER TABLE request_usage ADD COLUMN IF NOT EXISTS requested_service_tier_raw text;
ALTER TABLE request_usage ADD COLUMN IF NOT EXISTS service_tier text;
ALTER TABLE request_usage ADD COLUMN IF NOT EXISTS service_tier_raw text;
