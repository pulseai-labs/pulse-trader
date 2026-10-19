-- r5.s5.w1 — 0022 down: restore the exact 0021 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One table says 0021 cannot hold what is here: a
-- `power_review` row. That row IS the record of one count-only holdout read — who asked, on which version,
-- under which freeze, at what count and pass chance — and it is the account of what the operator was told
-- before an attempt was allowed or refused. Dropping it would erase the only record of that read.
-- Refused TRANSACTIONALLY and before anything moves, in the scratch-table + refuse-trigger pattern
-- 0010/0013/0019/0020/0021 set, so the error names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the schema — a database holding no power-review row downgrades
-- losslessly because nothing at 0021 references the table. Its two triggers and its index go with the
-- table.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0021 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0022_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0022_down_guard_power_review BEFORE INSERT ON _0022_down_guard
WHEN NEW.reason = 'power_review'
BEGIN
  SELECT RAISE(ABORT, 'migration 0022 down: a power_review row exists and 0021 has no table for it; refusing rather than erasing the record of a count-only holdout read');
END;

INSERT INTO _0022_down_guard (reason)
SELECT 'power_review' FROM power_review LIMIT 1;

DROP TRIGGER _0022_down_guard_power_review;
DROP TABLE _0022_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0022's table, restoring 0021 exactly (triggers + index go with it).
-- ---------------------------------------------------------------------------
DROP TABLE power_review;
