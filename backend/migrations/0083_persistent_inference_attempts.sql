CREATE TABLE "inference_attempts" (
    "id" bigserial PRIMARY KEY,
    "request_id" text NOT NULL,
    "attempt_index" integer NOT NULL,
    "provider" text NOT NULL,
    "model" text NOT NULL,
    "api_type" text,
    "status" text NOT NULL,
    "reason" text,
    "status_code" integer,
    "retryable" integer NOT NULL DEFAULT 0,
    "provider_response_headers" jsonb,
    "started_at" bigint NOT NULL,
    "completed_at" bigint,
    CONSTRAINT "inference_attempts_request_index_unique" UNIQUE("request_id", "attempt_index")
);
--> statement-breakpoint
CREATE INDEX "idx_inference_attempts_request" ON "inference_attempts" USING btree ("request_id", "attempt_index");
