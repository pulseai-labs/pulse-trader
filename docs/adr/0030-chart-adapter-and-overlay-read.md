# 30. The chart adapter boundary and the overlay read contract

Date: 2026-10-15

## Status

Accepted. Registers the chart library's adapter boundary and the overlay read contract owed by
RELEASE r5 (criterion 2) and ruled by PDR-0036. Refines [ADR-0015](0015-system-shape-modular-monolith-hexagonal-one-artifact.md)
(one process, one artifact) and [ADR-0026](0026-client-server-split.md) (an always-on server, a
thin client), and takes its read shape from [ADR-0020](0020-desktop-shell-tauri-react.md)'s
command bus and [ADR-0024](0024-dsl-schema-evolution-and-indicator-seam.md)'s indicator seam.

## Context

VISION.md's loop steps 1–2 (RELEASE r5, "Why"): a trader cannot show an agent what a strategy
looks like without a chart. Today the app has no chart library and no chart data: the Backtest
Lab draws hand-made SVG (`EquityChart`, `ChartTwins` in `ui/src/screens/BacktestLabScreen.tsx`),
and nothing on any surface exposes a run's candles with its trades and the levels, pivots and
candle patterns its rule actually computed.

Two rulings fix the shape before any UI exists:

- **PDR-0002 — the engine computes; the chart draws.** Indicator, level, pivot and
  candle-pattern maths lives in Rust, once, in the adapters that already produce it for
  backtests and for MCP `export_indicators`. A second implementation in TypeScript would be a
  second answer, and the two would drift on the first tie-break anyone touched.
- **PDR-0004 — a third-party chart library behind an adapter of our own.** Lightweight Charts
  (with its TradingView attribution, grill Q6) is the plan of record; the cost of a library
  swap must be one directory, not the product.
- **PDR-0011 — an honest UI.** No zeros or placeholders as results: what the read does not
  draw must be named, and a holdout cut must show as withheld (PDR-0007 / ADR-0028).

The read that makes both true is the subject of this ADR: one request that names a pair and
timeframe, optionally a backtest run, and gets back everything the tab draws — with the
agent-readable values (SPINE G2) and no later bar than the one being drawn (G3).

## Decision

