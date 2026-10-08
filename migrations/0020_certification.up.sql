-- r4.s1.w5 — 0020: the certification record (G7, C1, C4, C5; ADR-0010/0018/0019).
--
-- ONE immutable row per certification call. The certification step
-- (`certify_version`, agent scope) walks a version forward under `wf-v2` on
-- the search span, runs ONE holdout backtest on `[holdout_start, snapshot end)`
-- and applies the C1 holdout test at the OPEN freeze's H (ADR-0028). Whatever
-- the outcome, exactly one row lands here — one hypothesis is one call (Q2) —
-- and a refused or errored call writes NOTHING (so the budget it counts cannot
-- drift from the calls that actually ran).
--
-- THE BUDGET IS ENFORCED IN THE WRITE TRANSACTION, AND THE SCHEMA HOLDS THE
-- SAME LAW AS A BACKSTOP. The adapter mints the next index AND reads the
-- freeze's `h` inside the same `BEGIN IMMEDIATE` transaction that inserts the
-- row, refusing the write when the index would exceed the budget — a
-- concurrent second call waits for the write lock, sees its predecessor's row
-- and refuses rather than overrunning. For a raw INSERT:
-- `certification_freeze_index` (UNIQUE) refuses a duplicate position, and
-- `certification_hypothesis_budget` refuses any index above the freeze's
-- recorded `h`.
--
-- ONE LAW THE SCHEMA HOLDS ITSELF: `certified` is `search_pass AND
-- holdout_passes`, both halves recorded — a stored `certified` that disagrees
-- with its halves is unrepresentable.
--
-- THE IMMUTABILITY IS THE `0010` PATTERN, BLANKET THIS TIME: no UPDATE, no
-- DELETE. A certification is the audit trail's atom — the record of what a
-- hypothesis scored on data the campaign may never look at again — so there is
-- no legal mutation of it at all, not even a close.
--
-- TIMESTAMPS ARE MIXED BY COLUMN, deliberately: `holdout_start_ms` /
-- `holdout_end_ms` are INTEGER epoch ms (they are candle bounds, compared
-- against `open_time`/`close_time` with no parsing, exactly as
-- `certification_freeze`'s are), while `created_at` is the RFC3339 UTC text
-- every other record table uses. `holdout_mean_r` is Decimal-as-TEXT and the
-- three f64s stay REAL — no Decimal column degrades to a float (NFR-2).

-- ---------------------------------------------------------------------------
-- 1. `certification`: one immutable row per hypothesis.
-- ---------------------------------------------------------------------------
CREATE TABLE certification (
  id                         TEXT PRIMARY KEY NOT NULL,
  version_id                 TEXT NOT NULL REFERENCES strategy_version(id),
  freeze_id                  TEXT NOT NULL REFERENCES certification_freeze(id),
  hypothesis_index           INTEGER NOT NULL CHECK (hypothesis_index >= 1),  -- 1..=H under this freeze
  rule                       TEXT NOT NULL CHECK (length(trim(rule)) > 0),   -- the search rule's persisted name ('wf-v2')
  pair                       TEXT NOT NULL CHECK (length(trim(pair)) > 0),
  search_walk_forward_run_id TEXT NOT NULL REFERENCES walk_forward_run(id),
  search_pass                INTEGER NOT NULL CHECK (search_pass IN (0, 1)),
  holdout_start_ms           INTEGER NOT NULL,                               -- inclusive
  holdout_end_ms             INTEGER NOT NULL,                               -- exclusive (the primary snapshot's last close)
  holdout_n                  INTEGER NOT NULL CHECK (holdout_n >= 0),        -- the holdout's trade count (C5)
  holdout_mean_r             TEXT NOT NULL,                                  -- Decimal-as-TEXT (NFR-2)
  holdout_z                  REAL NOT NULL,                                  -- z(1 - 0.05/H) at this freeze's H
  holdout_lower_bound        REAL NOT NULL,                                  -- mean - z * sqrt(var/n)
  holdout_passes             INTEGER NOT NULL CHECK (holdout_passes IN (0, 1)),
  certified                  INTEGER NOT NULL CHECK (certified IN (0, 1)),   -- = search_pass AND holdout_passes
  -- The data versions the two halves evaluated, per timeframe (C5): the
  -- nested `{primary, htf?, d1?}` selection shape as JSON, the `0006`
  -- `backtest_run.inputs` precedent for recording run provenance verbatim.
  search_inputs              TEXT NOT NULL CHECK (length(trim(search_inputs)) > 0),
  holdout_inputs             TEXT NOT NULL CHECK (length(trim(holdout_inputs)) > 0),
  engine_fingerprint         TEXT NOT NULL CHECK (length(trim(engine_fingerprint)) > 0),  -- the search run's (all folds share it)
  created_at                 TEXT NOT NULL,                                  -- injected Clock (RFC3339 UTC)
  called_by                  TEXT NOT NULL CHECK (length(trim(called_by)) > 0),           -- the calling token's label
  CHECK (holdout_end_ms > holdout_start_ms),
  CHECK (certified = (search_pass AND holdout_passes))
);

CREATE TRIGGER certification_no_update BEFORE UPDATE ON certification
  BEGIN SELECT RAISE(ABORT, 'certification records are immutable'); END;
CREATE TRIGGER certification_no_delete BEFORE DELETE ON certification
  BEGIN SELECT RAISE(ABORT, 'certification records are never deleted'); END;

-- The budget backstop (the adapter's write transaction is the mechanism): a raw
-- INSERT past the freeze's H aborts by name, exactly as the adapter refuses it.
CREATE TRIGGER certification_hypothesis_budget BEFORE INSERT ON certification
  WHEN NEW.hypothesis_index > (SELECT h FROM certification_freeze WHERE id = NEW.freeze_id)
  BEGIN
    SELECT RAISE(ABORT, 'the freeze''s hypothesis budget is spent: hypothesis_index would exceed h');
  END;

-- One hypothesis index per freeze: a second row at the same position is
-- impossible even under a raw INSERT (an index ABOVE the budget is refused by
-- `certification_hypothesis_budget` above, and by the adapter's write
-- transaction, which is the mechanism).
CREATE UNIQUE INDEX certification_freeze_index
  ON certification (freeze_id, hypothesis_index);

-- The read the promotion gate and the app's detail view both make: a version's
-- records, newest first.
CREATE INDEX idx_certification_version ON certification (version_id, hypothesis_index);
