-- r6.s2.w4 — 0024 down: restore the exact 0023 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One state says 0023 cannot hold
-- what is here: a `backtest_run` row with a non-zero `skipped_stop_beyond_fill`
-- — 0023 has no column for the count, and dropping it would leave a run whose
-- stored content hash was derived with that count and no longer re-derives
-- (the read-time tamper guard would refuse the run). Refused TRANSACTIONALLY
-- and before anything moves, with the scratch table + trigger pattern of
-- 0011's and 0023's down files, so the refusal names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the column itself — a NULL row
-- (all pre-0024 history) and a row holding 0 downgrade losslessly, because a
-- zero count feeds nothing to the hash. The column drops by
-- `ALTER TABLE ... DROP COLUMN` (SQLite >= 3.35); no table rebuild is needed
-- or attempted.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0023 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0024_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0024_down_guard_skip BEFORE INSERT ON _0024_down_guard
WHEN NEW.reason = 'stop_beyond_fill'
BEGIN
  SELECT RAISE(ABORT, 'migration 0024 down: a run carries a non-zero stop_beyond_fill skip count and 0023 has no column for it; refusing rather than falsifying the run record');
END;

INSERT INTO _0024_down_guard (reason)
SELECT 'stop_beyond_fill' FROM backtest_run WHERE skipped_stop_beyond_fill > 0 LIMIT 1;

DROP TRIGGER _0024_down_guard_skip;
DROP TABLE _0024_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0024's column, restoring 0023 exactly.
-- ---------------------------------------------------------------------------
ALTER TABLE backtest_run DROP COLUMN skipped_stop_beyond_fill;
