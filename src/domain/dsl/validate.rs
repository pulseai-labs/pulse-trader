//! Semantic validation of a [`StrategyDsl`] — the 2.03 correctable-rejection
//! engine (FR-3).
//!
//! 2.01/2.02 guarantee a `StrategyDsl` is *structurally* sound (it deserialized)
//! but not *meaningful*. [`validate`] adds the **semantic** pass: it either
//! returns a [`ValidatedDsl`] newtype (the only thing 2.04's `compile` will
//! accept) or a **collection of field-pathed, correctable errors**
//! ([`ValidationErrors`]).
//!
//! **Collect-all, not fail-fast (grill 2026-06-09).** A single pass returns
//! *every* violation as a flat, traversal-order `Vec<FieldError>` (unbounded — a
//! strategy is small). demo-1's "correctable" UX means surfacing every problem
//! at once, not one-at-a-time.
//!
//! **Recurses to arbitrary depth (architect-critic C1).** Rules over
//! [`Condition`]s apply anywhere they nest — `entry`, every `filters[i]`, and all
//! sub-conditions inside `And`/`Or`/`Not`. Each [`FieldError::path`] is a
//! dotted/indexed locator that reflects the nesting (e.g.
//! `entry.and[0].not.lhs.indicator.rsi.period`, `exits[0].distance_pct`) so a
//! UI/LLM can point at the offending field.
//!
//! **Decimal-fraction convention (2.02).** `distance_pct`/`trail_pct` are
//! fractions in `(0, 1)`; `risk_per_trade_pct` is a fraction in `(0, 1]`;
//! `target_r` is a plain R-multiple `> 0`; `max_leverage` is a plain multiplier
//! `>= 1`. A `Sweep` value short-circuits to [`ValidationCode::SweepUnsupported`]
//! for that field (no spurious range error for the same field).
//!
//! Zero-I/O, no migration (2.05), no compilation (2.04) — this operates on an
//! already-deserialized `StrategyDsl`.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::condition::Condition;
use super::exit::ExitRule;
use super::strategy::StrategyDsl;
use super::sweepable::SweepableValue;
use super::value::{IndicatorSpec, ValueSource};

/// A machine-actionable classification of a single semantic violation.
///
/// `#[derive(Serialize, Deserialize, PartialEq)]` — crosses the Tauri boundary
/// later (mirrors VS-1.1.1's `ValidationError` style). `#[non_exhaustive]` so
/// downstream additive rules don't break match arms in consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ValidationCode {
    /// A `SweepableValue::Sweep` was used; v1 is fixed-only (rule 1).
    SweepUnsupported,
    /// A `CrossesAbove`/`CrossesBelow` with both operands `Constant` (rule 2).
    DegenerateCross,
    /// A `TakeProfit` with no `StopLoss` in the same strategy (rule 3).
    TakeProfitWithoutStop,
    /// The strategy has no exit rules (rule 4).
    NoExit,
    /// More than one of an exclusive exit kind
    /// (`StopLoss`/`TakeProfit`/`TrailingStop`/`TimeStop`) (rule 5).
    DuplicateExit,
    /// A numeric field is out of its declared bounds (rule 6).
    FieldRange,
    /// The strategy `name` is empty or whitespace-only (rule 7).
    EmptyName,
    /// An empty `And`/`Or` conjunction (vacuously true/false) (rule 8).
    EmptyConjunction,
}

/// A single field-level, correctable validation error.
///
/// `path` is a dotted/indexed locator (`entry.lhs.indicator.rsi.period`,
/// `exits[0].distance_pct`, `risk.risk_per_trade_pct`) so a UI/LLM can point at
/// the offending field; `code` is the typed classification; `message` is the
/// human/LLM-correctable text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldError {
    /// Dotted/indexed locator of the offending field.
    pub path: String,
    /// The machine-actionable classification.
    pub code: ValidationCode,
    /// Human/LLM-correctable description.
    pub message: String,
}

