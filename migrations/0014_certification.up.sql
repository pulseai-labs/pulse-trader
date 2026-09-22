-- r2.s3.w4 — 0014: certification as the `strategy_version` pointer (ADR-0025).
--
-- THE SEAM. A version is certified when its LATEST walk-forward run passed.
-- That is a pointer on the version row, not a flag: `certified` is derived on
-- read from the named run's `pass`, so a later FAILING run de-certifies the
-- version by simply being newer — no flag to forget to flip, and no stored
-- boolean that can disagree with the run log. New versions carry NULL and are
-- uncertified until walked forward; pre-0014 rows read `NULL`/`false` honestly.
--
-- THE IMMUTABILITY TRIGGER IS NARROWED, NOT WEAKENED. `strategy_version` stays
-- write-once for every 0001 column — the pointer is the one mutable cell, and
-- `strategy_version_certification_owner` below is what makes its mutability
-- honest: it may only name a walk-forward run OF THIS VERSION, and it may only
-- advance in `seq` order — the monotonic insertion sequence 0013 mints, which
-- is the true write order. A version cannot reach back to an earlier
-- run, cannot borrow another version's run, and (with the product never writing
-- NULL) cannot be quietly un-certified by clearing the cell.
--
-- `coaching_proposals` IS REBUILT for one word: `'walk_forward'` joins the
-- `accept_failure_stage` CHECK vocabulary — the accept path's gate is a new,
-- real stage a proposal must be able to record. SQLite cannot ALTER a CHECK,
-- so the ordering is create-new / copy / drop / rename: the new shape is built
-- under the archive name, every row is copied byte for byte, the old table is
-- dropped (its four triggers go with it, freeing their names), the archive is
-- renamed into place, and the four 0008 triggers are recreated verbatim.
--
-- The ordering is load-bearing, not cosmetic: `coaching_sessions_lifecycle`
-- (0008) reads `coaching_proposals` in a trigger on ANOTHER table, and
-- `ALTER TABLE ... RENAME` reparses EVERY stored trigger — a reparse that
-- fails while `coaching_proposals` is absent. So the trigger is dropped before
-- the table is, the swap runs with no dangling reference in the schema, and the
-- trigger is recreated verbatim once the new table carries the name again.
-- Nothing references `coaching_proposals` by foreign key, so the swap severs
-- no live edge — and the `foreign_key_check` assertion at the foot of this
-- file proves it inside the same transaction.
--
-- CONVENTIONS are the established ones: TEXT keys, RFC3339 UTC timestamps, and
-- no `f64` column anywhere (NFR-2).

-- ---------------------------------------------------------------------------
-- 1. The certification pointer.
-- ---------------------------------------------------------------------------
ALTER TABLE strategy_version
  ADD COLUMN latest_walk_forward_run_id TEXT NULL REFERENCES walk_forward_run(id);

-- ---------------------------------------------------------------------------
-- 2. Immutability, narrowed to admit the pointer and nothing else.
-- ---------------------------------------------------------------------------
-- Every 0001 column is named explicitly and compared with `IS NOT` (NULL-safe),
-- so an UPDATE touching anything but the pointer is still refused — including
-- an UPDATE that sets the pointer AND smuggles a second column's change.
DROP TRIGGER strategy_version_no_update;
CREATE TRIGGER strategy_version_no_update BEFORE UPDATE ON strategy_version
WHEN NEW.id IS NOT OLD.id
  OR NEW.strategy_id IS NOT OLD.strategy_id
  OR NEW.parent_version_id IS NOT OLD.parent_version_id
  OR NEW.dsl_schema_version IS NOT OLD.dsl_schema_version
  OR NEW.dsl IS NOT OLD.dsl
  OR NEW.dsl_original IS NOT OLD.dsl_original
  OR NEW.version_hash IS NOT OLD.version_hash
  OR NEW.created_by IS NOT OLD.created_by
  OR NEW.creating_llm_call_ids IS NOT OLD.creating_llm_call_ids
  OR NEW.created_at IS NOT OLD.created_at
BEGIN
  SELECT RAISE(ABORT, 'strategy_version is immutable except latest_walk_forward_run_id');
END;

