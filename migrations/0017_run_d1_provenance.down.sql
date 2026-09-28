-- r3.s2.w4 — 0017 down: restore the exact 0016 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One state says 0016 cannot
-- hold what is here: a `backtest_run` row carrying a recorded daily data
-- version — that tag is the only record of which daily snapshot the run's
-- `d1` operands actually read, and dropping the column over it would make the
-- run's result unexplainable (the same invented-fact argument as 0011's
-- refused stop price and 0015's refused symbol filters). Refused
-- TRANSACTIONALLY and before anything moves, with the scratch table + trigger
-- pattern 0011 established: the refusal names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the column itself — every NULL row
-- (all pre-0017 history, and every run whose strategy carries no `d1` operand)
-- downgrades losslessly because none of them have a daily snapshot to lose.
-- The column drops by `ALTER TABLE ... DROP COLUMN` (SQLite >= 3.35); no table
-- rebuild is needed or attempted, and the immutability triggers do not name
-- this column.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0016 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0017_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0017_down_guard_d1 BEFORE INSERT ON _0017_down_guard
WHEN NEW.reason = 'd1_data_version'
BEGIN
  SELECT RAISE(ABORT, 'migration 0017 down: a backtest_run carries a recorded daily data version and 0016 has no column for it; refusing rather than falsifying the run record');
END;

INSERT INTO _0017_down_guard (reason)
SELECT 'd1_data_version' FROM backtest_run
WHERE d1_data_version IS NOT NULL
LIMIT 1;

DROP TRIGGER _0017_down_guard_d1;
DROP TABLE _0017_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0017's column, restoring 0016 exactly.
-- ---------------------------------------------------------------------------
ALTER TABLE backtest_run DROP COLUMN d1_data_version;
