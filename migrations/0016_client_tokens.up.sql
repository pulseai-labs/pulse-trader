-- r3.s3.w1 — 0016: the client-token tables (ADR-0026; D5 tokens, D8 migration).
--
-- Conventions mirror `0004_llm_call` exactly (the established append-only shape):
-- TEXT keys, RFC3339 UTC timestamps injected via the `Clock`, a `created_by`
-- provenance tag, and a `schema_version` row tag. NO money columns here, so the
-- Decimal-as-TEXT rule has no surface; NO f64 column anywhere (NFR-2).
--
-- `client_token` stores ONLY the SHA-256 hex of the full token string (D5) — the
-- token itself is never persisted anywhere, in any table, in any form. Labels are
-- UNIQUE (revoked labels are never reused — the UNIQUE holds after revocation
-- because rows are never deleted and label is immutable). `scope` is checked
-- against the D5 vocabulary.
--
-- Immutability is NARROWED, not blanket: the one legal mutation of a
-- `client_token` row is its FIRST `revoked_at` transition (NULL -> a value).
-- Every other column is compared `IS NOT` (NULL-safe) and any second
-- `revoked_at` write is refused, so revocation is once-only and nothing else
-- about the row can drift. `token_audit` is fully append-only (no UPDATE, no
-- DELETE), mirroring `llm_call`.
--
-- 0015 is deliberately absent (reserved for r3.s1); sqlx records versions
-- individually, so the gap is legal and the embedded max is 16.

CREATE TABLE client_token (
  id             TEXT PRIMARY KEY NOT NULL,
  label          TEXT NOT NULL UNIQUE,
  scope          TEXT NOT NULL CHECK (scope IN ('app','agent')),
  token_sha256   TEXT NOT NULL UNIQUE CHECK (length(token_sha256) = 64),
  created_at     TEXT NOT NULL,                 -- injected Clock (RFC3339 UTC)
  revoked_at     TEXT,                          -- NULL until revoked, then final
  created_by     TEXT NOT NULL,                 -- provenance tag, e.g. `cli:token-issue`
  schema_version TEXT NOT NULL                  -- row-schema tag, asserted on read
);

-- The narrowed update law: refuse unless this is EXACTLY the first
-- `revoked_at` transition and every other column is untouched.
CREATE TRIGGER client_token_no_update BEFORE UPDATE ON client_token
WHEN NEW.id IS NOT OLD.id
  OR NEW.label IS NOT OLD.label
  OR NEW.scope IS NOT OLD.scope
  OR NEW.token_sha256 IS NOT OLD.token_sha256
  OR NEW.created_at IS NOT OLD.created_at
  OR NEW.created_by IS NOT OLD.created_by
  OR NEW.schema_version IS NOT OLD.schema_version
  OR NEW.revoked_at IS NULL
  OR OLD.revoked_at IS NOT NULL
BEGIN
  SELECT RAISE(ABORT, 'client_token is immutable except a first revoked_at');
END;

CREATE TRIGGER client_token_no_delete BEFORE DELETE ON client_token
  BEGIN SELECT RAISE(ABORT, 'client_token rows are never deleted'); END;

-- The audit trail: one row per issue, revoke and refusal (D5, the gate's audit
-- control). `route` carries the method and path only — never a query string and
-- never a header value; `peer` is the remote IP. `token_id` is NULL for
-- refusals where no token row exists (missing/unknown).
CREATE TABLE token_audit (
  id             TEXT PRIMARY KEY NOT NULL,
  at             TEXT NOT NULL,                 -- injected Clock (RFC3339 UTC)
  event          TEXT NOT NULL CHECK (event IN ('issued','revoked','refused')),
  token_id       TEXT REFERENCES client_token(id),
  label          TEXT,
  reason         TEXT,                          -- missing|unknown|revoked|scope for refusals
  route          TEXT,                          -- e.g. `GET /probe/app` — method + path only
  peer           TEXT,                          -- the remote IP
  schema_version TEXT NOT NULL
);

CREATE TRIGGER token_audit_no_update BEFORE UPDATE ON token_audit
  BEGIN SELECT RAISE(ABORT, 'token_audit is append-only'); END;
CREATE TRIGGER token_audit_no_delete BEFORE DELETE ON token_audit
  BEGIN SELECT RAISE(ABORT, 'token_audit is append-only'); END;

-- The audit is scanned by time (recent refusals first).
CREATE INDEX idx_token_audit_at ON token_audit(at);
