//! `compile` — turns a validated strategy document
//! ([`ValidatedDsl`](super::ValidatedDsl)) into an executable **evaluator tree**
//! ([`CompiledStrategy`]), the form the VS-1.2.x backtester walks per candle to
//! decide entries and exits (FR-3, BACKLOG-3). This is the slice's payoff and
//! the back-end of **demo-2** ("author an RSI-oversold strategy and inspect the
//! compiled evaluator tree").
//!
//! **Scope (2.04).** Compiles + evaluates *conditions* and exposes *pure
//! exit-geometry* helpers. It does NOT run the per-candle bar loop, position
//! sizing, fills, P&L, or indicator computation (VS-1.2.x backtester /
//! VS-1.1.3 indicator engine). The line: *"given a bar context, should I enter /
//! has an exit triggered?"* is here; *"step the backtest, size the position, book
//! the trade"* is the backtester.
//!
//! # Architecture
//!
//! - **Input is `ValidatedDsl` only** ([`compile`]). "Compile an unvalidated
//!   strategy" is a *compile* error, not a convention (the 2.03→2.04 contract).
//! - **`Result` is defensive.** Validation already guarantees every numeric leaf
//!   is a [`SweepableValue::Fixed`], but the only panic-free way to extract a
//!   `Fixed` payload under the crate's `unwrap_used`/`expect_used` lints is to
//!   return a should-be-unreachable [`CompileError::UnexpectedSweep`] rather than
//!   panic.
//! - **Effective entry = `entry` ∧ all `filters`.** The compiler folds the
//!   document's `entry` and every `filters[i]` into one
//!   [`CompiledCondition::And`] (a single condition if there are no filters), so
//!   the backtester evaluates one predicate.
//! - **Evaluation is stateless** ([`CompiledCondition::eval`]); the [`EvalContext`]
//!   carries current + prior bar values (the latter for crosses). A stateless
//!   `CompiledStrategy` is shareable across concurrent v2 parameter-sweep
//!   backtests.
//!
//! # Evaluation semantics (load-bearing)
//!
//! - **[`EvalContext`] returns `Option<Decimal>`** for BOTH `current` and
//!   `previous`. `None` means *unavailable*: `previous` is `None` on the first
//!   bar (no history), and `current` is `None` during an indicator's **warmup**
//!   (e.g. RSI(14) is undefined for the first ~14 bars). `Const` is always
//!   `Some`; `Price` is `Some` once the candle exists.
//! - **Any `None` operand → the `Compare`/cross evaluates to `false`** (no
//!   signal). This unifies indicator-warmup and first-bar-cross into one rule and
//!   makes eval self-protecting: even if the backtester forgets to gate entries
//!   on `required_indicators` readiness, warmup can't fire a wrong signal.
//! - **Crosses, first bar → false.** [`CompiledCondition::CrossesAbove`] is true
//!   iff both bars' values are present AND `previous(lhs) <= previous(rhs)` AND
//!   `current(lhs) > current(rhs)`; any `None` → false.
//!   [`CompiledCondition::CrossesBelow`] is the mirror.
//! - **[`Comparator::Eq`] is scale-insensitive.** `rust_decimal` compares by
//!   value (`30 == 30.00`), so an `Eq` compare does not depend on the operands'
//!   `Decimal` scale.
//!
//! # Exit geometry (direction-relative + pure)
//!
//! Helpers only — NO sizing or P&L. For a long, the stop sits *below* entry; for
//! a short, *above*. The stop distance **defines 1R** (`1R = entry ×
//! distance_pct`), and the take-profit is an R-multiple of that distance. See
//! [`stop_price`] and [`take_profit_price`].

use rust_decimal::Decimal;

use super::condition::{Comparator, Condition};
use super::exit::ExitRule;
use super::risk::{Direction, RiskParams};
use super::sweepable::SweepableValue;
use super::validate::ValidatedDsl;
use super::value::{IndicatorSpec, PriceField, Series, ValueSource};

/// An error produced while compiling a [`ValidatedDsl`].
///
/// [`CompileError::UnexpectedSweep`] is **defensive / should-be-unreachable**:
/// validation (2.03) already rejects every [`SweepableValue::Sweep`], so a
/// validated document only ever carries `Fixed` leaves. The variant exists so
/// Fixed extraction stays panic-free under the crate's `unwrap_used`/
/// `expect_used` lints. [`CompileError::HtfUnsupported`] is the schema-1.1.0
/// gate: the `series` tag is grammar, but evaluating a higher-timeframe
/// operand is r2.s2.w2's — until then the compiler refuses rather than
/// silently reading the primary series (w2 removes the variant).
///
/// Serde-serializable (r1.s2.w2): it rides inside
/// [`MutationError::CompileFailed`](super::mutate::MutationError::CompileFailed),
/// which a coaching session persists verbatim as a recorded failure reason.
/// Internally tagged with a struct variant — the DSL-wide serde invariant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CompileError {
    /// A [`SweepableValue::Sweep`] was encountered in a validated document.
    /// Unreachable in v1 — validation rejects sweeps — but represented so
    /// extraction never panics.
    #[error(
        "unexpected Sweep value at {field}: a validated strategy must contain only Fixed values"
    )]
    UnexpectedSweep {
        /// A human-readable field hint for where the stray sweep was found.
        field: String,
    },
    /// An operand tagged `series: "htf"` reached the compiler. Schema 1.1.0
    /// carries the tag only; r2.s2.w2 lands the aligned closed-bar evaluation
    /// and removes this variant.
    #[error("higher-timeframe operand at {field}: `series: htf` evaluation lands in r2.s2.w2")]
    HtfUnsupported {
        /// The operand's `validate.rs`-style locator (e.g. `entry.lhs`,
        /// `filters[0].rhs`).
        field: String,
    },
}

