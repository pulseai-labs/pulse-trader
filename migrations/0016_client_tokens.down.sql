-- r3.s3.w1 — 0016 down: reverse-order teardown (mirror `0004_llm_call.down.sql`).
-- Drops the audit side first (it references `client_token`), then the triggers,
-- then the tables. Each DROP is IF EXISTS for idempotent re-runs.

DROP TRIGGER IF EXISTS token_audit_no_delete;
DROP TRIGGER IF EXISTS token_audit_no_update;
DROP INDEX IF EXISTS idx_token_audit_at;
DROP TABLE IF EXISTS token_audit;
DROP TRIGGER IF EXISTS client_token_no_delete;
DROP TRIGGER IF EXISTS client_token_no_update;
DROP TABLE IF EXISTS client_token;
