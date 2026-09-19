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
