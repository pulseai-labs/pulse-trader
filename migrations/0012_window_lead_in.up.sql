-- r2.s3.w2 — 0012: the lead-in start a windowed run consumed, on `backtest_run`.
--
-- A windowed backtest no longer slices the series to `[from, to)` and warms
-- its indicators inside the window (the r2.s1.w3 ruling-1 semantics this
-- column supersedes). Both series now load from the snapshot's first candle
-- so the engines step every bar before `from`, while entries, fills, exits,
-- funding, equity and PnL count only on `[from, to)`. `window_lead_in_from_ms`
-- records the `open_time` of the first candle the engine consumed — the
-- snapshot's first candle under full-history lead-in, equal to
-- `window_from_ms` when nothing precedes `from`.
--
-- NULL means there is no lead-in to report: an unwindowed run, or any
-- pre-0012 row — the lead-in a legacy windowed run consumed is not
-- recoverable from anything stored, and ADR-0018 forbids backfilling
-- immutable records with invented facts.
--
-- The lead-in is only meaningful relative to a counted window, so a non-NULL
-- lead-in on a row whose window pair is incomplete is refused — the same
-- CHECK-style trigger shape as 0009's `backtest_run_window_pair`, which stays
-- untouched.

ALTER TABLE backtest_run ADD COLUMN window_lead_in_from_ms INTEGER;
CREATE TRIGGER backtest_run_window_lead_in_pair BEFORE INSERT ON backtest_run
  BEGIN SELECT RAISE(ABORT, 'window_lead_in_from_ms requires a complete window pair')
    WHERE NEW.window_lead_in_from_ms IS NOT NULL
      AND (NEW.window_from_ms IS NULL OR NEW.window_to_ms IS NULL);
  END;
