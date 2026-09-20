//! The six server-validated builder tools (VS-1.3.2 work-2.02, FR-3 heart) +
//! the isolated flat-primitive → tagged-DSL mapping seam.
//!
//! Each tool parses a **flat-primitive** JSON argument (never a serde-tagged
//! [`Condition`](crate::domain::Condition)/[`ExitRule`](crate::domain::ExitRule)
//! and never a whole [`StrategyDsl`](crate::domain::StrategyDsl)), runs a local
//! **correctable** structural/range check, mutates the
//! [`StrategyBuilder`](super::builder::StrategyBuilder), and returns a
//! [`ToolOutcome`]. `finalize_strategy` assembles the accumulated fields + runs
//! the whole-document [`validate`](crate::domain::validate) (reused verbatim; no
//! rule reimplemented here). A serde parse failure of any arg struct maps to a
//! [`FieldError`] — **never a panic, never `.unwrap()`** (FR-3 correctable
//! rejection).
//!
//! **Reversibility (README C4, load-bearing).** The flat-primitive → tagged
//! conversion lives ONLY in the [`mapping`] submodule. A future switch to
//! tagged-fragment args (option A) or schema-guided/constrained decoding is a
//! localized swap of *only* that module — `StrategyBuilder`, `validate()`, and
//! `schema_version` stay untouched.

// The tools are built-but-unwired this slice: their first production caller is
// the composer loop (2.04, R3), so the re-exported surface is otherwise
// `dead_code` under `deny(warnings)` — the VS-1.3.1 harvested dead-code gotcha.
#![allow(dead_code)]

use rust_decimal::Decimal;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::domain::{
    Direction, ExitRule, FieldError, ParamValue, RiskParams, SweepableValue, ToolDefinition,
    ValidationCode,
};

use super::builder::StrategyBuilder;

/// The correctable result each builder tool returns; the composer (2.04)
/// serializes it as the tool-result content the model reads back.
///
/// Reuses [`domain::dsl::FieldError`](crate::domain::FieldError) — the `Err` arm
/// is the correctable path FR-3 mandates (a field-pathed, coded, human/LLM
/// message the model can fix and re-call).
#[derive(Debug)]
pub(crate) enum ToolOutcome {
    /// The fragment was accepted; `summary` is a human/LLM-readable confirmation.
    Ok {
        /// What the tool set, in one line.
        summary: String,
    },
    /// The fragment was rejected; `errors` are correctable field-pathed errors.
    Err {
        /// The correctable field errors the model can fix and re-call.
        errors: Vec<FieldError>,
    },
}

/// Build a correctable [`FieldError`] (the single error-construction helper the
/// tools + the [`mapping`] seam share).
fn field_error(
    path: impl Into<String>,
    code: ValidationCode,
    message: impl Into<String>,
) -> FieldError {
    FieldError {
        path: path.into(),
        code,
        message: message.into(),
    }
}

/// A single-error [`ToolOutcome::Err`].
fn err(error: FieldError) -> ToolOutcome {
    ToolOutcome::Err {
        errors: vec![error],
    }
}

/// Deserialize a tool's flat arg struct from the opaque `serde_json::Value`,
/// mapping a **parse failure to a correctable [`FieldError`]** (never a panic).
fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, FieldError> {
    serde_json::from_value(args).map_err(|source| {
        field_error(
            "arguments",
            ValidationCode::FieldRange,
            format!("could not parse tool arguments: {source}"),
        )
    })
}

/// Deserialize one flat operand arg, pathed at the operand itself so a
/// misspelled key (e.g. `timefrom` for `timeframe`) names WHICH operand to fix
/// (r2.s2 round-1 fix F7).
fn parse_operand(path: &'static str, value: Value) -> Result<mapping::Operand, FieldError> {
    serde_json::from_value(value).map_err(|source| {
        field_error(
            path,
            ValidationCode::FieldRange,
            format!("could not parse operand: {source}"),
        )
    })
}

/// Parse the `{ left, right }` operand pair, collecting every parse failure so
/// a call with two malformed operands reports both at once (the same
/// collect-all-errors convention [`mapping::build_condition`] follows).
fn parse_operand_pair(
    left: Value,
    right: Value,
) -> Result<(mapping::Operand, mapping::Operand), Vec<FieldError>> {
    match (parse_operand("left", left), parse_operand("right", right)) {
        (Ok(left), Ok(right)) => Ok((left, right)),
        (left, right) => Err([left, right].into_iter().filter_map(Result::err).collect()),
    }
}

/// Map the flat `"long"`/`"short"` string to a tagged [`Direction`].
fn direction_from_str(raw: &str) -> Result<Direction, FieldError> {
    match raw {
        "long" => Ok(Direction::Long),
        "short" => Ok(Direction::Short),
        other => Err(field_error(
            "direction",
            ValidationCode::FieldRange,
            format!("unknown direction {other:?}; expected \"long\" or \"short\""),
        )),
    }
}

// -- the six tools (each `fn(&mut StrategyBuilder, Value) -> ToolOutcome`) --------

/// `create_strategy { name, direction }` — init/replace the name + direction.
pub(crate) fn create_strategy(builder: &mut StrategyBuilder, args: Value) -> ToolOutcome {
    let args: CreateStrategyArgs = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let mut errors = Vec::new();
    if args.name.trim().is_empty() {
        errors.push(field_error(
            "name",
            ValidationCode::EmptyName,
            "strategy name must not be empty or whitespace-only",
        ));
    }
    let direction = match direction_from_str(&args.direction) {
        Ok(dir) => Some(dir),
        Err(error) => {
            errors.push(error);
            None
        }
    };
    match direction {
        Some(direction) if errors.is_empty() => {
            let summary = format!("created strategy {:?} ({})", args.name, args.direction);
            builder.set_identity(args.name, direction);
            ToolOutcome::Ok { summary }
        }
        _ => ToolOutcome::Err { errors },
    }
}

/// `add_entry_signal { left, op, right }` — assemble a tagged [`Condition`] and
/// set (replace) it as the entry trigger.
pub(crate) fn add_entry_signal(builder: &mut StrategyBuilder, args: Value) -> ToolOutcome {
    let args: SignalArgs = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let (left, right) = match parse_operand_pair(args.left, args.right) {
        Ok(pair) => pair,
        Err(errors) => return ToolOutcome::Err { errors },
    };
    match mapping::build_condition(&left, &args.op, &right) {
        Ok(condition) => {
            builder.set_entry(condition);
            ToolOutcome::Ok {
                summary: "set entry signal".to_owned(),
            }
        }
        Err(errors) => ToolOutcome::Err { errors },
    }
}

