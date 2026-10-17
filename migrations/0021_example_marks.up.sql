-- r5.s3.w1 — 0021: the example-trade marks (PDR-0037; ADR-0010/0018/0019, ADR-0031).
--
-- A mark is a trader's or an agent's REFERENCE TRADE for one candle: the pair,
-- the timeframe, that candle's open time, a side, a setup label, an optional
-- note, and the writer that put it there. It is never an order and carries no
-- credential (PDR-0015, PDR-0037 Q1).
--
-- TWO TABLES, BECAUSE A WITHDRAW IS A SECOND FACT. A mark is immutable from the
-- instant it lands (Q3) — the audit of what an agent read cannot be rewritten —
-- so a withdraw does not touch the mark row: it appends one row to
-- `example_mark_withdrawal`. `mark_id` is that table's PRIMARY KEY, which makes a
-- SECOND withdraw unrepresentable rather than merely refused: one row per mark,
-- at most, for as long as the database lives (the `0020` `certification_freeze_index`
-- shape, one level simpler).
--
-- ONE USE CASE WRITES `example_mark` (SPINE G1). Both surfaces — the app's
-- `POST /api/v1/marks` and the `pulse mcp` tools — reach it through
-- `application::example_marks`, whose FIRST check is the holdout fence, before any
-- candle is loaded (G2). This schema is a backstop against a raw statement, not
-- the mechanism.
--
-- THE CHECK CONSTRAINTS ARE THE ENUMS. `timeframe`, `side`, `writer_kind` and the
-- setup's non-emptiness hold in the schema as well as in the domain newtypes, so a
-- raw INSERT cannot store a side the readers have no word for. `note` is the ONE
-- nullable column: an absent note is absent, never an empty string standing in for
-- one.
--
-- TIMESTAMPS ARE INTEGER EPOCH MS (`bar_open_ms`, `created_at_ms`,
-- `withdrawn_at_ms`), as `0019`'s freeze columns are: `bar_open_ms` is compared
-- against candle `open_time`s with no parsing, and the fence's whole point is that
-- the comparison is exact. `data_version` records the HEAD snapshot the candle was
-- found in, so a mark says which data it was marked against.

-- ---------------------------------------------------------------------------
-- 1. `example_mark`: one immutable row per reference trade.
-- ---------------------------------------------------------------------------
CREATE TABLE example_mark (
  id            TEXT PRIMARY KEY NOT NULL,
  pair          TEXT NOT NULL CHECK (length(trim(pair)) > 0),
  timeframe     TEXT NOT NULL CHECK (timeframe IN ('15m', '4h', '1d')),
  bar_open_ms   INTEGER NOT NULL,                                  -- the marked candle's open, epoch ms
  side          TEXT NOT NULL CHECK (side IN ('long', 'short')),
  setup         TEXT NOT NULL CHECK (length(trim(setup)) > 0 AND length(setup) <= 64),
  note          TEXT CHECK (note IS NULL OR length(note) <= 280),  -- NULL = no note
  writer_kind   TEXT NOT NULL CHECK (writer_kind IN ('trader', 'agent')),
  writer_name   TEXT NOT NULL CHECK (length(trim(writer_name)) > 0),  -- the token LABEL, or the self-reported agent name
  data_version  TEXT NOT NULL CHECK (length(trim(data_version)) > 0),
  created_at_ms INTEGER NOT NULL                                   -- injected Clock, epoch ms
);

-- A mark is never edited: there is no legal UPDATE of it at all (Q3, the `0020`
-- blanket pattern — a mark is the record of what a trader pointed at).
CREATE TRIGGER example_mark_no_update BEFORE UPDATE ON example_mark
  BEGIN SELECT RAISE(ABORT, 'example marks are immutable'); END;

-- A withdrawn mark stays stored: withdrawing appends a row to the withdrawal
-- table, and a DELETE would erase the only account of what was marked.
CREATE TRIGGER example_mark_no_delete BEFORE DELETE ON example_mark
  BEGIN SELECT RAISE(ABORT, 'example marks are never deleted'); END;

-- The read the list makes, and the index the marks table grows into (w3): the
-- marks of one pair and timeframe, in candle order.
CREATE INDEX idx_example_mark_candle ON example_mark (pair, timeframe, bar_open_ms);

-- ---------------------------------------------------------------------------
-- 2. `example_mark_withdrawal`: one row per withdrawn mark, ever.
-- ---------------------------------------------------------------------------
CREATE TABLE example_mark_withdrawal (
  mark_id           TEXT PRIMARY KEY NOT NULL REFERENCES example_mark(id),
  withdrawn_at_ms   INTEGER NOT NULL,                                    -- injected Clock, epoch ms
  withdrawn_by_kind TEXT NOT NULL CHECK (withdrawn_by_kind IN ('trader', 'agent')),
  withdrawn_by_name TEXT NOT NULL CHECK (length(trim(withdrawn_by_name)) > 0)
);

-- A withdrawal is written once and never edited: it says WHO took the mark back
-- and WHEN, and either half being rewritten would make the trail a guess (Q3).
CREATE TRIGGER example_mark_withdrawal_no_update BEFORE UPDATE ON example_mark_withdrawal
  BEGIN SELECT RAISE(ABORT, 'a withdrawal is written once and never edited'); END;

-- Un-withdrawing is not a state this schema can hold: the withdrawal row is the
-- mark's one-way door, and deleting it would silently return the mark to every
-- agent's read.
CREATE TRIGGER example_mark_withdrawal_no_delete BEFORE DELETE ON example_mark_withdrawal
  BEGIN SELECT RAISE(ABORT, 'a withdrawal is never deleted'); END;
