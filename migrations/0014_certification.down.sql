-- r2.s3.w4 — 0014 down: certification has no 0013 representation.
--
-- TRUTHFUL OR NOT AT ALL (the 0010/0011/0012/0013 guard pattern). A version
-- whose `latest_walk_forward_run_id` is set is certified-or-not by that pointer
-- — dropping the column would falsify the record, so the down refuses while any
-- pointer is set. Likewise a proposal whose latest accept failed at
-- `walk_forward` names a stage the 0008 vocabulary cannot store; it refuses
-- rather than demote the stage to a false one. Only when BOTH are absent does
-- the 0013 shape still say everything true, and only then does this file run.
--
-- Otherwise it restores the exact pre-0014 shape: 0001's blanket immutability
-- trigger, no certification triggers, no pointer column, and
-- `coaching_proposals` rebuilt back to the 0008 `accept_failure_stage` CHECK —
-- the same create-new / copy / drop / rename ordering the up migration used,
-- with `coaching_sessions_lifecycle` dropped before the swap for the same
-- reason (RENAME reparses every trigger, and its `coaching_proposals` read must
-- not dangle).

-- ---------------------------------------------------------------------------
-- 0. Refusal guards — the states 0013 cannot truthfully hold.
-- ---------------------------------------------------------------------------
CREATE TABLE _0014_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0014_down_refuse BEFORE INSERT ON _0014_down_guard
BEGIN
  SELECT RAISE(
    ABORT,
    'migration 0014 down: certification state exists that 0013 cannot represent (a set latest_walk_forward_run_id, or a proposal whose accept failed at walk_forward)'
  );
END;

INSERT INTO _0014_down_guard (reason)
SELECT 'certification pointer set' FROM strategy_version
 WHERE latest_walk_forward_run_id IS NOT NULL LIMIT 1;

INSERT INTO _0014_down_guard (reason)
SELECT 'walk_forward accept failure recorded' FROM coaching_proposals
 WHERE accept_failure_stage = 'walk_forward' LIMIT 1;

DROP TRIGGER _0014_down_refuse;
DROP TABLE _0014_down_guard;

-- ---------------------------------------------------------------------------
-- 1. The 0001 immutability pair, restored exactly.
-- ---------------------------------------------------------------------------
DROP TRIGGER strategy_version_certification_owner;
DROP TRIGGER strategy_version_no_update;

CREATE TRIGGER strategy_version_no_update BEFORE UPDATE ON strategy_version
  BEGIN SELECT RAISE(ABORT, 'strategy_version is immutable'); END;

-- ---------------------------------------------------------------------------
-- 2. The pointer column.
-- ---------------------------------------------------------------------------
ALTER TABLE strategy_version DROP COLUMN latest_walk_forward_run_id;

-- ---------------------------------------------------------------------------
-- 3. `coaching_proposals`, rebuilt to the 0008 CHECK vocabulary.
-- ---------------------------------------------------------------------------
CREATE TABLE coaching_proposals_0014 (
  id                    TEXT PRIMARY KEY NOT NULL,
  session_id            TEXT NOT NULL UNIQUE REFERENCES coaching_sessions(id),
  mutation              TEXT NOT NULL,
  hypothesis            TEXT NOT NULL,
  disposition           TEXT NOT NULL,
  child_version_id      TEXT REFERENCES strategy_version(id),
  accepted_run_id       TEXT REFERENCES backtest_run(id),
  accept_failure_stage  TEXT,
  accept_failure_detail TEXT,

  CHECK (disposition IN ('proposed', 'accepted', 'rejected', 'modified')),

  CHECK (
    length(trim(hypothesis, char(9, 10, 11, 12, 13, 32, 133, 160, 5760,
                                 8192, 8193, 8194, 8195, 8196, 8197, 8198,
                                 8199, 8200, 8201, 8202, 8232, 8233, 8239,
                                 8287, 12288))) > 0
  ),

  CHECK ((disposition = 'accepted') = (child_version_id IS NOT NULL)),
  CHECK ((disposition = 'accepted') = (accepted_run_id IS NOT NULL)),

  CHECK (accept_failure_stage IS NULL OR accept_failure_stage IN (
    'apply', 'load_inputs', 'load_snapshots', 'compile', 'backtest', 'persist'
  )),
  CHECK ((accept_failure_stage IS NULL) = (accept_failure_detail IS NULL)),
  CHECK (accept_failure_stage IS NULL OR disposition IN ('proposed', 'modified'))
);

INSERT INTO coaching_proposals_0014
  (id, session_id, mutation, hypothesis, disposition, child_version_id,
   accepted_run_id, accept_failure_stage, accept_failure_detail)
SELECT
   id, session_id, mutation, hypothesis, disposition, child_version_id,
   accepted_run_id, accept_failure_stage, accept_failure_detail
FROM coaching_proposals;

-- See the up migration: this cross-table trigger must not dangle across the
-- RENAME's whole-schema reparse.
DROP TRIGGER coaching_sessions_lifecycle;

DROP TABLE coaching_proposals;

ALTER TABLE coaching_proposals_0014 RENAME TO coaching_proposals;