/// `add_filter { left, op, right }` — assemble a tagged [`Condition`] and
/// **append** it (AND-conjoined per the `StrategyDsl` convention).
pub(crate) fn add_filter(builder: &mut StrategyBuilder, args: Value) -> ToolOutcome {
    let args: SignalArgs = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let (left, right) = match parse_operand_pair(args.left, args.right) {
        Ok(pair) => pair,
        Err(errors) => return ToolOutcome::Err { errors },
    };
    match mapping::build_condition(&left, &args.op, &right) {
        Ok(condition) => {
            builder.push_filter(condition);
            ToolOutcome::Ok {
                summary: "added filter".to_owned(),
            }
        }
        Err(errors) => ToolOutcome::Err { errors },
    }
}

/// `set_exit_rules { stop_loss_pct?, atr_stop_period?, atr_stop_multiple?,
/// take_profit_r?, trailing_pct?, time_bars? }` — build a `Vec<ExitRule>`
/// (replacing). Exactly ONE stop family: `stop_loss_pct` alone (a percent
/// [`ExitRule::StopLoss`], defines 1R) or `atr_stop_period` +
/// `atr_stop_multiple` together (an [`ExitRule::AtrStop`], defines 1R). Both
/// families, one ATR half without the other, or neither are a correctable
/// `FieldError` localized at `stop_loss_pct` — the tool refuses FIRST rather
/// than letting a duplicated `StopLoss` + `AtrStop` pair reach
/// `finalize_strategy`'s `DuplicateExit` rule (whose error would not name the
/// field to fix). The ATR bounds (`period ≥ 1`, `multiple ∈ (0, 10]`) stay
/// `validate()`'s at finalize — not re-implemented here. No duplicate exclusive
/// kinds are possible (each field appears at most once).
pub(crate) fn set_exit_rules(builder: &mut StrategyBuilder, args: Value) -> ToolOutcome {
    let args: ExitArgs = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let stop = match (
        args.stop_loss_pct,
        args.atr_stop_period,
        args.atr_stop_multiple,
    ) {
        (Some(stop_loss_pct), None, None) => ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(stop_loss_pct),
        },
        (None, Some(period), Some(multiple)) => ExitRule::AtrStop {
            period: SweepableValue::Fixed(period),
            multiple: SweepableValue::Fixed(multiple),
        },
        _ => {
            return err(field_error(
                "stop_loss_pct",
                ValidationCode::FieldRange,
                "exactly one stop family is required: give stop_loss_pct, or \
                 atr_stop_period with atr_stop_multiple",
            ));
        }
    };
    let mut exits = vec![stop];
    if let Some(target_r) = args.take_profit_r {
        exits.push(ExitRule::TakeProfit {
            target_r: SweepableValue::Fixed(target_r),
        });
    }
    if let Some(trail_pct) = args.trailing_pct {
        exits.push(ExitRule::TrailingStop {
            trail_pct: SweepableValue::Fixed(trail_pct),
        });
    }
    if let Some(max_bars) = args.time_bars {
        exits.push(ExitRule::TimeStop {
            max_bars: SweepableValue::Fixed(max_bars),
        });
    }
    let summary = format!("set {} exit rule(s)", exits.len());
    builder.set_exits(exits);
    ToolOutcome::Ok { summary }
}

/// `set_risk_params { risk_per_trade_pct, max_leverage }` — set (replace) risk;
/// `risk_per_trade_pct ∈ (0, 1]`, `max_leverage ≥ 1` (`FieldRange`).
pub(crate) fn set_risk_params(builder: &mut StrategyBuilder, args: Value) -> ToolOutcome {
    let args: RiskArgs = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(error) => return err(error),
    };
    let mut errors = Vec::new();
    let risk_pct = args.risk_per_trade_pct;
    if risk_pct <= Decimal::ZERO || risk_pct > Decimal::ONE {
        errors.push(field_error(
            "risk_per_trade_pct",
            ValidationCode::FieldRange,
            "risk_per_trade_pct must be a decimal fraction in the range (0, 1]",
        ));
    }
    let leverage = args.max_leverage;
    if leverage < Decimal::ONE {
        errors.push(field_error(
            "max_leverage",
            ValidationCode::FieldRange,
            "max_leverage must be at least 1",
        ));
    }
    if !errors.is_empty() {
        return ToolOutcome::Err { errors };
    }
    let summary = format!("set risk {risk_pct} per trade, {leverage}x max leverage");
    builder.set_risk(RiskParams {
        risk_per_trade_pct: SweepableValue::Fixed(risk_pct),
        max_leverage: SweepableValue::Fixed(leverage),
    });
    ToolOutcome::Ok { summary }
}

/// `finalize_strategy {}` — assemble + run the whole-document `validate()` (via
/// [`StrategyBuilder::finalize`]); a missing required piece → correctable `Err`.
///
/// 2.04 reads the [`ValidatedDsl`](crate::domain::ValidatedDsl) directly from
/// [`StrategyBuilder::finalize`] to build the `StrategyVersion`; this wrapper is
/// the uniform-dispatch/streaming surface (it maps to a [`ToolOutcome`]).
pub(crate) fn finalize_strategy(builder: &StrategyBuilder) -> ToolOutcome {
    match builder.finalize() {
        Ok(validated) => ToolOutcome::Ok {
            summary: format!("finalized strategy {:?}", validated.dsl().name),
        },
        Err(errors) => ToolOutcome::Err { errors },
    }
}

// -- per-tool flat arg structs (`#[derive(Deserialize)]`) ------------------------

/// `create_strategy` args (flat primitives).
///
/// `deny_unknown_fields` (PR #93 review): an unrecognized argument is a
/// CORRECTABLE `FieldError`, never a silent drop — see [`ExitArgs`] for why this
/// matters most on the optional-field structs.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateStrategyArgs {
    name: String,
    direction: String,
}

/// `add_entry_signal` / `add_filter` args (flat `{ left, op, right }`).
///
/// `left`/`right` stay raw [`Value`]s so each operand parses on its own: a
/// `deny_unknown_fields` failure inside an operand is then pathed at `left` /
/// `right` instead of collapsing into the unlocalized whole-struct
/// `arguments` error (r2.s2 round-1 fix F7).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignalArgs {
    left: Value,
    op: String,
    right: Value,
}

