-- r3.s4.w2 — 0018: the paper session aggregate (ADR-0027).
--
-- A live paper session is ONE immutable `paper_session` row, written once at
-- promotion, plus an append-only `paper_event` log whose state is a replay
-- (the row records the promotion decision; the log records everything that
-- happened since). The candles a shadow check materialises are `paper_bar`
-- rows — one row per consumed bar per timeframe, flagged `lead_in` for the
-- warm-up so `count_from_ms` is just the first non-lead-in `open_time`.
-- `fixture_snapshot` is the certify fixture's durable stamp: a certified
-- session whose every data version has a row here was certified on synthetic
-- data (`fixture = 1`, never OOS-comparable — A12).
--
-- THE GRADUATION IS A SUM ENCODED AS COLUMNS. `graduation` carries the
-- variant word; each variant names exactly its own columns and refuses the
-- other's: `certified` ⇒ a `walk_forward_run_id`, a non-empty
-- `certified_data_versions` JSON list and no override fields; `override` ⇒
-- no run id, a non-empty `override_reason` and an `override_at`, and no
-- certified versions to name. `promoted_by` is the promoting client token's
-- label (`0016`'s `client_token.label`, audit #6) — never empty or
-- whitespace.
--
-- CONVENTIONS are the established ones: TEXT keys, RFC3339 UTC timestamps,
-- Decimal-as-TEXT money (NFR-2), `MAX(seq)+1` insertion sequences minted
-- inside the write transaction, immutable rows by RAISE(ABORT) trigger
-- (ADR-0010/0019), and no f64 column anywhere.
--
-- `engine_upgraded` opens epochs at the LOG level (the typed event, E3);
-- the session row's `engine_fingerprint` is the fingerprint at start.

-- ---------------------------------------------------------------------------
-- 1. `paper_session`: one immutable row per promotion.
-- ---------------------------------------------------------------------------
CREATE TABLE paper_session (
  id                     TEXT PRIMARY KEY NOT NULL,
  seq                    INTEGER NOT NULL UNIQUE,  -- insertion order, minted MAX(seq)+1 inside the write tx
  strategy_version_id    TEXT NOT NULL REFERENCES strategy_version(id),
  created_at             TEXT NOT NULL,            -- injected Clock (RFC3339 UTC)
  pair                   TEXT NOT NULL,
  primary_timeframe      TEXT NOT NULL,            -- Binance interval text (15m/4h/1d), the 0006 convention
  htf_timeframe          TEXT,                     -- NULL = a single-timeframe session
  uses_d1                INTEGER NOT NULL CHECK (uses_d1 IN (0, 1)),
  starting_equity        TEXT NOT NULL,            -- Decimal-as-TEXT (NFR-2), USDT
  taker_fee_bps          TEXT NOT NULL,            -- Decimal-as-TEXT (NFR-2)
  slippage_bps           TEXT NOT NULL,            -- Decimal-as-TEXT (NFR-2)
  engine_fingerprint     TEXT NOT NULL,            -- the fingerprint at start (E3's first epoch)
  graduation             TEXT NOT NULL CHECK (graduation IN ('certified', 'override')),
  walk_forward_run_id    TEXT REFERENCES walk_forward_run(id),
  override_reason        TEXT,
  override_at            TEXT,
  certified_data_versions TEXT NOT NULL,           -- JSON [{timeframe, data_version}]
  fixture                INTEGER NOT NULL CHECK (fixture IN (0, 1)),
  min_trades             INTEGER NOT NULL,
  promoted_by            TEXT NOT NULL,            -- client_token.label (audit #6)

  -- `certified` names its run, its data versions, and nothing of the override.
  CHECK (
    graduation <> 'certified'
    OR (walk_forward_run_id IS NOT NULL
        AND json_array_length(certified_data_versions) > 0
        AND override_reason IS NULL
        AND override_at IS NULL)
  ),
  -- `override` names its reason (never empty/whitespace) and its instant,
  -- and carries no run and no certified versions. `override_reason IS NOT
  -- NULL` is spelled out because a bare `length(trim(NULL, ...)) > 0` is
  -- NULL in SQL, and a NULL CHECK result PASSES — the explicit IS NOT NULL
  -- makes the missing-reason case a violation, not a hole.
  CHECK (
    graduation <> 'override'
    OR (walk_forward_run_id IS NULL
        AND override_reason IS NOT NULL
        AND length(trim(override_reason, char(9, 10, 11, 12, 13, 32, 133, 160, 5760,
                                        8192, 8193, 8194, 8195, 8196, 8197, 8198,
                                        8199, 8200, 8201, 8202, 8232, 8233, 8239,
                                        8287, 12288))) > 0
        AND override_at IS NOT NULL
        AND json_array_length(certified_data_versions) = 0)
  ),
  -- The promoting token's label is a `client_token.label` — never blank.
  CHECK (
    length(trim(promoted_by, char(9, 10, 11, 12, 13, 32, 133, 160, 5760,
                                  8192, 8193, 8194, 8195, 8196, 8197, 8198,
                                  8199, 8200, 8201, 8202, 8232, 8233, 8239,
                                  8287, 12288))) > 0
  )
);
CREATE TRIGGER paper_session_no_update BEFORE UPDATE ON paper_session
  BEGIN SELECT RAISE(ABORT, 'paper_session is immutable'); END;
CREATE TRIGGER paper_session_no_delete BEFORE DELETE ON paper_session
  BEGIN SELECT RAISE(ABORT, 'paper_session is immutable'); END;

-- ---------------------------------------------------------------------------
-- 2. `paper_event`: the append-only log; stopped is read-only (A4).
-- ---------------------------------------------------------------------------
CREATE TABLE paper_event (
  session_id  TEXT NOT NULL REFERENCES paper_session(id),
  seq         INTEGER NOT NULL,          -- per session, minted MAX(seq)+1 inside the write tx
  at          TEXT NOT NULL,             -- injected Clock (RFC3339 UTC)
  kind        TEXT NOT NULL CHECK (kind IN (
                'bar_processed', 'order', 'fill', 'funding', 'stop',
                'data_event', 'engine_upgraded', 'shadow_checked')),
  payload     TEXT NOT NULL,             -- the typed event's JSON (internal "type" tag)
  UNIQUE (session_id, seq)
);
CREATE TRIGGER paper_event_no_update BEFORE UPDATE ON paper_event
  BEGIN SELECT RAISE(ABORT, 'paper_event is append-only'); END;
CREATE TRIGGER paper_event_no_delete BEFORE DELETE ON paper_event
  BEGIN SELECT RAISE(ABORT, 'paper_event is append-only'); END;
-- A stopped session's log is read-only: nothing appends after `stop` (A4).
CREATE TRIGGER paper_event_no_insert_after_stop BEFORE INSERT ON paper_event
  BEGIN
    SELECT RAISE(ABORT, 'paper_event: the session is stopped; the log is read-only')
    WHERE EXISTS (
      SELECT 1 FROM paper_event e
      WHERE e.session_id = NEW.session_id AND e.kind = 'stop'
    );
  END;

-- ---------------------------------------------------------------------------
-- 3. `paper_bar`: the recorded candles (audit #2 as corrected) — one row per
--    consumed bar per timeframe; `lead_in` flags the warm-up.
-- ---------------------------------------------------------------------------
CREATE TABLE paper_bar (
  session_id   TEXT NOT NULL REFERENCES paper_session(id),
  timeframe    TEXT NOT NULL,            -- Binance interval text (15m/4h/1d), the 0006 convention
  seq          INTEGER NOT NULL,         -- per session, minted MAX(seq)+1 inside the write tx
  open_time    INTEGER NOT NULL,
  close_time   INTEGER NOT NULL,
  open         TEXT NOT NULL,            -- Decimal-as-TEXT (NFR-2)
  high         TEXT NOT NULL,
  low          TEXT NOT NULL,
  close        TEXT NOT NULL,
  volume       TEXT NOT NULL,
  funding_rate TEXT,                     -- Decimal-as-TEXT; NULL when the bar carried none
  lead_in      INTEGER NOT NULL CHECK (lead_in IN (0, 1)),
  UNIQUE (session_id, timeframe, open_time)
);
CREATE TRIGGER paper_bar_no_update BEFORE UPDATE ON paper_bar
  BEGIN SELECT RAISE(ABORT, 'paper_bar is append-only'); END;
CREATE TRIGGER paper_bar_no_delete BEFORE DELETE ON paper_bar
  BEGIN SELECT RAISE(ABORT, 'paper_bar is append-only'); END;
-- Stopped is read-only (A4) for the recorded bars too: no bar lands once a
-- `stop` exists. The sanctioned `append_bar` writes bars before events, so
-- the event-log wall alone let a bare-bars batch (empty events) commit; this
-- mirror of `paper_event_no_insert_after_stop` makes the whole session
-- read-only at the schema, and the batch's transaction rolls back whole.
CREATE TRIGGER paper_bar_no_insert_after_stop BEFORE INSERT ON paper_bar
  BEGIN
    SELECT RAISE(ABORT, 'paper_bar: the session is stopped; the log is read-only')
    WHERE EXISTS (
      SELECT 1 FROM paper_event e
      WHERE e.session_id = NEW.session_id AND e.kind = 'stop'
    );
  END;

-- ---------------------------------------------------------------------------
-- 4. `fixture_snapshot`: the certify fixture's durable stamp (E4) — the rows
--    that make a certified session's `fixture` flag TRUE.
-- ---------------------------------------------------------------------------
CREATE TABLE fixture_snapshot (
  pair         TEXT NOT NULL,
  timeframe    TEXT NOT NULL,            -- Binance interval text (15m/4h/1d), the 0006 convention
  data_version TEXT NOT NULL,
  created_at   TEXT NOT NULL,            -- injected Clock (RFC3339 UTC)
  PRIMARY KEY (pair, timeframe, data_version)
);
CREATE TRIGGER fixture_snapshot_no_update BEFORE UPDATE ON fixture_snapshot
  BEGIN SELECT RAISE(ABORT, 'fixture_snapshot is append-only'); END;
CREATE TRIGGER fixture_snapshot_no_delete BEFORE DELETE ON fixture_snapshot
  BEGIN SELECT RAISE(ABORT, 'fixture_snapshot is append-only'); END;

CREATE INDEX idx_paper_event_session ON paper_event(session_id, seq);
CREATE INDEX idx_paper_bar_session ON paper_bar(session_id, timeframe, open_time);
