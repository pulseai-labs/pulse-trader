-- r6.s2.w3 — 0023 down: restore the exact 0022 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One state says 0022 cannot hold
-- what is here: a `trade` row carrying a setup trace — the arm, dip and anchor
-- times have no representation at 0022, and dropping the columns over them would
-- erase the only record of why the trade entered (the same invented-fact
-- argument as 0011's refused stop price). Refused TRANSACTIONALLY and before
-- anything moves, with the scratch table + trigger pattern of 0011's down file,
-- so the refusal names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the columns themselves — every NULL
-- row (all pre-0023 history, and every trade of a strategy without a setup)
-- downgrades losslessly because none of them have a trace to lose. The columns
-- drop by `ALTER TABLE ... DROP COLUMN` (SQLite >= 3.35), in reverse order of
-- their creation; no table rebuild is needed or attempted.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0022 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0023_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0023_down_guard_trace BEFORE INSERT ON _0023_down_guard
WHEN NEW.reason = 'setup_trace'
BEGIN
  SELECT RAISE(ABORT, 'migration 0023 down: a trade carries a setup trace and 0022 has no column for it; refusing rather than falsifying the trade record');
END;

INSERT INTO _0023_down_guard (reason)
SELECT 'setup_trace' FROM trade
WHERE setup_arm_time IS NOT NULL
   OR setup_dip_time IS NOT NULL
   OR setup_anchor_time IS NOT NULL
LIMIT 1;

DROP TRIGGER _0023_down_guard_trace;
DROP TABLE _0023_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0023's columns, restoring 0022 exactly.
-- ---------------------------------------------------------------------------
ALTER TABLE trade DROP COLUMN setup_anchor_time;
ALTER TABLE trade DROP COLUMN setup_dip_time;
ALTER TABLE trade DROP COLUMN setup_arm_time;