/// A resolved leaf value in a [`CompiledCondition`].
///
/// The DSL's `SweepableValue::Fixed` payloads have been extracted to concrete
/// `Decimal`/[`IndicatorSpec`] here. `Const` resolves to itself; `Price` and
/// `Indicator` are resolved against the [`EvalContext`] at eval time.
#[derive(Debug, Clone, PartialEq)]
pub enum CompiledValue {
    /// A literal constant (always available at eval time).
    Const(Decimal),
    /// A field of the current candle (available once the candle exists).
    Price {
        /// Which series the field reads (`Htf` is unreachable until w2 lifts
        /// the compile-time gate — the tag is carried for it).
        series: Series,
        /// Which OHLCV field to read.
        field: PriceField,
    },
    /// A technical-indicator output (unavailable during warmup → `None`).
    Indicator {
        /// Which series the indicator runs on (`Htf` is unreachable until
        /// w2 lifts the compile-time gate).
        series: Series,
        /// The indicator and its parameters.
        spec: IndicatorSpec,
    },
}

/// The compiled boolean predicate tree — mirrors
/// [`Condition`](super::Condition) but with resolved [`CompiledValue`] leaves.
///
/// Evaluated statelessly against an [`EvalContext`] via [`CompiledCondition::eval`].
#[derive(Debug, Clone, PartialEq)]
pub enum CompiledCondition {
    /// Compare two values with a [`Comparator`]. A `None` operand → `false`.
    Compare {
        /// Left-hand operand.
        lhs: CompiledValue,
        /// The comparison operator.
        op: Comparator,
        /// Right-hand operand.
        rhs: CompiledValue,
    },
    /// `lhs` crosses above `rhs`: `prev(lhs) <= prev(rhs)` AND
    /// `cur(lhs) > cur(rhs)`. Any `None` (incl. first bar) → `false`.
    CrossesAbove {
        /// Left-hand operand.
        lhs: CompiledValue,
        /// Right-hand operand.
        rhs: CompiledValue,
    },
    /// `lhs` crosses below `rhs`: `prev(lhs) >= prev(rhs)` AND
    /// `cur(lhs) < cur(rhs)`. Any `None` (incl. first bar) → `false`.
    CrossesBelow {
        /// Left-hand operand.
        lhs: CompiledValue,
        /// Right-hand operand.
        rhs: CompiledValue,
    },
    /// Logical conjunction (`true` over an empty list).
    And(Vec<CompiledCondition>),
    /// Logical disjunction (`false` over an empty list).
    Or(Vec<CompiledCondition>),
    /// Logical negation.
    Not(Box<CompiledCondition>),
}

impl CompiledCondition {
    /// Statelessly evaluate this predicate against `ctx`.
    ///
    /// A `Compare` or cross with **any** `None` operand evaluates to `false`
    /// (warmup / first-bar safety). `And`/`Or`/`Not` compose the leaves
    /// short-circuit-style. See the module docs for the full semantics.
    #[must_use]
    pub fn eval(&self, ctx: &dyn EvalContext) -> bool {
        match self {
            CompiledCondition::Compare { lhs, op, rhs } => {
                match (ctx.current(lhs), ctx.current(rhs)) {
                    (Some(l), Some(r)) => compare(l, *op, r),
                    // Any unavailable operand (e.g. indicator warmup) → no signal.
                    _ => false,
                }
            }
            CompiledCondition::CrossesAbove { lhs, rhs } => {
                match (
                    ctx.previous(lhs),
                    ctx.previous(rhs),
                    ctx.current(lhs),
                    ctx.current(rhs),
                ) {
                    (Some(pl), Some(pr), Some(cl), Some(cr)) => pl <= pr && cl > cr,
                    // First bar (no previous) or warmup → no cross.
                    _ => false,
                }
            }
            CompiledCondition::CrossesBelow { lhs, rhs } => {
                match (
                    ctx.previous(lhs),
                    ctx.previous(rhs),
                    ctx.current(lhs),
                    ctx.current(rhs),
                ) {
                    (Some(pl), Some(pr), Some(cl), Some(cr)) => pl >= pr && cl < cr,
                    _ => false,
                }
            }
            CompiledCondition::And(conditions) => conditions.iter().all(|c| c.eval(ctx)),
            CompiledCondition::Or(conditions) => conditions.iter().any(|c| c.eval(ctx)),
            CompiledCondition::Not(condition) => !condition.eval(ctx),
        }
    }
}

/// Apply a [`Comparator`] to two resolved `Decimal`s.
///
/// `Eq` is scale-insensitive — `rust_decimal` compares by value, so
/// `30 == 30.00`.
fn compare(lhs: Decimal, op: Comparator, rhs: Decimal) -> bool {
    match op {
        Comparator::Gt => lhs > rhs,
        Comparator::Gte => lhs >= rhs,
        Comparator::Lt => lhs < rhs,
        Comparator::Lte => lhs <= rhs,
        Comparator::Eq => lhs == rhs,
    }
}

