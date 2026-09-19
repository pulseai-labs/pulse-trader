//! Strategy DSL grammar (inner-ring, zero-I/O).
//!
//! The DSL models a trading strategy as **data**: serde-tagged Rust enums that
//! are the deterministic contract between the thin LLM layer (which composes
//! strategies via builder tools, FR-3) and the Rust engine (which executes
//! them).
//!
//! **Leaf + predicate layer:**
//! - [`SweepableValue`] — a tunable numeric leaf (fixed now; sweepable in v2).
//! - [`ValueSource`] / [`PriceField`] / [`IndicatorSpec`] — where a scalar comes
//!   from (a constant, a candle field, or an indicator output).
//! - [`Condition`] / [`Comparator`] — the boolean predicate tree over values.
//!
//! **Strategy document layer:**
//! - [`StrategyDsl`] — the top-level strategy: entry signal + filters + exits +
//!   risk + a [`SchemaVersion`]. The thing the LLM composes and the engine runs.
//! - [`ExitRule`] / [`RiskParams`] / [`Direction`] — exit and risk vocabulary.
//! - [`SchemaVersion`] — semver schema tag (string serde; migration is 2.05).
//!
//! Remaining items build on this: 2.03 validates a `StrategyDsl` into a checked
//! form, 2.04 compiles to an evaluator tree, 2.05 adds the version-safe migration
//! read-path. This module guarantees only the grammar shape + its serde
//! **round-trip** (value equality, not byte-canonical JSON); no evaluation, no
//! indicator math, no semantic validation lives here. **Direct deserialize is
//! migration-unaware** — the version-safe loader is 2.05's.
//!
//! **serde invariant (load-bearing):** the internally-tagged enums
//! ([`ValueSource`], [`IndicatorSpec`], [`Condition`]) use **only struct
//! variants** — serde cannot serialize an internally-tagged tuple/newtype
//! variant wrapping a `Vec`/scalar/enum. [`SweepableValue`] is `#[serde(untagged)]`
//! so `Fixed` is a bare value and `Sweep` is an object.

mod compile;
mod condition;
mod exit;
mod migrate;
mod mutate;
mod risk;
mod schema_version;
mod strategy;
mod sweepable;
mod validate;
mod value;

// VS-1.1.2 work-2.04: the compiler → executable evaluator tree (FR-3).
pub use compile::{
    CompileError, CompiledCondition, CompiledExit, CompiledRisk, CompiledStrategy, CompiledValue,
    EvalContext, atr_stop_price, compile, stop_price, take_profit_price,
};
pub use condition::{Comparator, Condition};
pub use exit::ExitRule;
pub use migrate::{LoadError, Loaded, Migration, MigrationError, MigrationKind, Migrator};
// r1.s2.w1 (ADR-0021): the one-mutation framework — a typed `Mutation` that
// applies to a strategy's DSL and is validated by construction.
pub use mutate::{
    CandidateDsl, Mutation, MutationError, ParamKind, ParamValue, apply, sweepable_paths,
};
pub use risk::{Direction, RiskParams};
pub use schema_version::{DSL_SCHEMA_VERSION, SchemaVersion, SchemaVersionParseError};
pub use strategy::StrategyDsl;
pub use sweepable::SweepableValue;
pub use validate::{FieldError, ValidatedDsl, ValidationCode, ValidationErrors, validate};
pub use value::{IndicatorSpec, PriceField, Series, ValueSource};
