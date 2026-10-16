-- Shared dashboard definitions. The version is incremented on each successful edit.
ALTER TABLE api_keys ADD COLUMN permission_group text;
ALTER TABLE request_usage ADD COLUMN permission_group text;
CREATE INDEX request_usage_analytics_idx ON request_usage (start_time, api_key, permission_group);

CREATE TABLE saved_dashboards (
    id text PRIMARY KEY,
    name text NOT NULL CHECK (length(name) BETWEEN 1 AND 120),
    config jsonb NOT NULL,
    version bigint NOT NULL DEFAULT 1 CHECK (version > 0),
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL
);

CREATE INDEX saved_dashboards_updated_at_idx ON saved_dashboards (updated_at DESC);
