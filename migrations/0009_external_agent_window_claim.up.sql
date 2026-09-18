-- r2.s1.w1 — 0009: the external-agent provenance contract, the run's date
-- window, and #158's one-pending-claim-per-run guard (ADR-0010 / ADR-0015 /
-- ADR-0018 / ADR-0019 / ADR-0021).
--
-- Three shapes `0008` cannot say, landed in one migration because they are one
-- contract — the spine's every later item reads them together:
--
--   1. `agent_submission` — the audit row an `external_agent` version carries
--      where a coach version carries a coaching session and an LlmCall. No
--      LlmCall is written: the call's cost is external to the app, so there is
--      nothing truthful to put in the ledger's token/cost columns. Normalized
--      from day one — ADR-0010 §3's regret about `creating_llm_call_ids`'s
--      denormalized JSON is not repeated; the submission's version link is a
--      UNIQUE FK, its kind is enforced by trigger, and the row is immutable
--      the way `strategy_version` itself is.
--   2. `window_from_ms` / `window_to_ms` on `backtest_run` — the candle
--      open_time bounds the run consumed, UTC epoch milliseconds, half-open
--      `[from, to)`. NULL/NULL is the whole snapshot: every pre-0009 row and
--      every unwindowed new run, with no backfill guess (0006's argument —
--      the bound a legacy run used is not recoverable from anything stored).
--   3. `coaching_sessions_one_pending_per_run` — a PARTIAL unique index making
--      #158's single-flight claim a property of the system of record. Two
--      processes could both pass the process-local registry and both bill a
--      provider call for the same run; the index refuses the second INSERT at
--      the only layer both writers share. Partial on `outcome = 'pending'`
--      deliberately: the guarantee guards the live claim, not the history —
--      a settled session must not block the next turn on that run.
--
-- CONVENTIONS ARE 0001's/0008's, deliberately: `TEXT` keys, RFC3339 UTC
-- `created_at`, and a BEFORE-INSERT guard trigger for the window pair for the
-- same reason `backtest_run_inputs_complete` is one — a table CHECK would
-- evaluate over the all-NULL legacy rows this migration must leave untouched.
--
-- THE INDEX IS THE MIGRATION'S ONE REFUSAL POINT. `0008` permits two pending
-- claims on one run; a db that actually holds one is a db whose claim guard
-- already failed. Building the index over it aborts the migration — the db
-- stays at 0008, both claims intact — rather than picking a winner.
--
-- ---------------------------------------------------------------------------
-- 1. `agent_submission`: the external agent's audit ledger.
-- ---------------------------------------------------------------------------
CREATE TABLE agent_submission (
  id          TEXT PRIMARY KEY,
  version_id  TEXT NOT NULL UNIQUE REFERENCES strategy_version(id),
  agent_name  TEXT NOT NULL CHECK (length(agent_name) BETWEEN 1 AND 64),
  hypothesis  TEXT NOT NULL CHECK (length(hypothesis) BETWEEN 1 AND 2000),
  created_at  TEXT NOT NULL
);
CREATE TRIGGER agent_submission_no_update BEFORE UPDATE ON agent_submission
  BEGIN SELECT RAISE(ABORT, 'agent_submission is immutable'); END;
CREATE TRIGGER agent_submission_no_delete BEFORE DELETE ON agent_submission
  BEGIN SELECT RAISE(ABORT, 'agent_submission is immutable'); END;
-- An agent_submission row may only point at an external_agent version. The
-- comparison is against the serde-JSON literal the adapter writes — the value
-- INCLUDES its double quotes.
CREATE TRIGGER agent_submission_version_kind BEFORE INSERT ON agent_submission
  BEGIN SELECT RAISE(ABORT, 'agent_submission requires an external_agent version')
    WHERE (SELECT created_by FROM strategy_version WHERE id = NEW.version_id) <> '"external_agent"';
  END;

-- ---------------------------------------------------------------------------
-- 2. The date window a run consumed: candle open_time bounds, UTC epoch ms,
--    [from, to). NULL/NULL = the whole snapshot.
-- ---------------------------------------------------------------------------
ALTER TABLE backtest_run ADD COLUMN window_from_ms INTEGER;
ALTER TABLE backtest_run ADD COLUMN window_to_ms   INTEGER;
CREATE TRIGGER backtest_run_window_pair BEFORE INSERT ON backtest_run
  BEGIN SELECT RAISE(ABORT, 'window bounds are both-or-neither and from < to')
    WHERE (NEW.window_from_ms IS NULL) <> (NEW.window_to_ms IS NULL)
       OR (NEW.window_from_ms IS NOT NULL AND NEW.window_from_ms >= NEW.window_to_ms);
  END;

-- ---------------------------------------------------------------------------
-- 3. #158: one pending coach claim per run, enforced by the system of record.
-- ---------------------------------------------------------------------------
CREATE UNIQUE INDEX coaching_sessions_one_pending_per_run
  ON coaching_sessions(backtest_run_id) WHERE outcome = 'pending';
