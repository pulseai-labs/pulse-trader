# 24 (addendum) — Structure indicators under schema 1.3.0

Date: 2026-10-10T00:00:00Z

## Status

Accepted — an addendum to
[ADR-0024](0024-dsl-schema-evolution-and-indicator-seam.md) (DSL schema evolution
and the indicator seam), recorded by `r4.s6.w1` under that decision's
additive-variant rule. The parent decision's status, number and body are
unchanged; this file extends them.

## Context

The r4.s1 campaign's read-out named the DSL as the bottleneck: it could not
express swing structure, support/resistance or candle anatomy. Both an operator
walking four coins by hand and an agent composing through the builder tools need
those constructs, and they need them without a new condition kind, without a
change to any shipped variant, and without any existing strategy's output
moving. ADR-0024 fixes how a bump may do that: a **minor** bump, additive only,
plus an identity migration that rewrites nothing but the `schema_version`
string. This addendum records the `1.3.0` bump it governs.

## Decision

**The bump: `1.3.0`, additive, identity-migrated.** The registered chain is now
`1.0.0 → 1.1.0 → 1.2.0 → 1.3.0`, every step identity (each `apply` rewrites
only the `schema_version` string; `dsl_original` stays verbatim). The served
schema's version enum lists `1.0.0`, `1.1.0`, `1.2.0` and `1.3.0`; every
version-pinned test surface moved to the new `CURRENT` per the parent decision's
keep-green rule. No shipped variant is renamed or reshaped, and every pre-bump
document still loads and runs with its meaning byte-identical.

**The seam stays "indicators, not conditions" (operator ruling, Q1).** The eight
new variants are `IndicatorSpec` variants — price-level (or price-distance)
indicators that reuse the existing `Compare` / `CrossesAbove` / `CrossesBelow`
comparisons and the existing `primary` / `htf` / `d1` `series` operand. No
`Condition` kind is added, no boolean signal type appears, and a breakout reads
`CrossesAbove(close, htf Resistance{…})` through machinery that already exists.

**The new variants (schema 1.3.0).** All are `None` until defined; the four
level/pivot variants take `SweepableValue<u32>` parameters, `tolerance_pct` is a
`SweepableValue<Decimal>` under the DSL's decimal-fraction convention for every
`_pct` field (`0.003` means 0.3%, the `distance_pct` precedent), and the four
anatomy variants take no parameters.

| Variant | Parameters | Value at bar t |
|---|---|---|
| `PivotHigh` | `left`, `right` | The high of the most recent bar p with `p + right ≤ t` whose high is strictly greater than the `left` bars before it and greater-or-equal to the `right` bars after it |
| `PivotLow` | `left`, `right` | Mirror of `PivotHigh` on lows |
| `Resistance` | `left`, `right`, `tolerance_pct`, `min_touches`, `lookback` | Among confirmed pivot highs within the last `lookback` bars, the lowest price level above the current close that at least `min_touches` pivot highs touched within `tolerance_pct` of each other; the level is the mean of its touching pivots |
| `Support` | `left`, `right`, `tolerance_pct`, `min_touches`, `lookback` | Mirror on pivot lows: the highest level below the current close |
| `Body` | — | `abs(close − open)` of the current bar |
| `UpperWick` | — | `high − max(open, close)` |
| `LowerWick` | — | `min(open, close) − low` |
| `Range` | — | `high − low` |

Equality in the pivot rule is deliberate: strictly greater on the left,
greater-or-equal on the right. A run of equal highs therefore resolves to its
first bar, and every later equal bar fails the strict-left test.

**No repaint, no look-ahead.** A structure indicator's value at bar t never
depends on bar t+1 or on any later bar. A swing pivot is confirmed only once its
`right` bars have closed — a pivot whose right bars are still open does not
exist for any value — so the earliest visible pivot value at bar t is the one
confirmed at exactly `p + right = t`. Each structure indicator is proved against
a truncation property: its values on a series truncated at any bar equal its
values on the full series over the overlap. The `htf` operand keeps the existing
alignment rule: an `htf` operand is evaluated against the most recent *closed*
higher-timeframe bar.

**Validation bounds.** `left`, `right` ≥ 1; `min_touches` ≥ 2; `lookback` >
`left + right`; `tolerance_pct` > 0 and ≤ 0.10 (a decimal fraction, so 0.10 is
10%). An out-of-range fixed parameter refuses with the engine's typed
`InvalidPeriod`, never a silent clamp.

**Touch surface:** `src/domain/dsl/**`, `src/adapters/indicators/**`.

**Revisit triggers:** a structure construct that cannot be expressed as a
price-level `Indicator` variant (a boolean pattern signal, a state machine); a
second higher timeframe; or any non-additive grammar change, exactly as the
parent decision lists them.

## Consequences

A strategy can now read swing structure, support/resistance and candle anatomy
through the comparisons and operands that already existed, so a breakout of a
4h resistance zone built from swing highs is expressible without widening the
evaluator. The engine fingerprint moves with the schema string (ADR-0024's
"pure schema bump changes the fingerprint while changing no behaviour recoverable
from it" is the honest answer) while every existing strategy's `result_content_hash`
and trade log stay identical — asserted by the frozen pre-bump goldens.

The pivot tracker behind `PivotHigh` / `PivotLow` is shared with the level
indicators and is reusable by the pattern variants (necklines and trendlines)
that the next work item builds, so the confirmation rule has exactly one
implementation. The cost of the next structure indicator stays inside ADR-0024's
seam list — one variant, one engine arm, one validation arm, one mutation arm,
one formatter arm, one read-path arm — and an indicator that cannot be expressed
inside that list remains a revisit trigger rather than a licence to widen the
seam mid-item.
