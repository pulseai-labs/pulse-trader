-- r2.s2.w2 — 0011 down: restore the exact 0010 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One state says 0010 cannot
-- hold what is here: a `trade` row carrying `stop_price` — the recorded
-- per-trade stop has no representation at 0010, and dropping the column over it
-- would erase the only record of the price that drove the trade's sizing, exit
-- resolution, and R math (the same invented-fact argument as 0010's refused
-- window-edge mark). Refused TRANSACTIONALLY and before anything moves: the
-- scratch table + trigger pattern is 0010's, so the refusal names the state
-- that blocked it.
--
-- What IS discarded, knowingly: nothing but the column itself — every NULL row
-- (all pre-0011 history) downgrades losslessly because none of them have a
-- recorded stop to lose. The column drops by `ALTER TABLE ... DROP COLUMN`
-- (SQLite >= 3.35); no table rebuild is needed or attempted.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0010 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0011_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0011_down_guard_stop BEFORE INSERT ON _0011_down_guard
WHEN NEW.reason = 'stop_price'
BEGIN
  SELECT RAISE(ABORT, 'migration 0011 down: a trade carries a recorded stop price and 0010 has no column for it; refusing rather than falsifying the trade record');
END;

INSERT INTO _0011_down_guard (reason)
SELECT 'stop_price' FROM trade WHERE stop_price IS NOT NULL LIMIT 1;

DROP TRIGGER _0011_down_guard_stop;
DROP TABLE _0011_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0011's column, restoring 0010 exactly.
-- ---------------------------------------------------------------------------
ALTER TABLE trade DROP COLUMN stop_price;
