# 24. DSL schema evolution and the indicator seam

Date: 2026-09-19T00:00:00Z

## Status

Proposed

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
to `primary` so every `1.0.0` document reads as primary-series). Schema 1.1.0
makes the tag grammar only: `compile()` rejects an `htf` operand with
`CompileError::HtfUnsupported`. The engine-side semantics — evaluating the
operand against the aligned, closed HTF bar — belong to the follow-on item
that removes the gate; likewise `IndicatorSpec::Atr` and `ExitRule::AtrStop`
are grammar here, with the indicator engine and backtester holding typed
`Unsupported` placeholders until their owning item lands the computation.

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
with exactly the arms the seam declares). The schema/engine boundary is
explicit: grammar may precede behaviour, and the gap is carried as typed
errors (`HtfUnsupported`, `Unsupported`, `UnsupportedExit`) with asserting
tests, never as `todo!`/`unimplemented!` or a silent fallback — a document the
grammar accepts but the engine cannot yet run fails loudly at the boundary
instead of producing plausible-but-wrong results. The cost is a transitional
state in which valid documents exist that do not compile, which is exactly
what `CompileError::HtfUnsupported` is for; the revisit triggers bound how far
the grammar may drift from the engine before this decision must be re-opened.
