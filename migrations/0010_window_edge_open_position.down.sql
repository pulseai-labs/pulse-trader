-- r2.s1 G1(b) — 0010 down: restore the exact 0009 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. One state says 0009 cannot
-- hold what is here: a `backtest_run` row carrying `open_position` — the
-- window-edge mark has no representation at 0009, and dropping the column
-- over it would erase the only record that a run ended holding a position
-- (the same invented-fact argument that refused a windowed run's 0008
-- downgrade). Refused TRANSACTIONALLY and before anything moves: the scratch
-- table + trigger pattern is 0009's, so the refusal names the state that
-- blocked it.
--
-- What IS discarded, knowingly: nothing but the column itself — every NULL
-- row (flat-at-edge, unwindowed, and all pre-0010 history) downgrades
-- losslessly because none of them have a mark to lose. The column drops by
-- `ALTER TABLE ... DROP COLUMN` (SQLite ≥ 3.35); no table rebuild is needed
-- or attempted.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0009 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0010_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0010_down_guard_mark BEFORE INSERT ON _0010_down_guard
WHEN NEW.reason = 'open_position'
BEGIN
  SELECT RAISE(ABORT, 'migration 0010 down: a backtest_run carries a window-edge open-position mark and 0009 has no column for it; refusing rather than falsifying the run record');
END;

INSERT INTO _0010_down_guard (reason)
SELECT 'open_position' FROM backtest_run WHERE open_position IS NOT NULL LIMIT 1;

DROP TRIGGER _0010_down_guard_mark;
DROP TABLE _0010_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop 0010's column, restoring 0009 exactly.
-- ---------------------------------------------------------------------------
ALTER TABLE backtest_run DROP COLUMN open_position;