/// A guaranteed-non-empty collection of [`FieldError`]s — the `Err` arm of
/// [`validate`].
///
/// Construction is private to this module ([`validate`] only produces a
/// `ValidationErrors` when it has gathered at least one error), so the
/// non-empty invariant holds by construction. serde-serializable to cross the
/// Tauri boundary later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("strategy validation failed with {} error(s)", .errors.len())]
pub struct ValidationErrors {
    errors: Vec<FieldError>,
}

impl ValidationErrors {
    /// The field errors, in traversal order. Guaranteed non-empty.
    #[must_use]
    pub fn errors(&self) -> &[FieldError] {
        &self.errors
    }

    /// Consume into the owned, guaranteed-non-empty `Vec<FieldError>`.
    #[must_use]
    pub fn into_errors(self) -> Vec<FieldError> {
        self.errors
    }
}

/// A [`StrategyDsl`] that has passed semantic [`validate`]ion.
///
/// **Constructible ONLY via [`validate`]** — the inner field is private and
/// there is no public constructor. This is the type-level guarantee 2.04's
/// `compile(ValidatedDsl)` relies on: "compile an unvalidated strategy" becomes
/// a *compile* error, not a convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedDsl {
    inner: StrategyDsl,
}

impl ValidatedDsl {
    /// Borrow the validated strategy document.
    #[must_use]
    pub fn dsl(&self) -> &StrategyDsl {
        &self.inner
    }

    /// Consume into the owned, validated [`StrategyDsl`].
    #[must_use]
    pub fn into_inner(self) -> StrategyDsl {
        self.inner
    }
}

/// Semantically validate a [`StrategyDsl`].
///
/// Returns [`ValidatedDsl`] if the document satisfies every rule, otherwise a
/// guaranteed-non-empty [`ValidationErrors`] carrying **all** violations in
/// traversal order (collect-all, not fail-fast).
///
/// # Errors
///
/// Returns [`ValidationErrors`] when the strategy violates any of the eight
/// semantic rules (sweep-reject, degenerate cross, take-profit-without-stop,
/// no-exit, duplicate-exit, field-range, empty-name, empty-conjunction).
pub fn validate(dsl: &StrategyDsl) -> Result<ValidatedDsl, ValidationErrors> {
    let mut errors: Vec<FieldError> = Vec::new();

    // Rule 7: non-empty name.
    if dsl.name.trim().is_empty() {
        errors.push(FieldError {
            path: "name".to_owned(),
            code: ValidationCode::EmptyName,
            message: "strategy name must not be empty or whitespace-only".to_owned(),
        });
    }

    // Rules 1/2/6 over the condition tree: entry + every filter.
    check_condition(&dsl.entry, "entry", &mut errors);
    for (i, filter) in dsl.filters.iter().enumerate() {
        check_condition(filter, &format!("filters[{i}]"), &mut errors);
    }

    // Rules 1/3/4/5/6 over the exits.
    check_exits(&dsl.exits, &mut errors);

    // Rule 6 over the risk params.
    check_risk(&dsl.risk, &mut errors);

    if errors.is_empty() {
        Ok(ValidatedDsl { inner: dsl.clone() })
    } else {
        Err(ValidationErrors { errors })
    }
}

