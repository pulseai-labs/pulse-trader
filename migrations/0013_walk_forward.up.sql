-- r2.s3.w3 — 0013: walk-forward as a run kind (ADR-0025).
--
-- A walk-forward run is ONE parent row (`walk_forward_run`) naming the scheme
-- (`rolling-oos/v1`), the verdict rule (`wf-v1`), the counted span
-- `[span_from_ms, span_to_ms)`, whether `from` was defaulted to the first
-- fully-warm bar, the engine fingerprint the folds share, and the recorded
-- `wf-v1` run verdict (folds_holding / folds_required / pooled_* / pass). Each
-- of its K folds is a `walk_forward_fold` row pointing at the ordinary
-- windowed `backtest_run` it ran as — the fold run is a REAL run: it carries
-- its own window pair (0009), its own recorded lead-in start (0012), its own
-- trades, summary and content hash, and it answers `get_run` /
-- `list_runs_for_version` like any other.
--
-- Membership is recorded BOTH ways: the fold row names its `backtest_run_id`,
-- and the `backtest_run` row carries `walk_forward_run_id` + `fold_index`, so
-- a run's provenance says which walk-forward it belongs to. The pair trigger
-- mirrors 0009's `backtest_run_window_pair` / 0012's
-- `backtest_run_window_lead_in_pair`: the two membership columns are set
-- together or not at all.
--
-- Decimal-as-TEXT (NFR-2): `pooled_mean_r` / `mean_r` are `.normalize()`d TEXT
-- like every run money column. `lower_bound` is the wf-v1 f64 bound — the one
-- quarantined conversion the stats discipline permits — stored REAL (an f64
-- verdict bound is not a money quantity; REAL round-trips it exactly on the
-- pinned toolchain). `holds`/`pass`/`from_defaulted` are INTEGER 0/1 flags.

-- ---------------------------------------------------------------------------
-- 1. `walk_forward_run`: the parent run row.
-- ---------------------------------------------------------------------------
CREATE TABLE walk_forward_run (
  id                   TEXT PRIMARY KEY NOT NULL,
  strategy_version_id  TEXT NOT NULL REFERENCES strategy_version(id),
  created_at           TEXT NOT NULL,                 -- injected Clock (RFC3339 UTC)
  scheme               TEXT NOT NULL,                 -- 'rolling-oos/v1'
  rule                 TEXT NOT NULL,                 -- 'wf-v1'
  k                    INTEGER NOT NULL,              -- the fold count, 2..=12
  span_from_ms         INTEGER NOT NULL,              -- counted span [from, to)
  span_to_ms           INTEGER NOT NULL,
  from_defaulted       INTEGER NOT NULL,              -- 0/1: `from` defaulted to the first warm bar
  engine_fingerprint   TEXT NOT NULL,                 -- the fingerprint the fold runs share
  folds_holding        INTEGER NOT NULL,
  folds_required       INTEGER NOT NULL,              -- ceil(2k/3)
  pooled_n             INTEGER NOT NULL,              -- trades across every fold
  pooled_mean_r        TEXT NOT NULL,                 -- Decimal-as-TEXT (NFR-2)
  pooled_lower_bound   REAL NOT NULL,                 -- the wf-v1 f64 bound
  pass                 INTEGER NOT NULL               -- 0/1: folds_holding >= folds_required && pooled holds
);
CREATE TRIGGER walk_forward_run_no_update BEFORE UPDATE ON walk_forward_run
  BEGIN SELECT RAISE(ABORT, 'walk_forward_run is immutable'); END;
CREATE TRIGGER walk_forward_run_no_delete BEFORE DELETE ON walk_forward_run
  BEGIN SELECT RAISE(ABORT, 'walk_forward_run is immutable'); END;

-- ---------------------------------------------------------------------------
-- 2. `walk_forward_fold`: one row per fold, naming its windowed run.
-- ---------------------------------------------------------------------------
CREATE TABLE walk_forward_fold (
  walk_forward_run_id  TEXT NOT NULL REFERENCES walk_forward_run(id),
  fold_index           INTEGER NOT NULL,
  window_from_ms       INTEGER NOT NULL,              -- the fold's counted window
  window_to_ms         INTEGER NOT NULL,
  backtest_run_id      TEXT NOT NULL REFERENCES backtest_run(id),
  n                    INTEGER NOT NULL,              -- the fold's trade count
  mean_r               TEXT NOT NULL,                 -- Decimal-as-TEXT (NFR-2)
  lower_bound          REAL NOT NULL,                 -- the wf-v1 f64 bound
  holds                INTEGER NOT NULL,              -- 0/1: n >= 20 && lower_bound > 0
  UNIQUE (walk_forward_run_id, fold_index)
);
CREATE TRIGGER walk_forward_fold_no_update BEFORE UPDATE ON walk_forward_fold
  BEGIN SELECT RAISE(ABORT, 'walk_forward_fold is immutable'); END;
CREATE TRIGGER walk_forward_fold_no_delete BEFORE DELETE ON walk_forward_fold
  BEGIN SELECT RAISE(ABORT, 'walk_forward_fold is immutable'); END;
-- A fold may only point at a WINDOWED backtest_run — the fold IS a window, so
-- a fold row whose run carries no window pair is a lie the schema refuses.
CREATE TRIGGER walk_forward_fold_windowed BEFORE INSERT ON walk_forward_fold
  BEGIN SELECT RAISE(ABORT, 'a walk_forward_fold may only reference a windowed backtest_run')
    WHERE (SELECT window_from_ms FROM backtest_run WHERE id = NEW.backtest_run_id) IS NULL
       OR (SELECT window_to_ms   FROM backtest_run WHERE id = NEW.backtest_run_id) IS NULL;
  END;

-- ---------------------------------------------------------------------------
-- 3. `backtest_run` membership columns + the pair trigger.
-- ---------------------------------------------------------------------------
ALTER TABLE backtest_run ADD COLUMN walk_forward_run_id TEXT REFERENCES walk_forward_run(id);
ALTER TABLE backtest_run ADD COLUMN fold_index          INTEGER;
CREATE TRIGGER backtest_run_walk_forward_pair BEFORE INSERT ON backtest_run
  BEGIN SELECT RAISE(ABORT, 'walk_forward_run_id and fold_index are both-or-neither')
    WHERE (NEW.walk_forward_run_id IS NULL) <> (NEW.fold_index IS NULL);
  END;
CREATE INDEX idx_backtest_run_walk_forward ON backtest_run(walk_forward_run_id, fold_index);