**The chart library is imported only by `ui/src/chart/`.** No screen, hook or component outside
that directory may import it, and no server type may name it. A swap (or a second renderer, or
the accessible table copy the spine's F2 requires) is therefore a change inside one directory.

**The UI draws only server values.** The Chart tab renders the DTO this read returns — candle
OHLCV, trade markers, overlay series and events — and computes no indicator, level, pivot,
pattern, position size or price of its own. What a chart cannot draw, it says it does not draw:
the read returns the rule's other operands by name.

**The read.** One app-scope (`Scope::App`) plain route, `POST /api/v1/chart`, and its bus
command `chart_data`, both straight through one transport-free core:

- **Request:** `{ pair, timeframe, fromMs?, toMs?, runId? }` — camelCase; `fromMs`/`toMs` are
  epoch-millisecond integer strings. The run is an optional LAYER (grill Q1), so the same read
  serves a run and a bare pair, which is what `r5.s3` needs to mark trades that have no run.
- **Response:** `{ pair, timeframe, dataVersion, candles[], holdout?, run? }` — times as
  millisecond integer strings, prices, values and R as exact decimal strings, counts as `u32`
  (specta refuses 64-bit integers). `dataVersion` names the snapshot the candles came from, so
  an agent can compare an overlay against `export_indicators` for the same data (G2).
- **`holdout` is `null` while no freeze is open**, else the holdout start and the number of
  candles the cut removed **inside the resolved window** — an explicit `fromMs`/`toMs`, else the
  run's own recorded window, else, for a read naming no run and no bounds, the whole pinned
  snapshot. The count is NOT narrowed by the 20,000-bar slice such a read returns: it answers
  "how many of this window's bars did the fence withhold" (r5.s2 close review). Every chart read
  cuts at an open freeze's holdout start BEFORE the engine steps (grill Q5), exactly as the agent
  exports do.
- **`run` is `null` when no run is named**, else `{ runId, versionId, series, trades, overlays,
  notDrawn }`. `series` is `primary` or `htf` — each overlay is computed on the timeframe its
  rule was compiled for (grill Q2), and trades ride both. `trades` are the run's stored rows
  whose **entry or exit fill falls in the interval the returned candles occupy** — the resolved
  bounds narrowed to the candles this read actually returns, not the requested window, so a
  trade no drawn candle can hold is never listed. On an HTF leg that interval stops at the last
  candle the run's **own last primary bar had closed**, and where the draw edge removes the
  whole tail the interval is empty and no trade is drawn. Each listed trade also **ends before
  an open freeze's holdout start**: a trade that reaches the fence is withheld whole, even when
  it entered before the cut, because its exit price, exit reason and realized R were produced
  inside the withheld period (r5.s2 close review; #423 repair in r5.s3.w1 — wording only;
  PDR-0007, Q5).
- **Overlays are keyed by the engine's own token labels** — `support:3-3-0.005-2-100`,
  `resistance:…`, `pivot_high:1-1`, `pivot_low:…`, `cdl:engulfing`, `ema:50` — the tokens MCP
  `export_indicators` accepts, produced by the token formatter beside its parser and proven by
  a round-trip test (`parse(label(spec)) == spec`). Each overlay carries a `kind`: `level` and
  `line` carry one value per window bar (`null` where the engine reads no value, so a level is
  drawn as it stood at each bar and never from a later bar, G3); `pivot` carries one event per
  confirmation with the confirmation bar, the price, and the peak bar `right` bars earlier
  (G3); `pattern` carries one event per bar whose value is non-zero (`±100` / `±80`).
- **The read is bounded.** A bounded window — an explicit one, or a run's own span — holding
  more than **20,000** candles is refused with a 422 naming the cap and the count, never
  silently truncated (grill Q3/G4). The plain pair read with no window takes the latest bars up
  to the cap. One request cannot ask the server for the whole store.
- **Refusals are typed and field-named:** an unknown `runId` is a 422 `not_found`; a `pair` or
  `timeframe` the run did not use is a 422 naming the field; an unreadable body is the existing
  bus `validation` refusal. A version whose stored DSL no longer compiles is a 422, never an
  empty chart.
- **No new maths, no DSL change, no new fake.** The core compiles the version's rule
  (`validate` + `compile`), takes its `required_indicators()` / `required_htf_indicators()`,
  loads the pinned series, cuts, and steps the SAME `IndicatorEngine` the backtester and
  `export_indicators` step — `from_specs`, one `current(...)` per bar, over the whole cut
  series before the window is kept.

## Consequences

- **A library swap touches `ui/src/chart/` only** — and a library that cannot draw a level
  series, an event marker or an accessible table copy is not adoptable without amending this
  ADR (the spine's F2 requires the table copy).
- **A new overlay kind is an ADR amendment,** not a quiet extension: the four kinds above are
  the contract the tab switches on. Oscillators (RSI, ADX, MACD, ATR) and the geometry beyond
  the per-bar series (necklines, trendlines, zone rectangles) stay OUT by ruling: they are
  named in `notDrawn`, not drawn (grill Q4; PDR-0011).
- **One token grammar, two directions.** The formatter (`indicator_label`) and the parser
  (`parse_indicator_specs`) are one module's pair, round-tripped by tests, so a new indicator
  that forgets its token fails a test rather than emitting a label no agent can use.
- **A pivot's confirmation is the engine's per-bar change.** A confirmation that repeats the
  held price leaves no observable change in the engine's public series, so it draws no event —
  a known limit recorded with the work item, and the reason a future "every confirmation"
  requirement would need an engine-level event port rather than a read-side re-derivation.
- **Costs:** the read re-loads the run's primary snapshot to resolve a defaulted window for an
  HTF series (one extra Parquet read per such request), and a chart read is bounded by the
  engine's own step cost over the cut series — measured well under the demo budget on the
  committed fixture.
- **Deny(warnings) and pedantic hold;** the module adds no dependency, and the read is
  read-only over the store, so no golden, pinned data version or determinism hash moves.

Touch surfaces for registration at spine close: `ui/src/chart/**`, `src/tauri/chart.rs`, the
`/api/v1/chart` route.