/// `set_exit_rules` args — flat scalar fields; `Decimal`s are carried as JSON
/// STRINGS (`"0.05"`), matching the DSL's `serde(with = "…str")` convention AND
/// the advertised schema. `str_option` (NFR-2) rejects a bare JSON number so no
/// float ever reaches a `Decimal` — a bare number becomes a correctable
/// `FieldError` via `parse_args` (slice-close FIX E).
///
/// `deny_unknown_fields` is load-bearing here (PR #93 review): EVERY field is
/// optional, so without it a misspelled `take_profit` (for `take_profit_r`)
/// deserializes fine, silently drops the requested take-profit, and finalizes a
/// VALID stop-only strategy — a different strategy than the trader asked for,
/// with no error anywhere. Rejecting the unknown key turns that into a
/// correctable `FieldError` the model can fix on the next turn.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitArgs {
    #[serde(default, with = "rust_decimal::serde::str_option")]
    stop_loss_pct: Option<Decimal>,
    // The ATR stop family (schema 1.1.0): `atr_stop_period` is a JSON integer,
    // `atr_stop_multiple` a decimal STRING like every other Decimal arg.
    #[serde(default)]
    atr_stop_period: Option<u32>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    atr_stop_multiple: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    take_profit_r: Option<Decimal>,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    trailing_pct: Option<Decimal>,
    #[serde(default)]
    time_bars: Option<u32>,
}

/// `set_risk_params` args — `Decimal`s carried as JSON STRINGS. `str` (NFR-2)
/// rejects a bare JSON number so no float ever reaches a `Decimal`; a bare number
/// becomes a correctable `FieldError` via `parse_args` (slice-close FIX E).
/// `deny_unknown_fields` per PR #93 review (see [`ExitArgs`]).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RiskArgs {
    #[serde(with = "rust_decimal::serde::str")]
    risk_per_trade_pct: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    max_leverage: Decimal,
}

// -- the isolated flat-primitive → tagged-DSL mapping seam (README C4) -----------

mod mapping {
    //! The **only** place flat-primitive tool args become tagged DSL fragments
    //! (README C4 reversibility). A future move to tagged-fragment args or
    //! schema-guided decoding swaps ONLY this module — the accumulator, the
    //! whole-document `validate()`, and `schema_version` are untouched.

    use rust_decimal::Decimal;
    use serde::Deserialize;

    use crate::domain::{
        Comparator, Condition, FieldError, IndicatorSpec, PriceField, Series, SweepableValue,
        ValidationCode, ValueSource,
    };

    use super::field_error;

    /// A flat, untagged operand — the LLM-facing scalar shape (README C5). It is
    /// assembled **server-side** into a tagged [`ValueSource`]; the LLM never
    /// emits the serde tag.
    ///
    /// `deny_unknown_fields` (r2.s2 round-1 fix F7): every field is optional,
    /// so without it a misspelled `timefrom` deserializes fine, silently drops
    /// the requested series, and degrades the operand to primary — a different
    /// operand than the caller sent, with no error anywhere (pulse-trader#160's
    /// silent-substitution class). Rejecting the unknown key makes it a
    /// correctable `FieldError` naming the operand path.
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub(super) struct Operand {
        // Defaulted so a MISSING `source` still deserializes and the mapping
        // layer reports a LOCALIZED `{left|right}.source` error (e.g. a constant
        // operand written as `{"value":"30"}`) instead of a terse, unlocalized
        // whole-struct "missing field `source`" pathed to `arguments` — the gap
        // that stalled the live gpt-oss:120b demo (VS-1.3.2 slice-close).
        #[serde(default)]
        source: String,
        #[serde(default)]
        indicator: Option<String>,
        #[serde(default)]
        period: Option<u32>,
        #[serde(default)]
        fast: Option<u32>,
        #[serde(default)]
        slow: Option<u32>,
        #[serde(default)]
        signal: Option<u32>,
        #[serde(default)]
        price_field: Option<String>,
        // A constant operand's `value` is a JSON STRING (`"30"`) per the advertised
        // schema; `str_option` (NFR-2) rejects a bare JSON number so no float ever
        // reaches a `Decimal` — a bare number becomes a correctable `FieldError`
        // via `parse_args` (slice-close FIX E).
        #[serde(default, with = "rust_decimal::serde::str_option")]
        value: Option<Decimal>,
        // The TOOL boundary names it `timeframe`; the DSL names it `series`
        // (SPINE.md grill ruling 1) — this field is the only translation point.
        // `"h4"` selects the run's higher-timeframe series (`Series::Htf`);
        // `"primary"` or absent is the primary series. No `timeframe` word ever
        // reaches a DSL type.
        #[serde(default)]
        timeframe: Option<String>,
    }

    /// The comparator/cross the flat `op` string selects.
    enum Op {
        Compare(Comparator),
        CrossesAbove,
        CrossesBelow,
    }

    /// Assemble `{ left, op, right }` flat primitives into a tagged
    /// [`Condition`], collecting **every** correctable field error (fast
    /// correctable feedback, FR-3). The non-degenerate-cross local check mirrors
    /// [`validate`](crate::domain::validate) rule 2 for immediate feedback; the
    /// whole-document validator re-checks it at finalize.
    pub(super) fn build_condition(
        left: &Operand,
        op: &str,
        right: &Operand,
    ) -> Result<Condition, Vec<FieldError>> {
        let mut errors = Vec::new();
        let lhs = collect(operand_to_value_source(left, "left"), &mut errors);
        let rhs = collect(operand_to_value_source(right, "right"), &mut errors);
        let parsed_op = collect(parse_op(op), &mut errors);
        let (Some(lhs), Some(rhs), Some(parsed_op)) = (lhs, rhs, parsed_op) else {
            return Err(errors);
        };
        let condition = match parsed_op {
            Op::Compare(operator) => Condition::Compare {
                lhs,
                op: operator,
                rhs,
            },
            Op::CrossesAbove => Condition::CrossesAbove { lhs, rhs },
            Op::CrossesBelow => Condition::CrossesBelow { lhs, rhs },
        };
        if is_degenerate_cross(&condition) {
            errors.push(field_error(
                "op",
                ValidationCode::DegenerateCross,
                "a cross needs at least one Price or Indicator operand; both are Constant",
            ));
        }
        if errors.is_empty() {
            Ok(condition)
        } else {
            Err(errors)
        }
    }

