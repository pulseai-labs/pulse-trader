# 27 (addendum) — The paper P&L read model

Date: 2026-10-11T00:00:00Z

## Status

Accepted — an addendum to
[ADR-0027](0027-paper-session-event-log.md) (paper session event log; state is
the log replayed), recorded by `r4.s4.w1`. The parent decision's status, number
and body are unchanged; this file extends them.

## Context

The only paper trade in r3 (`e962163a`'s long) was invisible to the operator:
the session view showed closed trades with R only — no USDT, no marked open
position, no running total and no entry fills (#344, #340, #341). The spine
r4.s4's read side is where the money has to come from, and the question it had
to answer is *where a USDT figure lives*. The two options were recording USDT
on new exit fills or deriving it from the log; recording would give the log two
money paths (what was recorded and what is derived) and would leave every
existing trade — the prod session's included — without a figure (spine grill
G1).

## Decision

**The read side derives every USDT figure from the log, at the session's own
`taker_fee_bps`.** No paper event gains a money field, no migration runs, and
the log's shape is unchanged. The derivation lives in one pure domain module
(`domain/paper/pnl.rs`) and uses the engine's own cost functions
(`realized_pnl`, `taker_fee`), so a derived figure equals the engine's
`Trade.realized_pnl` by construction — the spine's money rule and its test pin
that equality exactly, with no tolerance. A closed trade whose log has no entry
fill (w3-era payloads) has no USDT figure (`None`), and the totals carry
counted / total counts ("n of m") so a 0 sum is never read as data.

**Funding is attributed by step.** The log's step boundary is the
`bar_processed` event; a step is that bar and the events after it up to the
next one. A trade's funding is the sum of the `Funding` events of the steps
where the position was open **before** the step — from the step after its entry
fill's step through the step of its exit fill — which is exactly the engine's
`(entry_fill_time, exit_fill_time]` accrual window: the runtime writes the
bar's `Funding` event last, so a bar that closes the position still accrues its
funding to that trade. The open position's unrealized figure holds its funding
so far and its entry fee only; the exit fee and exit slippage are not charged
until the exit (grill Q1).

**The mark is the last processed primary bar.** The open position is marked at
the close and close time of the newest `paper_bar` row for the session's
primary timeframe — one row read, never the whole table. A session that is
stopped marks at its last processed bar, the same rule. Unrealized USDT is
`dir × qty × (mark − entry) + funding so far − entry fee`; unrealized R is the
engine's `realized_r(entry, mark, initial_stop, dir)`.

**The live stops come from the running engine, and are absent when it does not
answer.** The log does not record the open position's initial stop, current
stop or take-profit, so the read asks the runtime through a read-only control
command (`PaperCommand::Position`) under a short, named timeout
(`POSITION_REPLY_TIMEOUT_MS = 2_000`, in `application/paper_control.rs`). On a
timeout, with no runtime installed, or with the session not attached, the read
answers without them — the USDT figures stay available and the read never fails
because of the stops (grill Q3). The engine's only surface added is one
read-only accessor, `EngineSession::open_position_stops`.

**The wire shape is additive.** The session summary gains the running totals
(`realized_usdt_total`/`realized_usdt_count`, `realized_r_total`/
`realized_r_count`, `unrealized_usdt`, `session_pnl_usdt`) and a marked open
position (mark price and time, funding so far, entry fee, unrealized USDT and
R, and the three stops); the trades read gains `entries` (every order and fill
in log order, entry fills included, #344) and each closed trade's
`realized_usdt`, `funding_usdt`, `entry_fee_usdt` and `exit_fee_usdt`. Every
existing field keeps its name and meaning; money stays exact decimal text
(NFR-2, grill G7). The routes (`src/server/paper.rs`) and the MCP paper reads
share `application::paper_read`, so both carry the fields unchanged (ADR-0026,
ADR-0012/0015, G5/G6); the `/mcp` mount `pulse serve` builds sits beside the
runtime and passes it the server's control handle, while the stdio `pulse mcp`
process holds no runtime and answers without the stops.

## Consequences

- Reading a session derives the money every time; nothing about the write path,
  the event vocabulary or the engine's trading behaviour changes, and the
  determinism fixture, replay golden and content hash stay byte-identical.
- The UI computes no money (ADR-0020): it renders the server's text, and
  r4.s4.w2 mirrors these fields in `src/tauri/paper.rs` and the session view.
- A read may answer with stops absent (no runtime) — that is the documented
  degraded shape, not an error; the UI shows the USDT figures either way.
- Human-unit formatting is r4.s5's; this addendum's figures are exact decimal
  text (grill G7).
