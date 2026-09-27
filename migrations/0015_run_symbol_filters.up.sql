-- r3.s1.w4 — 0015 up: record the exchange symbol filters each run's sizer ran
-- under (#142), alongside the input provenance 0006 introduced.
--
-- Two runs are only comparable when the engine was given the same symbol
-- constraints (lot step, minimum quantity, minimum notional, maximum
-- leverage), so the filters become part of the run's recorded inputs and are
-- written on every new run — standalone and each walk-forward fold — from the
-- value the `ExchangeAdapter` port resolved (never a guessed constant).
--
-- All four columns are a GROUP: a row carries all four or none. The repository
-- decoder fails a partially-populated group as a corrupt row — the same
-- all-or-nothing discipline 0006 established for its eight columns.
--
-- All four NULL = a pre-0015 run: the filters are "not recorded", never
-- invented (ADR-0018 forbids rewriting immutable records with guessed facts).
-- The columns are nullable and no existing row is rewritten; the immutability
-- triggers are untouched.

ALTER TABLE backtest_run ADD COLUMN lot_step       TEXT; -- Decimal-as-TEXT (NFR-2)
ALTER TABLE backtest_run ADD COLUMN min_qty        TEXT; -- Decimal-as-TEXT (NFR-2)
ALTER TABLE backtest_run ADD COLUMN min_notional   TEXT; -- Decimal-as-TEXT (NFR-2)
ALTER TABLE backtest_run ADD COLUMN max_leverage   TEXT; -- Decimal-as-TEXT (NFR-2)
