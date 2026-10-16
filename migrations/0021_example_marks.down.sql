-- r5.s3.w1 — 0021 down: restore the exact 0020 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. Two tables say 0020 cannot hold
-- what is here: an `example_mark` row, and an `example_mark_withdrawal` row. A
-- mark IS the record of what a trader or an agent pointed at and, for a withdrawn
-- one, of who took it back and when — dropping either would erase the only account
-- of it, and the one-way withdraw would have nothing left to enforce. Refused
-- TRANSACTIONALLY and before anything moves, in the scratch-table + refuse-trigger
-- pattern 0010/0013/0019/0020 set, so the error names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the schema — a database holding no
-- mark and no withdrawal downgrades losslessly because nothing at 0020 references
-- either table. Both tables' triggers and the candle index go with their tables;
-- the withdrawal table drops first, since its `mark_id` REFERENCES the mark.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0020 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0021_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0021_down_guard_row BEFORE INSERT ON _0021_down_guard
BEGIN
  SELECT RAISE(ABORT, 'migration 0021 down: an example_mark or example_mark_withdrawal row exists and 0020 has no table for it; refusing rather than erasing an example mark or its withdrawal');
END;

INSERT INTO _0021_down_guard (reason)
SELECT 'example_mark' FROM example_mark LIMIT 1;

INSERT INTO _0021_down_guard (reason)
SELECT 'example_mark_withdrawal' FROM example_mark_withdrawal LIMIT 1;

DROP TRIGGER _0021_down_guard_row;
DROP TABLE _0021_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0021's tables, restoring 0020 exactly (triggers + index go with them).
-- ---------------------------------------------------------------------------
DROP TABLE example_mark_withdrawal;
DROP TABLE example_mark;