    /// Push `result`'s error (if any) into `errors`, yielding the `Ok` value.
    fn collect<T>(result: Result<T, FieldError>, errors: &mut Vec<FieldError>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                errors.push(error);
                None
            }
        }
    }

    /// Map the flat comparator string to an [`Op`].
    fn parse_op(op: &str) -> Result<Op, FieldError> {
        match op {
            "gt" => Ok(Op::Compare(Comparator::Gt)),
            "gte" => Ok(Op::Compare(Comparator::Gte)),
            "lt" => Ok(Op::Compare(Comparator::Lt)),
            "lte" => Ok(Op::Compare(Comparator::Lte)),
            "eq" => Ok(Op::Compare(Comparator::Eq)),
            "crosses_above" => Ok(Op::CrossesAbove),
            "crosses_below" => Ok(Op::CrossesBelow),
            other => Err(field_error(
                "op",
                ValidationCode::FieldRange,
                format!(
                    "unknown op {other:?}; expected gt|gte|lt|lte|eq|crosses_above|crosses_below"
                ),
            )),
        }
    }

    /// Map a flat operand to a tagged [`ValueSource`].
    fn operand_to_value_source(operand: &Operand, path: &str) -> Result<ValueSource, FieldError> {
        match operand.source.as_str() {
            "constant" => {
                // A constant has no series — a `timeframe` token on one is a
                // correctable error, never a silent drop (#160's class).
                if operand.timeframe.is_some() {
                    return Err(field_error(
                        format!("{path}.timeframe"),
                        ValidationCode::FieldRange,
                        "a constant operand has no series — drop `timeframe`",
                    ));
                }
                let value = operand.value.ok_or_else(|| {
                    field_error(
                        format!("{path}.value"),
                        ValidationCode::FieldRange,
                        "a constant operand requires a `value` (decimal string)",
                    )
                })?;
                Ok(ValueSource::Constant { value })
            }
            "price" => Ok(ValueSource::Price {
                series: series_from(operand.timeframe.as_deref(), path)?,
                field: price_field(operand.price_field.as_deref(), path)?,
            }),
            "indicator" => Ok(ValueSource::Indicator {
                series: series_from(operand.timeframe.as_deref(), path)?,
                spec: indicator_spec(operand, path)?,
            }),
            other => Err(field_error(
                format!("{path}.source"),
                ValidationCode::FieldRange,
                format!("unknown operand source {other:?}; expected indicator|price|constant"),
            )),
        }
    }

    /// Map the flat `timeframe` token to a tagged [`Series`] — the ONLY place
    /// the tool boundary's `timeframe` becomes the DSL's `series` (SPINE.md
    /// grill ruling 1). `"h4"` is the sole higher-timeframe token (the run pairs
    /// exactly one HTF series); an unknown token is a correctable
    /// `FieldError` localized at `{left|right}.timeframe`.
    fn series_from(timeframe: Option<&str>, path: &str) -> Result<Series, FieldError> {
        match timeframe {
            None | Some("primary") => Ok(Series::Primary),
            Some("h4") => Ok(Series::Htf),
            Some(other) => Err(field_error(
                format!("{path}.timeframe"),
                ValidationCode::FieldRange,
                format!("unknown timeframe {other:?}; expected primary|h4"),
            )),
        }
    }

    /// Map the flat `price_field` string to a tagged [`PriceField`].
    fn price_field(field: Option<&str>, path: &str) -> Result<PriceField, FieldError> {
        match field {
            Some("open") => Ok(PriceField::Open),
            Some("high") => Ok(PriceField::High),
            Some("low") => Ok(PriceField::Low),
            Some("close") => Ok(PriceField::Close),
            Some("volume") => Ok(PriceField::Volume),
            Some(other) => Err(field_error(
                format!("{path}.price_field"),
                ValidationCode::FieldRange,
                format!("unknown price_field {other:?}; expected open|high|low|close|volume"),
            )),
            None => Err(field_error(
                format!("{path}.price_field"),
                ValidationCode::FieldRange,
                "a price operand requires a `price_field`",
            )),
        }
    }

    /// Map a flat indicator operand to a tagged [`IndicatorSpec`] (periods become
    /// `SweepableValue::Fixed`; v1 constructs only `Fixed`). Period range (`> 0`)
    /// and the MACD `fast < slow` rule are enforced by the whole-document
    /// `validate()` at finalize — not reimplemented here.
    fn indicator_spec(operand: &Operand, path: &str) -> Result<IndicatorSpec, FieldError> {
        let indicator = operand.indicator.as_deref().ok_or_else(|| {
            field_error(
                format!("{path}.indicator"),
                ValidationCode::FieldRange,
                "an indicator operand requires an `indicator` name",
            )
        })?;
        match indicator {
            "rsi" => Ok(IndicatorSpec::Rsi {
                period: fixed_period(operand, path)?,
            }),
            "ema" => Ok(IndicatorSpec::Ema {
                period: fixed_period(operand, path)?,
            }),
            "adx" => Ok(IndicatorSpec::Adx {
                period: fixed_period(operand, path)?,
            }),
            "macd" => Ok(IndicatorSpec::Macd {
                fast: fixed_u32(operand.fast, &format!("{path}.fast"))?,
                slow: fixed_u32(operand.slow, &format!("{path}.slow"))?,
                signal: fixed_u32(operand.signal, &format!("{path}.signal"))?,
            }),
            "atr" => Ok(IndicatorSpec::Atr {
                period: fixed_period(operand, path)?,
            }),
            other => Err(field_error(
                format!("{path}.indicator"),
                ValidationCode::FieldRange,
                format!("unknown indicator {other:?}; expected rsi|ema|adx|macd|atr"),
            )),
        }
    }

    /// A single-period indicator's `period` field as a `SweepableValue::Fixed`.
    fn fixed_period(operand: &Operand, path: &str) -> Result<SweepableValue<u32>, FieldError> {
        fixed_u32(operand.period, &format!("{path}.period"))
    }

    /// Wrap a required `u32` field into `SweepableValue::Fixed`, or a correctable
    /// "field required" error.
    fn fixed_u32(value: Option<u32>, path: &str) -> Result<SweepableValue<u32>, FieldError> {
        value.map(SweepableValue::Fixed).ok_or_else(|| {
            field_error(
                path.to_owned(),
                ValidationCode::FieldRange,
                "this indicator requires a positive integer period",
            )
        })
    }

    /// Whether `condition` is a cross with both operands constant (rule 2 shape).
    fn is_degenerate_cross(condition: &Condition) -> bool {
        match condition {
            Condition::CrossesAbove { lhs, rhs } | Condition::CrossesBelow { lhs, rhs } => {
                is_constant(lhs) && is_constant(rhs)
            }
            _ => false,
        }
    }

    /// Whether a [`ValueSource`] is a literal constant (no series operand).
    fn is_constant(value: &ValueSource) -> bool {
        matches!(value, ValueSource::Constant { .. })
    }
}

