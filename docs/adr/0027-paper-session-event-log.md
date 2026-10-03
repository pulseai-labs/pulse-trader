# 27. The paper session: a row plus an append-only event log, graduated two ways

Date: 2026-10-02

## Status

Proposed

Refines [ADR-0025](0025-walk-forward-run-kind-and-certification-seam.md)'s
certification seam into the paper-trading gate, and rides [ADR-0026](0026-client-server-split.md)'s
always-on server (the session's runtime home). The spine close flips this to
Accepted.

## Context

The spine plan (`r3.s4`) adds paper trading: a strategy version graduates from
certification to a LIVE paper session that consumes bars, fills orders, accrues
funding and is shadow-checked against its certification. Two facts shape the
design. First, the gate must stay honest: a passing walk-forward under engine
`A` is not a certification under engine `B`, a human override is not a gate
pass at all, and data certified on synthetic fixture data is not
out-of-sample-comparable. Second, the session's state must be reconstructible:
the always-on server restarts (systemd), and a session whose state lived only
in memory would be a lie after every restart.

The engine fingerprint already has a build-time identity (FR-7); the walk-forward
run kind (ADR-0025) already records which fingerprint certified a version; and
the snapshot store already gives every candle series a content-hash
`data_version` (ADR-0009). What does not exist yet is the session itself —
the row that says "this version trades paper, this is how it graduated", and
the log of everything that happened since.

## Decision

A live paper session is ONE immutable `paper_session` row plus an append-only
`paper_event` log; the state is the replay of the log
(`PaperSessionState::replay`), never a second mutable projection. The row is
written once, at promotion; the log appends and never rewrites (`0018`'s
triggers enforce both in the schema, and a `stop` event makes the log
read-only).

**The two graduation variants** are the row's `graduation` sum, and each names
exactly its own columns:

- `certified` — the promotion's certifying walk-forward run passed on THIS
  build (E2, a refinement of ADR-0025's seam: a run whose recorded engine
  fingerprint differs from the current build's refuses, with or without an
  override — re-running the walk-forward is the only re-certification). The
  row names the run AND the exact `(timeframe, data_version)` pairs its fold
  runs consumed, in fold order: certification names its data versions.
- `override` — a human's reasoned manual promotion (non-empty reason, the
  instant it was taken, the promoting client token's label). An override is
  never a live basis (A5): it starts a session for shadow-listing, and the
  recorded fact that it was an override travels with the row forever.

**Fixture certification is visible, not hidden.** A certified session whose
every named data version has a `fixture_snapshot` row is `fixture = true` and
never OOS-comparable (A12): the certification is real, the data is synthetic,
and w4 renders "OOS comparison: n/a, certified on fixture data" instead of an
OOS verdict.

**Epochs per fingerprint (E3).** The session starts in its row's engine
fingerprint; every accepted `engine_upgraded` event opens a new epoch. The
event's `old` must equal the epoch the session actually runs in, or replay
refuses — an upgrade log that lied about its starting point would launder a
foreign fingerprint back into the session's history.

**Recorded bars are `paper_bar` rows, one per consumed bar per timeframe**
(A6, as the pre-launch audit corrected). The candles the shadow check
materialises are content-addressed snapshot files — written only at a shadow
check, never at bar time — and each carries the `data_version` the
`shadow_checked` event records. Bar consumption is transactional: a bar's rows
and its events land in one `append_bar` batch or not at all (audit #4).

**Live reconciliation is recorded here, not built here.** The aggregate
defines the types (the event vocabulary, the replay, the atomic append); the
live path that consumes real bars over REST polling (E1, amending design
§4.1's push assumption: the server polls Binance's REST API for klines rather
than holding a websocket), fills fills, and schedules shadow checks is w3's
work, and the API surface is w4's.

## Consequences

- A restart reconstructs every session exactly: state is replay, and the log
  is the only source. The cost is replay time proportional to log length —
  bounded by one M15 bar's worth of events per session per 15 minutes.
- The gate's honesty is enforceable at the schema: the graduation CHECKs make
  an override without a reason, or a certified promotion without its run, an
  `INSERT` error rather than a code-review note.
- `pulse fixture seed` keeps the fixture-certified path a REAL gate pass: the
  synthetic series rides the same walk-forward, the same promotion gate and
  the same E2 refusal as market data, so a fixture session demonstrates the
  graduation path end to end on every build without ever claiming an edge.
- Sessions are per version with the A1 settings (10,000 USDT, 4 bps taker,
  1 bps slippage, `min_trades = 20`); several sessions may shadow the same
  version.
- What this ADR deliberately does NOT decide: the shadow backtest's scheduling
  policy (w3), the REST API's shapes (w4), and the UI rendering (w5).

## Revisit triggers

This decision is revisited when any of the following lands: live execution
(the paper log becomes the live log, and the stop wall's semantics face real
money), a second pair (the fixture stamp and the `pair` columns grow a second
identity), or a websocket feed (REST polling's amend to design §4.1 is
re-amended, and `data_event`'s payload grows the reconnect vocabulary).
