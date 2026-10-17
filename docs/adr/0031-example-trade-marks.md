# 31. Example-trade marks: the store, its writers, the holdout fence and the explain contract

Date: 2026-10-16

## Status

Accepted. Registers the example-trade mark store, the two surfaces that write it, the holdout fence every
write passes and the `explain_marks` contract owed by RELEASE r5 (criterion 3) and ruled by PDR-0037. Refines
[ADR-0018](0018-evolution-forward-only-migrations-immutable-snapshots.md) (forward-only migrations, immutable
records), [ADR-0019](0019-stack-rust-core-embedded-sqlite-and-parquet.md) (SQLite for records, Parquet for
candles), [ADR-0020](0020-desktop-shell-tauri-react.md)'s command bus, [ADR-0026](0026-client-server-split.md)
(an always-on server, a thin client) and [ADR-0028](0028-wf-v2-holdout-and-hypothesis-budget.md) (the holdout
fence), and takes its authority rule from [ADR-0012](0012-agents-via-mcp-tools.md) and PDR-0015.

## Context

VISION.md's loop steps 1–5 and RELEASE r5's criterion 3: **"It is very difficult to communicate with my
coding agent, which is connecting via an MCP server, to really explain what kind of trading strategy I
use."** (operator, 2026-10-14). The answer the release chose is that a trader marks three or four example
trades on the chart — or an agent writes them through `pulse mcp`, each mark naming its writer — and an agent
then reads them, sees which conditions held at each example's bar, proposes a rule every example satisfies,
backtests it, and says what does not qualify and why.

Three rulings fixed the shape before any code existed:

- **PDR-0001 — drawings and example trades are strategy input.** A mark is not decoration; it is the trader's
  statement about what a setup looks like, and it is what the agent reasons over.
- **PDR-0007 / ADR-0028 — the holdout fence.** An open freeze's holdout is the data no one may look at while
  the campaign runs. The critic at plan-release r5 found the defect this ADR exists to close: **marks could
  carry holdout bars into discovery.** A trader could mark a trade inside the holdout, and the agent would
  then fit a rule to data the campaign is not allowed to have seen — through the back door, without ever
  running a backtest on it.
- **PDR-0015 — the agent authority boundary.** An agent may reach only what is its own, and what it says
  about itself is self-reported.

The repo had no marks, drawings or annotations of any kind, and no code path reported which conditions held
at one bar (PDR-0037; SPINE Survey facts).

## Decision

