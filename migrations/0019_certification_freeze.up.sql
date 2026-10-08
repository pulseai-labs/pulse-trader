-- r4.s1.w4 — 0019: the certification freeze (F1, C4; ADR-0010/0018/0019).
--
-- ONE row per freeze. The operator opens a freeze with `pulse certify
-- freeze`, which records the holdout start, the hypothesis budget H, the
-- measured alpha, the C1 holdout test's name and z rule, and the open instant.
-- `pulse certify close-freeze` writes `closed_at_ms` once, at the campaign's
-- end; the holdout guard is active only while a freeze is OPEN (it reads the
-- holdout start off the open row), and a spent holdout is never reused (F1).
--
-- THE IMMUTABILITY IS NARROWED, NOT BLANKET — the `0016` `client_token`
-- shape. Every column is compared `IS NOT` (NULL-safe) and the ONE legal
-- mutation is the first `closed_at_ms` transition (NULL -> a value): a second
-- close, an edit, and a DELETE are each refused by trigger, so the record the
-- campaign ran under cannot drift afterwards.
--
-- TWO LAWS THE SCHEMA HOLDS ITSELF, both also enforced by the repository with
-- named refusals (a raw `INSERT` cannot talk its way past either):
--
--   1. AT MOST ONE OPEN FREEZE — `certification_freeze_one_open`, a partial
--      UNIQUE index over `(closed_at_ms IS NULL)` restricted to the open rows,
--      so the constant `1` every open row indexes collides on the second one.
--   2. A NEW FREEZE STARTS AFTER EVERY EARLIER CLOSE —
--      `certification_freeze_start_after_last_close` refuses an INSERT whose
--      `holdout_start_ms` is not strictly later than every earlier
--      `closed_at_ms`, and refuses a row inserted already-closed (closing is
--      an UPDATE, and only one).
--
-- TIMESTAMPS ARE INTEGER EPOCH MS, not the RFC3339 TEXT the other tables use:
-- the columns are named `*_ms` by the spec, the guard compares them against
-- candle `open_time`s with no parsing, and `h`/`alpha`/`holdout_test` carry the
-- frozen parameters verbatim (`alpha` is Decimal-as-TEXT, NFR-2; no f64 column
-- anywhere).

-- ---------------------------------------------------------------------------
-- 1. `certification_freeze`: one immutable row per freeze, closed at most once.
-- ---------------------------------------------------------------------------
CREATE TABLE certification_freeze (
  id                TEXT PRIMARY KEY NOT NULL,
  holdout_start_ms  INTEGER NOT NULL,            -- the holdout's inclusive start, epoch ms
  h                 INTEGER NOT NULL CHECK (h BETWEEN 1 AND 12),
  alpha             TEXT NOT NULL CHECK (length(trim(alpha)) > 0),          -- Decimal-as-TEXT (NFR-2)
  holdout_test      TEXT NOT NULL CHECK (length(trim(holdout_test)) > 0),   -- the C1 test's name + z rule
  opened_at_ms      INTEGER NOT NULL,            -- injected Clock, epoch ms
  closed_at_ms      INTEGER,                     -- NULL while open, then final
  CHECK (closed_at_ms IS NULL OR closed_at_ms >= opened_at_ms)
);

-- The narrowed update law: refuse unless this is EXACTLY the first
-- `closed_at_ms` transition and every other column is untouched. A closed row
-- is fully immutable; clearing the close is unrepresentable.
CREATE TRIGGER certification_freeze_no_update BEFORE UPDATE ON certification_freeze
WHEN NEW.id IS NOT OLD.id
  OR NEW.holdout_start_ms IS NOT OLD.holdout_start_ms
  OR NEW.h IS NOT OLD.h
  OR NEW.alpha IS NOT OLD.alpha
  OR NEW.holdout_test IS NOT OLD.holdout_test
  OR NEW.opened_at_ms IS NOT OLD.opened_at_ms
  OR NEW.closed_at_ms IS NULL
  OR OLD.closed_at_ms IS NOT NULL
BEGIN
  SELECT RAISE(ABORT, 'certification_freeze is immutable except a first closed_at_ms');
END;

CREATE TRIGGER certification_freeze_no_delete BEFORE DELETE ON certification_freeze
  BEGIN SELECT RAISE(ABORT, 'certification_freeze rows are never deleted'); END;

-- Law 1: at most one open freeze. The indexed expression is the constant `1`
-- on every row inside the partial index, so a second open row collides.
CREATE UNIQUE INDEX certification_freeze_one_open
  ON certification_freeze ((closed_at_ms IS NULL))
  WHERE closed_at_ms IS NULL;

-- Law 2: a new freeze starts its holdout strictly after every earlier close,
-- and a row is inserted OPEN (closing is the one UPDATE the trigger above
-- admits).
CREATE TRIGGER certification_freeze_start_after_last_close BEFORE INSERT ON certification_freeze
WHEN NEW.closed_at_ms IS NOT NULL
  OR EXISTS (
       SELECT 1 FROM certification_freeze f
        WHERE f.closed_at_ms IS NULL
           OR NEW.holdout_start_ms <= f.closed_at_ms)
BEGIN
  SELECT RAISE(ABORT, 'certification_freeze: a freeze is inserted open, at most one may be open, and a new holdout starts after every earlier close');
END;
