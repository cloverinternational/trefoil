CREATE TABLE "oauth_login_sessions" (
    "id" text PRIMARY KEY NOT NULL,
    "provider_id" text NOT NULL,
    "account_id" text NOT NULL,
    "status" text NOT NULL,
    "auth_info" jsonb,
    "prompt" jsonb,
    "progress" jsonb NOT NULL DEFAULT '[]'::jsonb,
    "error" text,
    "created_at" bigint NOT NULL,
    "updated_at" bigint NOT NULL,
    "expires_at" bigint NOT NULL
);
CREATE INDEX "idx_oauth_login_sessions_expires_at" ON "oauth_login_sessions" ("expires_at");
CREATE INDEX "idx_oauth_login_sessions_provider_account" ON "oauth_login_sessions" ("provider_id", "account_id");
