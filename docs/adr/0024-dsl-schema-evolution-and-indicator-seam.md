# 24. DSL schema evolution and the indicator seam

Date: 2026-09-19T00:00:00Z

## Status

Accepted

## Context

The strategy DSL is a persisted, versioned grammar: every `StrategyVersion`
stores a `schema_version`, the verbatim source (`dsl_original`), and the typed
document. Issues #160 and #163 showed the `1.0.0` grammar could not express two
constructs the operator's setups need — a condition operand evaluated on the
higher-timeframe series, and a stop placed at a multiple of ATR. Closing them
required the first-ever schema revision, which forces the project to say how
the grammar is allowed to evolve at all: what a bump may change, what it must
preserve, and what it costs to add the *next* indicator. Without a recorded
rule, each of those questions is re-litigated per item, and the seams that make
a bump safe (migration registry, exhaustive match sites, the machine-served
JSON Schema) are discovered by breakage rather than declared up front.

## Decision

**The DSL schema follows ADR-0018's forward-only discipline.** A *minor* bump
(`1.0.0` → `1.1.0`) is additive only: a new enum variant, or a new field that
deserializes to its default when absent — plus an identity migration that
rewrites nothing but the `schema_version` string, so every persisted document
loads with its meaning byte-identical. A *major* bump may rewrite documents,
but only through a migration `apply` registered on `Migrator::v1()`.
`dsl_original` is never re-serialized: it stays the verbatim bytes the version
was created from. `SchemaVersion::CURRENT` is the single source of the current
version, emitted through `schema_version_const.rs` (a bare `pub const`,
`include!`d by `build.rs`) and folded into the engine fingerprint — a pure
schema bump therefore changes the fingerprint while changing no behaviour,
which is the honest answer.

**Adding an indicator is a registered seam, not an expedition.** One new
indicator costs exactly: one `IndicatorSpec` variant, one `build_indicator`
arm in the indicator engine, one `check_indicator` arm in validation, one
`visit_value_source` arm in the mutation traversal, one formatter arm in each
renderer, one cross-validation column, and one determinism snapshot field —
and nothing else. An indicator that cannot be expressed inside that list is a
revisit trigger, not a licence to widen the seam mid-item.

**Operands are series-scoped.** `ValueSource::Price` and
`ValueSource::Indicator` carry a `series` tag (`primary` | `htf`, defaulting
to `primary` so every `1.0.0` document reads as primary-series). An `htf`
operand is evaluated against the aligned, already-closed higher-timeframe bar
— the most recent HTF candle whose `close_time` is at or before the primary
bar's, and no pairing at all before the first HTF close — so a run never
reads an HTF bar that has not finished. `IndicatorSpec::Atr` is computed by
the indicator engine (the streaming Wilder adapter), and `ExitRule::AtrStop`
derives its stop from `multiple × ATR(period)` **frozen at the signal bar**.
`compile()` is pure and never refuses a strategy merely because an `htf`
operand is present: a run that needs the higher timeframe but is invoked
without an HTF input fails as the typed backtest input error `HtfRequired`,
not as a compile-time rejection.

**Touch surface:** `src/domain/dsl/**`, `src/adapters/indicators/**`.

**Revisit triggers:** a second higher timeframe; a non-additive grammar change
(a field removal, a variant rewrite, a semantic change to an existing leaf);
or an indicator that cannot be expressed as one streamed `Indicator` impl
inside the seam above.

Registered in the bones registry at spine close (`oss bone_add`).

## Consequences

Every persisted `1.0.0` document loads unchanged — identity migration, no
re-serialization, no backfill — so the version store keeps ADR-0018's
correction-is-a-new-version guarantee without a rewrite path. The next
indicator is a one-item flesh job: the seam enumerates its entire touch
surface, and the schema-1.1.0 item demonstrates it end to end (`Atr` landed
with exactly the arms the seam declares). The schema/engine boundary stays
explicit: every construct the 1.1.0 grammar admits is computable, and the
boundary errors that remain are typed and asserted — `HtfRequired` for a run
missing its HTF input, `HtfNotHigher` for a timeframe selection that is not
strictly higher, `UnsupportedExit` for an exit kind the backtester does not
model — never `todo!`/`unimplemented!` or a silent fallback. The transitional
state in which valid documents did not compile is closed: the grammar and the
engine agree, and the revisit triggers bound how far the grammar may drift
before this decision must be re-opened.

## Amendment (r3.s2, 2026-09-27)

