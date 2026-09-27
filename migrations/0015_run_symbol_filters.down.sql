-- r3.s1.w4 — 0015 down: restore the exact 0014 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One state says 0014 cannot
-- hold what is here: a `backtest_run` row carrying recorded symbol filters —
-- the four values the run's sizer actually ran under are the comparability
-- key r3.s1.w4 exists to make checkable, and dropping the columns over them
-- would erase the only record of the constraints that shaped every trade
-- (the same invented-fact argument as 0011's refused stop price). Refused
-- TRANSACTIONALLY and before anything moves, with the scratch table + trigger
-- pattern 0011 established: the refusal names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the columns themselves — every
-- NULL row (all pre-0015 history) downgrades losslessly because none of them
-- have recorded filters to lose. The columns drop by
-- `ALTER TABLE ... DROP COLUMN` (SQLite >= 3.35); no table rebuild is needed
-- or attempted, and the immutability triggers do not name these columns.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0014 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0015_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0015_down_guard_filters BEFORE INSERT ON _0015_down_guard
WHEN NEW.reason = 'symbol_filters'
BEGIN
  SELECT RAISE(ABORT, 'migration 0015 down: a backtest_run carries recorded symbol filters and 0014 has no columns for them; refusing rather than falsifying the run record');
END;

INSERT INTO _0015_down_guard (reason)
SELECT 'symbol_filters' FROM backtest_run
WHERE lot_step IS NOT NULL OR min_qty IS NOT NULL
   OR min_notional IS NOT NULL OR max_leverage IS NOT NULL
LIMIT 1;

DROP TRIGGER _0015_down_guard_filters;
DROP TABLE _0015_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0015's columns, restoring 0014 exactly.
-- ---------------------------------------------------------------------------
ALTER TABLE backtest_run DROP COLUMN lot_step;
ALTER TABLE backtest_run DROP COLUMN min_qty;
ALTER TABLE backtest_run DROP COLUMN min_notional;
ALTER TABLE backtest_run DROP COLUMN max_leverage;
