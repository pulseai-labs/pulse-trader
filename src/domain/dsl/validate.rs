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

use super::condition::{Comparator, Condition};
use super::exit::ExitRule;
use super::strategy::StrategyDsl;
use super::sweepable::SweepableValue;
use super::value::{IndicatorSpec, PriceField, ValueSource};
use crate::domain::DSL_SCHEMA_VERSION;

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
    /// A leaf condition that can never evaluate true on any bar (rule 9, G2).
    ///
    /// Judged per leaf: a same-series price pair the OHLC invariant rules out,
    /// a same-operand strict comparison or cross, or a both-`Constant`
    /// comparison that is false. Never judged under `Not` (a negation of an
    /// impossible leaf is satisfiable), across `Series::Primary`/`Series::Htf`
    /// (different bars), or on non-invariant fields (`Volume`, indicators).
    ImpossibleCondition,
    /// A raw-document object key the deserialized value cannot express (r3.s2
    /// write-path strictness): present in the submitted/serialized raw JSON at
    /// some path, absent from the re-serialized parsed value at the same path.
    /// Refused on every WRITE prelude; never enforced on the lenient read path.
    UnknownField,
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

    // Rules 1/2/6/9 over the condition tree: entry + every filter.
    check_condition(&dsl.entry, "entry", &mut errors, true);
    for (i, filter) in dsl.filters.iter().enumerate() {
        check_condition(filter, &format!("filters[{i}]"), &mut errors, true);
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
///
/// `judge` carries rule 9's reach (G2): a leaf is judged where it sits under
/// the entry, a filter, a signal exit, or an `And` chain; under an `Or` the
/// children are NOT judged (the `Or` is judged as a whole), and under a `Not`
/// nothing is judged at all.
fn check_condition(cond: &Condition, path: &str, errors: &mut Vec<FieldError>, judge: bool) {
    match cond {
        Condition::Compare { lhs, op: _, rhs } => {
            check_value_source(lhs, &format!("{path}.lhs"), errors);
            check_value_source(rhs, &format!("{path}.rhs"), errors);
            if judge && let Some(message) = impossibility(cond) {
                errors.push(FieldError {
                    path: path.to_owned(),
                    code: ValidationCode::ImpossibleCondition,
                    message,
                });
            }
        }
        Condition::CrossesAbove { lhs, rhs } | Condition::CrossesBelow { lhs, rhs } => {
            // Rule 2: a cross needs ≥1 series operand (Price/Indicator). The
            // both-Constant cross reports ONLY DegenerateCross — never also
            // rule 9 (no double report).
            if is_constant(lhs) && is_constant(rhs) {
                errors.push(FieldError {
                    path: cross_path(cond, path),
                    code: ValidationCode::DegenerateCross,
                    message: "a cross comparison needs at least one Price or Indicator operand; \
                              both operands are Constant"
                        .to_owned(),
                });
            } else if judge && let Some(message) = impossibility(cond) {
                errors.push(FieldError {
                    path: cross_path(cond, path),
                    code: ValidationCode::ImpossibleCondition,
                    message,
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
                check_condition(sub, &format!("{path}.and[{i}]"), errors, judge);
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
            // An impossible child of an Or is legal on its own — never report
            // one per branch; the Or is refused once, at its own node, when
            // EVERY branch is impossible (G2).
            for (i, sub) in conditions.iter().enumerate() {
                check_condition(sub, &format!("{path}.or[{i}]"), errors, false);
            }
            if judge && let Some(message) = impossibility(cond) {
                errors.push(FieldError {
                    path: format!("{path}.or"),
                    code: ValidationCode::ImpossibleCondition,
                    message,
                });
            }
        }
        Condition::Not { condition } => {
            // G2: a leaf under a `Not` is never judged — the negation of an
            // impossible leaf is trivially satisfiable.
            check_condition(condition, &format!("{path}.not"), errors, false);
        }
    }
}

/// Rule 9 (G2): why a [`Condition`] subtree can never evaluate true, if it can
/// never. `None` — possible, or not judged.
///
/// A leaf is judged per the impossibility table (same-series OHLC invariant,
/// same-operand strict comparisons and crosses, false both-`Constant`
/// comparisons). An `And` is impossible when any child is; an `Or` only when
/// every child is; a `Not` is never descended into.
fn impossibility(cond: &Condition) -> Option<String> {
    match cond {
        Condition::Compare { lhs, op, rhs } => impossible_compare(lhs, *op, rhs),
        Condition::CrossesAbove { lhs, rhs } => impossible_cross(lhs, rhs, true),
        Condition::CrossesBelow { lhs, rhs } => impossible_cross(lhs, rhs, false),
        Condition::And { conditions } => conditions.iter().find_map(impossibility),
        Condition::Or { conditions } => {
            let reasons: Option<Vec<String>> = conditions.iter().map(impossibility).collect();
            reasons.map(|reasons| {
                format!(
                    "every branch of this Or can never be true ({})",
                    reasons.join("; ")
                )
            })
        }
        Condition::Not { .. } => None,
    }
}

/// The impossibility table for one `Compare` leaf (rule 9).
fn impossible_compare(lhs: &ValueSource, op: Comparator, rhs: &ValueSource) -> Option<String> {
    let a = describe(lhs);
    let b = describe(rhs);
    // The same operand on both sides of a STRICT comparison: the two sides are
    // equal on every bar, so `>`/`<` can never hold — for any operand variant,
    // including two identical `Indicator`s.
    if matches!(op, Comparator::Gt | Comparator::Lt) && lhs == rhs {
        let word = if op == Comparator::Gt {
            "above"
        } else {
            "below"
        };
        return Some(format!("{a} can never be {word} itself"));
    }
    // Same-series price pairs the OHLC invariant rules out:
    // low ≤ open, low ≤ close, low ≤ high, open ≤ high, close ≤ high.
    if let (
        ValueSource::Price {
            series: s1,
            field: f1,
        },
        ValueSource::Price {
            series: s2,
            field: f2,
        },
    ) = (lhs, rhs)
        && s1 == s2
    {
        if op == Comparator::Gt && provably_le(*f1, *f2) {
            return Some(format!("{a} can never be above {b} on the same bar"));
        }
        if op == Comparator::Lt && provably_le(*f2, *f1) {
            return Some(format!("{a} can never be below {b} on the same bar"));
        }
        // Gte/Lte/Eq over prices stay unjudged: a flat bar makes `low >= high`
        // and `high == low` possible.
        return None;
    }
    // Two constants: the literal comparison is decidable — refused when false.
    if let (ValueSource::Constant { value: l }, ValueSource::Constant { value: r }) = (lhs, rhs) {
        let holds = match op {
            Comparator::Gt => l > r,
            Comparator::Gte => l >= r,
            Comparator::Lt => l < r,
            Comparator::Lte => l <= r,
            Comparator::Eq => l == r,
        };
        if !holds {
            return Some(format!(
                "the constant comparison {a} {} {b} is never true",
                op_text(op)
            ));
        }
    }
    None
}

/// The impossibility table for one `CrossesAbove`/`CrossesBelow` leaf (rule 9):
/// a cross needs `lhs` strictly beyond `rhs` on some bar, so a same-operand
/// cross — or one where the same-series invariant pins `lhs` on the wrong side
/// of `rhs` on every bar — can never fire.
fn impossible_cross(lhs: &ValueSource, rhs: &ValueSource, above: bool) -> Option<String> {
    let a = describe(lhs);
    let b = describe(rhs);
    let word = if above { "above" } else { "below" };
    if lhs == rhs {
        return Some(format!("{a} can never cross {word} itself"));
    }
    if let (
        ValueSource::Price {
            series: s1,
            field: f1,
        },
        ValueSource::Price {
            series: s2,
            field: f2,
        },
    ) = (lhs, rhs)
        && s1 == s2
    {
        let wrong_side = if above {
            provably_le(*f1, *f2)
        } else {
            provably_le(*f2, *f1)
        };
        if wrong_side {
            return Some(format!(
                "{a} can never cross {word} {b}: it is never {word} {b} on any bar"
            ));
        }
    }
    None
}

/// Whether the OHLC invariant (`low ≤ open ≤ high`, `low ≤ close ≤ high`) puts
/// `a` at or below `b` on every bar of the same series. Exactly the five
/// ordered pairs it licenses — nothing else (not `open` vs `close`: a flat bar
/// is possible; not `Volume`; never a price against an indicator, and never
/// across `Series::Primary` and `Series::Htf` — different bars).
fn provably_le(a: PriceField, b: PriceField) -> bool {
    use PriceField::{Close, High, Low, Open};
    matches!((a, b), (Low, Open | Close | High) | (Open | Close, High))
}

/// A trader-terms name for a [`ValueSource`] leaf, used in refusal messages.
fn describe(v: &ValueSource) -> String {
    match v {
        ValueSource::Constant { value } => format!("the constant {value}"),
        ValueSource::Price { field, .. } => field_name(*field).to_owned(),
        ValueSource::Indicator { spec, .. } => format!("the {} value", spec_name(spec)),
    }
}

/// The wire-independent lowercase name of a [`PriceField`].
fn field_name(field: PriceField) -> &'static str {
    match field {
        PriceField::Open => "open",
        PriceField::High => "high",
        PriceField::Low => "low",
        PriceField::Close => "close",
        PriceField::Volume => "volume",
    }
}

/// A short trader-terms name for an [`IndicatorSpec`].
fn spec_name(spec: &IndicatorSpec) -> String {
    match spec {
        IndicatorSpec::Rsi { period } => format!("RSI({})", period_value(period)),
        IndicatorSpec::Ema { period } => format!("EMA({})", period_value(period)),
        IndicatorSpec::Adx { period } => format!("ADX({})", period_value(period)),
        IndicatorSpec::Macd { .. } => "MACD".to_owned(),
        IndicatorSpec::Atr { period } => format!("ATR({})", period_value(period)),
    }
}

/// The rendered period of a sweepable indicator period.
fn period_value(period: &SweepableValue<u32>) -> String {
    match period {
        SweepableValue::Fixed(n) => n.to_string(),
        SweepableValue::Sweep { .. } => "sweep".to_owned(),
    }
}

/// The symbol for a [`Comparator`], as a trader would write it.
fn op_text(op: Comparator) -> &'static str {
    match op {
        Comparator::Gt => ">",
        Comparator::Gte => ">=",
        Comparator::Lt => "<",
        Comparator::Lte => "<=",
        Comparator::Eq => "==",
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
        IndicatorSpec::Macd {
            fast, slow, signal, ..
        } => {
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
                check_condition(condition, &format!("{base}.condition"), errors, true);
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

/// The write-path strictness check (r3.s2): reject raw-document object keys
/// the parsed [`StrategyDsl`] cannot express.
///
/// Runs ONLY on the write preludes — MCP [`submit_agent_version`](crate::submit_agent_version),
/// `strategy_repo::load_agent_document`, and `strategy_repo::create_version` —
/// NEVER on [`Migrator::load`](super::migrate::Migrator::load) or the repo's
/// `row_to_version` read defense: the read path stays lenient so pre-r3
/// database rows survive engine upgrades (ADR-0024's forward-compatibility
/// refusal would otherwise be the only protection, and it cannot see
/// same-version typos).
///
/// Compares the raw document's object keys, recursively, against the keys of
/// the re-serialized parsed value at the same path. A key present in raw and
/// absent from the re-serialization is an [`ValidationCode::UnknownField`]
/// error at that dotted/indexed path. Serde tags (`type`, `indicator`) and
/// defaulted fields are present in the re-serialization, so they pass;
/// defaulted fields MISSING from raw are fine — only raw-only keys are
/// errors.
///
/// # Errors
///
/// [`ValidationErrors`] collecting every unknown field, in raw-document
/// traversal order.
///
/// # Migration caveat
///
/// The comparison is against the value the caller holds after the migration
/// produced the parsed document. For identity migrations (1.0.0/1.1.0 →
/// 1.2.0) the raw keys equal the migrated keys, so comparing the submitted
/// raw document directly is correct. A FUTURE migration that RENAMES a key
/// must pass the migrated raw value (not the verbatim input), or the old
/// name would be flagged as unknown.
pub fn check_unknown_fields(
    raw: &serde_json::Value,
    parsed: &StrategyDsl,
) -> Result<(), ValidationErrors> {
    // Unreachable for a parsed `StrategyDsl`: serialization of these types
    // can only fail on a non-finite `Decimal`, and `rust_decimal::Decimal`
    // has no non-finite representation — so the re-serialization is total.
    #[allow(clippy::expect_used)]
    let current = serde_json::to_value(parsed).expect("a parsed StrategyDsl serializes");
    let mut errors = Vec::new();
    walk_unknown_fields(raw, &current, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationErrors { errors })
    }
}

/// Recursive key walk: objects compare key-by-key at the same path; arrays
/// compare element-by-element (`path[i]`); scalars carry no keys. A raw-only
/// key (or raw-only array element) is one [`ValidationCode::UnknownField`]
/// error; the walk CONTINUES so every unknown field is reported at once.
fn walk_unknown_fields(
    raw: &serde_json::Value,
    current: &serde_json::Value,
    path: &str,
    errors: &mut Vec<FieldError>,
) {
    match (raw, current) {
        (serde_json::Value::Object(raw_map), serde_json::Value::Object(cur_map)) => {
            for (key, raw_child) in raw_map {
                if let Some(cur_child) = cur_map.get(key) {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk_unknown_fields(raw_child, cur_child, &child_path, errors);
                } else {
                    let child_path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    errors.push(FieldError {
                        path: child_path,
                        code: ValidationCode::UnknownField,
                        message: format!(
                            "unknown field `{key}` is not part of DSL schema {DSL_SCHEMA_VERSION}"
                        ),
                    });
                }
            }
        }
        (serde_json::Value::Array(raw_arr), serde_json::Value::Array(cur_arr)) => {
            for (i, raw_child) in raw_arr.iter().enumerate() {
                if let Some(cur_child) = cur_arr.get(i) {
                    let child_path = format!("{path}[{i}]");
                    walk_unknown_fields(raw_child, cur_child, &child_path, errors);
                } else {
                    errors.push(FieldError {
                        path: format!("{path}[{i}]"),
                        code: ValidationCode::UnknownField,
                        message: format!(
                            "unknown element at index {i}: not part of DSL schema {DSL_SCHEMA_VERSION}"
                        ),
                    });
                }
            }
        }
        // Scalars (and raw/current kind mismatches — unreachable after a
        // successful deserialization) carry no keys to compare.
        _ => {}
    }
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
    use crate::domain::dsl::value::{IndicatorSpec, MacdOutput, Series, ValueSource};
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
                    output: MacdOutput::Line,
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

    // -- rule 9 (ImpossibleCondition): the judgment helper's corners ----------

    use crate::domain::dsl::value::PriceField;

    fn price(field: PriceField) -> ValueSource {
        ValueSource::Price {
            series: Series::Primary,
            field,
        }
    }

    fn htf_price(field: PriceField) -> ValueSource {
        ValueSource::Price {
            series: Series::Htf,
            field,
        }
    }

    fn rsi(period: u32) -> ValueSource {
        ValueSource::Indicator {
            series: Series::Primary,
            spec: IndicatorSpec::Rsi {
                period: SweepableValue::Fixed(period),
            },
        }
    }

    fn const_(mantissa: i64) -> ValueSource {
        ValueSource::Constant {
            value: Decimal::new(mantissa, 0),
        }
    }

    fn cmp(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
        Condition::Compare { lhs, op, rhs }
    }

    fn crosses(kind: CrossKind, lhs: ValueSource, rhs: ValueSource) -> Condition {
        match kind {
            CrossKind::Above => Condition::CrossesAbove { lhs, rhs },
            CrossKind::Below => Condition::CrossesBelow { lhs, rhs },
        }
    }

    #[derive(Clone, Copy)]
    enum CrossKind {
        Above,
        Below,
    }

    fn entry_dsl(entry: Condition) -> StrategyDsl {
        let mut s = valid_base();
        s.entry = entry;
        s
    }

    fn impossible_errs(s: &StrategyDsl) -> Vec<FieldError> {
        validate(s)
            .expect_err("refused")
            .into_errors()
            .into_iter()
            .filter(|e| e.code == ValidationCode::ImpossibleCondition)
            .collect()
    }

    /// A same-operand strict comparison is impossible for ANY operand variant,
    /// including an `Indicator` — the two sides are equal on every bar, so
    /// `>`/`<` can never hold (G2).
    #[test]
    fn same_operand_indicator_strict_compare_is_impossible() {
        let s = entry_dsl(cmp(rsi(14), Comparator::Gt, rsi(14)));
        let errs = impossible_errs(&s);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].path, "entry");
    }

    /// `Gte`/`Lte`/`Eq` are only judged on two constants: a same-operand or
    /// same-series non-strict comparison stays possible (a flat bar makes
    /// `low >= high` true), and an always-true leaf is out of scope (G2).
    #[test]
    fn non_strict_comparisons_are_only_judged_on_two_constants() {
        for op in [Comparator::Gte, Comparator::Lte, Comparator::Eq] {
            // Same operand, same-series prices, indicator pairs: accepted.
            for entry in [
                cmp(rsi(14), op, rsi(14)),
                cmp(price(PriceField::Low), op, price(PriceField::High)),
                cmp(price(PriceField::High), op, price(PriceField::Low)),
            ] {
                validate(&entry_dsl(entry))
                    .unwrap_or_else(|e| panic!("{op:?} over non-constants must be judged: {e:?}"));
            }
            // Two constants: refused only when the comparison is false.
            let false_pair = if op == Comparator::Lte {
                cmp(const_(2), op, const_(1))
            } else {
                // Gt, Gte, Eq over (1, 2) are all false.
                cmp(const_(1), op, const_(2))
            };
            let errs = impossible_errs(&entry_dsl(false_pair));
            assert_eq!(errs.len(), 1, "{op:?} over a false constant pair: {errs:?}");
            assert_eq!(errs[0].path, "entry");

            let true_pair = if op == Comparator::Eq {
                cmp(const_(1), op, const_(1))
            } else if op == Comparator::Lte {
                cmp(const_(1), op, const_(2))
            } else {
                // Gt, Gte over (2, 1) are both true.
                cmp(const_(2), op, const_(1))
            };
            validate(&entry_dsl(true_pair))
                .unwrap_or_else(|e| panic!("{op:?} over a true constant pair must pass: {e:?}"));
        }
    }

    /// Operands on different series are different bars: never judged (G2),
    /// in either direction.
    #[test]
    fn cross_series_price_operands_are_never_judged() {
        for entry in [
            cmp(
                price(PriceField::Close),
                Comparator::Gt,
                htf_price(PriceField::High),
            ),
            cmp(
                htf_price(PriceField::High),
                Comparator::Gt,
                price(PriceField::Close),
            ),
            cmp(
                price(PriceField::Close),
                Comparator::Lt,
                htf_price(PriceField::Low),
            ),
        ] {
            validate(&entry_dsl(entry)).expect("cross-series leaves are never judged");
        }
    }

    /// `Volume` sits in no OHLC invariant: a strict comparison against a price
    /// field is never judged.
    #[test]
    fn volume_is_never_judged() {
        let s = entry_dsl(cmp(
            price(PriceField::Volume),
            Comparator::Gt,
            price(PriceField::High),
        ));
        validate(&s).expect("volume against a price field is not judged");
    }

    /// `Not` blocks judgment at any depth — a negation of an impossible leaf
    /// is satisfiable (G2) — including through `And`/`Or` inside the `Not`.
    #[test]
    fn not_blocks_judgment_at_depth() {
        for entry in [
            Condition::Not {
                condition: Box::new(cmp(
                    price(PriceField::Close),
                    Comparator::Gt,
                    price(PriceField::High),
                )),
            },
            Condition::Not {
                condition: Box::new(Condition::Or {
                    conditions: vec![
                        cmp(
                            price(PriceField::Close),
                            Comparator::Gt,
                            price(PriceField::High),
                        ),
                        cmp(
                            price(PriceField::Low),
                            Comparator::Gt,
                            price(PriceField::High),
                        ),
                    ],
                }),
            },
        ] {
            validate(&entry_dsl(entry)).expect("nothing under Not is judged");
        }
    }

    /// An `Or` whose branch is an `And` with one impossible leaf: the branch is
    /// impossible, so an all-impossible `Or` is refused at the `Or`'s node.
    #[test]
    fn or_branch_may_be_an_and_with_one_impossible_leaf() {
        let s = entry_dsl(Condition::Or {
            conditions: vec![
                Condition::And {
                    conditions: vec![
                        cmp(rsi(14), Comparator::Lt, const_(30)),
                        cmp(
                            price(PriceField::Close),
                            Comparator::Gt,
                            price(PriceField::High),
                        ),
                    ],
                },
                cmp(
                    price(PriceField::Low),
                    Comparator::Gt,
                    price(PriceField::High),
                ),
            ],
        });
        let errs = impossible_errs(&s);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert_eq!(errs[0].path, "entry.or");
    }

    /// The both-`Constant` cross stays `DegenerateCross` and emits NO
    /// `ImpossibleCondition` — the no-double-report guarantee (G2).
    #[test]
    fn both_constant_cross_is_degenerate_only() {
        for entry in [
            crosses(CrossKind::Above, const_(1), const_(2)),
            crosses(CrossKind::Below, const_(2), const_(1)),
        ] {
            let errs = validate(&entry_dsl(entry))
                .expect_err("the both-Constant cross is refused")
                .into_errors();
            assert_eq!(errs.len(), 1, "exactly one error: {errs:?}");
            assert_eq!(errs[0].code, ValidationCode::DegenerateCross);
            assert!(!has_code(&errs, ValidationCode::ImpossibleCondition));
        }
    }

    /// The new code serializes under its variant name, like the existing
    /// variants (the wire shape is unchanged — no rename).
    #[test]
    fn impossible_condition_code_serializes_by_variant_name() {
        let e = FieldError {
            path: "entry".to_owned(),
            code: ValidationCode::ImpossibleCondition,
            message: "close can never be above high on the same bar".to_owned(),
        };
        let json = serde_json::to_string(&e).expect("serialize FieldError");
        assert!(json.contains("ImpossibleCondition"), "{json}");
        let back: FieldError = serde_json::from_str(&json).expect("deserialize FieldError");
        assert_eq!(back, e);
    }
}