Recorded by r3.s2.w1 when the pinned slot-and-fix decision (d1) forced the
minor bump this decision's rules govern. Three changes land under the SAME
decision; the status, number and original body above are unchanged.

**The bump: `1.2.0`, additive, identity-migrated.** The registered chain is
now `1.0.0 → 1.1.0 → 1.2.0`, both steps identity (each `apply` rewrites only
the `schema_version` string; `dsl_original` stays verbatim). The served
schema's version enum lists `1.0.0`, `1.1.0` and `1.2.0`; every
version-pinned test surface moved to the new `CURRENT` per this decision's
keep-green rule. Every pre-r3 document still runs byte-identically: the
frozen pre-bump `result_content_hash` + full trade log for the fixture
documents (including a MACD-carrying document) are asserted unchanged by
`tests/dsl_schema_1_2.rs`.

**The revisit trigger FIRED, and the `d1` slot is fixed (Q3).** This decision
named "a second higher timeframe" among its revisit triggers; r3.s2's spine
(daily regime gate, w4) is exactly that trigger, so this amendment IS the
revisit. The slot for the new constructs is `d1` — the **builder-tool
grammar**, not the composer tools — pinned rather than freely re-chosen:
a free second choice would put a request argument on every surface for
timeframes no strategy asks for yet. Rounds 2 and 3 build inside that slot.

**The new constructs, as the contract rounds 2 and 3 build to (Q1, Q2).**
`Highest`/`Lowest` cover the PRIOR N closed bars EXCLUDING the current bar
(the Donchian convention): warm-up N+1; `source` any price field, defaulting
to `high` for `Highest` and `low` for `Lowest`; working on any series.
`Arith{op ∈ add|sub|mul|div, lhs, rhs}` is a BINARY operation with two sides:
division by zero, or any operand without a value, gives NO value (the leaf
compares false, exactly as a warming indicator does); arithmetic is `Decimal`
throughout; nesting depth is capped at 4 and refused with `FieldRange` beyond
it. `Lag{value, bars}` shifts an operand `bars` closed bars back, with
`bars ∈ 1..=500`; the lag is counted on the OPERAND'S OWN SERIES (an `htf`
operand lags its own H4 series, never the primary series); a lag of a lag is
refused, and a lag over a mixed-series expression is refused.
`Rising`/`Falling{value, bars}` take `bars ∈ 1..=500`, defaulting to 1, are
both STRICT, and each compiles to exactly `Compare{value, Gt|Lt,
Lag{value, bars}}` — so a rising filter and its hand-written equivalent
produce identical runs. These land as builder-tool vocabulary in rounds 2 and
3; this amendment only records the fixed slot and answers so the next spine
round does not re-litigate them.

**MACD gains an `output` selector (b2).** `IndicatorSpec::Macd` carries
`output: line | signal | histogram`, defaulted to `line` — the historical
behaviour — and always written (the `series` precedent). The selector rides
the additive-field minor-bump rule; signal and histogram warm up over
`max(fast, slow) + signal − 1` candles (the line first exists at
`max(fast, slow)`; the seeded signal EMA needs `signal` line values), and
ta-rs's signal EMA is seeded at candle 1 over the unblanked line — the
pandas-ta reference composes it the same way, so the cross-validation needs
no settling window. `Macd{…, output: line}` and `Macd{…, output: signal}` are
distinct specs and therefore distinct engine slots via `from_specs`; a rule
condition comparing the line output to the signal output
(`macd.line > macd.signal`) compiles to two slots.

**Write-path strictness: unknown fields are refused at every write, never at
a read.** A raw-document object key the parsed `StrategyDsl` cannot express
(an LLM's `entry.typo_field`, a top-level `name_of_thing`) used to deserialize
silently as nothing — a typo became a different strategy than the agent
believed it wrote. `check_unknown_fields` now runs on the MCP write path
(`submit_agent_version`) and on both repository write preludes
(`load_agent_document`, `create_version`), reporting every raw-only key at
its dotted/indexed path with a new `ValidationCode::UnknownField`; it never
runs on the migrator's `load` or on `row_to_version` — the read path stays
lenient so pre-r3 database rows survive engine upgrades (forward-compatibility
refusal is a load-time guarantee; a same-version typo is invisible to it).
The walk compares the MIGRATED raw value (`Migrator::migrated_value`), so a
renaming migration's consumed keys are not flagged; for the identity
1.0.0/1.1.0 → 1.2.0 steps that equals the submitted keys. Clean documents
with defaulted fields omitted pass unchanged.
