-- r4.s1.w5 — 0020 down: restore the exact 0019 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One table says 0019 cannot
-- hold what is here: a `certification` row. That row IS the record of one
-- hypothesis — the search verdict, the holdout's window, trade count, mean,
-- bound and the pass/fail the campaign's budget was spent on. Dropping it
-- would erase the only account of what a spent hypothesis scored, and the
-- budget count (`(freeze_id, hypothesis_index)`) would have nothing left to
-- check against. Refused TRANSACTIONALLY and before anything moves, in the
-- scratch-table + refuse-trigger pattern 0010/0013/0019 set, so the error
-- names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the schema — a database holding no
-- certification row downgrades losslessly because nothing at 0019 references
-- the table. Its two triggers and both indexes go with the table.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0019 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0020_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0020_down_guard_certification BEFORE INSERT ON _0020_down_guard
WHEN NEW.reason = 'certification'
BEGIN
  SELECT RAISE(ABORT, 'migration 0020 down: a certification row exists and 0019 has no table for it; refusing rather than erasing an immutable certification record');
END;

INSERT INTO _0020_down_guard (reason)
SELECT 'certification' FROM certification LIMIT 1;

DROP TRIGGER _0020_down_guard_certification;
DROP TABLE _0020_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0020's table, restoring 0019 exactly (triggers + indexes go with it).
-- ---------------------------------------------------------------------------
DROP TABLE certification;
