# 24 (addendum) — Named candlestick patterns under schema 1.4.0

Date: 2026-10-14T00:00:00Z

## Status

Accepted — an addendum to
[ADR-0024](0024-dsl-schema-evolution-and-indicator-seam.md) (DSL schema evolution
and the indicator seam), recorded by `r5.s1.w1` under that decision's
additive-variant rule. The parent decision's status, number and body are
unchanged; this file extends them.

## Context

The r4.s6 work gave the DSL swing structure, support/resistance, candle anatomy
and price patterns, but no way to name a **candlestick pattern**. The operator's
setups read a rejection candle at a 4h support or resistance zone
(`docs/decisions/TRADER.md:17`), which the anatomy variants express only as raw
body and wick measurements — every pattern would have to be re-derived as a
hand-written set of comparisons, and no two derivations would agree. ADR-0024
fixes how a bump may close that gap: a **minor** bump, additive only, plus an
identity migration that rewrites nothing but the `schema_version` string. This
addendum records the `1.4.0` bump it governs, and it answers the revisit trigger
the r4.s6 addendum left open.

## Decision

**The bump: `1.4.0`, additive, identity-migrated.** The registered chain is now
`1.0.0 → 1.1.0 → 1.2.0 → 1.3.0 → 1.4.0`, every step identity (each `apply`
rewrites only the `schema_version` string; `dsl_original` stays verbatim). The
served schema's version enum lists `1.0.0`, `1.1.0`, `1.2.0`, `1.3.0` and
`1.4.0`; every version-pinned test surface moved to the new `CURRENT` per the
parent decision's keep-green rule. No shipped variant is renamed or reshaped,
and every pre-bump document still loads and runs with its meaning
byte-identical.

**It answers the 1.3.0 addendum's revisit trigger — and the answer is "a scalar,
not a boolean".** That addendum listed "a boolean pattern signal" as a revisit
trigger. A candlestick pattern is recorded here as an `IndicatorSpec` variant
whose value is **TA-Lib's own integer**, read through the existing `Compare` /
`CrossesAbove` / `CrossesBelow` comparisons and the existing `primary` / `htf` /
`d1` `series` operand. **No boolean type and no `Condition` kind is added**, and
the seam stays "indicators, not conditions".

**The new variant (schema 1.4.0).** One variant, with no sweepable parameter:

| Variant | Parameters | Value at bar t |
|---|---|---|
| `CandlePattern` | `pattern: CandlePatternName` | TA-Lib's own integer for that named pattern: `+100` bullish, `-100` bearish, `0` no pattern, and `±80` for the **partial** Engulfing and Harami forms |

`CandlePatternName` is a serde enum serialized **by variant name** (no
`rename_all`), so the wire form is
`{"indicator":"CandlePattern","pattern":"Engulfing"}` — the same spelling
discipline as `PriceField` and the `CandlePattern` tag itself. Its values are
the spine's full **17** named patterns: the one-bar `Doji`, `DragonflyDoji`,
`GravestoneDoji`, `Hammer`, `InvertedHammer`, `HangingMan`, `ShootingStar`,
`Marubozu` and `SpinningTop`; the two-bar `Engulfing`, `Harami`, `Piercing` and
`DarkCloudCover`; and the three-bar `MorningStar`, `EveningStar`,
`ThreeWhiteSoldiers` and `ThreeBlackCrows`. `r5.s1.w1` shipped the first 13 and
`r5.s1.w2` added the four three-bar values **inside 1.4.0** — additive, as
`r4.s6.w3` was inside 1.3.0, with **no second version step**: the wire form, the
served schema's version enum and the registered migration chain are unchanged by
w2, and no existing document's meaning moves.

**The `±80` partial forms, and why they are not folded into `±100`.** TA-Lib
emits `±80` where a body end of the second candle coincides with the prior
body's edge, which on continuous crypto bars (a bar opens at the prior close) is
the common case: on the committed BTCUSDT 15m fixture, `CDLENGULFING` reads
`±80` 401 times against `±100` 161 times. The distinction is kept, so a rule that
wants "a bullish engulfing fired" reads `Gt 0` and one that wants only the full
form reads `Eq 100`. Folding the two together would lose that and bend the
reference-match rule.

