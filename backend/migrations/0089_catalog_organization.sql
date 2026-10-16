CREATE TABLE catalog_organization_groups (
    name text PRIMARY KEY CHECK (name ~ '^[a-z0-9]+(-[a-z0-9]+)*$')
);

CREATE TABLE catalog_organization_items (
    kind text NOT NULL CHECK (kind IN ('provider', 'model', 'alias', 'key')),
    resource_id text NOT NULL,
    group_name text REFERENCES catalog_organization_groups(name) ON DELETE SET NULL,
    tags text[] NOT NULL DEFAULT '{}',
    PRIMARY KEY (kind, resource_id)
);

CREATE INDEX catalog_organization_items_group_idx ON catalog_organization_items (group_name);