// -- the tool definitions advertised to the model (2.04 assembles the list) ------

/// The six builder-tool [`ToolDefinition`]s (name + inline default description +
/// flat-arg JSON Schema). The tool *names* + *flat arg shapes* are public
/// contract; 2.03/2.04 overlay richer description prose from the config seam.
pub(crate) fn builder_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        def_create_strategy(),
        def_add_entry_signal(),
        def_add_filter(),
        def_set_exit_rules(),
        def_set_risk_params(),
        def_finalize_strategy(),
    ]
}

/// The reusable flat `operand` JSON Schema (README C5) — a `ValueSource` without
/// serde tags.
fn operand_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "enum": ["indicator", "price", "constant"] },
            "indicator": { "type": "string", "enum": ["rsi", "ema", "adx", "macd", "atr"] },
            "timeframe": {
                "type": "string",
                "enum": ["primary", "h4"],
                "description": "series the operand is evaluated on; \"h4\" uses the last closed H4 bar, omit for the primary series"
            },
            "period": { "type": "integer", "minimum": 1 },
            "fast": { "type": "integer", "minimum": 1 },
            "slow": { "type": "integer", "minimum": 1 },
            "signal": { "type": "integer", "minimum": 1 },
            "price_field": {
                "type": "string",
                "enum": ["open", "high", "low", "close", "volume"]
            },
            "value": { "type": "string", "description": "a decimal as a string, e.g. \"30\"" }
        },
        "required": ["source"]
    })
}

/// The reusable `{ left, op, right }` JSON Schema for the two condition tools.
fn signal_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "left": operand_schema(),
            "op": {
                "type": "string",
                "enum": ["gt", "gte", "lt", "lte", "eq", "crosses_above", "crosses_below"]
            },
            "right": operand_schema()
        },
        "required": ["left", "op", "right"]
    })
}

fn def_create_strategy() -> ToolDefinition {
    ToolDefinition {
        name: "create_strategy".to_owned(),
        description: "Initialize a new strategy with a name and trade direction. Call this \
                      first; re-calling replaces the name and direction."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "human-readable strategy name" },
                "direction": { "type": "string", "enum": ["long", "short"] }
            },
            "required": ["name", "direction"]
        }),
    }
}

fn def_add_entry_signal() -> ToolDefinition {
    ToolDefinition {
        name: "add_entry_signal".to_owned(),
        description: "Set the entry trigger as `left op right` over flat operands (indicator, \
                      price field, or a decimal-string constant). Re-calling replaces the entry."
            .to_owned(),
        parameters: signal_schema(),
    }
}

fn def_add_filter() -> ToolDefinition {
    ToolDefinition {
        name: "add_filter".to_owned(),
        description: "Append an AND-conjoined filter condition (same `left op right` shape as \
                      add_entry_signal). Call repeatedly to conjoin several filters."
            .to_owned(),
        parameters: signal_schema(),
    }
}

fn def_set_exit_rules() -> ToolDefinition {
    ToolDefinition {
        name: "set_exit_rules".to_owned(),
        description: "Set the exit rules (replacing). Exactly one stop family is required: \
                      `stop_loss_pct`, or `atr_stop_period` with `atr_stop_multiple` — either \
                      defines 1R. Decimal fields are JSON strings (e.g. \"0.05\"); \
                      `take_profit_r` is a plain R-multiple string; `time_bars` and \
                      `atr_stop_period` are integers."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "stop_loss_pct": { "type": "string", "description": "stop distance fraction, e.g. \"0.05\"" },
                "atr_stop_period": { "type": "integer", "minimum": 1, "description": "ATR lookback period, e.g. 14" },
                "atr_stop_multiple": { "type": "string", "description": "ATR multiple as a decimal string, e.g. \"2\"" },
                "take_profit_r": { "type": "string", "description": "take-profit R-multiple, e.g. \"2\"" },
                "trailing_pct": { "type": "string", "description": "trailing distance fraction" },
                "time_bars": { "type": "integer", "minimum": 1 }
            }
        }),
    }
}

fn def_set_risk_params() -> ToolDefinition {
    ToolDefinition {
        name: "set_risk_params".to_owned(),
        description: "Set risk sizing (replacing). `risk_per_trade_pct` is a decimal-string \
                      fraction in (0, 1] (e.g. \"0.01\"); `max_leverage` is a decimal-string \
                      multiplier >= 1 (e.g. \"3\")."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "risk_per_trade_pct": { "type": "string", "description": "fraction in (0,1], e.g. \"0.01\"" },
                "max_leverage": { "type": "string", "description": "multiplier >= 1, e.g. \"3\"" }
            },
            "required": ["risk_per_trade_pct", "max_leverage"]
        }),
    }
}