**Warm-up.** Each pattern has a fixed **lookback** `L` — TA-Lib's own
`Function(name).lookback` with the default settings. For the 0-based bar index
`i < L` the variant yields no value; from `i >= L` it yields the pattern's
integer on **every** bar and never a missing value again. `L` is used exactly as
TA-Lib declares it, including where it is one bar more than the conditions need
(the hammer family, `L = 11`) or where the conditions need only one prior bar
(engulfing, `L = 2`). Because the value is always present after warm-up, the
variant is deliberately **not** one of the engine's level/pattern carve-out
variants: the ordinary all-`Some` warm rule applies to it.

**The clean-room rule (PDR-0003).** The patterns were implemented from a
plain-words specification of TA-Lib's behaviour and from TA-Lib's own output,
committed as reference CSVs. **No TA-Lib source code — C, the Python wrapper's
internals, or any port of it — was read, fetched, copied or transcribed** while
building this variant, and none exists in this repository. The generator scripts
that produced the CSVs call the TA-Lib Python wrapper as a black box, which
PDR-0003 allows.

**The reference pin.** TA-Lib Python **0.6.8** / TA-Lib C **0.6.4**. On the
committed BTCUSDT fixture series (15m, 2,976 bars; 4h, 186 bars) and three
hand-built sequences, an exact-decimal implementation of the plain-words
definitions matches TA-Lib on **every bar of all 17 patterns**, with no tie
deciding any output; the parity suite therefore carries an **empty** exception
list. `Piercing`, `DarkCloudCover` and `ThreeBlackCrows` never fire on the
fixture bars (crypto perpetual bars open at the prior close, so the strict
real-body gap and "opens inside the prior body" tests reject every bar), so each
is read from a hand-built sequence of its own; the rest reach the three-occurrence
floor on the reference series.

**The candle ring.** All 17 variants read one shared ring of the last
`RING_LEN = 13` bars, plus the TA-Lib candle settings' trailing averages
(`BodyLong`, `BodyShort`, `BodyDoji`, `ShadowLong`, `ShadowVeryShort`, `Near`,
and `Far`, which only `ThreeWhiteSoldiers` needs). One evaluator function of the
ring per pattern, so w2's four three-bar patterns added four evaluators and four
enum values and **no** new `IndicatorSpec` variant, engine, validation, mutation
or formatter arm — the one new shared constant is `Far`. The deepest window any
of the 17 reads is bar `t-12` (offset 12, inside `RING_LEN = 13`): the stars'
`avg(BodyLong, t-2)` and the crows' `avg(ShadowVeryShort, t-2)` both end there.
The averages are evaluated as `N × x ⋛ f × sum` (the definitions' §2.2
exact-decimal form) so no division enters a comparison, and the stars' fixed
`0.3` penetration is the exact `3 × body / 10`.

**No repaint, no look-ahead.** A pattern's value at bar t reads only bars at or
before t, so the values on a series truncated at any bar equal the full series'
values over the overlap. The `htf` operand keeps the existing alignment rule: an
`htf` operand is evaluated against the most recent *closed* higher-timeframe bar,
which needs no new code for this variant.

**Touch surface:** `src/domain/dsl/**`, `src/adapters/indicators/**`.

**Revisit triggers:** a pattern that needs a parameter (TA-Lib's penetration or
candle-setting periods — additive, and deliberately out of scope here); a
pattern with no TA-Lib definition, or one whose definition this repository
cannot state in plain words; or any non-additive grammar change, exactly as the
parent decision lists them.

## Consequences

A strategy can now name a candlestick pattern and read it through the
comparisons and operands that already existed, so "a bullish engulfing fired at
the 4h support zone" is expressible without widening the evaluator — the
combination the journey's rule needs. The engine fingerprint moves with the
schema string (ADR-0024's "pure schema bump changes the fingerprint while
changing no behaviour recoverable from it" is the honest answer) while every
existing strategy's `result_content_hash` and trade log stay identical: a
variant that no existing document names cannot change a single number, and the
`1.3.0 → 1.4.0` step rewrites only the version string. The frozen pre-bump
goldens assert that.

The cost of the next candle pattern stays inside ADR-0024's seam list — one
enum value, one evaluator, plus the arms a new *variant* would need (which 1.4.0
already carries) — and a pattern that cannot be expressed inside that list
remains a revisit trigger rather than a licence to widen the seam mid-item.
