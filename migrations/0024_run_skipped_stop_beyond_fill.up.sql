-- r6.s2.w4 — 0024: the structure-stop skip count on `backtest_run`.
--
-- ADR-0033 (decision 6): when the bar after a setup's trigger opens at or past
-- the structure stop, the entry is skipped and counted, not filled and not a
-- failed run. A stored run keeps that count beside the other three
-- `skipped_*` columns, so the count re-enters the content hash when the run is
-- read back. The column is a nullable INTEGER.
--
-- NULL for every row written before this migration. A pre-0024 run has no
-- recorded count, and none is invented for it (ADR-0018 forbids rewriting
-- immutable records with invented facts); the read maps NULL to 0, and the hash
-- feed appends nothing for a zero count, so such a row re-derives the
-- identical byte stream it hashed under — the 0011 and 0023 precedent.
--
-- New runs always write the count of the result they save, 0 included, so the
-- stored count is the one the engine recorded.

ALTER TABLE backtest_run ADD COLUMN skipped_stop_beyond_fill INTEGER;