/// The seam through which a [`CompiledCondition`] reads bar values.
///
/// Implemented by the indicator engine (VS-1.1.3) and the backtester (VS-1.2.x);
/// 2.04 ships only a test double. Both accessors return `Option` because a value
/// may be **unavailable**: `previous` is `None` on the first bar, and `current`
/// is `None` during an indicator's warmup. See the module docs for how `None`
/// propagates to `false`.
pub trait EvalContext {
    /// The value of `value` on the current bar, or `None` if unavailable
    /// (indicator warmup).
    fn current(&self, value: &CompiledValue) -> Option<Decimal>;
    /// The value of `value` on the previous bar, or `None` if unavailable
    /// (first bar, or indicator warmup on the prior bar).
    fn previous(&self, value: &CompiledValue) -> Option<Decimal>;
}

/// A compiled exit rule — mirrors [`ExitRule`](super::ExitRule) with extracted
/// `Decimal`/`u32` payloads. Geometry helpers ([`stop_price`],
/// [`take_profit_price`]) operate on the `StopLoss`/`TakeProfit` distances; the
/// rest are carried structurally for the backtester.
#[derive(Debug, Clone, PartialEq)]
pub enum CompiledExit {
    /// A fixed stop a decimal-fraction distance from entry. Defines 1R.
    StopLoss {
        /// Stop distance as a decimal fraction (`0.05` = 5%).
        distance_pct: Decimal,
    },
    /// A take-profit target as an R-multiple of the stop distance.
    TakeProfit {
        /// Target as a plain R-multiple (`2.0` = 2R).
        target_r: Decimal,
    },
    /// A trailing stop a decimal-fraction distance behind the favourable extreme.
    TrailingStop {
        /// Trail distance as a decimal fraction.
        trail_pct: Decimal,
    },
    /// A time-based exit after a maximum number of bars in the trade.
    TimeStop {
        /// Maximum number of bars to hold before closing.
        max_bars: u32,
    },
    /// Close when a [`CompiledCondition`] becomes true.
    SignalExit {
        /// The condition that, when true, closes the position.
        condition: CompiledCondition,
    },
    /// An ATR-multiple stop (schema 1.1.0): pure data — `period` selects the
    /// primary-series ATR, `multiple` scales it via [`atr_stop_price`]. The
    /// fill behaviour is w2's; the backtester rejects it with a typed
    /// `UnsupportedExit` until then.
    AtrStop {
        /// ATR lookback period.
        period: u32,
        /// ATR multiple.
        multiple: Decimal,
    },
}

/// The compiled risk / sizing inputs — carried for the backtester; no sizing
/// math is performed in 2.04.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledRisk {
    /// Fraction of account equity risked per trade (`0.01` = 1%).
    pub risk_per_trade_pct: Decimal,
    /// Maximum leverage cap (a plain multiplier).
    pub max_leverage: Decimal,
}

/// An executable strategy — the output of [`compile`].
///
/// `entry` is the **effective entry** (`entry` ∧ all `filters`). `Debug` +
/// accessor methods (+ [`describe`](CompiledStrategy::describe)) make it
/// inspectable for demo-2.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledStrategy {
    direction: Direction,
    entry: CompiledCondition,
    exits: Vec<CompiledExit>,
    risk: CompiledRisk,
    required_indicators: Vec<IndicatorSpec>,
    required_htf_indicators: Vec<IndicatorSpec>,
}

impl CompiledStrategy {
    /// The trade side this strategy takes.
    #[must_use]
    pub fn direction(&self) -> Direction {
        self.direction
    }

    /// The effective entry predicate (`entry` ∧ all `filters`).
    #[must_use]
    pub fn entry(&self) -> &CompiledCondition {
        &self.entry
    }

    /// The compiled exit rules.
    #[must_use]
    pub fn exits(&self) -> &[CompiledExit] {
        &self.exits
    }

    /// The compiled risk inputs.
    #[must_use]
    pub fn risk(&self) -> &CompiledRisk {
        &self.risk
    }

    /// The de-duplicated **primary-series** indicators this strategy references
    /// — what the backtester must compute (+ lookback). Includes an
    /// `AtrStop`'s `Atr(period)` (the stop reads the primary ATR).
    #[must_use]
    pub fn required_indicators(&self) -> &[IndicatorSpec] {
        &self.required_indicators
    }

    /// The de-duplicated **higher-timeframe** indicators this strategy
    /// references (schema 1.1.0). Always empty while the compile-time
    /// [`CompileError::HtfUnsupported`] gate rejects `Htf` operands; w2 fills
    /// and consumes it.
    #[must_use]
    pub fn required_htf_indicators(&self) -> &[IndicatorSpec] {
        &self.required_htf_indicators
    }

    /// Whether any operand is tagged `series: "htf"` (schema 1.1.0). Always
    /// `false` while the compile-time gate rejects them; kept honest by walking
    /// the compiled tree rather than a stored flag, so w2's ungate flips it
    /// with no further edit here.
    #[must_use]
    pub fn needs_htf(&self) -> bool {
        fn value_is_htf(value: &CompiledValue) -> bool {
            matches!(
                value,
                CompiledValue::Price {
                    series: Series::Htf,
                    ..
                } | CompiledValue::Indicator {
                    series: Series::Htf,
                    ..
                }
            )
        }
        fn cond_has_htf(condition: &CompiledCondition) -> bool {
            match condition {
                CompiledCondition::Compare { lhs, rhs, .. }
                | CompiledCondition::CrossesAbove { lhs, rhs }
                | CompiledCondition::CrossesBelow { lhs, rhs } => {
                    value_is_htf(lhs) || value_is_htf(rhs)
                }
                CompiledCondition::And(conditions) | CompiledCondition::Or(conditions) => {
                    conditions.iter().any(cond_has_htf)
                }
                CompiledCondition::Not(condition) => cond_has_htf(condition),
            }
        }
        cond_has_htf(&self.entry)
            || self.exits.iter().any(|exit| {
                matches!(exit, CompiledExit::SignalExit { condition } if cond_has_htf(condition))
            })
    }

