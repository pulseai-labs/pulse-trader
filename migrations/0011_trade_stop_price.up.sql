-- r2.s2.w2 — 0011: the recorded per-trade stop on `trade`.
--
-- Schema 1.1.0's `AtrStop` derives the stop from the primary-series ATR read at
-- the signal bar and freezes it through the fill — that frozen absolute price
-- is what the run must record, for BOTH stop kinds (`StopLoss`'s fixed-fraction
-- stop and `AtrStop`'s ATR-multiple one). The column is the TEXT of the decimal
-- (the `entry_price`/`realized_pnl` representation), NULL for every row written
-- before this migration — a pre-0011 trade has no recorded stop, and none is
-- invented for it (ADR-0018 forbids rewriting immutable records with invented
-- facts; the hash feed reads `NULL` as `None` and appends nothing, so such a
-- row re-derives the identical byte stream it hashed under).
--
-- New runs always write `Some(position.stop_price)` — the value already frozen
-- on the open position at fill — so the stored stop is the same one that drove
-- sizing, exit resolution, excursion, and the R math.

ALTER TABLE trade ADD COLUMN stop_price TEXT;
