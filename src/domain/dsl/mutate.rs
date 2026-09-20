//! The one-mutation framework — a typed [`Mutation`] that applies to a strategy's
//! DSL and is **validated by construction** (r1.s2.w1, ADR-0021).
//!
//! This is the pure-domain half of the coach: `r1.s2.w2` persists a [`Mutation`]
//! typed, `r1.s2.w3` has the LLM propose one, and `r1.s4`'s accept path re-runs
//! [`apply`] before minting a child version. Nothing here calls an LLM, touches a
//! database, or knows about a session — it is a total function over the DSL.
//!
//! **Parameter-only vocabulary (ADR-0021 decision 1, grill L1).** The vocabulary
//! is one variant, [`Mutation::SetParam`], over the DSL's sweepable numeric leaves
//! — the surface [`SweepableValue`] already encloses. Structural mutations (add a
//! condition, swap an indicator) are excluded from r1 and are the ADR's named
//! rejected alternative; adding them later is an additive enum variant that forces
//! every `match` here to be revisited at compile time.
//!
//! **Success means validated AND compiled (ADR-0021 decision 2).** [`apply`]
//! writes into a *clone*, then runs the existing [`validate`](super::validate) and
//! [`compile`](super::compile) over the result. There is no second validation
//! path: every rule the composer's output must satisfy, a coach mutation satisfies
//! by construction. Each way that can fail is a typed [`MutationError`] carrying
//! enough context to be persisted verbatim as a recorded failure reason — never a
//! panic, never a silence.
//!
//! **Validity is use-time, never stored (ADR-0021 decision 3, audit C4).** Nothing
//! here returns a "this mutation is valid" fact to persist. A stored proposal is
//! re-checked by calling [`apply`] again at the moment of use.
//!
//! **One address grammar (ADR-0021 decision 4, audit C6).** Paths are
//! `validate.rs`'s dotted/indexed locators — `entry.and[0].not.lhs.indicator.rsi.period`,
//! `exits[0].distance_pct`, `risk.max_leverage` — so a coach mutation and a
//! validation error name the same field the same way. The traversal below is the
//! single place that grammar is produced, which is what keeps the two in step.

use std::fmt;
use std::ops::ControlFlow;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::compile::{CompileError, compile};
use super::condition::Condition;
use super::exit::ExitRule;
use super::strategy::StrategyDsl;
use super::sweepable::SweepableValue;
use super::validate::{ValidatedDsl, ValidationErrors, validate};
use super::value::{IndicatorSpec, ValueSource};

/// Which numeric a sweepable leaf holds.
///
/// The DSL has exactly two tunable numeric kinds (`sweepable.rs`): indicator
/// periods and bar counts are [`ParamKind::Period`] (`u32`); thresholds, stop
/// distances, R-multiples and risk fractions are [`ParamKind::Threshold`]
/// (`Decimal`). No `f64` anywhere (NFR-2).
///
/// Serde-serializable (r1.s2.w2): it rides inside
/// [`MutationError::TypeMismatch`], which a coaching session persists verbatim as
/// a recorded failure reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    /// A `SweepableValue<u32>` leaf — an indicator period or a bar count.
    Period,
    /// A `SweepableValue<Decimal>` leaf — a threshold, distance, R-multiple or
    /// risk fraction.
    Threshold,
}

impl fmt::Display for ParamKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Period => f.write_str("period (u32)"),
            Self::Threshold => f.write_str("threshold (Decimal)"),
        }
    }
}