fn def_finalize_strategy() -> ToolDefinition {
    ToolDefinition {
        name: "finalize_strategy".to_owned(),
        description: "Assemble the accumulated fields into a schema-valid strategy and run \
                      whole-document validation. Call last, after every other piece is set."
            .to_owned(),
        parameters: json!({ "type": "object", "properties": {} }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        StrategyBuilder, ToolOutcome, add_entry_signal, add_filter, builder_tool_definitions,
        create_strategy, finalize_strategy, set_exit_rules, set_risk_params,
    };
    use crate::domain::{Direction, ValidationCode};
    use serde_json::json;

    fn assert_ok(outcome: ToolOutcome) {
        match outcome {
            ToolOutcome::Ok { .. } => {}
            ToolOutcome::Err { errors } => panic!("expected Ok, got Err({errors:?})"),
        }
    }

    /// Drive the demo strategy through all six mutating tools:
    /// RSI(14) < 30 entry, Close > EMA(200) filter, [5% stop, 2R TP] exits,
    /// 1% risk / 3x leverage.
    fn assemble_demo_builder() -> StrategyBuilder {
        let mut builder = StrategyBuilder::new();
        assert_ok(create_strategy(
            &mut builder,
            json!({ "name": "RSI Oversold", "direction": "long" }),
        ));
        assert_ok(add_entry_signal(
            &mut builder,
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "lt",
                "right": { "source": "constant", "value": "30" }
            }),
        ));
        assert_ok(add_filter(
            &mut builder,
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 200 }
            }),
        ));
        assert_ok(set_exit_rules(
            &mut builder,
            json!({ "stop_loss_pct": "0.05", "take_profit_r": "2" }),
        ));
        assert_ok(set_risk_params(
            &mut builder,
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ));
        builder
    }

    /// AC-6: assemble the demo via the six tools then `finalize` → `Ok`; a
    /// missing-`entry` case → `Err`.
    #[test]
    fn finalize_validates_assembled_strategy() {
        let builder = assemble_demo_builder();
        let validated = builder
            .finalize()
            .expect("the assembled demo strategy must validate");
        assert_eq!(validated.dsl().name, "RSI Oversold");
        assert_eq!(validated.dsl().filters.len(), 1);
        assert_ok(finalize_strategy(&builder));

        // Missing entry → correctable Err (via both the builder + the tool).
        let mut incomplete = StrategyBuilder::new();
        let _ = create_strategy(
            &mut incomplete,
            json!({ "name": "no entry", "direction": "long" }),
        );
        let _ = set_exit_rules(&mut incomplete, json!({ "stop_loss_pct": "0.05" }));
        let _ = set_risk_params(
            &mut incomplete,
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        );
        let errs = incomplete
            .finalize()
            .expect_err("a missing entry must not finalize");
        assert!(errs.iter().any(|e| e.path == "entry"));
        assert!(matches!(
            finalize_strategy(&incomplete),
            ToolOutcome::Err { .. }
        ));
    }

    /// AC-7: an out-of-range `set_risk_params(risk_per_trade_pct = "2.0")` →
    /// `ToolOutcome::Err` with a `FieldError` whose `code == FieldRange`.
    #[test]
    fn invalid_tool_input_returns_correctable_error() {
        let mut builder = StrategyBuilder::new();
        let outcome = set_risk_params(
            &mut builder,
            json!({ "risk_per_trade_pct": "2.0", "max_leverage": "3" }),
        );
        match outcome {
            ToolOutcome::Err { errors } => assert!(
                errors
                    .iter()
                    .any(|e| e.code == ValidationCode::FieldRange && e.path == "risk_per_trade_pct")
            ),
            ToolOutcome::Ok { .. } => panic!("out-of-range risk must be a correctable Err"),
        }
    }

    /// AC-8: a `serde_json::Value` that fails the arg-struct `Deserialize` maps
    /// to a `FieldError` — no panic, no `.unwrap()`.
    #[test]
    fn malformed_args_map_to_field_error_not_panic() {
        let mut builder = StrategyBuilder::new();
        // `name` is the wrong JSON type and `direction` is absent → Deserialize fails.
        match create_strategy(&mut builder, json!({ "name": 123 })) {
            ToolOutcome::Err { errors } => assert!(!errors.is_empty()),
            ToolOutcome::Ok { .. } => panic!("malformed args must map to a FieldError, not Ok"),
        }
        // A structurally-malformed operand (unknown source) is also correctable.
        assert!(matches!(
            add_entry_signal(
                &mut builder,
                json!({
                    "left": { "source": "bogus" },
                    "op": "gt",
                    "right": { "source": "constant", "value": "1" }
                })
            ),
            ToolOutcome::Err { .. }
        ));
    }

    /// A constant operand written WITHOUT `source` (e.g. `{"value":"30"}`) is a
    /// correctable error LOCALIZED to the offending operand (`right.source`), NOT
    /// a terse unlocalized whole-struct parse error at `arguments` — the gap that
    /// stalled the live gpt-oss:120b demo (VS-1.3.2 slice-close: the model kept
    /// "fixing" the left operand because the error never named the right one).
    #[test]
    fn operand_missing_source_errors_localize_to_the_operand() {
        let mut builder = StrategyBuilder::new();
        let outcome = add_entry_signal(
            &mut builder,
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "lt",
                "right": { "value": "30" }
            }),
        );
        match outcome {
            ToolOutcome::Err { errors } => {
                assert!(
                    errors.iter().any(|e| e.path == "right.source"),
                    "expected a localized `right.source` error, got {errors:?}"
                );
                assert!(
                    errors.iter().all(|e| e.path != "arguments"),
                    "the error must NOT be the unlocalized whole-struct `arguments` parse error"
                );
            }
            ToolOutcome::Ok { .. } => {
                panic!("a missing operand source must be a correctable error")
            }
        }
    }

    /// FIX E (NFR-2): a flat `Decimal` arg sent as a JSON NUMBER (not a string) is
    /// a correctable error — the f64 ingress path is closed, only decimal STRINGS
    /// are accepted (matching the advertised schema), so no float ever reaches a
    /// `Decimal`. A bare `0.01` (not `"0.01"`) must reject.
    #[test]
    fn numeric_decimal_arg_is_a_correctable_error() {
        let mut builder = StrategyBuilder::new();
        // `risk_per_trade_pct` as a bare JSON number 0.01 (NOT "0.01").
        match set_risk_params(
            &mut builder,
            json!({ "risk_per_trade_pct": 0.01, "max_leverage": "3" }),
        ) {
            ToolOutcome::Err { errors } => assert!(!errors.is_empty()),
            ToolOutcome::Ok { .. } => {
                panic!("a numeric (f64) decimal arg must be a correctable Err, not Ok")
            }
        }
        // A bare-number `stop_loss_pct` (Option<Decimal> via str_option) also rejects.
        assert!(matches!(
            set_exit_rules(&mut builder, json!({ "stop_loss_pct": 0.05 })),
            ToolOutcome::Err { .. }
        ));
        // A bare-number constant operand `value` (Option<Decimal>) also rejects.
        assert!(matches!(
            add_entry_signal(
                &mut builder,
                json!({
                    "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                    "op": "lt",
                    "right": { "source": "constant", "value": 30 }
                })
            ),
            ToolOutcome::Err { .. }
        ));
    }

    /// PR #93 review: a MISSPELLED optional arg must be a correctable error, not
    /// a silent drop. Every `ExitArgs` field is optional, so before
    /// `deny_unknown_fields` a `take_profit` typo (for `take_profit_r`)
    /// deserialized fine and finalized a VALID stop-only strategy — silently a
    /// different strategy than the trader asked for.
    #[test]
    fn misspelled_optional_exit_arg_is_correctable_not_silently_dropped() {
        let mut builder = StrategyBuilder::new();
        let outcome = set_exit_rules(
            &mut builder,
            json!({ "stop_loss_pct": "0.05", "take_profit": "2.0" }),
        );
        match outcome {
            ToolOutcome::Err { errors } => {
                assert!(!errors.is_empty(), "an unknown field must report an error");
            }
            ToolOutcome::Ok { .. } => {
                panic!("a misspelled optional arg must be a correctable Err, not a silent drop")
            }
        }
        // The correctly-spelled call still works.
        assert_ok(set_exit_rules(
            &mut builder,
            json!({ "stop_loss_pct": "0.05", "take_profit_r": "2.0" }),
        ));
    }

    /// The same guard on the other three arg structs: an unknown key is always a
    /// correctable error, never accepted.
    #[test]
    fn unknown_args_are_rejected_on_every_tool() {
        let mut builder = StrategyBuilder::new();
        assert!(matches!(
            create_strategy(
                &mut builder,
                json!({ "name": "S", "direction": "long", "typo": 1 })
            ),
            ToolOutcome::Err { .. }
        ));
        assert!(matches!(
            set_risk_params(
                &mut builder,
                json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3", "typo": 1 })
            ),
            ToolOutcome::Err { .. }
        ));
        assert!(matches!(
            add_entry_signal(
                &mut builder,
                json!({
                    "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                    "op": "lt",
                    "right": { "source": "constant", "value": "30" },
                    "typo": 1
                })
            ),
            ToolOutcome::Err { .. }
        ));
    }

    /// r2.s2 round-1 fix F7: a MISSPELLED key inside an operand (`timefrom`
    /// for `timeframe`) is a correctable `FieldError` pathed at that operand —
    /// never a silent drop. Before `deny_unknown_fields` on `Operand` the key
    /// deserializes fine, `series_from(None)` degrades the operand to the
    /// primary series, and the composed strategy silently differs from the
    /// request (pulse-trader#160's silent-substitution class).
    #[test]
    fn misspelled_operand_key_is_correctable_and_names_the_operand() {
        let mut builder = StrategyBuilder::new();
        let outcome = add_entry_signal(
            &mut builder,
            json!({
                "left": { "source": "price", "price_field": "close", "timefrom": "h4" },
                "op": "gt",
                "right": { "source": "constant", "value": "100" }
            }),
        );
        match outcome {
            ToolOutcome::Err { errors } => {
                assert!(
                    errors.iter().any(|e| e.path == "left"),
                    "the misspelled operand must be pathed at `left`, got {errors:?}"
                );
                assert!(
                    errors.iter().all(|e| e.path != "arguments"),
                    "the error must NOT be the unlocalized whole-struct `arguments` parse error"
                );
            }
            ToolOutcome::Ok { .. } => {
                panic!("a misspelled operand key must be a correctable Err, not a silent drop")
            }
        }
        // The same misspelling inside `right` names `right`.
        let outcome = add_entry_signal(
            &mut builder,
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "constant", "value": "100", "timefrom": "h4" }
            }),
        );
        match outcome {
            ToolOutcome::Err { errors } => {
                assert!(
                    errors.iter().any(|e| e.path == "right"),
                    "the misspelled operand must be pathed at `right`, got {errors:?}"
                );
            }
            ToolOutcome::Ok { .. } => {
                panic!("a misspelled operand key must be a correctable Err, not a silent drop")
            }
        }
        // The correctly-spelled `timeframe` still composes the htf operand.
        assert_ok(add_entry_signal(
            &mut builder,
            json!({
                "left": { "source": "price", "price_field": "close", "timeframe": "h4" },
                "op": "gt",
                "right": { "source": "constant", "value": "100" }
            }),
        ));
    }

    /// AC-12: calling `set_risk_params` (and the other setters) BEFORE
    /// `create_strategy` still finalizes correctly (order-independence, audit F3).
    #[test]
    fn tools_are_order_independent() {
        let mut builder = StrategyBuilder::new();
        assert_ok(set_risk_params(
            &mut builder,
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        ));
        assert_ok(set_exit_rules(
            &mut builder,
            json!({ "stop_loss_pct": "0.05", "take_profit_r": "2" }),
        ));
        assert_ok(add_entry_signal(
            &mut builder,
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "lt",
                "right": { "source": "constant", "value": "30" }
            }),
        ));
        assert_ok(create_strategy(
            &mut builder,
            json!({ "name": "Reordered", "direction": "long" }),
        ));
        let validated = builder
            .finalize()
            .expect("order-independent assembly still finalizes");
        assert_eq!(validated.dsl().name, "Reordered");
    }

    /// Re-call semantics: the set-tools replace, `add_filter` appends.
    #[test]
    fn setters_replace_and_filters_append() {
        let mut builder = StrategyBuilder::new();
        let _ = create_strategy(
            &mut builder,
            json!({ "name": "first", "direction": "long" }),
        );
        let _ = create_strategy(
            &mut builder,
            json!({ "name": "second", "direction": "short" }),
        );
        let _ = add_entry_signal(
            &mut builder,
            json!({
                "left": { "source": "indicator", "indicator": "rsi", "period": 14 },
                "op": "lt",
                "right": { "source": "constant", "value": "30" }
            }),
        );
        let _ = add_filter(
            &mut builder,
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 50 }
            }),
        );
        let _ = add_filter(
            &mut builder,
            json!({
                "left": { "source": "price", "price_field": "close" },
                "op": "gt",
                "right": { "source": "indicator", "indicator": "ema", "period": 200 }
            }),
        );
        let _ = set_exit_rules(
            &mut builder,
            json!({ "stop_loss_pct": "0.05", "take_profit_r": "2" }),
        );
        let _ = set_risk_params(
            &mut builder,
            json!({ "risk_per_trade_pct": "0.01", "max_leverage": "3" }),
        );
        let validated = builder.finalize().expect("finalizes");
        assert_eq!(validated.dsl().name, "second");
        assert_eq!(validated.dsl().direction, Direction::Short);
        assert_eq!(validated.dsl().filters.len(), 2);
    }

    /// An all-constant cross is a correctable degenerate-cross error (the local
    /// check, mirroring `validate` rule 2).
    #[test]
    fn degenerate_cross_is_correctable() {
        let mut builder = StrategyBuilder::new();
        match add_entry_signal(
            &mut builder,
            json!({
                "left": { "source": "constant", "value": "1" },
                "op": "crosses_above",
                "right": { "source": "constant", "value": "2" }
            }),
        ) {
            ToolOutcome::Err { errors } => assert!(
                errors
                    .iter()
                    .any(|e| e.code == ValidationCode::DegenerateCross)
            ),
            ToolOutcome::Ok { .. } => panic!("an all-constant cross must be a correctable Err"),
        }
    }

    /// The six tool definitions carry exactly the six public-contract tool names.
    #[test]
    fn tool_definitions_list_the_six_tools() {
        let defs = builder_tool_definitions();
        assert_eq!(defs.len(), 6);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        for expected in [
            "create_strategy",
            "add_entry_signal",
            "add_filter",
            "set_exit_rules",
            "set_risk_params",
            "finalize_strategy",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool definition {expected}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// r1.s2.w3 — the coach's tools (ADR-0013 conventions, ADR-0021 decision 7),
// widened to TWO mutually exclusive tools by r1.s4.w1 (#131).
// ---------------------------------------------------------------------------

/// The coach's mutation tool. Exactly one well-formed call ends the turn (A3);
/// zero calls and two calls are both recorded typed failures, not retries.
pub(crate) const PROPOSE_MUTATION_TOOL: &str = "propose_mutation";

/// The coach's HONESTY tool (r1.s4.w1, `pulseai-labs/pulse-trader#131`).
///
/// The second of two mutually exclusive tools: a turn calls `propose_mutation`
/// **or** this one, exactly once. It exists because the `r1` vocabulary is
/// parameter-only, so a coach whose best advice is structural ("add an ADX
/// filter", "trade the other side") previously had two options and both were
/// dishonest — approximate the advice with whichever parameter sits nearest it, or
/// say nothing. This is the third option: record what it wanted to change and what
/// in the result motivated it, as a typed
/// [`CoachFailure::InapplicableAdvice`](crate::domain::CoachFailure::InapplicableAdvice),
/// and let the evidence accumulate for the release that widens the vocabulary.
///
/// It creates no proposal, mints no child version, and needs no DSL locator —
/// which is exactly why it can say things the locator grammar cannot.
pub(crate) const RECORD_INAPPLICABLE_TOOL: &str = "record_inapplicable";

/// The `propose_mutation` arguments, as the model supplies them.
///
/// `new_value` deserializes DIRECTLY into the domain's [`ParamValue`] rather than
/// into a loose string the coach then has to guess the type of: the tagged shape
/// (`{"type":"Period","value":21}` / `{"type":"Threshold","value":"0.03"}`) is
/// what `apply()` already speaks, so a wrong shape is one clean serde failure
/// recorded as `MalformedArguments` instead of a heuristic that silently picks the
/// wrong numeric kind.
///
/// `hypothesis` is a plain `String` here and is validated into the domain's
/// non-empty `Hypothesis` by the caller — an empty one is `MalformedArguments`.
///
/// `deny_unknown_fields` (PR #93 review, the rule every other tool-arg struct in
/// this file follows): an unrecognized argument is a misunderstanding of the tool,
/// and accepting it silently is how a model's `"value_pct"` typo becomes a
/// proposal that mutates something other than what it described.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProposeMutationArgs {
    /// The `validate.rs` dotted/indexed locator of the leaf to retune.
    pub(crate) path: String,
    /// The typed value to write there.
    pub(crate) new_value: ParamValue,
    /// Why the coach believes this helps.
    pub(crate) hypothesis: String,
}

/// The `record_inapplicable` arguments, as the model supplies them (r1.s4.w1).
///
/// Two required strings and nothing else. There is deliberately no `path` and no
/// locator of any kind: the whole point of the tool is to say something the
/// parameter-only locator grammar cannot express, and a locator field would invite
/// the model to aim the honest record at a parameter anyway.
///
/// `deny_unknown_fields`, like every other tool-arg struct in this file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordInapplicableArgs {
    /// What the coach wanted to change, structurally.
    pub(crate) intent: String,
    /// Which observed run facts motivated it.
    pub(crate) evidence: String,
}

