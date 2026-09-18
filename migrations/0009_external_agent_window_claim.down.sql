-- r2.s1.w1 — 0009 down: reconstruct the EXACT 0008 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL, which for this one means it
-- has to answer "can 0008 hold what is in these tables?" before it does
-- anything. Three states say no:
--
--   * an `agent_submission` row — 0008 has no table for the external-agent
--     audit record, and dropping it would erase the only durable link between
--     an `external_agent` version and the agent + hypothesis that produced it;
--   * a `strategy_version` row with `created_by = '"external_agent"'` — the
--     0008-era `CreatedBy` enum has no such variant, so a downgraded binary
--     cannot deserialize the row at all. The generic `create_version` path
--     can write a BARE external-agent version (no `agent_submission`), so the
--     submission check alone does not cover this state;
--   * a `backtest_run` row carrying `window_from_ms`/`window_to_ms` — 0008 has
--     no column for the window the run consumed, and NULLing it would falsify
--     the run's recorded inputs (the same invented-fact argument that kept
--     0006's provenance columns nullable rather than backfilled).
--
-- Each is refused, TRANSACTIONALLY and before a single row moves, so a
-- downgrade either restores 0008 exactly or leaves 0009 exactly as it was.
-- `RAISE(ABORT, ...)` is trigger-only, hence the scratch table: one trigger per
-- reason, so the refusal names WHICH state blocked it.
--
-- What IS discarded, knowingly: the `coaching_sessions_one_pending_per_run`
-- index itself, and the two NULL window columns. A pending claim under 0008 is
-- a native shape — the index guards FUTURE writes, not a stored record — so a
-- live claim downgrades losslessly (unlike 0008's down, where pending had no
-- representation at all). The columns drop by `ALTER TABLE ... DROP COLUMN`
-- (SQLite ≥ 3.35); no table rebuild is needed or attempted.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0008 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0009_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0009_down_guard_submission BEFORE INSERT ON _0009_down_guard
WHEN NEW.reason = 'agent_submission'
BEGIN
  SELECT RAISE(ABORT, 'migration 0009 down: an agent_submission row exists and 0008 has no table for it; refusing rather than discarding the external-agent audit record');
END;

CREATE TRIGGER _0009_down_guard_agent_version BEFORE INSERT ON _0009_down_guard
WHEN NEW.reason = 'external_agent_version'
BEGIN
  SELECT RAISE(ABORT, 'migration 0009 down: a strategy_version has created_by=''external_agent'' and the 0008 binary cannot deserialize it; refusing rather than stranding a row it cannot read');
END;

CREATE TRIGGER _0009_down_guard_window BEFORE INSERT ON _0009_down_guard
WHEN NEW.reason = 'windowed_run'
BEGIN
  SELECT RAISE(ABORT, 'migration 0009 down: a backtest run records a consumed date window and 0008 has no column for it; refusing rather than falsifying the run''s inputs');
END;

INSERT INTO _0009_down_guard (reason)
SELECT 'agent_submission' FROM agent_submission LIMIT 1;

INSERT INTO _0009_down_guard (reason)
SELECT 'external_agent_version' FROM strategy_version
WHERE created_by = '"external_agent"' LIMIT 1;

INSERT INTO _0009_down_guard (reason)
SELECT 'windowed_run' FROM backtest_run
WHERE window_from_ms IS NOT NULL OR window_to_ms IS NOT NULL LIMIT 1;

DROP TRIGGER _0009_down_guard_submission;
DROP TRIGGER _0009_down_guard_agent_version;
DROP TRIGGER _0009_down_guard_window;
DROP TABLE _0009_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0009's rules and shapes, restoring 0008 exactly.
-- ---------------------------------------------------------------------------
DROP INDEX IF EXISTS coaching_sessions_one_pending_per_run;

-- The trigger references the columns it guards; it must go first, or the
-- DROP COLUMN below is refused as a schema dependency.
DROP TRIGGER IF EXISTS backtest_run_window_pair;
ALTER TABLE backtest_run DROP COLUMN window_to_ms;
ALTER TABLE backtest_run DROP COLUMN window_from_ms;

DROP TRIGGER IF EXISTS agent_submission_version_kind;
DROP TRIGGER IF EXISTS agent_submission_no_delete;
DROP TRIGGER IF EXISTS agent_submission_no_update;
DROP TABLE agent_submission;