/// Recursively validate a `Condition` subtree, threading the nesting-aware path.
fn check_condition(cond: &Condition, path: &str, errors: &mut Vec<FieldError>) {
    match cond {
        Condition::Compare { lhs, op: _, rhs } => {
            check_value_source(lhs, &format!("{path}.lhs"), errors);
            check_value_source(rhs, &format!("{path}.rhs"), errors);
        }
        Condition::CrossesAbove { lhs, rhs } | Condition::CrossesBelow { lhs, rhs } => {
            // Rule 2: a cross needs ≥1 series operand (Price/Indicator).
            if is_constant(lhs) && is_constant(rhs) {
                errors.push(FieldError {
                    path: cross_path(cond, path),
                    code: ValidationCode::DegenerateCross,
                    message: "a cross comparison needs at least one Price or Indicator operand; \
                              both operands are Constant"
                        .to_owned(),
                });
            }
            check_value_source(lhs, &format!("{path}.lhs"), errors);
            check_value_source(rhs, &format!("{path}.rhs"), errors);
        }
        Condition::And { conditions } => {
            // Rule 8: reject an empty And (vacuously true — "always fires").
            if conditions.is_empty() {
                errors.push(FieldError {
                    path: format!("{path}.and"),
                    code: ValidationCode::EmptyConjunction,
                    message: "an empty `And` is vacuously true (always fires); add at least one \
                              condition"
                        .to_owned(),
                });
            }
            for (i, sub) in conditions.iter().enumerate() {
                check_condition(sub, &format!("{path}.and[{i}]"), errors);
            }
        }
        Condition::Or { conditions } => {
            // Rule 8: reject an empty Or (vacuously false — "never fires").
            if conditions.is_empty() {
                errors.push(FieldError {
                    path: format!("{path}.or"),
                    code: ValidationCode::EmptyConjunction,
                    message: "an empty `Or` is vacuously false (never fires); add at least one \
                              condition"
                        .to_owned(),
                });
            }
            for (i, sub) in conditions.iter().enumerate() {
                check_condition(sub, &format!("{path}.or[{i}]"), errors);
            }
        }
        Condition::Not { condition } => {
            check_condition(condition, &format!("{path}.not"), errors);
        }
    }
}

/// The path suffix for a cross condition's degenerate-cross error.
fn cross_path(cond: &Condition, path: &str) -> String {
    match cond {
        Condition::CrossesAbove { .. } => format!("{path}.crosses_above"),
        Condition::CrossesBelow { .. } => format!("{path}.crosses_below"),
        _ => path.to_owned(),
    }
}

/// Whether a `ValueSource` is a literal constant (no series operand).
fn is_constant(v: &ValueSource) -> bool {
    matches!(v, ValueSource::Constant { .. })
}

/// Validate a `ValueSource` — only `Indicator` carries sweepable period fields
/// (rules 1/6); `Constant`/`Price` carry no validatable numeric leaf. `series`
/// is total over its two values at deserialization and carries **no** rule —
/// which series a run loads is an engine concern (w2's typed `HtfRequired`), a
/// document is valid independent of it.
fn check_value_source(v: &ValueSource, path: &str, errors: &mut Vec<FieldError>) {
    if let ValueSource::Indicator { spec, .. } = v {
        check_indicator(spec, &format!("{path}.indicator"), errors);
    }
}

/// Validate an `IndicatorSpec`'s period fields (rules 1 + 6: periods > 0; MACD
/// fast < slow).
fn check_indicator(spec: &IndicatorSpec, path: &str, errors: &mut Vec<FieldError>) {
    match spec {
        IndicatorSpec::Rsi { period } => {
            check_u32_positive(period, &format!("{path}.rsi.period"), "RSI period", errors);
        }
        IndicatorSpec::Ema { period } => {
            check_u32_positive(period, &format!("{path}.ema.period"), "EMA period", errors);
        }
        IndicatorSpec::Adx { period } => {
            check_u32_positive(period, &format!("{path}.adx.period"), "ADX period", errors);
        }
        IndicatorSpec::Macd { fast, slow, signal } => {
            check_u32_positive(fast, &format!("{path}.macd.fast"), "MACD fast", errors);
            check_u32_positive(slow, &format!("{path}.macd.slow"), "MACD slow", errors);
            check_u32_positive(
                signal,
                &format!("{path}.macd.signal"),
                "MACD signal",
                errors,
            );
            // MACD fast < slow — only checkable when both are Fixed.
            if let (SweepableValue::Fixed(f), SweepableValue::Fixed(s)) = (fast, slow)
                && f >= s
            {
                errors.push(FieldError {
                    path: format!("{path}.macd.fast"),
                    code: ValidationCode::FieldRange,
                    message: format!(
                        "MACD fast period ({f}) must be strictly less than slow period ({s})"
                    ),
                });
            }
        }
        IndicatorSpec::Atr { period } => {
            check_u32_positive(period, &format!("{path}.atr.period"), "ATR period", errors);
        }
    }
}