/// The coach's tool surface: exactly two MUTUALLY EXCLUSIVE tools, in this
/// advertisement order.
///
/// The order is load-bearing twice over: it is the order the request fingerprint
/// feeds tool definitions in (`application::coach`), and it is the order the model
/// reads them in. `propose_mutation` stays first because it is the turn's expected
/// outcome; `record_inapplicable` is the honest exit, not the default.
pub(crate) fn coach_tool_definitions() -> Vec<ToolDefinition> {
    vec![def_propose_mutation(), def_record_inapplicable()]
}

fn def_propose_mutation() -> ToolDefinition {
    ToolDefinition {
        name: PROPOSE_MUTATION_TOOL.to_owned(),
        description: "Propose EXACTLY ONE parameter change to the strategy, with the hypothesis \
                      behind it. Call this once; the first well-formed call ends your turn. \
                      `path` is the dotted/indexed locator of an existing numeric leaf (e.g. \
                      `entry.lhs.indicator.rsi.period`, `exits[0].distance_pct`, \
                      `risk.max_leverage`). Structural edits are not available in this release."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "dotted/indexed locator of the numeric leaf to retune"
                },
                "new_value": {
                    "type": "object",
                    "description": "the typed replacement value",
                    "oneOf": [
                        {
                            "type": "object",
                            "properties": {
                                "type": { "const": "Period" },
                                "value": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "an indicator period or bar count"
                                }
                            },
                            "required": ["type", "value"]
                        },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "const": "Threshold" },
                                "value": {
                                    "type": "string",
                                    "description": "a decimal STRING (e.g. \"0.03\"), never a float"
                                }
                            },
                            "required": ["type", "value"]
                        }
                    ]
                },
                "hypothesis": {
                    "type": "string",
                    "description": "one sentence: what you expect the change to do, and which \
                                    number in the result led you there. Must not be empty."
                }
            },
            "required": ["path", "new_value", "hypothesis"]
        }),
    }
}

