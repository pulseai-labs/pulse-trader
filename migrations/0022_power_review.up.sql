-- r5.s5.w1 — 0022: the power review (PDR-0040; ADR-0032; ADR-0028; ADR-0010/0018/0019).
--
-- ONE immutable row per count-only holdout read. Before a certify attempt spends one of the open freeze's
-- hypotheses, an operator asks how many trades the candidate makes in that freeze's holdout window — the
-- COUNT and nothing else about those trades (SPINE G1) — and the chance ADR-0028's C1 test would pass a
-- true +0.25R edge at that count. The read spends NO hypothesis and changes no budget; it is recorded so
-- the audit trail names who looked.
--
-- THE COUNT-ONLY LAW IS THE COLUMN SET. There is no column that could hold an R, a mean, a bound, a P&L,
-- a win rate, a trade time or a holdout verdict: the holdout side of this row is `holdout_n`, the window
-- and the pinned inputs, and nothing else. The bar's arithmetic (z, sigma_r, pass_chance, bar,
-- min_holdout_trades, allowed, reason) describes the SEARCH span and the pre-committed bar, never the
-- holdout's outcomes.
--
-- THE IMMUTABILITY IS THE 0020 PATTERN, BLANKET: no UPDATE, no DELETE. A review is the record of a read,
-- and rewriting it would rewrite what the operator was told before the attempt.
--
-- TIMESTAMPS ARE MIXED BY COLUMN, deliberately: `holdout_start_ms` / `holdout_end_ms` are INTEGER epoch ms
-- (candle bounds, compared with no parsing, as `0020`'s are), while `created_at` is the RFC3339 UTC text
-- every other record table uses. `sigma_r`, `assumed_edge_r` and `bar` are Decimal-as-TEXT (NFR-2); `z`
-- and `pass_chance` stay REAL, as `0020`'s three f64s do.

-- ---------------------------------------------------------------------------
-- 1. `power_review`: one immutable row per count-only read.
-- ---------------------------------------------------------------------------
CREATE TABLE power_review (
  id                         TEXT PRIMARY KEY NOT NULL,
  version_id                 TEXT NOT NULL REFERENCES strategy_version(id),
  freeze_id                  TEXT NOT NULL REFERENCES certification_freeze(id),
  pair                       TEXT NOT NULL CHECK (length(trim(pair)) > 0),
  h                          INTEGER NOT NULL CHECK (h BETWEEN 1 AND 12),
  z                          REAL NOT NULL,                                  -- C1's own z(1 - 0.05/H)
  holdout_start_ms           INTEGER NOT NULL,                               -- inclusive
  holdout_end_ms             INTEGER NOT NULL,                               -- exclusive
  holdout_n                  INTEGER NOT NULL CHECK (holdout_n >= 0),        -- THE count, and all we record
  holdout_inputs             TEXT NOT NULL CHECK (length(trim(holdout_inputs)) > 0),  -- pinned versions, JSON
  search_walk_forward_run_id TEXT NOT NULL REFERENCES walk_forward_run(id),
  search_pass                INTEGER NOT NULL CHECK (search_pass IN (0, 1)),
  search_n                   INTEGER NOT NULL CHECK (search_n >= 0),
  sigma_r                    TEXT NOT NULL,                                  -- the candidate's search-span σ
  assumed_edge_r             TEXT NOT NULL,                                  -- "0.25"
  bar                        TEXT NOT NULL,                                  -- "0.50"
  min_holdout_trades         INTEGER NOT NULL,                               -- 20
  pass_chance                REAL NOT NULL,
  allowed                    INTEGER NOT NULL CHECK (allowed IN (0, 1)),
  reason                     TEXT CHECK (reason IS NULL OR reason IN
                               ('search_span_fails', 'too_few_holdout_trades', 'below_bar')),
  engine_fingerprint         TEXT NOT NULL CHECK (length(trim(engine_fingerprint)) > 0),
  called_by                  TEXT NOT NULL CHECK (length(trim(called_by)) > 0),
  created_at                 TEXT NOT NULL,                                  -- injected Clock (RFC3339 UTC)
  CHECK (holdout_end_ms > holdout_start_ms),
  CHECK ((allowed = 1) = (reason IS NULL))
);

CREATE TRIGGER power_review_no_update BEFORE UPDATE ON power_review
  BEGIN SELECT RAISE(ABORT, 'power review records are immutable'); END;
CREATE TRIGGER power_review_no_delete BEFORE DELETE ON power_review
  BEGIN SELECT RAISE(ABORT, 'power review records are never deleted'); END;

-- The read the certify gate (w2) and the app's panel (w3) both make: the newest review for a version, pair
-- and freeze.
CREATE INDEX idx_power_review_latest ON power_review (version_id, pair, freeze_id, created_at);