    /// A human-readable rendering of the evaluator tree, for demo-2's "inspect
    /// the compiled evaluator tree" step. Backed by `Debug` of the entry tree.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "CompiledStrategy {{ direction: {:?}, entry: {:?}, exits: {:?}, required_indicators: {:?} }}",
            self.direction, self.entry, self.exits, self.required_indicators
        )
    }
}

/// Direction-relative stop price (pure; NO sizing).
///
/// `Long`: `entry × (1 − distance_pct)` (the stop sits *below* entry).
/// `Short`: `entry × (1 + distance_pct)` (the stop sits *above* entry).
#[must_use]
pub fn stop_price(entry: Decimal, distance_pct: Decimal, direction: Direction) -> Decimal {
    match direction {
        Direction::Long => entry * (Decimal::ONE - distance_pct),
        Direction::Short => entry * (Decimal::ONE + distance_pct),
    }
}

/// Direction-relative take-profit price (pure; NO sizing).
///
/// `1R = entry × distance_pct` (the stop distance in price units). The target is
/// `target_r` R-multiples away from entry, in the favourable direction:
/// `Long`: `entry + target_r × 1R`; `Short`: `entry − target_r × 1R`.
#[must_use]
pub fn take_profit_price(
    entry: Decimal,
    distance_pct: Decimal,
    target_r: Decimal,
    direction: Direction,
) -> Decimal {
    let one_r = entry * distance_pct;
    match direction {
        Direction::Long => entry + target_r * one_r,
        Direction::Short => entry - target_r * one_r,
    }
}

/// Direction-relative ATR-multiple stop price (pure; NO sizing) — schema 1.1.0.
///
/// `Long`: `entry − multiple × atr` (the stop sits *below* entry).
/// `Short`: `entry + multiple × atr` (the stop sits *above* entry).
/// `atr` is the **primary** series' ATR (the stop always reads primary). w2
/// calls this at fill time; this item only lands the pure helper.
#[must_use]
pub fn atr_stop_price(
    entry: Decimal,
    atr: Decimal,
    multiple: Decimal,
    direction: Direction,
) -> Decimal {
    match direction {
        Direction::Long => entry - multiple * atr,
        Direction::Short => entry + multiple * atr,
    }
}

/// Extract the `Fixed` payload of a [`SweepableValue`], or `None` for a `Sweep`.
///
/// Private helper kept here (NOT on `sweepable.rs`, which is frozen). A validated
/// document only ever carries `Fixed`, so a `None` here is the defensive
/// should-be-unreachable path that becomes a [`CompileError::UnexpectedSweep`].
fn fixed<T>(value: &SweepableValue<T>) -> Option<&T> {
    match value {
        SweepableValue::Fixed(inner) => Some(inner),
        SweepableValue::Sweep { .. } => None,
    }
}

/// Resolve a [`ValueSource`] into a [`CompiledValue`], extracting any `Fixed`
/// indicator periods. Pure — no eval-context dependency. `path` is the
/// operand's `validate.rs`-style locator, carried into
/// [`CompileError::HtfUnsupported`]: schema 1.1.0's `series` tag is grammar
/// only — compiling an `Htf` operand is a typed error until w2 evaluates it.
fn compile_value(source: &ValueSource, path: &str) -> Result<CompiledValue, CompileError> {
    match source {
        ValueSource::Constant { value } => Ok(CompiledValue::Const(*value)),
        ValueSource::Price { series, field } => match series {
            Series::Primary => Ok(CompiledValue::Price {
                series: *series,
                field: *field,
            }),
            Series::Htf => Err(CompileError::HtfUnsupported {
                field: path.to_owned(),
            }),
        },
        ValueSource::Indicator { series, spec } => match series {
            Series::Primary => Ok(CompiledValue::Indicator {
                series: *series,
                spec: spec.clone(),
            }),
            Series::Htf => Err(CompileError::HtfUnsupported {
                field: path.to_owned(),
            }),
        },
    }
}

/// Append every [`IndicatorSpec`] referenced by `value` to the vec for its
/// series — `primary` for the run's series, `htf` for the higher-timeframe one
/// (schema 1.1.0; the htf vec feeds [`CompiledStrategy::required_htf_indicators`]).
/// De-duplicates via `Vec::contains` (avoids needing `Eq`/`Hash` on the frozen
/// `IndicatorSpec`).
fn collect_indicators_from_value(
    source: &ValueSource,
    primary: &mut Vec<IndicatorSpec>,
    htf: &mut Vec<IndicatorSpec>,
) {
    if let ValueSource::Indicator { series, spec } = source {
        let acc = match series {
            Series::Primary => primary,
            Series::Htf => htf,
        };
        if !acc.contains(spec) {
            acc.push(spec.clone());
        }
    }
}