/// Rule 1 + rule 6 for a `SweepableValue<u32>` that must be `> 0`.
fn check_u32_positive(
    v: &SweepableValue<u32>,
    path: &str,
    label: &str,
    errors: &mut Vec<FieldError>,
) {
    match v {
        SweepableValue::Sweep { .. } => push_sweep(path, errors),
        SweepableValue::Fixed(n) => {
            if *n == 0 {
                errors.push(FieldError {
                    path: path.to_owned(),
                    code: ValidationCode::FieldRange,
                    message: format!("{label} must be greater than 0"),
                });
            }
        }
    }
}

/// Validate the exits list (rules 1/3/4/5/6).
fn check_exits(exits: &[ExitRule], errors: &mut Vec<FieldError>) {
    // Rule 4: ≥1 exit.
    if exits.is_empty() {
        errors.push(FieldError {
            path: "exits".to_owned(),
            code: ValidationCode::NoExit,
            message: "a strategy must declare at least one exit rule".to_owned(),
        });
    }

    let mut has_stop = false;
    let mut has_take_profit = false;
    let mut count_stop = 0u32;
    let mut count_take_profit = 0u32;
    let mut count_trailing = 0u32;
    let mut count_time = 0u32;

    for (i, exit) in exits.iter().enumerate() {
        let base = format!("exits[{i}]");
        match exit {
            ExitRule::StopLoss { distance_pct } => {
                has_stop = true;
                count_stop += 1;
                // Rule 6: distance_pct in (0, 1).
                check_decimal_open_unit(
                    distance_pct,
                    &format!("{base}.distance_pct"),
                    "stop distance_pct",
                    errors,
                );
            }
            ExitRule::TakeProfit { target_r } => {
                has_take_profit = true;
                count_take_profit += 1;
                // Rule 6: target_r > 0.
                check_decimal_positive(
                    target_r,
                    &format!("{base}.target_r"),
                    "take-profit target_r",
                    errors,
                );
            }
            ExitRule::TrailingStop { trail_pct } => {
                count_trailing += 1;
                // Rule 6: trail_pct in (0, 1).
                check_decimal_open_unit(
                    trail_pct,
                    &format!("{base}.trail_pct"),
                    "trail_pct",
                    errors,
                );
            }
            ExitRule::TimeStop { max_bars } => {
                count_time += 1;
                // Rule 6: max_bars > 0.
                check_u32_positive(
                    max_bars,
                    &format!("{base}.max_bars"),
                    "time-stop max_bars",
                    errors,
                );
            }
            ExitRule::SignalExit { condition } => {
                check_condition(condition, &format!("{base}.condition"), errors);
            }
            ExitRule::AtrStop { period, multiple } => {
                // The ATR-multiple stop shares StopLoss's exclusive family
                // (rule 5) and satisfies rule 3's stop requirement.
                has_stop = true;
                count_stop += 1;
                // Rule 6: period > 0; multiple in (0, 10].
                check_u32_positive(period, &format!("{base}.period"), "ATR period", errors);
                check_decimal_atr_multiple(
                    multiple,
                    &format!("{base}.multiple"),
                    "atr-stop multiple",
                    errors,
                );
            }
        }
    }

    // Rule 3: TakeProfit requires a stop (StopLoss or AtrStop) in the same
    // strategy.
    if has_take_profit && !has_stop {
        errors.push(FieldError {
            path: "exits".to_owned(),
            code: ValidationCode::TakeProfitWithoutStop,
            message: "a TakeProfit exit requires a StopLoss or AtrStop in the same strategy (R is \
                      undefined without a stop)"
                .to_owned(),
        });
    }

    // Rule 5: no duplicate exclusive exits (StopLoss and AtrStop are ONE
    // exclusive family; multiple SignalExit allowed).
    push_dup(count_stop, "stop (StopLoss or AtrStop)", errors);
    push_dup(count_take_profit, "TakeProfit", errors);
    push_dup(count_trailing, "TrailingStop", errors);
    push_dup(count_time, "TimeStop", errors);
}