/// A typed numeric offered for a sweepable leaf.
///
/// Internally-tagged with **struct variants only** — the DSL-wide serde invariant
/// (`dsl/mod.rs`): serde cannot serialize an internally-tagged tuple/newtype
/// variant.
///
/// **`Threshold` is a decimal STRING on the wire, in both directions** (NFR-2,
/// `tools.rs`'s `serde(with = "…str")` convention). `rust_decimal`'s default
/// `Deserialize` also accepts a bare JSON float, which would reopen the f64
/// ingress path the DSL closes everywhere else — and the `propose_mutation` tool
/// schema promises `"a decimal STRING (e.g. \"0.03\"), never a float"`, so a
/// model that sends `0.03` must get one clean `MalformedArguments` rather than a
/// silently binary-rounded threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub enum ParamValue {
    /// A period / bar-count value for a `SweepableValue<u32>` leaf.
    Period {
        /// The new period.
        value: u32,
    },
    /// A threshold / distance / fraction value for a `SweepableValue<Decimal>`
    /// leaf.
    Threshold {
        /// The new threshold — a decimal string (`"0.03"`), never a bare float.
        #[serde(with = "rust_decimal::serde::str")]
        value: Decimal,
    },
}

impl ParamValue {
    /// Which leaf kind this value can be written into.
    #[must_use]
    pub fn kind(&self) -> ParamKind {
        match self {
            Self::Period { .. } => ParamKind::Period,
            Self::Threshold { .. } => ParamKind::Threshold,
        }
    }
}

/// One coach mutation over a strategy's DSL.
///
/// One variant in r1 (ADR-0021 decision 1). Internally-tagged with struct
/// variants so it round-trips through serde losslessly — `r1.s2.w2` stores it
/// typed and `r1.s4`'s modify path edits it before re-applying.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Mutation {
    /// Retune one sweepable numeric leaf, addressed by its `validate.rs` locator.
    SetParam {
        /// The dotted/indexed locator of the leaf to retune.
        path: String,
        /// The value to write there.
        new_value: ParamValue,
    },
}

/// Every way [`apply`] can decline a mutation.
///
/// Each variant carries the path it failed on plus its cause, so the whole error
/// can be persisted verbatim as a coaching session's recorded failure reason
/// (`r1.s2.w2`/`w3`) rather than collapsing into "something went wrong".
///
/// **Serde-serializable (r1.s2.w2).** `CoachFailure::InapplicableMutation` carries
/// this error verbatim and a coaching session persists it, so the typed reason
/// survives into the record and back out of it rather than being flattened to a
/// `Display` string. Internally tagged with struct variants only — the DSL-wide
/// serde invariant (`dsl/mod.rs`).
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MutationError {
    /// The path addresses no sweepable numeric leaf of this strategy — a typo, a
    /// structural node (`entry`, `exits[0]`), a non-parameter field (`name`), or
    /// an index past the end. A **typed inapplicability**, not a partial write.
    #[error("no sweepable parameter is addressable at `{path}`")]
    UnknownPath {
        /// The locator that addressed nothing.
        path: String,
    },
    /// The leaf exists but holds the other numeric kind — e.g. a `Decimal`
    /// offered where the leaf is a `u32` period.
    #[error("`{path}` is a {expected} leaf, but the mutation offered a {offered} value")]
    TypeMismatch {
        /// The addressed leaf.
        path: String,
        /// The kind the leaf holds.
        expected: ParamKind,
        /// The kind the mutation offered.
        offered: ParamKind,
    },
    /// The addressed leaf already holds the offered value — a mutation that would
    /// change nothing.
    ///
    /// A **typed inapplicability, not a success** (PR #128, finding C3). `apply`
    /// returning `Ok` here would let the coach record a `Proposed` no-op and
    /// `r1.s4`'s accept mint a child version byte-identical to its parent, which
    /// is a version-tree entry that says a change happened when none did.
    ///
    /// Numeric equality, so a `Threshold` differing only in scale (`0.030` offered
    /// against a leaf holding `0.03`) is the same number and is declined. A
    /// `Sweep` leaf is NEVER this error: pinning a swept parameter at one of its
    /// own points removes the sweep, which is a real change — and the only reason
    /// the candidate compiles at all.
    ///
    /// The offered value is deliberately not duplicated into the variant:
    /// [`CoachFailure::InapplicableMutation`](crate::domain::CoachFailure) persists
    /// the whole [`Mutation`] alongside this error, and a direct `apply` caller
    /// supplied it.
    #[error("`{path}` already holds the offered value; the mutation would change nothing")]
    NoChange {
        /// The leaf that already holds it.
        path: String,
    },
    /// The value was written, and the resulting strategy failed semantic
    /// validation — an out-of-domain value (a period of 0) or a cross-field rule
    /// the change broke (MACD `fast >= slow`). Carries every [`ValidationErrors`]
    /// field error, so the coach's failure can be shown against the same fields a
    /// composer rejection would highlight.
    #[error("the candidate produced by mutating `{path}` failed validation: {errors}")]
    ValidationFailed {
        /// The mutated leaf.
        path: String,
        /// Every violation the candidate carries, in traversal order.
        #[source]
        errors: ValidationErrors,
    },
    /// The candidate validated but did not compile.
    ///
    /// **Defensive / should-be-unreachable through [`apply`]**, exactly as
    /// [`CompileError::UnexpectedSweep`] is in `compile.rs`: the only compile
    /// error is a stray `Sweep`, and [`validate`](super::validate::validate)
    /// rejects every `Sweep` before [`compile`](super::compile::compile) is
    /// reached. The variant exists so this seam stays total if `compile()` ever
    /// gains error cases.
    #[error("the candidate produced by mutating `{path}` failed to compile: {source}")]
    CompileFailed {
        /// The mutated leaf.
        path: String,
        /// The compiler's reason.
        #[source]
        source: CompileError,
    },
}