/// Walk a [`Condition`] tree appending every referenced [`IndicatorSpec`] to
/// its series' vec (de-duplicated).
fn collect_indicators(
    condition: &Condition,
    primary: &mut Vec<IndicatorSpec>,
    htf: &mut Vec<IndicatorSpec>,
) {
    match condition {
        Condition::Compare { lhs, rhs, .. }
        | Condition::CrossesAbove { lhs, rhs }
        | Condition::CrossesBelow { lhs, rhs } => {
            collect_indicators_from_value(lhs, primary, htf);
            collect_indicators_from_value(rhs, primary, htf);
        }
        Condition::And { conditions } | Condition::Or { conditions } => {
            for c in conditions {
                collect_indicators(c, primary, htf);
            }
        }
        Condition::Not { condition } => collect_indicators(condition, primary, htf),
    }
}

/// Compile a [`Condition`] into a [`CompiledCondition`] (recursive, pure apart
/// from the `Htf` gate). `path` threads the `validate.rs` locator grammar so an
/// `Htf` operand reports where it sits.
fn compile_condition(condition: &Condition, path: &str) -> Result<CompiledCondition, CompileError> {
    Ok(match condition {
        Condition::Compare { lhs, op, rhs } => CompiledCondition::Compare {
            lhs: compile_value(lhs, &format!("{path}.lhs"))?,
            op: *op,
            rhs: compile_value(rhs, &format!("{path}.rhs"))?,
        },
        Condition::CrossesAbove { lhs, rhs } => CompiledCondition::CrossesAbove {
            lhs: compile_value(lhs, &format!("{path}.lhs"))?,
            rhs: compile_value(rhs, &format!("{path}.rhs"))?,
        },
        Condition::CrossesBelow { lhs, rhs } => CompiledCondition::CrossesBelow {
            lhs: compile_value(lhs, &format!("{path}.lhs"))?,
            rhs: compile_value(rhs, &format!("{path}.rhs"))?,
        },
        Condition::And { conditions } => CompiledCondition::And(
            conditions
                .iter()
                .enumerate()
                .map(|(i, c)| compile_condition(c, &format!("{path}.and[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        Condition::Or { conditions } => CompiledCondition::Or(
            conditions
                .iter()
                .enumerate()
                .map(|(i, c)| compile_condition(c, &format!("{path}.or[{i}]")))
                .collect::<Result<_, _>>()?,
        ),
        Condition::Not { condition } => CompiledCondition::Not(Box::new(compile_condition(
            condition,
            &format!("{path}.not"),
        )?)),
    })
}

/// Compile a single [`ExitRule`] into a [`CompiledExit`], extracting `Fixed`
/// payloads (defensive `Err` on the unreachable `Sweep`). `base` is the exit's
/// locator root (`exits[i]`), threaded into a nested [`SignalExit`] condition's
/// operand paths.
fn compile_exit(rule: &ExitRule, base: &str) -> Result<CompiledExit, CompileError> {
    match rule {
        ExitRule::StopLoss { distance_pct } => {
            let distance_pct =
                *fixed(distance_pct).ok_or_else(|| CompileError::UnexpectedSweep {
                    field: "exit.StopLoss.distance_pct".to_owned(),
                })?;
            Ok(CompiledExit::StopLoss { distance_pct })
        }
        ExitRule::TakeProfit { target_r } => {
            let target_r = *fixed(target_r).ok_or_else(|| CompileError::UnexpectedSweep {
                field: "exit.TakeProfit.target_r".to_owned(),
            })?;
            Ok(CompiledExit::TakeProfit { target_r })
        }
        ExitRule::TrailingStop { trail_pct } => {
            let trail_pct = *fixed(trail_pct).ok_or_else(|| CompileError::UnexpectedSweep {
                field: "exit.TrailingStop.trail_pct".to_owned(),
            })?;
            Ok(CompiledExit::TrailingStop { trail_pct })
        }
        ExitRule::TimeStop { max_bars } => {
            let max_bars = *fixed(max_bars).ok_or_else(|| CompileError::UnexpectedSweep {
                field: "exit.TimeStop.max_bars".to_owned(),
            })?;
            Ok(CompiledExit::TimeStop { max_bars })
        }
        ExitRule::SignalExit { condition } => Ok(CompiledExit::SignalExit {
            condition: compile_condition(condition, &format!("{base}.condition"))?,
        }),
        ExitRule::AtrStop { period, multiple } => {
            let period = *fixed(period).ok_or_else(|| CompileError::UnexpectedSweep {
                field: "exit.AtrStop.period".to_owned(),
            })?;
            let multiple = *fixed(multiple).ok_or_else(|| CompileError::UnexpectedSweep {
                field: "exit.AtrStop.multiple".to_owned(),
            })?;
            Ok(CompiledExit::AtrStop { period, multiple })
        }
    }
}

/// Compile the [`RiskParams`] into a [`CompiledRisk`] (extract `Fixed` payloads).
fn compile_risk(risk: &RiskParams) -> Result<CompiledRisk, CompileError> {
    let risk_per_trade_pct =
        *fixed(&risk.risk_per_trade_pct).ok_or_else(|| CompileError::UnexpectedSweep {
            field: "risk.risk_per_trade_pct".to_owned(),
        })?;
    let max_leverage = *fixed(&risk.max_leverage).ok_or_else(|| CompileError::UnexpectedSweep {
        field: "risk.max_leverage".to_owned(),
    })?;
    Ok(CompiledRisk {
        risk_per_trade_pct,
        max_leverage,
    })
}

/// Compile a [`ValidatedDsl`] into an executable [`CompiledStrategy`].
///
/// Folds `entry` ∧ all `filters` into one effective-entry predicate, extracts
/// every `Fixed` numeric leaf, and collects the de-duplicated
/// `required_indicators`.
///
/// # Errors
///
/// Returns [`CompileError::UnexpectedSweep`] if a numeric leaf is a
/// [`SweepableValue::Sweep`]. This is **defensive / should-be-unreachable**:
/// validation (2.03) already rejects every sweep, so a `ValidatedDsl` never
/// carries one. Returns [`CompileError::HtfUnsupported`] for any
/// `series: "htf"` operand — the schema-1.1.0 gate w2 lifts.
pub fn compile(validated: &ValidatedDsl) -> Result<CompiledStrategy, CompileError> {
    let dsl = validated.dsl();

    // Effective entry = entry ∧ all filters. Fold into a single And (or the bare
    // entry condition when there are no filters).
    let entry = if dsl.filters.is_empty() {
        compile_condition(&dsl.entry, "entry")?
    } else {
        let mut parts = Vec::with_capacity(1 + dsl.filters.len());
        parts.push(compile_condition(&dsl.entry, "entry")?);
        for (i, filter) in dsl.filters.iter().enumerate() {
            parts.push(compile_condition(filter, &format!("filters[{i}]"))?);
        }
        CompiledCondition::And(parts)
    };

    let exits = dsl
        .exits
        .iter()
        .enumerate()
        .map(|(i, rule)| compile_exit(rule, &format!("exits[{i}]")))
        .collect::<Result<Vec<_>, _>>()?;

    let risk = compile_risk(&dsl.risk)?;

    // Required indicators: de-dup via Vec + PartialEq `contains` (NOT a HashSet,
    // so IndicatorSpec needs no Eq/Hash). Walk entry, filters, and any
    // signal-exit condition trees — split by series (schema 1.1.0). An
    // `AtrStop`'s ATR is always the primary series' — register it here too.
    let mut required_indicators: Vec<IndicatorSpec> = Vec::new();
    let mut required_htf_indicators: Vec<IndicatorSpec> = Vec::new();
    collect_indicators(
        &dsl.entry,
        &mut required_indicators,
        &mut required_htf_indicators,
    );
    for filter in &dsl.filters {
        collect_indicators(
            filter,
            &mut required_indicators,
            &mut required_htf_indicators,
        );
    }
    for exit in &dsl.exits {
        match exit {
            ExitRule::SignalExit { condition } => collect_indicators(
                condition,
                &mut required_indicators,
                &mut required_htf_indicators,
            ),
            ExitRule::AtrStop {
                period: SweepableValue::Fixed(period),
                ..
            } => {
                let spec = IndicatorSpec::Atr {
                    period: SweepableValue::Fixed(*period),
                };
                if !required_indicators.contains(&spec) {
                    required_indicators.push(spec);
                }
            }
            _ => {}
        }
    }

    Ok(CompiledStrategy {
        direction: dsl.direction,
        entry,
        exits,
        risk,
        required_indicators,
        required_htf_indicators,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        CompileError, CompiledCondition, CompiledExit, CompiledStrategy, CompiledValue,
        EvalContext, atr_stop_price, compile, stop_price, take_profit_price,
    };
    use crate::domain::dsl::condition::{Comparator, Condition};
    use crate::domain::dsl::exit::ExitRule;
    use crate::domain::dsl::risk::{Direction, RiskParams};
    use crate::domain::dsl::schema_version::SchemaVersion;
    use crate::domain::dsl::strategy::StrategyDsl;
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::validate::{ValidatedDsl, validate};
    use crate::domain::dsl::value::{IndicatorSpec, PriceField, Series, ValueSource};
    use rust_decimal::Decimal;

    // ---- test fixtures -----------------------------------------------------

    /// The demo-2 strategy: long when RSI(14) < 30, 5% stop, 2R take profit,
    /// 1% risk per trade. Returns a *validated* document.
    fn rsi_oversold() -> ValidatedDsl {
        let dsl = StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "RSI Oversold".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Rsi {
                        period: SweepableValue::Fixed(14),
                    },
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant {
                    value: Decimal::new(30, 0),
                },
            },
            filters: vec![],
            exits: vec![
                ExitRule::StopLoss {
                    distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
                },
                ExitRule::TakeProfit {
                    target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
                },
            ],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        };
        validate(&dsl).expect("rsi_oversold fixture must validate")
    }

    /// A test-double `EvalContext` backed by two lookup tables (current + prev),
    /// keyed by a stable string id derived from the `CompiledValue`. A missing
    /// key returns `None` (models warmup / first bar).
    struct FakeCtx {
        current: Vec<(String, Decimal)>,
        previous: Vec<(String, Decimal)>,
    }

    impl FakeCtx {
        fn key(value: &CompiledValue) -> String {
            match value {
                CompiledValue::Const(d) => format!("const:{d}"),
                CompiledValue::Price { series, field } => {
                    format!("price:{series:?}:{field:?}")
                }
                CompiledValue::Indicator { series, spec } => {
                    format!("ind:{series:?}:{spec:?}")
                }
            }
        }

        fn empty() -> Self {
            FakeCtx {
                current: Vec::new(),
                previous: Vec::new(),
            }
        }

        fn with_current(mut self, value: &CompiledValue, d: Decimal) -> Self {
            self.current.push((Self::key(value), d));
            self
        }

        fn with_previous(mut self, value: &CompiledValue, d: Decimal) -> Self {
            self.previous.push((Self::key(value), d));
            self
        }
    }

    impl EvalContext for FakeCtx {
        fn current(&self, value: &CompiledValue) -> Option<Decimal> {
            // Consts resolve to themselves; everything else is a table lookup.
            if let CompiledValue::Const(d) = value {
                return Some(*d);
            }
            let key = Self::key(value);
            self.current
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, d)| *d)
        }

        fn previous(&self, value: &CompiledValue) -> Option<Decimal> {
            if let CompiledValue::Const(d) = value {
                return Some(*d);
            }
            let key = Self::key(value);
            self.previous
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, d)| *d)
        }
    }

    // ---- AC-5 --------------------------------------------------------------

    #[test]
    fn compiles_rsi_oversold_strategy() {
        let compiled = compile(&rsi_oversold()).expect("RSI-oversold must compile");
        assert_eq!(compiled.direction(), Direction::Long);
        assert!(
            compiled
                .required_indicators()
                .contains(&IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(14),
                }),
            "required_indicators must contain Rsi {{ period: 14 }}, was {:?}",
            compiled.required_indicators()
        );
        // entry present (a Compare leaf, no filters → not folded into And).
        assert!(matches!(
            compiled.entry(),
            CompiledCondition::Compare { .. }
        ));
        assert!(!compiled.exits().is_empty(), "must carry >= 1 exit");
    }

    // ---- AC-6 --------------------------------------------------------------

    #[test]
    fn effective_entry_is_entry_and_filters() {
        // entry: Price(Close) > 100 ; filter: Rsi(14) < 30.
        let close_gt_100 = Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::new(100, 0),
            },
        };
        let rsi_lt_30 = Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(14),
                },
            },
            op: Comparator::Lt,
            rhs: ValueSource::Constant {
                value: Decimal::new(30, 0),
            },
        };
        let dsl = StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "Entry + filter".to_owned(),
            direction: Direction::Long,
            entry: close_gt_100,
            filters: vec![rsi_lt_30],
            exits: vec![ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
            }],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        };
        let validated = validate(&dsl).expect("fixture must validate");
        let compiled = compile(&validated).expect("must compile");

        // Effective entry folded into one And of two compares.
        let entry = compiled.entry();
        assert!(
            matches!(entry, CompiledCondition::And(parts) if parts.len() == 2),
            "effective entry must be And(entry, filter), was {entry:?}"
        );

        let close = CompiledValue::Price {
            series: Series::Primary,
            field: PriceField::Close,
        };
        let rsi = CompiledValue::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Rsi {
                period: SweepableValue::Fixed(14),
            },
        };

        // true ∧ true → true (Close=150 > 100, Rsi=20 < 30).
        let both = FakeCtx::empty()
            .with_current(&close, Decimal::new(150, 0))
            .with_current(&rsi, Decimal::new(20, 0));
        assert!(entry.eval(&both), "true ∧ true must be true");

        // true ∧ false → false (Close=150 > 100, Rsi=40 NOT < 30).
        let one = FakeCtx::empty()
            .with_current(&close, Decimal::new(150, 0))
            .with_current(&rsi, Decimal::new(40, 0));
        assert!(!entry.eval(&one), "true ∧ false must be false");
    }

    // ---- AC-7 --------------------------------------------------------------

    #[test]
    fn evaluates_boolean_tree_against_context() {
        let close = CompiledValue::Price {
            series: Series::Primary,
            field: PriceField::Close,
        };
        let rsi = CompiledValue::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Rsi {
                period: SweepableValue::Fixed(14),
            },
        };

        // Compare: Close > 100.
        let close_gt_100 = CompiledCondition::Compare {
            lhs: close.clone(),
            op: Comparator::Gt,
            rhs: CompiledValue::Const(Decimal::new(100, 0)),
        };
        // Compare: Rsi < 30.
        let rsi_lt_30 = CompiledCondition::Compare {
            lhs: rsi.clone(),
            op: Comparator::Lt,
            rhs: CompiledValue::Const(Decimal::new(30, 0)),
        };

        let ctx = FakeCtx::empty()
            .with_current(&close, Decimal::new(150, 0))
            .with_current(&rsi, Decimal::new(20, 0));

        // And(true, true) → true.
        let and = CompiledCondition::And(vec![close_gt_100.clone(), rsi_lt_30.clone()]);
        assert!(and.eval(&ctx));

        // Or(false, true) → true (Close < 100 false; Rsi < 30 true).
        let close_below_100 = CompiledCondition::Compare {
            lhs: close.clone(),
            op: Comparator::Lt,
            rhs: CompiledValue::Const(Decimal::new(100, 0)),
        };
        let or = CompiledCondition::Or(vec![close_below_100.clone(), rsi_lt_30.clone()]);
        assert!(or.eval(&ctx));

        // Not(true) → false.
        let not = CompiledCondition::Not(Box::new(close_gt_100.clone()));
        assert!(!not.eval(&ctx));

        // Each comparator at least once.
        let gte = CompiledCondition::Compare {
            lhs: close.clone(),
            op: Comparator::Gte,
            rhs: CompiledValue::Const(Decimal::new(150, 0)),
        };
        assert!(gte.eval(&ctx), "150 >= 150");
        let lte = CompiledCondition::Compare {
            lhs: rsi.clone(),
            op: Comparator::Lte,
            rhs: CompiledValue::Const(Decimal::new(20, 0)),
        };
        assert!(lte.eval(&ctx), "20 <= 20");

        // Warmup case (C1): Rsi `current` is None → Compare → false (even though
        // 20 < 30 would be true if available).
        let warmup = FakeCtx::empty().with_current(&close, Decimal::new(150, 0));
        // rsi has NO current entry → None.
        assert!(
            !rsi_lt_30.eval(&warmup),
            "a Compare with a None operand (warmup) must be false"
        );
        // And short-circuits to false too.
        assert!(
            !and.eval(&warmup),
            "And with a warming-up operand must be false"
        );

        // Eq is scale-insensitive: 30 vs 30.00.
        let eq = CompiledCondition::Compare {
            lhs: CompiledValue::Const(Decimal::new(30, 0)),
            op: Comparator::Eq,
            rhs: CompiledValue::Const(Decimal::new(3000, 2)),
        };
        assert!(
            eq.eval(&FakeCtx::empty()),
            "30 == 30.00 (scale-insensitive)"
        );
    }

    // ---- AC-8 --------------------------------------------------------------

    #[test]
    fn cross_is_false_on_first_bar_then_detects() {
        let fast = CompiledValue::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Ema {
                period: SweepableValue::Fixed(9),
            },
        };
        let slow = CompiledValue::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Ema {
                period: SweepableValue::Fixed(21),
            },
        };
        let cross = CompiledCondition::CrossesAbove {
            lhs: fast.clone(),
            rhs: slow.clone(),
        };

        // First bar: current present, previous absent (None) → false.
        let first_bar = FakeCtx::empty()
            .with_current(&fast, Decimal::new(11, 0))
            .with_current(&slow, Decimal::new(10, 0));
        assert!(
            !cross.eval(&first_bar),
            "cross on first bar (previous None) must be false"
        );

        // A genuine cross: prev fast(9) <= prev slow(10); cur fast(11) > cur slow(10).
        let crossing = FakeCtx::empty()
            .with_previous(&fast, Decimal::new(9, 0))
            .with_previous(&slow, Decimal::new(10, 0))
            .with_current(&fast, Decimal::new(11, 0))
            .with_current(&slow, Decimal::new(10, 0));
        assert!(cross.eval(&crossing), "prev<= and cur> must detect a cross");

        // Non-cross: fast already above on the previous bar (prev 12 > prev 10).
        let non_cross = FakeCtx::empty()
            .with_previous(&fast, Decimal::new(12, 0))
            .with_previous(&slow, Decimal::new(10, 0))
            .with_current(&fast, Decimal::new(13, 0))
            .with_current(&slow, Decimal::new(10, 0));
        assert!(
            !cross.eval(&non_cross),
            "already-above (prev>rhs) is not a fresh cross"
        );
    }

    // ---- AC-9 --------------------------------------------------------------

    #[test]
    fn stop_and_target_geometry_is_direction_relative() {
        let entry = Decimal::new(100, 0);
        let distance_pct = Decimal::new(5, 2); // 0.05
        let target_r = Decimal::new(2, 0); // 2.0

        // stop_price(100, 0.05): Long → 95, Short → 105.
        assert_eq!(
            stop_price(entry, distance_pct, Direction::Long),
            Decimal::new(95, 0)
        );
        assert_eq!(
            stop_price(entry, distance_pct, Direction::Short),
            Decimal::new(105, 0)
        );

        // take_profit, target_r = 2.0, 1R = 100*0.05 = 5: Long → 110, Short → 90.
        assert_eq!(
            take_profit_price(entry, distance_pct, target_r, Direction::Long),
            Decimal::new(110, 0)
        );
        assert_eq!(
            take_profit_price(entry, distance_pct, target_r, Direction::Short),
            Decimal::new(90, 0)
        );

        // atr_stop_price(100, atr=3, multiple=2): Long → 94, Short → 106.
        let atr = Decimal::new(3, 0);
        let multiple = Decimal::new(2, 0);
        assert_eq!(
            atr_stop_price(entry, atr, multiple, Direction::Long),
            Decimal::new(94, 0)
        );
        assert_eq!(
            atr_stop_price(entry, atr, multiple, Direction::Short),
            Decimal::new(106, 0)
        );
    }

    // ---- AC-10 -------------------------------------------------------------

    #[test]
    fn compiled_strategy_is_inspectable() {
        let compiled: CompiledStrategy = compile(&rsi_oversold()).expect("must compile");

        // Debug rendering and describe() both expose the entry predicate.
        let debug = format!("{compiled:?}");
        let described = compiled.describe();

        for rendering in [&debug, &described] {
            assert!(rendering.contains("Rsi"), "must mention Rsi: {rendering}");
            assert!(rendering.contains("Lt"), "must mention Lt: {rendering}");
            assert!(rendering.contains("30"), "must mention 30: {rendering}");
        }
    }

    // ---- defensive: exits carry through structurally ------------------------

    #[test]
    fn exits_are_compiled_with_extracted_decimals() {
        let compiled = compile(&rsi_oversold()).expect("must compile");
        assert_eq!(
            compiled.exits()[0],
            CompiledExit::StopLoss {
                distance_pct: Decimal::new(5, 2),
            }
        );
        assert_eq!(
            compiled.exits()[1],
            CompiledExit::TakeProfit {
                target_r: Decimal::new(2, 0),
            }
        );
    }

    // ---- defensive: CompileError is constructible/inspectable --------------

    #[test]
    fn compile_error_displays() {
        let err = CompileError::UnexpectedSweep {
            field: "risk.max_leverage".to_owned(),
        };
        assert!(err.to_string().contains("risk.max_leverage"));
    }
}
