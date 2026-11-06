-- r6.s2.w3 — 0023: the setup trace on `trade`.
--
-- ADR-0033 (decision 7): a trade opened by a setup records when the setup
-- armed, when it dipped, and when its stop anchor closed, so a stored run says
-- WHY each trade entered. The trigger time needs no column: it is the trade's
-- `entry_signal_time`. Each column is the INTEGER close time of that bar in
-- epoch milliseconds (the `entry_signal_time` representation).
--
-- `setup_arm_time` and `setup_dip_time` are set together or not at all (a trace
-- always has both); `setup_anchor_time` is NULL when the setup found no anchor
-- bar. All three are NULL for every row written before this migration, and for
-- every trade of a strategy without a setup. A pre-0023 trade has no recorded
-- trace, and none is invented for it (ADR-0018 forbids rewriting immutable
-- records with invented facts; the hash feed reads a missing trace as `None`
-- and appends nothing, so such a row re-derives the identical byte stream it
-- hashed under — the 0011 precedent).
--
-- New runs write the trace of the position's setup, so the stored trace is the
-- one the engine recorded at the fire.

ALTER TABLE trade ADD COLUMN setup_arm_time INTEGER;
ALTER TABLE trade ADD COLUMN setup_dip_time INTEGER;
ALTER TABLE trade ADD COLUMN setup_anchor_time INTEGER;