/// Push a [`ValidationCode::DuplicateExit`] when an exclusive exit kind appears
/// more than once.
fn push_dup(count: u32, kind: &str, errors: &mut Vec<FieldError>) {
    if count > 1 {
        errors.push(FieldError {
            path: "exits".to_owned(),
            code: ValidationCode::DuplicateExit,
            message: format!("at most one {kind} exit is allowed; found {count}"),
        });
    }
}

/// Validate the risk params (rule 6: `risk_per_trade_pct` in (0, 1];
/// `max_leverage` >= 1).
fn check_risk(risk: &super::risk::RiskParams, errors: &mut Vec<FieldError>) {
    // risk_per_trade_pct in (0, 1].
    match &risk.risk_per_trade_pct {
        SweepableValue::Sweep { .. } => push_sweep("risk.risk_per_trade_pct", errors),
        SweepableValue::Fixed(v) => {
            if *v <= Decimal::ZERO || *v > Decimal::ONE {
                errors.push(FieldError {
                    path: "risk.risk_per_trade_pct".to_owned(),
                    code: ValidationCode::FieldRange,
                    message: "risk_per_trade_pct must be a decimal fraction in the range (0, 1]"
                        .to_owned(),
                });
            }
        }
    }

    // max_leverage >= 1.
    match &risk.max_leverage {
        SweepableValue::Sweep { .. } => push_sweep("risk.max_leverage", errors),
        SweepableValue::Fixed(v) => {
            if *v < Decimal::ONE {
                errors.push(FieldError {
                    path: "risk.max_leverage".to_owned(),
                    code: ValidationCode::FieldRange,
                    message: "max_leverage must be at least 1".to_owned(),
                });
            }
        }
    }
}

/// Rule 6 for a `SweepableValue<Decimal>` that must lie in the open unit
/// interval `(0, 1)`.
fn check_decimal_open_unit(
    v: &SweepableValue<Decimal>,
    path: &str,
    label: &str,
    errors: &mut Vec<FieldError>,
) {
    match v {
        SweepableValue::Sweep { .. } => push_sweep(path, errors),
        SweepableValue::Fixed(d) => {
            if *d <= Decimal::ZERO || *d >= Decimal::ONE {
                errors.push(FieldError {
                    path: path.to_owned(),
                    code: ValidationCode::FieldRange,
                    message: format!("{label} must be a decimal fraction in the range (0, 1)"),
                });
            }
        }
    }
}

/// Rule 6 for a `SweepableValue<Decimal>` that must be strictly positive.
fn check_decimal_positive(
    v: &SweepableValue<Decimal>,
    path: &str,
    label: &str,
    errors: &mut Vec<FieldError>,
) {
    match v {
        SweepableValue::Sweep { .. } => push_sweep(path, errors),
        SweepableValue::Fixed(d) => {
            if *d <= Decimal::ZERO {
                errors.push(FieldError {
                    path: path.to_owned(),
                    code: ValidationCode::FieldRange,
                    message: format!("{label} must be greater than 0"),
                });
            }
        }
    }
}

/// Rule 6 for a `SweepableValue<Decimal>` that must lie in `(0, 10]` — the
/// `AtrStop` `multiple` bound (schema 1.1.0). A `Sweep` short-circuits to
/// [`ValidationCode::SweepUnsupported`] exactly as the other decimal checks do.
fn check_decimal_atr_multiple(
    v: &SweepableValue<Decimal>,
    path: &str,
    label: &str,
    errors: &mut Vec<FieldError>,
) {
    match v {
        SweepableValue::Sweep { .. } => push_sweep(path, errors),
        SweepableValue::Fixed(d) => {
            if *d <= Decimal::ZERO || *d > Decimal::TEN {
                errors.push(FieldError {
                    path: path.to_owned(),
                    code: ValidationCode::FieldRange,
                    message: format!("{label} must be an ATR multiple in the range (0, 10]"),
                });
            }
        }
    }
}