-- The pointer's own law: this version's runs only, and strictly newer than the
-- run it replaces by `seq` ALONE. `seq` is the monotonic insertion sequence
-- 0013 mints (`MAX(seq)+1` inside the write transaction), so it IS the order in
-- which the runs were written — and it is the only ordering that is:
-- `created_at` comes from the wall clock, which is not monotonic, and it is
-- minted BEFORE the transaction takes its write lock, so a clock correction (or
-- two saves ordered by lock acquisition) can hand a later insertion an earlier
-- instant. Ordering by `(created_at, seq)` let that earlier instant outrank the
-- higher sequence and refused an otherwise valid save with 'only advances'.
-- A NULL NEW is allowed only because
-- 0014 introduces the column NULL on every pre-existing row; the product never
-- writes NULL over a set pointer — and this trigger refuses it as a backward
-- move anyway, since certification is revoked by a NEWER failing run, not by
-- erasing the record.
CREATE TRIGGER strategy_version_certification_owner
  BEFORE UPDATE OF latest_walk_forward_run_id ON strategy_version
WHEN NEW.latest_walk_forward_run_id IS NOT OLD.latest_walk_forward_run_id
BEGIN
  -- The named run must belong to THIS version — the certification a version
  -- carries is its own run's verdict, never another version's.
  SELECT CASE
    WHEN NEW.latest_walk_forward_run_id IS NOT NULL
     AND (SELECT w.strategy_version_id FROM walk_forward_run w
          WHERE w.id = NEW.latest_walk_forward_run_id) IS NOT NEW.id
    THEN RAISE(ABORT, 'strategy_version: latest_walk_forward_run_id must name a walk-forward run of this version')
  END;
  -- Once set, the pointer only advances: clearing it, or pointing it at a run
  -- whose `seq` is not strictly higher, is refused.
  SELECT CASE
    WHEN OLD.latest_walk_forward_run_id IS NOT NULL
     AND (NEW.latest_walk_forward_run_id IS NULL
          OR NOT EXISTS (
            SELECT 1
              FROM walk_forward_run n
              JOIN walk_forward_run o ON o.id = OLD.latest_walk_forward_run_id
             WHERE n.id = NEW.latest_walk_forward_run_id
               AND n.seq > o.seq))
    THEN RAISE(ABORT, 'strategy_version: latest_walk_forward_run_id only advances')
  END;
END;

-- ---------------------------------------------------------------------------
-- 3. `coaching_proposals`, rebuilt for the `walk_forward` accept-failure stage.
-- ---------------------------------------------------------------------------
-- The new shape goes up under the archive name FIRST: 0008's table verbatim,
-- one vocabulary word added. The UNIQUE on `session_id` is part of the shape —
-- its auto-index comes with the table.
CREATE TABLE coaching_proposals_0014 (
  id                    TEXT PRIMARY KEY NOT NULL,
  session_id            TEXT NOT NULL UNIQUE REFERENCES coaching_sessions(id),
  mutation              TEXT NOT NULL,
  hypothesis            TEXT NOT NULL,
  disposition           TEXT NOT NULL,
  child_version_id      TEXT REFERENCES strategy_version(id),
  -- The re-backtest OF the accepted child. Together with `child_version_id` this
  -- is the release's "no accepted proposal lacks its child and no child lacks its
  -- run" written as schema instead of as intent.
  accepted_run_id       TEXT REFERENCES backtest_run(id),
  -- The LATEST accept outcome, on the existing mutable proposal projection — not a
  -- new append-only decision-attempt entity. A later valid modify clears it; a
  -- successful accept clears it inside the same transaction that writes the child.
  accept_failure_stage  TEXT,
  accept_failure_detail TEXT,

  CHECK (disposition IN ('proposed', 'accepted', 'rejected', 'modified')),

  -- 0008's hypothesis rule, carried across verbatim (see 0005/0008 for the
  -- char-set derivation and the `migration_0005` test that keeps it in parity
  -- with Rust).
  CHECK (
    length(trim(hypothesis, char(9, 10, 11, 12, 13, 32, 133, 160, 5760,
                                 8192, 8193, 8194, 8195, 8196, 8197, 8198,
                                 8199, 8200, 8201, 8202, 8232, 8233, 8239,
                                 8287, 12288))) > 0
  ),

  -- Both links exist exactly when the proposal is accepted, and nothing else may
  -- name either. This is 0005's child rule PLUS the run half it could not state.
  CHECK ((disposition = 'accepted') = (child_version_id IS NOT NULL)),
  CHECK ((disposition = 'accepted') = (accepted_run_id IS NOT NULL)),

  -- The accept progression w2/w3 report, enumerated — PLUS `walk_forward`, the
  -- certification gate a certified parent's accept must survive (r2.s3.w4).
  -- There is deliberately NO `read_back` stage: once the child and the run are
  -- committed the accept SUCCEEDED, and a read-back failure is a
  -- saved-but-unreadable accepted outcome carrying both ids (the r1.s3
  -- precedent) — a shape the accepted row below forbids from carrying failure
  -- fields at all.
  CHECK (accept_failure_stage IS NULL OR accept_failure_stage IN (
    'apply', 'load_inputs', 'load_snapshots', 'compile', 'backtest',
    'walk_forward', 'persist'
  )),
  -- Half a failure is silence wearing a record's clothes.
  CHECK ((accept_failure_stage IS NULL) = (accept_failure_detail IS NULL)),
  -- A failed accept leaves the proposal OPEN. An accepted row therefore has no
  -- accept-failure fields, and a rejected row has no child, no run and no failure.
  CHECK (accept_failure_stage IS NULL OR disposition IN ('proposed', 'modified'))
);