/// A mutated strategy that **passed validation and compiled**.
///
/// Constructible only by [`apply`], and it carries the [`ValidatedDsl`] that
/// proves it — which in turn is constructible only by
/// [`validate`](super::validate::validate). "A candidate that was never checked"
/// is therefore not representable.
///
/// It is a *candidate*, not a version: minting a child `StrategyVersion` is
/// `r1.s4`'s accept path (ADR-0010), not this framework's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDsl {
    validated: ValidatedDsl,
}

impl CandidateDsl {
    /// The mutated strategy document.
    #[must_use]
    pub fn dsl(&self) -> &StrategyDsl {
        self.validated.dsl()
    }

    /// The validation proof, ready for [`compile`](super::compile::compile).
    #[must_use]
    pub fn validated(&self) -> &ValidatedDsl {
        &self.validated
    }

    /// Consume into the owned [`ValidatedDsl`].
    #[must_use]
    pub fn into_validated(self) -> ValidatedDsl {
        self.validated
    }
}

/// Every sweepable numeric leaf of `dsl`, as its locator, in traversal order.
///
/// This is the tunable surface a [`Mutation`] may address — total by
/// construction, because it is produced by the same walk [`apply`] resolves
/// against. A path this does not return is a [`MutationError::UnknownPath`], and
/// nothing else.
///
/// It walks a throwaway clone rather than duplicating the traversal for shared
/// references: one walk means one locator grammar, which is the property audit C6
/// asked for.
#[must_use]
pub fn sweepable_paths(dsl: &StrategyDsl) -> Vec<String> {
    let mut paths = Vec::new();
    let mut scratch = dsl.clone();
    let _ = visit_leaves(&mut scratch, &mut |path, _leaf| {
        paths.push(path.to_owned());
        ControlFlow::Continue(())
    });
    paths
}

