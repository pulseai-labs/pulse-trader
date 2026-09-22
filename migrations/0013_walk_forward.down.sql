-- r2.s3.w3 — 0013 down: restore the exact 0012 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. Three states say 0012 cannot
-- hold what is here:
--   * a `walk_forward_run` row — the run kind has no table at 0012;
--   * a `walk_forward_fold` row — same;
--   * a `backtest_run` row carrying `walk_forward_run_id` / `fold_index` — the
--     membership has no column at 0012, and dropping the columns over it would
--     erase the only record of which walk-forward run the fold belonged to
--     (the same invented-fact argument as 0010's refused window-edge mark,
--     0011's refused stop price and 0012's refused lead-in start).
-- Refused TRANSACTIONALLY and before anything moves: the scratch table +
-- trigger pattern is 0010's/0011's/0012's, so the refusal names the state that
-- blocked it.
--
-- What IS discarded, knowingly: nothing but the schema — a database holding no
-- walk-forward rows downgrades losslessly because none of its `backtest_run`
-- rows have membership to lose. The guarding triggers drop BEFORE the schema
-- they guard (the 0012 precedent); the fold table drops before the membership
-- columns so the `walk_forward_fold_windowed` trigger's referenced table is
-- still there while it could matter, and `walk_forward_run` drops last.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0012 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0013_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0013_down_guard_run BEFORE INSERT ON _0013_down_guard
WHEN NEW.reason = 'walk_forward_run'
BEGIN
  SELECT RAISE(ABORT, 'migration 0013 down: a walk_forward_run row exists and 0012 has no table for it; refusing rather than falsifying the run record');
END;
CREATE TRIGGER _0013_down_guard_fold BEFORE INSERT ON _0013_down_guard
WHEN NEW.reason = 'walk_forward_fold'
BEGIN
  SELECT RAISE(ABORT, 'migration 0013 down: a walk_forward_fold row exists and 0012 has no table for it; refusing rather than falsifying the fold record');
END;
CREATE TRIGGER _0013_down_guard_membership BEFORE INSERT ON _0013_down_guard
WHEN NEW.reason = 'membership'
BEGIN
  SELECT RAISE(ABORT, 'migration 0013 down: a backtest_run carries walk-forward membership and 0012 has no columns for it; refusing rather than falsifying the run record');
END;

INSERT INTO _0013_down_guard (reason)
SELECT 'walk_forward_run' FROM walk_forward_run LIMIT 1;
INSERT INTO _0013_down_guard (reason)
SELECT 'walk_forward_fold' FROM walk_forward_fold LIMIT 1;
INSERT INTO _0013_down_guard (reason)
SELECT 'membership' FROM backtest_run
 WHERE walk_forward_run_id IS NOT NULL OR fold_index IS NOT NULL LIMIT 1;

DROP TRIGGER _0013_down_guard_run;
DROP TRIGGER _0013_down_guard_fold;
DROP TRIGGER _0013_down_guard_membership;
DROP TABLE _0013_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop the schema in dependency order, triggers before what they guard.
-- ---------------------------------------------------------------------------
DROP TRIGGER walk_forward_fold_windowed;
DROP TRIGGER walk_forward_fold_no_update;
DROP TRIGGER walk_forward_fold_no_delete;
DROP TABLE walk_forward_fold;

DROP TRIGGER backtest_run_walk_forward_pair;
DROP INDEX idx_backtest_run_walk_forward;
ALTER TABLE backtest_run DROP COLUMN fold_index;
ALTER TABLE backtest_run DROP COLUMN walk_forward_run_id;

DROP TRIGGER walk_forward_run_no_update;
DROP TRIGGER walk_forward_run_no_delete;
DROP TABLE walk_forward_run;