/// The `record_inapplicable` tool definition (r1.s4.w1, #131).
///
/// Two required strings, described so the model knows this is the honest exit and
/// not a way to avoid work: it is for advice the parameter vocabulary CANNOT
/// express, never for a parameter change the coach is unsure about.
fn def_record_inapplicable() -> ToolDefinition {
    ToolDefinition {
        name: RECORD_INAPPLICABLE_TOOL.to_owned(),
        description: "Record that your best advice is STRUCTURAL and this release cannot express \
                      it as a parameter change. Call this instead of `propose_mutation` — never \
                      both, and never twice. Use it when what you want is to add or remove a \
                      condition or filter, swap an indicator, change an exit's kind, or trade the \
                      other side. Do NOT use it for a parameter change you are merely unsure \
                      about: propose that one. Do NOT approximate a structural change with the \
                      nearest parameter."
            .to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "intent": {
                    "type": "string",
                    "description": "what you would change, structurally, in one sentence (e.g. \
                                    \"add an ADX>25 trend filter to the entry\"). Must not be \
                                    empty."
                },
                "evidence": {
                    "type": "string",
                    "description": "which numbers in the persisted result led you there (e.g. \
                                    \"most losses are in the ranging regime\"). Must not be empty."
                }
            },
            "required": ["intent", "evidence"]
        }),
    }
}