**One mark names one candle.** A mark holds the pair, the timeframe (`15m`, `4h` or `1d`), that candle's
**open time**, a side (`long` or `short`), a setup label (the trader's own words, 1–64 characters), an
optional note (at most 280 characters) and the writer that put it there (PDR-0037 Q1). **No stop and no
target**: exits come from the rule's risk section, and the position tool — stop, target, "what happened
next" — belongs with the drawing tools on the feature map.

**The candle must exist, and the fence runs first.** A mark's time must name a candle the pair's and
timeframe's HEAD snapshot actually holds; and a mark whose candle **reaches** an open freeze's holdout start
— whose close, the open plus the timeframe's nominal duration, lands **after** that start — is refused for
every writer. A bar closing *exactly* at the holdout start is legal: the holdout is open at that instant and
the bar never reaches past it. **The fence answers before the candle is looked up**, so a time after the
holdout start where no candle exists is refused for the holdout reason, never for the missing candle
(PDR-0037 Q2, Q4). A refused write writes nothing at all.

**Marks are immutable, and a withdraw is one-way.** A mark is never edited. It can be withdrawn once; the
row stays stored, and the withdrawal is a second row that records who took it back and when. The trader —
the app — may withdraw any mark; an agent may withdraw only a mark it wrote (PDR-0037 Q3). The reads hide a
withdrawn mark; nothing deletes one.

**One use case writes the table (G1).** `application::example_marks::write_mark` is the only code that
inserts an `example_mark` row, and both writers reach it: the app's `POST /api/v1/marks`, and the
`write_mark` MCP tool. No second path writes the table — a second one would be a second place for the fence
to be forgotten, and it would drift.

**The edge reads the freeze, the use case applies it.** Every delivery edge reads the open freeze
**inside the same call it serves**, immediately before the use case, and passes the read result down;
**no edge passes a hard-coded `None`**, which would be a fence switched off rather than a fence passed. A
freeze read failure is an error on every edge, so an unreadable freeze table cannot quietly disable the
control. **Every edge that exists is named here**, and each reads it:

- the app's three mark cores — `write_mark_core`, `list_marks_core` and `withdraw_mark_core` in
  `src/tauri/marks.rs` — through `edge_holdout`;
- the four MCP mark tools — `write_mark`, `list_marks`, `withdraw_mark` and `explain_marks` — through
  `open_holdout`, each inside its own tool call.

**What each use case then does with it.** `write_mark` applies `guard_candle` as its **first** check,
before any candle load (G2), so a refused write writes nothing. `list_marks` removes every mark whose
candle reaches the holdout start and **counts** them. `explain_marks` hides and counts every mark whose
evaluation bar opens at or after the holdout start, and steps no engine onto a candle at or after it (G4).
`withdraw_mark` applies the **same** `guard_candle` refusal a write gets, so a mark the fence covers cannot
be withdrawn while the freeze is open; but it applies it **after** its `not_found`, `already_withdrawn` and
`not_writer` checks and **before** it withdraws anything — the order is the decision, not an accident: an
agent's refusal for another writer's fenced mark is `not_writer` alone, so the fence cannot become a way to
learn where another writer's mark sits against the holdout. A fenced mark is not withdrawn and none of its
fields are returned (#440, PDR-0039).

**The writer is honest, and it is never a request member (G5, PDR-0015).** A trader mark is named by the app
token's **label** — never the token, which is stored nowhere and logged nowhere. An agent mark is named by
the identity the MCP session resolved for itself (`--agent-name`, else the handshake's `clientInfo.name`,
else `unknown`; over HTTP `/mcp`, the token's label). Because an agent's name is its own claim, every read
marks it as **self-reported**, and the app shows it as such.

**Marks are reference trades, never orders, and carry no credential.** A mark write touches no paper, order
or credential table, and no broker, exchange or execution adapter is reachable from the use case. The risk
gate's kill-switch is discharged, as everywhere else in this repo, by the dependency set being incapable of
placing an order rather than by a flag that disables one.

**The `explain_marks` contract (w2 implements it against this store).**

- **Input:** the mark ids to explain, or a setup label naming a group of them, plus a rule — an inline DSL
  document or the id of a stored version.
- **Evaluation bar (PDR-0037 Q5):** a `15m` mark is evaluated at **its own bar**; a `4h` or `1d` mark is
  evaluated at **the first `15m` bar that opens at its candle's close** — the earliest bar at which the
  backtest could have entered on it.
- **Output:** for each mark, every top-level conjunct of `entry ∧ filters` with **held / not held / not
  warm** and the operand values behind that answer, plus **the 17 candle-pattern values** at the mark's own
  candle (`CandlePatternName`: Doji, DragonflyDoji, GravestoneDoji, Hammer, InvertedHammer, HangingMan,
  ShootingStar, Marubozu, SpinningTop, Engulfing, Harami, Piercing, DarkCloudCover, MorningStar, EveningStar,
  ThreeWhiteSoldiers, ThreeBlackCrows). Marks the fence hides are **counted**, never explained.
- **G3 — the same truth as the backtest:** a condition `explain_marks` reports as held at a mark's evaluation
  bar is exactly what the backtest's entry gate sees at that bar for that rule; and on a backtest's own entry
  bars, every condition reads held. There is no second evaluator: explain steps the same engine the backtest
  steps.
- **G4 — no later bar:** explain steps engines over candles strictly before the evaluation bar's close and
  never onto a candle at or after the holdout start. A `4h` or `1d` candle is seen only once it has closed.
- It reads through this ADR's port and use case, so an explained mark is the same mark the app and the agent
  wrote.

**The read-side hide is reported (Q4, PDR-0011).** While a freeze is open, marks whose candle reaches its
holdout start are removed from the list and counted: the answer carries the holdout start and how many marks
were hidden. Hiding is a read-side act — the rows stay stored — and an unreported hide would be the app
telling a trader their mark had vanished.

## Consequences

- **The store is two tables and one migration.** `0021_example_marks` adds `example_mark` (immutable, four
  CHECK-held enums, `no_update`/`no_delete` triggers, an index on `(pair, timeframe, bar_open_ms)`) and
  `example_mark_withdrawal` (`mark_id` PRIMARY KEY, so a second withdraw is unrepresentable rather than merely
  refused; `no_update`/`no_delete` triggers). The down file restores the exact 0020 shape, and **refuses over
  any mark or withdrawal row** rather than erasing a record 0020 cannot hold — ADR-0018's "truthful or
  refuse". The embedded max moves 20 → 21.
- **The touch surface** joins the bones registry at the r5.s3 close: `migrations/0021_example_marks.*`,
  `src/domain/example_mark.rs`, `src/application/example_marks.rs`, `src/adapters/db/example_mark_repo.rs`,
  `src/tauri/marks.rs`, and (w2) `src/application/mark_explain.rs`.
- **The w2 and w3 edges.** w2 (`explain_marks`) reads marks through this ADR's port and use case, and is the
  bone half of the spine. w3 (the Chart tab's marking UI) writes and reads them through the three bus
  commands `write_mark`, `list_marks` and `withdraw_mark` and their regenerated bindings. Neither adds a
  write path.
- **The fence rule itself is unchanged.** `guard_candle` is one new single-candle decision beside
  `guard_span` in `src/application/holdout.rs`; the freeze tables, `certify` and the holdout test are
  untouched.
- **Determinism is held, though it is not a gated surface here.** The engine fingerprint's **value** does
  move: this spine rewrites `src/adapters/backtest/session.rs` and `engine.rs`, both under the
  `src/adapters/backtest` root of `build_support/engine_source_set.rs`'s `ENGINE_SOURCE_ROOTS`, and the
  fingerprint is persisted to `backtest_run.engine_fingerprint`. Adding the read-only `EvalProbe` seam and
  correcting its warm clause each change a hashed engine source file, so the fingerprint's value follows the
  engine source set. **No backtest golden, pinned hash, fixture or trade log moves**: a mark is a stored
  reference trade, read by no engine, and the engine's own results are unchanged. The `b8e91b89…`
  determinism pin is a run-content hash, not the fingerprint, and it holds.

## Sources

PDR-0037 (the spine's rulings, Q1–Q7); PDR-0001, PDR-0007, PDR-0011, PDR-0015; ADR-0018, ADR-0019, ADR-0020,
ADR-0026, ADR-0028; RELEASE r5 criterion 3; `docs/specs/r5/r5.s3-example-trades-on-the-chart-become-a-strategy/SPINE.md`;
operator, 2026-10-14 (the real-use finding quoted above); r5.s3.w1.
