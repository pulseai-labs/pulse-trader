# 24 (addendum) — Cipher building blocks under schema 1.5.0

Date: 2026-11-06T00:00:00Z

## Status

Accepted — an addendum to
[ADR-0024](0024-dsl-schema-evolution-and-indicator-seam.md) (DSL schema evolution
and the indicator seam), recorded by `r6.s1.w1` under that decision's
additive-variant rule. The parent decision's status, number and body are
unchanged; this file extends them.

## Context

Release r6 teaches the engine the Cipher indicators a trader reads on TradingView:
the WaveTrend oscillator, a ribbon of eight EMAs and the signals built on them.
The engine lacks three building blocks they need (PDR-0042's engine-gaps table):
a **simple moving average** (the Cipher's WaveTrend smooths with SMAs, and STR-0003's
design examples read an SMA 50), a way to take the **higher or lower of two
operands** (the ribbon's band top is the highest of eight EMAs), and the
**WaveTrend** itself. ADR-0024 fixes how the DSL may grow to hold them: a
**minor** bump, additive only, plus an identity migration that rewrites nothing
but the `schema_version` string. This addendum records the `1.5.0` bump it
governs.

## Decision

**The bump: `1.5.0`, additive, identity-migrated.** The registered chain is now
`1.0.0 → 1.1.0 → 1.2.0 → 1.3.0 → 1.4.0 → 1.5.0`, every step identity (each
`apply` rewrites only the `schema_version` string; `dsl_original` stays
verbatim). The served schema's version enum lists `1.0.0` through `1.5.0`; every
version-pinned test surface moved to the new `CURRENT` per the parent decision's
keep-green rule, and the tests that pin a historical version stay. No shipped
variant is renamed or reshaped, and every pre-bump document still loads and runs
with its meaning byte-identical.

**What 1.5.0 adds, in `r6.s1.w1`.** Two things, both additive:

| Addition | Shape | Value at bar t |
|---|---|---|
| `IndicatorSpec::Sma` | `{ period: SweepableValue<u32>, source: PriceField }`, `source` defaulting to `close` when absent (writes always emit it) | The arithmetic mean of `source` over the last `period` bars **including bar t**, rounded to 8 decimal places half-even. The first value lands on candle index `period − 1`. |
| `ArithOp::Max`, `ArithOp::Min` | serialized `"max"` / `"min"` | The larger / smaller of the two operands. Like every `Arith` node, no value when either operand has none. They cannot overflow. |

The wire form of the SMA is
`{"indicator":"Sma","period":50,"source":"Close"}`. `max` and `min` render as
calls, `max(a,b)`, the file's lowercase no-space convention, while the four
earlier operations keep their infix form.

**Why both are additive.** A new `IndicatorSpec` variant is an unused tag in every
existing document, so no pre-1.5.0 strategy can name it; a new `ArithOp` value is
likewise a string no existing document carries. The `source` default means a
hand-written `Sma` without a field still deserializes. Neither change touches the
meaning of a field that already exists, which is ADR-0024's bar for a minor bump.
The two share one bump because they ship in one item and one release; the engine
fingerprint moves once, with the schema string.

**`WaveTrend` joins under the same 1.5.0.** `r6.s1.w2` adds the `WaveTrend`
indicator variant **inside 1.5.0** — additive, as `r5.s1.w2` added the three-bar
patterns inside 1.4.0 — with **no second version step**: the wire form, the
served schema's version enum and the registered migration chain are unchanged by
w2, and no existing document's meaning moves. This addendum is the record for both
items; w2 extends it rather than opening another.

**The SMA arithmetic.** All `Decimal`, no `f64`: a window of the last `period`
source values and an exact running sum, so the sum never drifts. Only the final
division rounds (8 decimal places, half-even — the `convert` seam's indicator
scale). Every step is checked: an overflow returns no value and leaves the window
as it was (the EMA adapter's "convert before advancing" rule). `is_ready` follows
the `Indicator` port's contract — true iff the next call returns a value — like
the EMA and the rolling extremes, not "once the window is full".

**The ribbon's band top fits the depth limit.** The highest of eight EMAs is
`max(max(max(EMA5,EMA11),max(EMA15,EMA18)),max(max(EMA21,EMA24),max(EMA28,EMA34)))`:
three levels of `max`, inside `MAX_EXPRESSION_DEPTH = 4`. The explain surfaces
(the render line, the validation messages, the mark explanation) show it as the
nested call, so a trader sees the band top's value as one operand.

**Touch surface:** `src/domain/dsl/**`, `src/adapters/indicators/**`, and the
composer's hand-written tool schema and the MCP schema resource
(`src/agent/tools.rs`, `src/mcp/resources.rs` — ADR-0012, ADR-0013). Not
`src/domain/backtest/**`: no risk-gate surface moves.

**Revisit triggers:** an SMA of a non-price operand (of `Range`, `Body` or of
another indicator — a different shape, not a new field here); exponential
weighting, offsets or smoothing options on the SMA (additive, out of scope); or
any non-additive grammar change, exactly as the parent decision lists them.

## Consequences

A strategy can now write a simple moving average of any price field and combine
two operands with `max` or `min`, so the Cipher ribbon's band top is one operand
whose value explain can show. The engine fingerprint moves with the schema
string (ADR-0024's "pure schema bump changes the fingerprint while changing no
behaviour recoverable from it" is the honest answer) while every existing
strategy's `result_content_hash` and trade log stay identical: variants no
existing document names cannot change a number, and the `1.4.0 → 1.5.0` step
rewrites only the version string.

The cost of the next building block stays inside ADR-0024's seam list. The SMA
needed one enum variant and its arms in each explicit `IndicatorSpec` match (the
engine factory, validation, render, mutation, the MCP token parser and label, the
composer tool schema and the proptest generators); none of those matches has a
wildcard, so the compiler listed every site. A block that does not fit that list
remains a revisit trigger rather than a licence to widen the seam mid-item.
