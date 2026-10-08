-- r4.s1.w4 — 0019 down: restore the exact 0018 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One table says 0018 cannot
-- hold what is here: a `certification_freeze` row. The row IS the record of
-- the holdout the campaign ran under — its start, H, alpha, the C1 test, and
-- (once closed) the fact that the holdout is spent. Dropping it would erase
-- the only account of a freeze a certification may have been made under, and
-- F1's "a spent holdout is never reused" would have nothing left to enforce.
-- Refused TRANSACTIONALLY and before anything moves, in the scratch-table +
-- refuse-trigger pattern 0010/0013/0018 set, so the error names the state that
-- blocked it.
--
-- What IS discarded, knowingly: nothing but the schema — a database holding no
-- freeze row downgrades losslessly because nothing at 0018 references the
-- table. The guard trigger drops before the schema it guards; the table's own
-- triggers and the one-open index go with the table.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0018 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0019_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0019_down_guard_freeze BEFORE INSERT ON _0019_down_guard
WHEN NEW.reason = 'certification_freeze'
BEGIN
  SELECT RAISE(ABORT, 'migration 0019 down: a certification_freeze row exists and 0018 has no table for it; refusing rather than erasing the freeze record');
END;

INSERT INTO _0019_down_guard (reason)
SELECT 'certification_freeze' FROM certification_freeze LIMIT 1;

DROP TRIGGER _0019_down_guard_freeze;
DROP TABLE _0019_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0019's table, restoring 0018 exactly (triggers + index go with it).
-- ---------------------------------------------------------------------------
DROP TABLE certification_freeze;
