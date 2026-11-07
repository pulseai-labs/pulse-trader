# 24 (addendum) — Previous pivots under schema 1.7.0

Date: 2026-11-07T00:00:00Z

## Status

Accepted — an addendum to
[ADR-0024](0024-dsl-schema-evolution-and-indicator-seam.md) (DSL schema evolution
and the indicator seam), recorded by `r6.s3.w2` under that decision's
additive-variant rule. The parent decision's status, number and body are
unchanged; this file extends them. It also rides the structure addendum
([1.3.0](0024-addendum-structure-indicators-1-3-0.md)), which it does not
revisit.

## Context

STR-0003 v2 refuses to arm where the 4H swings do not trend. Its rule reads two
swings at once: "of the last two confirmed 4H swing highs, the later is higher",
and the same on lows (PDR-0046, decision 3). The engine's `PivotHigh` and
`PivotLow` give only the **latest** confirmed swing, so a rule cannot compare it
with the one before. This addendum records the `1.7.0` bump that adds the swing
before the latest one, under ADR-0024's bar for a minor bump: additive only, plus
an identity migration that rewrites nothing but the `schema_version` string.

## Decision

**The bump: `1.7.0`, additive, identity-migrated.** The registered chain is now
`1.0.0 → 1.1.0 → … → 1.6.0 → 1.7.0`, every step identity (each `apply` rewrites
only the `schema_version` string; `dsl_original` stays verbatim). The served
schema's version enum lists `1.0.0` through `1.7.0`; every version-pinned test
surface moved to the new `CURRENT` per the parent decision's keep-green rule, and
the tests that pin a historical version stay. No shipped variant is renamed or
reshaped, and every pre-bump document still loads and runs with its meaning
byte-identical.

**What 1.7.0 adds, in `r6.s3.w2`.** Two indicator variants:

| Addition | Shape | Value at bar t |
|---|---|---|
| `IndicatorSpec::PreviousPivotHigh` | `{ left: SweepableValue<u32>, right: SweepableValue<u32> }` | The high of the confirmed swing high **before** the most recent confirmed one. `None` until two pivots are confirmed. |
| `IndicatorSpec::PreviousPivotLow` | `{ left, right }`, the mirror on lows | The low of the confirmed swing low before the most recent confirmed one. `None` until two are confirmed. |

The wire forms are `{"indicator":"PreviousPivotHigh","left":3,"right":3}` and
`{"indicator":"PreviousPivotLow",…}`; they render `previous pivot high(3/3)` and
`previous pivot low(3/3)`; the `export_indicators` and explain token is
`prev_pivot_high:<left>-<right>` / `prev_pivot_low:<left>-<right>`; the composer's
tool schema takes `"indicator": "prev_pivot_high"` / `"prev_pivot_low"` with
`left` and `right`; the mutation leaves are `previous_pivot_high.left` /
`.right` and the low twins. With `PivotHigh` and `PivotLow` they say STR-0003 v2's
rule as `PivotHigh > PreviousPivotHigh` (the later of the last two swing highs is
higher) and `PivotLow > PreviousPivotLow`.

**Both are price-level indicators, inside the 1.3.0 addendum's rule.** Like the
eight structure variants, they are `IndicatorSpec` variants that return a price,
not conditions (that addendum's Q1 ruling). The rule is not revisited: a trend
test is a `Compare` of two price levels, and no new condition kind or state
machine is needed. The 1.3.0 no-repaint rule holds too: a value at bar t never
depends on a bar after t, and `tests/previous_pivots.rs` carries the truncation
test that every structure variant owes.

**Confirmation.** A pivot exists for no value until bar `p + right` has closed,
exactly as for `PivotHigh`. The previous pivot reads the same tracker as the
latest one, so it changes on the same bar the latest changes: when a new pivot
confirms, the old latest becomes the previous value on that bar, and not before.
On an `h4:` operand it changes only on the primary bar where the confirming 4H
bar closed.

**Equal prices are two pivots.** The pivot rule already refuses to call a run of
equal extremes more than one pivot (a tie resolves to its first bar). Two
**separate** confirmed pivots at one price are two pivots: the previous value
takes the old price even when it equals the new one. A rule that compares them
sees equality, not a missing value.

**One adapter, a read selector.** `PivotIndicator` keeps the latest and the
previous confirmed price and reads one of them by `PivotRead`. `is_ready` is true
iff the selected price exists, so a previous-pivot slot stays cold until a second
pivot confirms. No second type and no second tracker exist.

**STR-0003 v2 is the first user.** Cipher v2 (`r6.s3.w3`) uses both variants for
its 4H swing-trend rule.

**Not drawn.** The chart tab does not draw the previous pivots; ADR-0030's
overlay read lists them as "not drawn" until `r6.s4` decides how a previous pivot
is drawn (its peak is an older bar than the confirmation bar's `right` offset
points to).

**Touch surface:** `src/domain/dsl/**`, `src/adapters/indicators/**`, the
application read path (`src/application/mcp_read.rs`), and the composer's
hand-written tool schema and the MCP schema resource (`src/agent/tools.rs`,
`src/mcp/resources.rs` — ADR-0012, ADR-0013). Not `src/domain/backtest/**`: no
risk-gate surface moves.

**Revisit triggers:** an n-th swing operand beyond the previous one (a different
shape, not a new field here); drawing a previous pivot on the chart (`r6.s4`); or
any non-additive grammar change, exactly as the parent decision lists them.

## Consequences

A rule can now compare the last two confirmed swings, so STR-0003 v2's trend test
is one `Compare` whose two prices explain shows (`pivot_high:3-3` and
`prev_pivot_high:3-3`). The engine fingerprint moves with the schema string
(ADR-0024's "pure schema bump changes the fingerprint while changing no behaviour
recoverable from it" is the honest answer) while every existing strategy's
`result_content_hash` and trade log stay identical: variants no existing document
names cannot change a number, and the `1.6.0 → 1.7.0` step rewrites only the
version string.

The cost stayed inside ADR-0024's seam list: two enum variants and their arms in
each explicit `IndicatorSpec` match (the engine factory, validation, render,
mutation, the MCP token parser and label, the composer tool schema and the
proptest generators). The chart's overlay read matches with a wildcard, so the
compiler did not list it; `drawn_kind` leaves the two variants undrawn on purpose.
