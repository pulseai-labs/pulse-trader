-- r2.s3.w2 — 0012 down: restore the exact 0011 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One state says 0011 cannot
-- hold what is here: a `backtest_run` row carrying `window_lead_in_from_ms` —
-- the lead-in start has no representation at 0011, and dropping the column
-- over it would erase the only record of which bars the engine warmed on
-- (the same invented-fact argument as 0010's refused window-edge mark and
-- 0011's refused stop price). Refused TRANSACTIONALLY and before anything
-- moves: the scratch table + trigger pattern is 0010's/0011's, so the
-- refusal names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the trigger and the column —
-- every NULL row (unwindowed runs and all pre-0012 history) downgrades
-- losslessly because none of them have a lead-in to lose. The trigger drops
-- BEFORE the `ALTER TABLE ... DROP COLUMN`, then the column drops the same
-- way (SQLite >= 3.35); no table rebuild is needed or attempted.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0011 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0012_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0012_down_guard_lead_in BEFORE INSERT ON _0012_down_guard
WHEN NEW.reason = 'window_lead_in'
BEGIN
  SELECT RAISE(ABORT, 'migration 0012 down: a backtest_run carries a recorded window lead-in start and 0011 has no column for it; refusing rather than falsifying the run record');
END;

INSERT INTO _0012_down_guard (reason)
SELECT 'window_lead_in' FROM backtest_run WHERE window_lead_in_from_ms IS NOT NULL LIMIT 1;

DROP TRIGGER _0012_down_guard_lead_in;
DROP TABLE _0012_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0012's trigger, then its column, restoring 0011 exactly.
-- ---------------------------------------------------------------------------
DROP TRIGGER backtest_run_window_lead_in_pair;
ALTER TABLE backtest_run DROP COLUMN window_lead_in_from_ms;