/// Apply one [`Mutation`] to a strategy's DSL.
///
/// The input document is never touched: the value is written into a clone, which
/// is then validated and compiled. `Ok` therefore means the candidate **is**
/// applicable — that fact is established here, at use-time, and is deliberately
/// not something to store (ADR-0021 decision 3, audit C4).
///
/// # Errors
///
/// Returns [`MutationError::UnknownPath`] when the locator addresses no sweepable
/// leaf, [`MutationError::TypeMismatch`] when it addresses one of the other
/// numeric kind, [`MutationError::NoChange`] when the leaf already holds the
/// offered value, [`MutationError::ValidationFailed`] when the mutated strategy
/// violates a semantic rule, and [`MutationError::CompileFailed`] if it validated
/// but did not compile.
pub fn apply(dsl: &StrategyDsl, mutation: &Mutation) -> Result<CandidateDsl, MutationError> {
    let Mutation::SetParam { path, new_value } = mutation;

    let mut candidate = dsl.clone();
    let mut written: Option<Result<(), MutationError>> = None;

    // The walk's own ControlFlow says only whether it stopped early; `written`
    // carries the outcome, so the return value is deliberately discarded.
    let _ = visit_leaves(&mut candidate, &mut |leaf_path, leaf| {
        if leaf_path != path.as_str() {
            return ControlFlow::Continue(());
        }
        written = Some(match (leaf, new_value) {
            // The `Fixed(current) == offered` guard runs BEFORE the write in both
            // arms: a leaf that already holds the value is a no-op, and a no-op is
            // inapplicable (PR #128, finding C3). Matching `Fixed` specifically is
            // what keeps `Sweep -> Fixed` a real change.
            (LeafMut::Period(slot), ParamValue::Period { value }) => {
                if matches!(slot, SweepableValue::Fixed(current) if *current == *value) {
                    Err(MutationError::NoChange { path: path.clone() })
                } else {
                    *slot = SweepableValue::Fixed(*value);
                    Ok(())
                }
            }
            (LeafMut::Threshold(slot), ParamValue::Threshold { value }) => {
                if matches!(slot, SweepableValue::Fixed(current) if *current == *value) {
                    Err(MutationError::NoChange { path: path.clone() })
                } else {
                    *slot = SweepableValue::Fixed(*value);
                    Ok(())
                }
            }
            (addressed, offered) => Err(MutationError::TypeMismatch {
                path: path.clone(),
                expected: addressed.kind(),
                offered: offered.kind(),
            }),
        });
        ControlFlow::Break(())
    });

    match written {
        None => Err(MutationError::UnknownPath { path: path.clone() }),
        Some(Err(e)) => Err(e),
        Some(Ok(())) => {
            let validated =
                validate(&candidate).map_err(|errors| MutationError::ValidationFailed {
                    path: path.clone(),
                    errors,
                })?;
            compile(&validated).map_err(|source| MutationError::CompileFailed {
                path: path.clone(),
                source,
            })?;
            Ok(CandidateDsl { validated })
        }
    }
}

// ---------------------------------------------------------------------------
// The traversal — the ONE place the locator grammar is produced (audit C6).
//
// It mirrors `validate.rs`'s `check_*` walk segment for segment; a divergence
// here would give the coach a second address language, which ADR-0021 rejects.
// ---------------------------------------------------------------------------

/// A mutable borrow of one sweepable numeric leaf.
enum LeafMut<'a> {
    Period(&'a mut SweepableValue<u32>),
    Threshold(&'a mut SweepableValue<Decimal>),
}

impl LeafMut<'_> {
    fn kind(&self) -> ParamKind {
        match self {
            Self::Period(_) => ParamKind::Period,
            Self::Threshold(_) => ParamKind::Threshold,
        }
    }
}