/// Push a [`ValidationCode::SweepUnsupported`] for a `Sweep` value at `path`
/// (rule 1). A `Sweep` short-circuits the field's range check (no spurious
/// `FieldRange` for the same field).
fn push_sweep(path: &str, errors: &mut Vec<FieldError>) {
    errors.push(FieldError {
        path: path.to_owned(),
        code: ValidationCode::SweepUnsupported,
        message: "parameter sweeps are not supported in v1; use a Fixed value".to_owned(),
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]
mod tests {
    use super::{FieldError, ValidationCode, validate};
    use crate::domain::dsl::condition::{Comparator, Condition};
    use crate::domain::dsl::exit::ExitRule;
    use crate::domain::dsl::risk::{Direction, RiskParams};
    use crate::domain::dsl::schema_version::SchemaVersion;
    use crate::domain::dsl::strategy::StrategyDsl;
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{IndicatorSpec, Series, ValueSource};
    use rust_decimal::Decimal;

    /// The canonical demo-1 RSI-oversold strategy (the 2.02 fixture): long when
    /// RSI(14) < 30, 5% stop, 2R take-profit, 1% risk/trade, 3x max leverage.
    fn rsi_oversold_strategy() -> StrategyDsl {
        StrategyDsl {
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
        }
    }

    /// A minimal valid strategy with a custom entry/exits; reused as a base to
    /// mutate into single-rule-violating fixtures.
    fn valid_base() -> StrategyDsl {
        rsi_oversold_strategy()
    }

    fn has_code(errs: &[FieldError], code: ValidationCode) -> bool {
        errs.iter().any(|e| e.code == code)
    }

    fn has_path(errs: &[FieldError], path: &str) -> bool {
        errs.iter().any(|e| e.path == path)
    }

    /// AC-5: the canonical RSI-oversold strategy validates to `Ok(ValidatedDsl)`.
    #[test]
    fn valid_rsi_oversold_strategy_validates() {
        let s = rsi_oversold_strategy();
        let validated = validate(&s).expect("canonical RSI-oversold strategy must validate");
        assert_eq!(validated.dsl(), &s);
    }

    /// AC-6: one case per rule 1–5 + rule 8 — each `Err` with the expected
    /// `ValidationCode`.
    #[test]
    fn rejects_each_semantic_rule() {
        // Rule 1: a Sweep anywhere → SweepUnsupported.
        let mut s = valid_base();
        s.entry = Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Sweep {
                        start: 5,
                        end: 20,
                        step: 5,
                    },
                },
            },
            op: Comparator::Lt,
            rhs: ValueSource::Constant {
                value: Decimal::new(30, 0),
            },
        };
        let errs = validate(&s).unwrap_err();
        assert!(
            has_code(errs.errors(), ValidationCode::SweepUnsupported),
            "rule 1 (Sweep) must reject: {:?}",
            errs.errors()
        );

        // Rule 2: a cross with both operands Constant → DegenerateCross.
        let mut s = valid_base();
        s.entry = Condition::CrossesAbove {
            lhs: ValueSource::Constant {
                value: Decimal::new(1, 0),
            },
            rhs: ValueSource::Constant {
                value: Decimal::new(2, 0),
            },
        };
        let errs = validate(&s).unwrap_err();
        assert!(
            has_code(errs.errors(), ValidationCode::DegenerateCross),
            "rule 2 (degenerate cross) must reject: {:?}",
            errs.errors()
        );

        // Rule 3: a TakeProfit with no StopLoss → TakeProfitWithoutStop.
        let mut s = valid_base();
        s.exits = vec![ExitRule::TakeProfit {
            target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
        }];
        let errs = validate(&s).unwrap_err();
        assert!(
            has_code(errs.errors(), ValidationCode::TakeProfitWithoutStop),
            "rule 3 (TP without SL) must reject: {:?}",
            errs.errors()
        );

        // Rule 4: empty exits → NoExit.
        let mut s = valid_base();
        s.exits = vec![];
        let errs = validate(&s).unwrap_err();
        assert!(
            has_code(errs.errors(), ValidationCode::NoExit),
            "rule 4 (no exit) must reject: {:?}",
            errs.errors()
        );

        // Rule 5: duplicate StopLoss → DuplicateExit.
        let mut s = valid_base();
        s.exits = vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
            },
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(3, 2)),
            },
        ];
        let errs = validate(&s).unwrap_err();
        assert!(
            has_code(errs.errors(), ValidationCode::DuplicateExit),
            "rule 5 (duplicate exit) must reject: {:?}",
            errs.errors()
        );

        // Rule 8: empty And → EmptyConjunction.
        let mut s = valid_base();
        s.entry = Condition::And { conditions: vec![] };
        let errs = validate(&s).unwrap_err();
        assert!(
            has_code(errs.errors(), ValidationCode::EmptyConjunction),
            "rule 8 (empty And) must reject: {:?}",
            errs.errors()
        );

        // Rule 8: empty Or → EmptyConjunction.
        let mut s = valid_base();
        s.entry = Condition::Or { conditions: vec![] };
        let errs = validate(&s).unwrap_err();
        assert!(
            has_code(errs.errors(), ValidationCode::EmptyConjunction),
            "rule 8 (empty Or) must reject: {:?}",
            errs.errors()
        );
    }

    /// AC-7: out-of-range field values → `Err` with the right code + field path.
    #[test]
    fn rejects_out_of_range_field_values() {
        // Zero RSI period.
        let mut s = valid_base();
        s.entry = Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(0),
                },
            },
            op: Comparator::Lt,
            rhs: ValueSource::Constant {
                value: Decimal::new(30, 0),
            },
        };
        let errs = validate(&s).unwrap_err();
        assert!(has_code(errs.errors(), ValidationCode::FieldRange));
        assert!(
            has_path(errs.errors(), "entry.lhs.indicator.rsi.period"),
            "zero RSI period path: {:?}",
            errs.errors()
        );

        // MACD fast >= slow.
        let mut s = valid_base();
        s.entry = Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Macd {
                    fast: SweepableValue::Fixed(26),
                    slow: SweepableValue::Fixed(12),
                    signal: SweepableValue::Fixed(9),
                },
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::new(0, 0),
            },
        };
        let errs = validate(&s).unwrap_err();
        assert!(has_code(errs.errors(), ValidationCode::FieldRange));
        assert!(
            has_path(errs.errors(), "entry.lhs.indicator.macd.fast"),
            "MACD fast>=slow path: {:?}",
            errs.errors()
        );

        // risk_per_trade_pct = 0.
        let mut s = valid_base();
        s.risk.risk_per_trade_pct = SweepableValue::Fixed(Decimal::ZERO);
        let errs = validate(&s).unwrap_err();
        assert!(has_path(errs.errors(), "risk.risk_per_trade_pct"));

        // risk_per_trade_pct > 1.
        let mut s = valid_base();
        s.risk.risk_per_trade_pct = SweepableValue::Fixed(Decimal::new(15, 1)); // 1.5
        let errs = validate(&s).unwrap_err();
        assert!(has_path(errs.errors(), "risk.risk_per_trade_pct"));
        assert!(has_code(errs.errors(), ValidationCode::FieldRange));

        // distance_pct = 0.
        let mut s = valid_base();
        s.exits = vec![ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(Decimal::ZERO),
        }];
        let errs = validate(&s).unwrap_err();
        assert!(has_path(errs.errors(), "exits[0].distance_pct"));
        assert!(has_code(errs.errors(), ValidationCode::FieldRange));

        // max_leverage < 1.
        let mut s = valid_base();
        s.risk.max_leverage = SweepableValue::Fixed(Decimal::new(5, 1)); // 0.5
        let errs = validate(&s).unwrap_err();
        assert!(has_path(errs.errors(), "risk.max_leverage"));
        assert!(has_code(errs.errors(), ValidationCode::FieldRange));

        // Empty name.
        let mut s = valid_base();
        s.name = "   ".to_owned();
        let errs = validate(&s).unwrap_err();
        assert!(has_path(errs.errors(), "name"));
        assert!(has_code(errs.errors(), ValidationCode::EmptyName));
    }

    /// AC-8: ≥2 violations, including one nested inside `And`/`Or`/`Not`, are all
    /// returned with correct (incl. nested) field paths.
    #[test]
    fn collects_all_errors_with_field_paths() {
        let mut s = valid_base();
        // Top-level violation: empty name.
        s.name = String::new();
        // Nested violation: entry = And[ Not( Compare RSI(0) < 30 ) ] — a zero
        // RSI period buried inside And[0].not.lhs.
        s.entry = Condition::And {
            conditions: vec![Condition::Not {
                condition: Box::new(Condition::Compare {
                    lhs: ValueSource::Indicator {
                        series: Series::Primary,
                        spec: IndicatorSpec::Rsi {
                            period: SweepableValue::Fixed(0),
                        },
                    },
                    op: Comparator::Lt,
                    rhs: ValueSource::Constant {
                        value: Decimal::new(30, 0),
                    },
                }),
            }],
        };

        let errs = validate(&s).unwrap_err();
        let errs = errs.errors();
        assert!(
            errs.len() >= 2,
            "expected ≥2 collected errors, got {}: {:?}",
            errs.len(),
            errs
        );
        // Top-level path.
        assert!(
            has_path(errs, "name"),
            "missing top-level `name` error: {errs:?}"
        );
        // Nested path proves recursion into And → Not → Compare → lhs.
        assert!(
            has_path(errs, "entry.and[0].not.lhs.indicator.rsi.period"),
            "missing nested path: {errs:?}"
        );
    }

    /// AC-9: `validate(&s).unwrap().into_inner() == s` for a valid `s`.
    #[test]
    fn validated_dsl_round_trips_inner() {
        let s = rsi_oversold_strategy();
        let inner = validate(&s).unwrap().into_inner();
        assert_eq!(inner, s);
    }

    /// A `Sweep` field short-circuits to `SweepUnsupported` and does NOT also
    /// emit a spurious `FieldRange` for the same field (spec §3).
    #[test]
    fn sweep_short_circuits_range_check() {
        let mut s = valid_base();
        s.risk.risk_per_trade_pct = SweepableValue::Sweep {
            start: Decimal::new(1, 2),
            end: Decimal::new(5, 2),
            step: Decimal::new(1, 2),
        };
        let errs = validate(&s).unwrap_err();
        let sweep_errs: Vec<_> = errs
            .errors()
            .iter()
            .filter(|e| e.path == "risk.risk_per_trade_pct")
            .collect();
        assert_eq!(
            sweep_errs.len(),
            1,
            "exactly one error for the field: {sweep_errs:?}"
        );
        assert_eq!(sweep_errs[0].code, ValidationCode::SweepUnsupported);
    }

    /// `FieldError`/`ValidationCode` serde round-trip (crosses the Tauri boundary
    /// later — mirrors VS-1.1.1's `ValidationError` style).
    #[test]
    fn field_error_serde_round_trips() {
        let e = FieldError {
            path: "entry.lhs.indicator.rsi.period".to_owned(),
            code: ValidationCode::FieldRange,
            message: "RSI period must be greater than 0".to_owned(),
        };
        let json = serde_json::to_string(&e).expect("serialize FieldError");
        let back: FieldError = serde_json::from_str(&json).expect("deserialize FieldError");
        assert_eq!(back, e);
    }
}