INSERT INTO coaching_proposals_0014
  (id, session_id, mutation, hypothesis, disposition, child_version_id,
   accepted_run_id, accept_failure_stage, accept_failure_detail)
SELECT
   id, session_id, mutation, hypothesis, disposition, child_version_id,
   accepted_run_id, accept_failure_stage, accept_failure_detail
FROM coaching_proposals;

-- `coaching_sessions_lifecycle` (0008, on `coaching_sessions`) reads
-- `coaching_proposals` in its body. It survives the table drop — a trigger's
-- home is its own table — and RENAME's whole-schema reparse would then fail on
-- the dangling reference. It goes first and returns verbatim below.
DROP TRIGGER coaching_sessions_lifecycle;

-- The old table goes — its four triggers go with it, freeing their names for
-- the verbatim recreation below. Nothing references `coaching_proposals` by
-- foreign key, so the drop refuses nothing.
DROP TABLE coaching_proposals;

ALTER TABLE coaching_proposals_0014 RENAME TO coaching_proposals;

-- The session trigger, recreated verbatim from 0008 now that the name it reads
-- resolves again.
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

-- ---------------------------------------------------------------------------
-- 4. The four 0008 proposal triggers, recreated verbatim (the session trigger
--    above is the fifth piece of 0008's trigger set this rebuild had to move).
-- ---------------------------------------------------------------------------

-- A proposal belongs to a turn that PRODUCED one. Attaching it to a pending claim
-- would assert an outcome the session does not have; attaching it to a failed turn
-- would contradict the one the session does have.
CREATE TRIGGER coaching_proposals_session_must_be_proposed BEFORE INSERT ON coaching_proposals
BEGIN
  SELECT CASE
    WHEN (SELECT outcome FROM coaching_sessions WHERE id = NEW.session_id) IS NOT 'proposed'
    THEN RAISE(ABORT, 'coaching_proposals: a proposal may be attached only to a proposed session')
  END;
END;

-- The accepted lineage, on insert and on update.
--
-- The FKs can say "some version exists" and "some run exists"; they cannot say
-- that THIS run is the re-backtest of THIS child, or that THIS child descends from
-- the version the session coached. `r1.s4` reads that lineage AS the version tree,
-- so a false edge is not recoverable from the row afterwards.
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

-- The disposition transition matrix, mirroring the domain's `Proposal::transition`:
--
--     proposed -> modified | rejected | accepted
--     modified -> modified | rejected | accepted
--     accepted, rejected    (terminal)
--
-- Column-presence checks alone would admit a BACKWARD transition — an accepted row
-- rewritten to `modified` with both links cleared satisfies every CHECK above and
-- is exactly the un-settling the session-id accept key exists to prevent. Rewriting
-- a terminal row with the IDENTICAL disposition and links is the idempotent no-op a
-- retrying client lands on; anything else is refused. An update that leaves the
-- disposition alone is not a transition at all — that is how a failed accept
-- records itself on a still-open proposal.
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

-- ---------------------------------------------------------------------------
-- 5. Proof, inside this transaction: no dangling foreign key may survive the
-- rebuild. `foreign_key_check` cannot abort a migration on its own, so its rows
-- feed the same scratch-table-and-refuse-trigger pattern 0008's preflight used.
-- ---------------------------------------------------------------------------
CREATE TABLE _0014_fk_check (reason TEXT NOT NULL);

CREATE TRIGGER _0014_fk_refuse BEFORE INSERT ON _0014_fk_check
BEGIN
  SELECT RAISE(
    ABORT,
    'migration 0014: PRAGMA foreign_key_check is not clean after the coaching_proposals rebuild'
  );
END;

INSERT INTO _0014_fk_check (reason)
SELECT 'dangling foreign key' FROM pragma_foreign_key_check LIMIT 1;

DROP TRIGGER _0014_fk_refuse;
DROP TABLE _0014_fk_check;