/// The visitor: called once per sweepable leaf with its locator, in traversal
/// order. Returning [`ControlFlow::Break`] stops the walk.
type Visit<'f> = &'f mut dyn FnMut(&str, LeafMut<'_>) -> ControlFlow<()>;

/// Walk every sweepable numeric leaf of a strategy, in `validate.rs` order:
/// `entry`, each `filters[i]`, each `exits[i]`, then `risk`.
fn visit_leaves(dsl: &mut StrategyDsl, f: Visit<'_>) -> ControlFlow<()> {
    visit_condition(&mut dsl.entry, "entry", f)?;

    for (i, filter) in dsl.filters.iter_mut().enumerate() {
        visit_condition(filter, &format!("filters[{i}]"), f)?;
    }

    for (i, exit) in dsl.exits.iter_mut().enumerate() {
        let base = format!("exits[{i}]");
        match exit {
            ExitRule::StopLoss { distance_pct } => {
                f(
                    &format!("{base}.distance_pct"),
                    LeafMut::Threshold(distance_pct),
                )?;
            }
            ExitRule::TakeProfit { target_r } => {
                f(&format!("{base}.target_r"), LeafMut::Threshold(target_r))?;
            }
            ExitRule::TrailingStop { trail_pct } => {
                f(&format!("{base}.trail_pct"), LeafMut::Threshold(trail_pct))?;
            }
            ExitRule::TimeStop { max_bars } => {
                f(&format!("{base}.max_bars"), LeafMut::Period(max_bars))?;
            }
            ExitRule::SignalExit { condition } => {
                visit_condition(condition, &format!("{base}.condition"), f)?;
            }
            ExitRule::AtrStop { period, multiple } => {
                f(&format!("{base}.period"), LeafMut::Period(period))?;
                f(&format!("{base}.multiple"), LeafMut::Threshold(multiple))?;
            }
        }
    }

    f(
        "risk.risk_per_trade_pct",
        LeafMut::Threshold(&mut dsl.risk.risk_per_trade_pct),
    )?;
    f(
        "risk.max_leverage",
        LeafMut::Threshold(&mut dsl.risk.max_leverage),
    )?;

    ControlFlow::Continue(())
}

/// Walk a condition subtree, threading the nesting-aware path exactly as
/// `validate.rs`'s `check_condition` does.
fn visit_condition(cond: &mut Condition, path: &str, f: Visit<'_>) -> ControlFlow<()> {
    match cond {
        Condition::Compare { lhs, rhs, .. }
        | Condition::CrossesAbove { lhs, rhs }
        | Condition::CrossesBelow { lhs, rhs } => {
            visit_value_source(lhs, &format!("{path}.lhs"), f)?;
            visit_value_source(rhs, &format!("{path}.rhs"), f)?;
        }
        Condition::And { conditions } => {
            for (i, sub) in conditions.iter_mut().enumerate() {
                visit_condition(sub, &format!("{path}.and[{i}]"), f)?;
            }
        }
        Condition::Or { conditions } => {
            for (i, sub) in conditions.iter_mut().enumerate() {
                visit_condition(sub, &format!("{path}.or[{i}]"), f)?;
            }
        }
        Condition::Not { condition } => {
            visit_condition(condition, &format!("{path}.not"), f)?;
        }
    }
    ControlFlow::Continue(())
}

/// Only `Indicator` carries sweepable leaves; `Constant`/`Price` carry none —
/// the same asymmetry `validate.rs`'s `check_value_source` encodes. `series` is
/// not a leaf (it is a series tag, not a numeric parameter).
fn visit_value_source(v: &mut ValueSource, path: &str, f: Visit<'_>) -> ControlFlow<()> {
    if let ValueSource::Indicator { spec, .. } = v {
        let base = format!("{path}.indicator");
        match spec {
            IndicatorSpec::Rsi { period } => {
                f(&format!("{base}.rsi.period"), LeafMut::Period(period))?;
            }
            IndicatorSpec::Ema { period } => {
                f(&format!("{base}.ema.period"), LeafMut::Period(period))?;
            }
            IndicatorSpec::Adx { period } => {
                f(&format!("{base}.adx.period"), LeafMut::Period(period))?;
            }
            IndicatorSpec::Macd { fast, slow, signal } => {
                f(&format!("{base}.macd.fast"), LeafMut::Period(fast))?;
                f(&format!("{base}.macd.slow"), LeafMut::Period(slow))?;
                f(&format!("{base}.macd.signal"), LeafMut::Period(signal))?;
            }
            IndicatorSpec::Atr { period } => {
                f(&format!("{base}.atr.period"), LeafMut::Period(period))?;
            }
        }
    }
    ControlFlow::Continue(())
}