-- `coaching_sessions_lifecycle`, verbatim from 0008.
CREATE TRIGGER coaching_sessions_lifecycle BEFORE UPDATE ON coaching_sessions
BEGIN
  SELECT CASE WHEN NEW.id <> OLD.id
              OR NEW.backtest_run_id <> OLD.backtest_run_id
              OR NEW.strategy_version_id <> OLD.strategy_version_id
              OR NEW.created_at <> OLD.created_at
    THEN RAISE(ABORT, 'coaching_sessions: a recorded session''s identity is immutable')
  END;
  SELECT CASE WHEN NEW.request_fingerprint IS NOT OLD.request_fingerprint
    THEN RAISE(ABORT, 'coaching_sessions: a session''s request fingerprint is immutable')
  END;
  SELECT CASE WHEN OLD.outcome <> 'pending' AND NEW.outcome <> OLD.outcome
    THEN RAISE(ABORT, 'coaching_sessions: a settled outcome is terminal')
  END;
  SELECT CASE WHEN OLD.outcome = 'pending' AND NEW.outcome NOT IN ('proposed', 'failed')
    THEN RAISE(ABORT, 'coaching_sessions: a claim settles once, to proposed or failed')
  END;
  -- A turn that produced a proposal did not fail. Without this the disposition
  -- rail could be handed a proposal whose own session says the turn never happened.
  SELECT CASE WHEN NEW.outcome = 'failed'
                AND EXISTS (SELECT 1 FROM coaching_proposals WHERE session_id = OLD.id)
    THEN RAISE(ABORT, 'coaching_sessions: a session carrying a proposal cannot be recorded as failed')
  END;
END;

-- The four 0008 proposal triggers, verbatim.
CREATE TRIGGER coaching_proposals_session_must_be_proposed BEFORE INSERT ON coaching_proposals
BEGIN
  SELECT CASE
    WHEN (SELECT outcome FROM coaching_sessions WHERE id = NEW.session_id) IS NOT 'proposed'
    THEN RAISE(ABORT, 'coaching_proposals: a proposal may be attached only to a proposed session')
  END;
END;

CREATE TRIGGER coaching_proposals_accept_lineage_insert BEFORE INSERT ON coaching_proposals
WHEN NEW.disposition = 'accepted'
BEGIN
  SELECT CASE
    WHEN (SELECT strategy_version_id FROM backtest_run WHERE id = NEW.accepted_run_id)
         IS NOT NEW.child_version_id
    THEN RAISE(ABORT, 'coaching_proposals: the accepted run is not a run of the accepted child version')
  END;
  SELECT CASE
    WHEN (SELECT parent_version_id FROM strategy_version WHERE id = NEW.child_version_id)
         IS NOT (SELECT strategy_version_id FROM coaching_sessions WHERE id = NEW.session_id)
    THEN RAISE(ABORT, 'coaching_proposals: the accepted child is not a child of the coached version')
  END;
  SELECT CASE
    WHEN (SELECT strategy_id FROM strategy_version WHERE id = NEW.child_version_id)
         IS NOT (SELECT p.strategy_id FROM strategy_version p
                 JOIN coaching_sessions s ON s.strategy_version_id = p.id
                 WHERE s.id = NEW.session_id)
    THEN RAISE(ABORT, 'coaching_proposals: the accepted child belongs to another strategy')
  END;
END;

CREATE TRIGGER coaching_proposals_accept_lineage_update BEFORE UPDATE ON coaching_proposals
WHEN NEW.disposition = 'accepted'
BEGIN
  SELECT CASE
    WHEN (SELECT strategy_version_id FROM backtest_run WHERE id = NEW.accepted_run_id)
         IS NOT NEW.child_version_id
    THEN RAISE(ABORT, 'coaching_proposals: the accepted run is not a run of the accepted child version')
  END;
  SELECT CASE
    WHEN (SELECT parent_version_id FROM strategy_version WHERE id = NEW.child_version_id)
         IS NOT (SELECT strategy_version_id FROM coaching_sessions WHERE id = NEW.session_id)
    THEN RAISE(ABORT, 'coaching_proposals: the accepted child is not a child of the coached version')
  END;
  SELECT CASE
    WHEN (SELECT strategy_id FROM strategy_version WHERE id = NEW.child_version_id)
         IS NOT (SELECT p.strategy_id FROM strategy_version p
                 JOIN coaching_sessions s ON s.strategy_version_id = p.id
                 WHERE s.id = NEW.session_id)
    THEN RAISE(ABORT, 'coaching_proposals: the accepted child belongs to another strategy')
  END;
END;

CREATE TRIGGER coaching_proposals_transition BEFORE UPDATE ON coaching_proposals
BEGIN
  SELECT CASE WHEN NEW.session_id <> OLD.session_id
    THEN RAISE(ABORT, 'coaching_proposals: a proposal cannot change session')
  END;
  SELECT CASE
    WHEN OLD.disposition IN ('accepted', 'rejected')
     AND NOT (NEW.disposition = OLD.disposition
              AND NEW.child_version_id IS OLD.child_version_id
              AND NEW.accepted_run_id IS OLD.accepted_run_id)
    THEN RAISE(ABORT, 'coaching_proposals: accepted and rejected are terminal')
  END;
  SELECT CASE
    WHEN OLD.disposition IN ('proposed', 'modified')
     AND NEW.disposition <> OLD.disposition
     AND NEW.disposition NOT IN ('modified', 'rejected', 'accepted')
    THEN RAISE(ABORT, 'coaching_proposals: nothing returns to `proposed`')
  END;
END;

-- Same in-transaction proof the up migration makes: nothing left dangling.
CREATE TABLE _0014_fk_check (reason TEXT NOT NULL);

CREATE TRIGGER _0014_fk_refuse BEFORE INSERT ON _0014_fk_check
BEGIN
  SELECT RAISE(
    ABORT,
    'migration 0014 down: PRAGMA foreign_key_check is not clean after the coaching_proposals rebuild'
  );
END;

INSERT INTO _0014_fk_check (reason)
SELECT 'dangling foreign key' FROM pragma_foreign_key_check LIMIT 1;

DROP TRIGGER _0014_fk_refuse;
DROP TABLE _0014_fk_check;
