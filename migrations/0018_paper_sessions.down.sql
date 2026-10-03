-- r3.s4.w2 — 0018 down: restore the exact 0017 shape, or refuse.
--
-- ADR-0018 asks a down migration to be TRUTHFUL. Four tables say 0017 cannot
-- hold what is here: a `paper_session` row (the promotion decision), a
-- `paper_event` row (the session's log — dropping it would erase everything
-- that happened after promotion), a `paper_bar` row (the recorded candles),
-- a `fixture_snapshot` row (the fixture stamp). Any of them present refuses
-- the downgrade TRANSACTIONALLY and before anything moves, in the
-- scratch-table-and-refuse-trigger pattern 0013's down uses, so the error
-- names the state that blocked it.
--
-- What IS discarded, knowingly: nothing but the schema — a database holding
-- no paper rows downgrades losslessly because nothing at 0017 references
-- them. The guarding triggers drop BEFORE the schema they guard; the child
-- tables (`paper_event`, `paper_bar`) drop before their parent
-- (`paper_session`), and the indexes go with their tables.

-- ---------------------------------------------------------------------------
-- 0. Refuse anything 0017 cannot say.
-- ---------------------------------------------------------------------------
CREATE TABLE _0018_down_guard (reason TEXT NOT NULL);

CREATE TRIGGER _0018_down_guard_session BEFORE INSERT ON _0018_down_guard
WHEN NEW.reason = 'paper_session'
BEGIN
  SELECT RAISE(ABORT, 'migration 0018 down: a paper_session row exists and 0017 has no table for it; refusing rather than falsifying the promotion record');
END;
CREATE TRIGGER _0018_down_guard_event BEFORE INSERT ON _0018_down_guard
WHEN NEW.reason = 'paper_event'
BEGIN
  SELECT RAISE(ABORT, 'migration 0018 down: a paper_event row exists and 0017 has no table for it; refusing rather than falsifying the session log');
END;
CREATE TRIGGER _0018_down_guard_bar BEFORE INSERT ON _0018_down_guard
WHEN NEW.reason = 'paper_bar'
BEGIN
  SELECT RAISE(ABORT, 'migration 0018 down: a paper_bar row exists and 0017 has no table for it; refusing rather than falsifying the recorded candles');
END;
CREATE TRIGGER _0018_down_guard_fixture BEFORE INSERT ON _0018_down_guard
WHEN NEW.reason = 'fixture_snapshot'
BEGIN
  SELECT RAISE(ABORT, 'migration 0018 down: a fixture_snapshot row exists and 0017 has no table for it; refusing rather than falsifying the fixture stamp');
END;

INSERT INTO _0018_down_guard (reason) SELECT 'paper_session' FROM paper_session LIMIT 1;
INSERT INTO _0018_down_guard (reason) SELECT 'paper_event' FROM paper_event LIMIT 1;
INSERT INTO _0018_down_guard (reason) SELECT 'paper_bar' FROM paper_bar LIMIT 1;
INSERT INTO _0018_down_guard (reason) SELECT 'fixture_snapshot' FROM fixture_snapshot LIMIT 1;

DROP TRIGGER _0018_down_guard_session;
DROP TRIGGER _0018_down_guard_event;
DROP TRIGGER _0018_down_guard_bar;
DROP TRIGGER _0018_down_guard_fixture;
DROP TABLE _0018_down_guard;

-- ---------------------------------------------------------------------------
-- 1. Drop the schema in dependency order: children before their parent,
--    guarding triggers before what they guard.
-- ---------------------------------------------------------------------------
DROP TRIGGER paper_event_no_insert_after_stop;
DROP TRIGGER paper_event_no_update;
DROP TRIGGER paper_event_no_delete;
DROP INDEX idx_paper_event_session;
DROP TABLE paper_event;

DROP TRIGGER paper_bar_no_update;
DROP TRIGGER paper_bar_no_delete;
DROP INDEX idx_paper_bar_session;
DROP TABLE paper_bar;

DROP TRIGGER fixture_snapshot_no_update;
DROP TRIGGER fixture_snapshot_no_delete;
DROP TABLE fixture_snapshot;

DROP TRIGGER paper_session_no_update;
DROP TRIGGER paper_session_no_delete;
DROP TABLE paper_session;
