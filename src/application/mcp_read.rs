//! Pure read projections for the `pulse mcp` delivery ring (r2.s1.w2).
//!
//! Two unrelated helpers share this file because both are use-case logic shared
//! by every delivery adapter (grill ruling 2):
//!
//! 1. [`parse_indicator_specs`] / [`IndicatorColumn`] — the `<kind>:<period>`
//!    indicator-spec parser moved verbatim out of `src/cli/indicators.rs` so the
//!    MCP `export_indicators` tool and the `pulse indicators` viewer run the
//!    SAME parse/dedup/default behaviour (AC-9 guards the move). The CLI keeps
//!    byte-identical output through a `pub(crate) use` re-export.
//! 2. The wire projections the read tools return: [`VersionEntry`],
//!    [`StrategyEntry`], [`RunListEntry`], [`RunDetail`], [`MfeMaeWire`] — typed
//!    `Serialize` shapes mapped straight off the domain records so `src/mcp/**`
//!    never re-derives its own projection.
//!
//! The application ring may name NO adapter namespace other than
//! `crate::adapters::backtest` (the `tests/tauri_backtest.rs` source scan), so
//! everything here is pure: domain types in, `serde`-serializable structs out.

use std::collections::HashSet;

use rust_decimal::Decimal;
use serde::Serialize;

use crate::domain::strategy::{CreatedBy, Strategy, StrategyVersion};
use crate::domain::{
    BacktestInputs, CandlePatternName, IndicatorSpec, MacdOutput, MfeMaeAggregates,
    OpenPositionMark, PersistedRun, PriceField, RegimeBreakdown, SkippedEntryCounts, SummaryStats,
    SweepableValue,
};

/// One column of indicator output: the label a client sees (`<kind>:<period>`,
/// case-normalized) paired with the parsed [`IndicatorSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndicatorColumn {
    /// The display/CSV-header label, e.g. `rsi:14`.
    pub label: String,
    /// The parsed fixed-period spec.
    pub spec: IndicatorSpec,
}

/// Parse `<kind>:<period>` indicator tokens into labelled specs.
///
/// Behaviour is byte-identical to the CLI viewer's (r1.s3.w1): an empty list
/// defaults to `rsi:14, ema:50, adx:14`; duplicate/case-variant tokens dedup to
/// one column each, order preserved; `macd`, unknown kinds, and non-u32 or
/// zero periods are rejected.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the offending token on any malformed
/// spec.
pub fn parse_indicator_specs(raw: &[String]) -> anyhow::Result<Vec<IndicatorColumn>> {
    let tokens = if raw.is_empty() {
        ["rsi:14", "ema:50", "adx:14"]
            .iter()
            .map(|token| (*token).to_owned())
            .collect::<Vec<_>>()
    } else {
        raw.to_vec()
    };

    // The engine dedups specs, but the viewer renders one column per flag — so
    // repeated `--indicator` flags would print duplicate columns. Dedup here,
    // order-preserving, keyed on the case-normalized `kind:period` label (which
    // is 1:1 with the parsed spec for rsi/ema/adx/atr).
    let mut seen = HashSet::new();
    let mut columns = Vec::with_capacity(tokens.len());
    for token in &tokens {
        let column = parse_one_indicator(token)?;
        if seen.insert(column.label.clone()) {
            columns.push(column);
        }
    }
    Ok(columns)
}

fn parse_one_indicator(token: &str) -> anyhow::Result<IndicatorColumn> {
    // The four candle-anatomy kinds take no parameters and are written bare —
    // no `:` at all (r4.s6.w1).
    let bare = token.trim().to_ascii_lowercase();
    if let Some(spec) = anatomy_spec(&bare) {
        return Ok(IndicatorColumn { label: bare, spec });
    }

    // The named candle patterns (schema 1.4.0, r5.s1.w3) are written
    // `cdl:<name>`. The name is required, so a bare `cdl` is its own case: it
    // has no `:` to split on and would otherwise fall into the `<kind>:<period>`
    // refusal. r5.s1.w1 added the pattern specs; the token is this item's.
    if bare == "cdl" {
        anyhow::bail!("{}", cdl_token_error(token, ""));
    }

    let (kind, rest) = token.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("invalid --indicator {token:?}: expected <kind>:<period>")
    })?;
    let kind = kind.trim().to_ascii_lowercase();

    if kind == "cdl" {
        return parse_candle_pattern(token, rest);
    }

    // MACD carries three periods and (schema 1.2.0, r3.s2 — b2) an optional
    // output selector: `macd:<fast>-<slow>-<signal>` or
    // `macd:<fast>-<slow>-<signal>:<output>` with output in
    // line|signal|histogram (absent → line). The bare `macd:<period>` form
    // stays rejected.
    if kind == "macd" {
        let parts: Vec<&str> = rest.split(':').collect();
        let (periods, output, output_label) = match parts.as_slice() {
            [periods] => (periods, MacdOutput::Line, String::new()),
            [periods, output] => {
                let trimmed = output.trim();
                let selected = match trimmed {
                    "line" => MacdOutput::Line,
                    "signal" => MacdOutput::Signal,
                    "histogram" => MacdOutput::Histogram,
                    other => anyhow::bail!(
                        "invalid --indicator {token:?}: unknown macd output {other:?} (expected line|signal|histogram)"
                    ),
                };
                (periods, selected, format!(":{trimmed}"))
            }
            _ => anyhow::bail!(
                "invalid --indicator {token:?}: macd expects <fast>-<slow>-<signal>[:<output>]"
            ),
        };
        let nums: Vec<&str> = periods.split('-').collect();
        if nums.len() != 3 {
            anyhow::bail!(
                "invalid --indicator {token:?}: macd expects <fast>-<slow>-<signal>[:<output>]"
            );
        }
        let parsed: Vec<u32> = nums
            .iter()
            .map(|p| {
                p.trim().parse::<u32>().map_err(|e| {
                    anyhow::anyhow!("invalid --indicator {token:?}: period must be u32: {e}")
                })
            })
            .collect::<anyhow::Result<_>>()?;
        let [fast, slow, signal] = parsed[..] else {
            anyhow::bail!("invalid --indicator {token:?}: macd expects three periods");
        };
        if fast == 0 || slow == 0 || signal == 0 {
            anyhow::bail!("invalid --indicator {token:?}: periods must be >= 1");
        }
        let fixed = SweepableValue::Fixed;
        return Ok(IndicatorColumn {
            label: format!("macd:{fast}-{slow}-{signal}{output_label}"),
            spec: IndicatorSpec::Macd {
                fast: fixed(fast),
                slow: fixed(slow),
                signal: fixed(signal),
                output,
            },
        });
    }

    // r3.s2.w2 (Q1): the rolling extremes carry one period and an optional
    // price-field selector — `highest:<period>` / `highest:<period>:<field>`
    // (the field defaults to high for `highest`, low for `lowest`; explicit
    // writes always show in the label, the macd output precedent).
    if kind == "highest" || kind == "lowest" {
        return parse_rolling_extreme(token, &kind, rest);
    }

    // r4.s6.w1: the structure indicators. The pivot variants carry the
    // `left`/`right` span; the level variants add `tolerance_pct` (a decimal
    // FRACTION — `0.003` is 0.3%), `min_touches` and `lookback`.
    if kind == "pivot_high" || kind == "pivot_low" {
        return parse_pivot(token, &kind, rest);
    }
    if kind == "resistance" || kind == "support" {
        return parse_level(token, &kind, rest);
    }

    // r4.s6.w3: the pattern indicators. The necklines mirror the level
    // grammar's tolerance leaf; the trendlines carry the pivot span alone.
    if kind == "double_top_neckline" || kind == "double_bottom_neckline" {
        return parse_neckline(token, &kind, rest);
    }
    if kind == "trendline_lows" || kind == "trendline_highs" {
        return parse_trendline(token, &kind, rest);
    }

    let period = parse_period(token, rest)?;
    let fixed = SweepableValue::Fixed(period);
    let spec = match kind.as_str() {
        "rsi" => IndicatorSpec::Rsi { period: fixed },
        "ema" => IndicatorSpec::Ema { period: fixed },
        "adx" => IndicatorSpec::Adx { period: fixed },
        "atr" => IndicatorSpec::Atr { period: fixed },
        "macd" => anyhow::bail!(
            "invalid --indicator {token:?}: MACD needs fast/slow/signal and is not supported by <kind>:<period>"
        ),
        _ => anyhow::bail!(
            "invalid --indicator {token:?}: unknown kind {kind:?} (expected rsi, ema, adx, atr, highest, or lowest)"
        ),
    };
    Ok(IndicatorColumn {
        label: format!("{kind}:{period}"),
        spec,
    })
}

/// Parse the `highest:<period>[:<field>]` / `lowest:<period>[:<field>]`
/// forms (schema 1.2.0, r3.s2 — Q1).
fn parse_rolling_extreme(token: &str, kind: &str, rest: &str) -> anyhow::Result<IndicatorColumn> {
    let parts: Vec<&str> = rest.split(':').collect();
    let (period_str, field, field_label) = match parts.as_slice() {
        [period] => (period, None, String::new()),
        [period, field] => {
            let trimmed = field.trim().to_ascii_lowercase();
            let selected = match trimmed.as_str() {
                "open" => PriceField::Open,
                "high" => PriceField::High,
                "low" => PriceField::Low,
                "close" => PriceField::Close,
                "volume" => PriceField::Volume,
                other => anyhow::bail!(
                    "invalid --indicator {token:?}: unknown {kind} field {other:?} \
                     (expected open|high|low|close|volume)"
                ),
            };
            (period, Some(selected), format!(":{trimmed}"))
        }
        _ => anyhow::bail!("invalid --indicator {token:?}: {kind} expects <period>[:<field>]"),
    };
    let period = parse_period(token, period_str)?;
    let fixed = SweepableValue::Fixed(period);
    let default_source = if kind == "highest" {
        PriceField::High
    } else {
        PriceField::Low
    };
    let spec = if kind == "highest" {
        IndicatorSpec::Highest {
            period: fixed,
            source: field.unwrap_or(default_source),
        }
    } else {
        IndicatorSpec::Lowest {
            period: fixed,
            source: field.unwrap_or(default_source),
        }
    };
    Ok(IndicatorColumn {
        label: format!("{kind}:{period}{field_label}"),
        spec,
    })
}

fn parse_period(token: &str, period: &str) -> anyhow::Result<u32> {
    let period = period
        .trim()
        .parse::<u32>()
        .map_err(|e| anyhow::anyhow!("invalid --indicator {token:?}: period must be u32: {e}"))?;
    if period == 0 {
        anyhow::bail!("invalid --indicator {token:?}: period must be >= 1");
    }
    Ok(period)
}

/// The four parameterless candle-anatomy kinds, written bare: `body`,
/// `upper_wick`, `lower_wick`, `range` (r4.s6.w1).
fn anatomy_spec(kind: &str) -> Option<IndicatorSpec> {
    match kind {
        "body" => Some(IndicatorSpec::Body),
        "upper_wick" => Some(IndicatorSpec::UpperWick),
        "lower_wick" => Some(IndicatorSpec::LowerWick),
        "range" => Some(IndicatorSpec::Range),
        _ => None,
    }
}

/// Parse the `pivot_high:<left>-<right>` / `pivot_low:<left>-<right>` forms
/// (r4.s6.w1).
fn parse_pivot(token: &str, kind: &str, rest: &str) -> anyhow::Result<IndicatorColumn> {
    let parts: Vec<&str> = rest.split('-').collect();
    let [left, right] = parts[..] else {
        anyhow::bail!("invalid --indicator {token:?}: {kind} expects <left>-<right>");
    };
    let left = parse_period(token, left)?;
    let right = parse_period(token, right)?;
    let fixed = SweepableValue::Fixed;
    let spec = if kind == "pivot_high" {
        IndicatorSpec::PivotHigh {
            left: fixed(left),
            right: fixed(right),
        }
    } else {
        IndicatorSpec::PivotLow {
            left: fixed(left),
            right: fixed(right),
        }
    };
    Ok(IndicatorColumn {
        label: format!("{kind}:{left}-{right}"),
        spec,
    })
}

/// Parse the
/// `resistance:<left>-<right>-<tolerance_pct>-<min_touches>-<lookback>` /
/// `support:<…>` forms (r4.s6.w1). `tolerance_pct` is a decimal FRACTION on the
/// `distance_pct` convention — `0.003` means 0.3% — and is normalized in the
/// label, so `0.0030` and `0.003` dedup to one column.
fn parse_level(token: &str, kind: &str, rest: &str) -> anyhow::Result<IndicatorColumn> {
    let parts: Vec<&str> = rest.split('-').collect();
    let [left, right, tolerance, min_touches, lookback] = parts[..] else {
        anyhow::bail!(
            "invalid --indicator {token:?}: {kind} expects \
             <left>-<right>-<tolerance_pct>-<min_touches>-<lookback>, where tolerance_pct is a \
             decimal fraction (0.003 means 0.3%)"
        );
    };
    let left = parse_period(token, left)?;
    let right = parse_period(token, right)?;
    let tolerance_pct = tolerance.trim().parse::<Decimal>().map_err(|e| {
        anyhow::anyhow!("invalid --indicator {token:?}: tolerance_pct must be a decimal: {e}")
    })?;
    if tolerance_pct <= Decimal::ZERO {
        anyhow::bail!("invalid --indicator {token:?}: tolerance_pct must be > 0");
    }
    let min_touches = parse_period(token, min_touches)?;
    let lookback = parse_period(token, lookback)?;
    let fixed = SweepableValue::Fixed;
    let spec = if kind == "resistance" {
        IndicatorSpec::Resistance {
            left: fixed(left),
            right: fixed(right),
            tolerance_pct: SweepableValue::Fixed(tolerance_pct),
            min_touches: fixed(min_touches),
            lookback: fixed(lookback),
        }
    } else {
        IndicatorSpec::Support {
            left: fixed(left),
            right: fixed(right),
            tolerance_pct: SweepableValue::Fixed(tolerance_pct),
            min_touches: fixed(min_touches),
            lookback: fixed(lookback),
        }
    };
    Ok(IndicatorColumn {
        label: format!(
            "{kind}:{left}-{right}-{}-{min_touches}-{lookback}",
            tolerance_pct.normalize()
        ),
        spec,
    })
}

/// Parse the `double_top_neckline:<left>-<right>-<tolerance_pct>` /
/// `double_bottom_neckline:<…>` forms (r4.s6.w3). `tolerance_pct` is a decimal
/// FRACTION on the `distance_pct` convention — `0.003` means 0.3% — and is
/// normalized in the label, so `0.0030` and `0.003` dedup to one column (the
/// `parse_level` precedent).
fn parse_neckline(token: &str, kind: &str, rest: &str) -> anyhow::Result<IndicatorColumn> {
    let parts: Vec<&str> = rest.split('-').collect();
    let [left, right, tolerance] = parts[..] else {
        anyhow::bail!(
            "invalid --indicator {token:?}: {kind} expects <left>-<right>-<tolerance_pct>, where \
             tolerance_pct is a decimal fraction (0.003 means 0.3%)"
        );
    };
    let left = parse_period(token, left)?;
    let right = parse_period(token, right)?;
    let tolerance_pct = tolerance.trim().parse::<Decimal>().map_err(|e| {
        anyhow::anyhow!("invalid --indicator {token:?}: tolerance_pct must be a decimal: {e}")
    })?;
    if tolerance_pct <= Decimal::ZERO {
        anyhow::bail!("invalid --indicator {token:?}: tolerance_pct must be > 0");
    }
    let fixed = SweepableValue::Fixed;
    let spec = if kind == "double_top_neckline" {
        IndicatorSpec::DoubleTopNeckline {
            left: fixed(left),
            right: fixed(right),
            tolerance_pct: SweepableValue::Fixed(tolerance_pct),
        }
    } else {
        IndicatorSpec::DoubleBottomNeckline {
            left: fixed(left),
            right: fixed(right),
            tolerance_pct: SweepableValue::Fixed(tolerance_pct),
        }
    };
    Ok(IndicatorColumn {
        label: format!("{kind}:{left}-{right}-{}", tolerance_pct.normalize()),
        spec,
    })
}

/// Parse the `trendline_lows:<left>-<right>` / `trendline_highs:<left>-<right>`
/// forms (r4.s6.w3) — the pivot span alone, like `pivot_high`/`pivot_low`.
fn parse_trendline(token: &str, kind: &str, rest: &str) -> anyhow::Result<IndicatorColumn> {
    let parts: Vec<&str> = rest.split('-').collect();
    let [left, right] = parts[..] else {
        anyhow::bail!("invalid --indicator {token:?}: {kind} expects <left>-<right>");
    };
    let left = parse_period(token, left)?;
    let right = parse_period(token, right)?;
    let fixed = SweepableValue::Fixed;
    let spec = if kind == "trendline_lows" {
        IndicatorSpec::TrendlineLows {
            left: fixed(left),
            right: fixed(right),
        }
    } else {
        IndicatorSpec::TrendlineHighs {
            left: fixed(left),
            right: fixed(right),
        }
    };
    Ok(IndicatorColumn {
        label: format!("{kind}:{left}-{right}"),
        spec,
    })
}

/// The token a client accepts for `spec` (r5.s2.w1) — the exact inverse of
/// [`parse_indicator_specs`], so a label this ring's callers emit is always a
/// token `pulse indicators` and MCP `export_indicators` read back:
/// `parse_indicator_specs(&[indicator_label(spec)])` returns one column whose
/// `spec` is `spec`.
///
/// Spelled exactly as the parser's own labels: the case-normalized kind, a
/// `normalize()`d decimal tolerance, the fixed `cdl:<snake_case>` name, and the
/// `highest`/`lowest` field elided when it is that kind's default (an explicit
/// write always shows, the macd-output precedent). A [`SweepableValue::Sweep`]
/// has no token in this grammar — validation rejects every sweep before a
/// compiled rule can carry one — so a swept leaf renders as its bounds
/// (`50..100..10`), diagnostic text no parser accepts.
#[must_use]
pub fn indicator_label(spec: &IndicatorSpec) -> String {
    match spec {
        IndicatorSpec::Rsi { period } => format!("rsi:{}", period_label(period)),
        IndicatorSpec::Ema { period } => format!("ema:{}", period_label(period)),
        IndicatorSpec::Adx { period } => format!("adx:{}", period_label(period)),
        IndicatorSpec::Atr { period } => format!("atr:{}", period_label(period)),
        IndicatorSpec::Highest { period, source } => {
            rolling_label("highest", period, *source, PriceField::High)
        }
        IndicatorSpec::Lowest { period, source } => {
            rolling_label("lowest", period, *source, PriceField::Low)
        }
        IndicatorSpec::Macd {
            fast,
            slow,
            signal,
            output,
        } => {
            let output = match output {
                MacdOutput::Line => "",
                MacdOutput::Signal => ":signal",
                MacdOutput::Histogram => ":histogram",
            };
            format!(
                "macd:{}-{}-{}{output}",
                period_label(fast),
                period_label(slow),
                period_label(signal)
            )
        }
        IndicatorSpec::PivotHigh { left, right } => {
            format!("pivot_high:{}-{}", period_label(left), period_label(right))
        }
        IndicatorSpec::PivotLow { left, right } => {
            format!("pivot_low:{}-{}", period_label(left), period_label(right))
        }
        IndicatorSpec::Resistance {
            left,
            right,
            tolerance_pct,
            min_touches,
            lookback,
        } => level_label(
            "resistance",
            left,
            right,
            tolerance_pct,
            min_touches,
            lookback,
        ),
        IndicatorSpec::Support {
            left,
            right,
            tolerance_pct,
            min_touches,
            lookback,
        } => level_label("support", left, right, tolerance_pct, min_touches, lookback),
        IndicatorSpec::DoubleTopNeckline {
            left,
            right,
            tolerance_pct,
        } => neckline_label("double_top_neckline", left, right, tolerance_pct),
        IndicatorSpec::DoubleBottomNeckline {
            left,
            right,
            tolerance_pct,
        } => neckline_label("double_bottom_neckline", left, right, tolerance_pct),
        IndicatorSpec::TrendlineLows { left, right } => {
            format!(
                "trendline_lows:{}-{}",
                period_label(left),
                period_label(right)
            )
        }
        IndicatorSpec::TrendlineHighs { left, right } => {
            format!(
                "trendline_highs:{}-{}",
                period_label(left),
                period_label(right)
            )
        }
        IndicatorSpec::Body => "body".to_owned(),
        IndicatorSpec::UpperWick => "upper_wick".to_owned(),
        IndicatorSpec::LowerWick => "lower_wick".to_owned(),
        IndicatorSpec::Range => "range".to_owned(),
        IndicatorSpec::CandlePattern { pattern } => format!("cdl:{}", pattern_snake(*pattern)),
    }
}

/// A period leaf as its token text (`50`), or a sweep's bounds (unreachable from
/// a compiled rule, and read by no parser).
fn period_label(period: &SweepableValue<u32>) -> String {
    match period {
        SweepableValue::Fixed(period) => period.to_string(),
        SweepableValue::Sweep { start, end, step } => format!("{start}..{end}..{step}"),
    }
}

/// A decimal leaf as its token text, `normalize()`d like the parser's own label
/// (`0.0030` and `0.003` are one column).
fn decimal_label(value: &SweepableValue<Decimal>) -> String {
    match value {
        SweepableValue::Fixed(value) => value.normalize().to_string(),
        SweepableValue::Sweep { start, end, step } => format!("{start}..{end}..{step}"),
    }
}

/// The `highest:` / `lowest:` token — the field elided when it is that kind's
/// default, exactly as [`parse_rolling_extreme`] builds its label.
fn rolling_label(
    kind: &str,
    period: &SweepableValue<u32>,
    source: PriceField,
    default: PriceField,
) -> String {
    if source == default {
        format!("{kind}:{}", period_label(period))
    } else {
        format!(
            "{kind}:{}:{}",
            period_label(period),
            price_field_label(source)
        )
    }
}

/// The `support:` / `resistance:` token: the span, the normalized tolerance
/// fraction, `min_touches` and `lookback`.
fn level_label(
    kind: &str,
    left: &SweepableValue<u32>,
    right: &SweepableValue<u32>,
    tolerance_pct: &SweepableValue<Decimal>,
    min_touches: &SweepableValue<u32>,
    lookback: &SweepableValue<u32>,
) -> String {
    format!(
        "{kind}:{}-{}-{}-{}-{}",
        period_label(left),
        period_label(right),
        decimal_label(tolerance_pct),
        period_label(min_touches),
        period_label(lookback)
    )
}

/// The neckline tokens: the span and the normalized tolerance fraction.
fn neckline_label(
    kind: &str,
    left: &SweepableValue<u32>,
    right: &SweepableValue<u32>,
    tolerance_pct: &SweepableValue<Decimal>,
) -> String {
    format!(
        "{kind}:{}-{}-{}",
        period_label(left),
        period_label(right),
        decimal_label(tolerance_pct)
    )
}

/// The one spelling the parser accepts for a price field.
fn price_field_label(field: PriceField) -> &'static str {
    match field {
        PriceField::Open => "open",
        PriceField::High => "high",
        PriceField::Low => "low",
        PriceField::Close => "close",
        PriceField::Volume => "volume",
    }
}

/// The 17 named candle patterns of schema 1.4.0 (r5.s1.w1) with their
/// `cdl:` read-path names (r5.s1.w3), in catalog order.
const CANDLE_PATTERNS: [(CandlePatternName, &str); 17] = [
    (CandlePatternName::Doji, "doji"),
    (CandlePatternName::DragonflyDoji, "dragonfly_doji"),
    (CandlePatternName::GravestoneDoji, "gravestone_doji"),
    (CandlePatternName::Hammer, "hammer"),
    (CandlePatternName::InvertedHammer, "inverted_hammer"),
    (CandlePatternName::HangingMan, "hanging_man"),
    (CandlePatternName::ShootingStar, "shooting_star"),
    (CandlePatternName::Marubozu, "marubozu"),
    (CandlePatternName::SpinningTop, "spinning_top"),
    (CandlePatternName::Engulfing, "engulfing"),
    (CandlePatternName::Harami, "harami"),
    (CandlePatternName::Piercing, "piercing"),
    (CandlePatternName::DarkCloudCover, "dark_cloud_cover"),
    (CandlePatternName::MorningStar, "morning_star"),
    (CandlePatternName::EveningStar, "evening_star"),
    (
        CandlePatternName::ThreeWhiteSoldiers,
        "three_white_soldiers",
    ),
    (CandlePatternName::ThreeBlackCrows, "three_black_crows"),
];

/// The `cdl:<snake_case>` name of a pattern — the ONE spelling
/// [`CANDLE_PATTERNS`] carries, written as an exhaustive match so a new pattern
/// is a compile error here rather than a mislabelled overlay, and asserted
/// against that table by this module's own test so the two cannot drift.
fn pattern_snake(pattern: CandlePatternName) -> &'static str {
    match pattern {
        CandlePatternName::Doji => "doji",
        CandlePatternName::DragonflyDoji => "dragonfly_doji",
        CandlePatternName::GravestoneDoji => "gravestone_doji",
        CandlePatternName::Hammer => "hammer",
        CandlePatternName::InvertedHammer => "inverted_hammer",
        CandlePatternName::HangingMan => "hanging_man",
        CandlePatternName::ShootingStar => "shooting_star",
        CandlePatternName::Marubozu => "marubozu",
        CandlePatternName::SpinningTop => "spinning_top",
        CandlePatternName::Engulfing => "engulfing",
        CandlePatternName::Harami => "harami",
        CandlePatternName::Piercing => "piercing",
        CandlePatternName::DarkCloudCover => "dark_cloud_cover",
        CandlePatternName::MorningStar => "morning_star",
        CandlePatternName::EveningStar => "evening_star",
        CandlePatternName::ThreeWhiteSoldiers => "three_white_soldiers",
        CandlePatternName::ThreeBlackCrows => "three_black_crows",
    }
}

/// Parse the `cdl:<name>` form (r5.s1.w3). `<name>` matches a pattern ignoring
/// case and underscores, so `cdl:morning_star`, `cdl:MorningStar` and
/// `cdl:morningstar` are the same pattern; the column label is always the fixed
/// `cdl:<snake_case>` of the matched pattern, so spelling variants dedup through
/// the existing label set.
fn parse_candle_pattern(token: &str, rest: &str) -> anyhow::Result<IndicatorColumn> {
    // Exactly one `:` field: a second one (the macd shape) is refused, never
    // read as part of the name.
    let name = rest.trim();
    if !name.contains(':') {
        let wanted = normalize_pattern_name(name);
        if let Some((pattern, snake)) = CANDLE_PATTERNS
            .iter()
            .find(|(_, snake)| normalize_pattern_name(snake) == wanted)
        {
            return Ok(IndicatorColumn {
                label: format!("cdl:{snake}"),
                spec: IndicatorSpec::CandlePattern { pattern: *pattern },
            });
        }
    }
    anyhow::bail!("{}", cdl_token_error(token, name))
}

/// The name normalization the `cdl` token matches on: case and underscores are
/// ignored.
fn normalize_pattern_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "")
}

/// The one refusal every malformed `cdl` token gets: it names the token and
/// lists the 17 `snake_case` names.
fn cdl_token_error(token: &str, name: &str) -> String {
    let names: Vec<&str> = CANDLE_PATTERNS.iter().map(|(_, snake)| *snake).collect();
    if name.is_empty() {
        format!(
            "invalid --indicator {token:?}: expected cdl:<pattern>, one of {}",
            names.join(", ")
        )
    } else {
        format!(
            "invalid --indicator {token:?}: unknown candle pattern {name:?}; expected one of {}",
            names.join(", ")
        )
    }
}

/// One version row inside [`StrategyEntry::versions`], in `version_tree`
/// (parent-first) order — the `list_strategies` wire shape.
#[derive(Debug, Clone, Serialize)]
pub struct VersionEntry {
    /// The version id.
    pub id: String,
    /// The parent version id, when this version was cloned from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// The `created_by` provenance string (whatever the enum serializes to).
    pub created_by: String,
    /// The adapter-minted creation timestamp (RFC3339).
    pub created_at: String,
    /// The strategy name stored inside the version's DSL document.
    pub dsl_name: String,
    /// Whether the version's latest walk-forward run passed (r2.s3.w4).
    pub certified: bool,
    /// The certifying walk-forward run's id — `null` until one is persisted.
    pub latest_walk_forward_run_id: Option<String>,
}

/// One strategy row of `list_strategies` — meta plus its version subtree.
#[derive(Debug, Clone, Serialize)]
pub struct StrategyEntry {
    /// The strategy id.
    pub id: String,
    /// The human-readable name.
    pub name: String,
    /// Whether the strategy is archived.
    pub archived: bool,
    /// The pinned "canonical" version, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_version_id: Option<String>,
    /// The version subtree in `version_tree` (parent-first) order.
    pub versions: Vec<VersionEntry>,
}

/// The `created_by` wire string — the serde `snake_case` column text
/// (`"human"`, `"composer_llm"`, …). The `Debug` fallback is unreachable:
/// `CreatedBy` is a fieldless enum whose serde repr is always a string.
fn created_by_wire(created_by: CreatedBy) -> String {
    serde_json::to_value(created_by)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{created_by:?}"))
}

/// Project one [`StrategyVersion`] onto its [`VersionEntry`] wire row.
#[must_use]
pub fn version_entry(version: &StrategyVersion) -> VersionEntry {
    VersionEntry {
        id: version.id.as_str().to_owned(),
        parent_id: version
            .parent_version_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        created_by: created_by_wire(version.created_by),
        created_at: version.created_at.to_rfc3339(),
        dsl_name: version.dsl.name.clone(),
        certified: version.certified,
        latest_walk_forward_run_id: version
            .latest_walk_forward_run_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
    }
}

/// The `get_version` wire shape — the full immutable version record, with the
/// DSL as the current-schema JSON object alongside the verbatim original.
#[derive(Debug, Clone, Serialize)]
pub struct VersionDetail {
    /// The version id.
    pub id: String,
    /// The owning strategy id.
    pub strategy_id: String,
    /// The parent version id, when cloned from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_version_id: Option<String>,
    /// The `created_by` provenance string.
    pub created_by: String,
    /// The recorded schema version of the stored document (`"MAJOR.MINOR.PATCH"`).
    pub dsl_schema_version: String,
    /// The migrated current DSL document (current-schema JSON object).
    pub dsl: serde_json::Value,
    /// The verbatim pre-migration source bytes.
    pub dsl_original: String,
    /// The adapter-computed content hash.
    pub version_hash: String,
    /// The adapter-minted creation timestamp (RFC3339).
    pub created_at: String,
    /// Whether the version's latest walk-forward run passed (r2.s3.w4).
    pub certified: bool,
    /// The certifying walk-forward run's id — `null` until one is persisted.
    pub latest_walk_forward_run_id: Option<String>,
}

/// Project a [`StrategyVersion`] onto its [`VersionDetail`] wire shape.
#[must_use]
pub fn version_detail(version: &StrategyVersion) -> VersionDetail {
    VersionDetail {
        id: version.id.as_str().to_owned(),
        strategy_id: version.strategy_id.as_str().to_owned(),
        parent_version_id: version
            .parent_version_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        created_by: created_by_wire(version.created_by),
        dsl_schema_version: version.dsl_schema_version.to_string(),
        dsl: serde_json::to_value(&version.dsl).unwrap_or(serde_json::Value::Null),
        dsl_original: version.dsl_original.clone(),
        version_hash: version.version_hash.clone(),
        created_at: version.created_at.to_rfc3339(),
        certified: version.certified,
        latest_walk_forward_run_id: version
            .latest_walk_forward_run_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
    }
}

/// Project a [`Strategy`] + its already-ordered version subtree onto
/// [`StrategyEntry`].
#[must_use]
pub fn strategy_entry(strategy: &Strategy, versions: Vec<VersionEntry>) -> StrategyEntry {
    StrategyEntry {
        id: strategy.id.as_str().to_owned(),
        name: strategy.name.clone(),
        archived: strategy.archived,
        pinned_version_id: strategy
            .pinned_version_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        versions,
    }
}

/// One `list_runs` row — the headline catalog stats plus the persisted input
/// provenance (which only `get_run` carries, so a row is built off the full
/// [`PersistedRun`], not the lighter [`RunSummary`](crate::domain::RunSummary)).
#[derive(Debug, Clone, Serialize)]
pub struct RunListEntry {
    /// The run id.
    pub run_id: String,
    /// The adapter-minted run timestamp (RFC3339 text on the column).
    pub created_at: String,
    /// Mean P&L per trade.
    pub expectancy: Decimal,
    /// Net P&L across the run.
    pub net_pnl: Decimal,
    /// Number of completed trades.
    pub trade_count: usize,
    /// The persisted inputs, or `null` for a pre-0006 row with no provenance.
    pub inputs: Option<BacktestInputs>,
    /// The walk-forward membership this run carries (r2.s3.w3): the parent run
    /// id + fold index for a `rolling-oos/v1` fold, `null` for a standalone
    /// run and for every row persisted before migration `0013`.
    pub walk_forward: Option<crate::domain::WalkForwardMembership>,
}

/// Project a full [`PersistedRun`] onto its [`RunListEntry`] wire row.
#[must_use]
pub fn run_list_entry(run: &PersistedRun) -> RunListEntry {
    RunListEntry {
        run_id: run.id.as_str().to_owned(),
        created_at: run.created_at.clone(),
        expectancy: run.summary.expectancy,
        net_pnl: run.net_pnl,
        trade_count: run.summary.trade_count,
        inputs: run.inputs.clone(),
        walk_forward: run.walk_forward.clone(),
    }
}

/// The `get_run` MFE/MAE wire shape — a three-field projection of
/// [`crate::domain::MfeMaeAggregates`].
#[derive(Debug, Clone, Serialize)]
pub struct MfeMaeWire {
    /// Mean maximum favourable excursion, in R.
    pub mean_mfe_r: Decimal,
    /// Mean maximum adverse excursion, in R.
    pub mean_mae_r: Decimal,
    /// How many trades the aggregate covers.
    pub count: usize,
}

/// The `get_run` wire shape — the persisted-run projection with no inline
/// trades.
#[derive(Debug, Clone, Serialize)]
pub struct RunDetail {
    /// Every [`SummaryStats`] field, as persisted.
    pub summary: SummaryStats,
    /// The per-regime trade-count / net-P&L breakdown, as persisted.
    pub regime_breakdown: RegimeBreakdown,
    /// The counts of entries the sizer skipped, as persisted.
    pub skipped_entries: SkippedEntryCounts,
    /// The MFE/MAE projection over the run's persisted trades.
    pub mfe_mae: MfeMaeWire,
    /// The persisted inputs, or `null` for a pre-0006 row.
    pub inputs: Option<BacktestInputs>,
    /// The still-open position a windowed run ended holding (r2.s1 G1): the
    /// direction, entry fill and size the strategy opened, marked at the last
    /// in-window candle's close. `null` for a run that ended flat or at the
    /// snapshot's real last bar. Never counted in `summary`'s closed-trade
    /// statistics — it is reported explicitly precisely because it is not a
    /// trade.
    pub open_position: Option<OpenPositionMark>,
    /// The recording engine's build-time fingerprint.
    pub engine_fingerprint: String,
    /// The recording engine's compiled target triple.
    pub engine_target: String,
    /// The stored integrity hash (re-derived and checked on read).
    pub result_content_hash: String,
    /// The equity-curve base the run started from.
    pub starting_equity: Decimal,
    /// The walk-forward membership this run carries (r2.s3.w3), or `null` for a
    /// standalone run.
    pub walk_forward: Option<crate::domain::WalkForwardMembership>,
}

/// Project a [`PersistedRun`] + its [`MfeMaeAggregates`] onto [`RunDetail`].
#[must_use]
pub fn run_detail(run: &PersistedRun, mfe_mae: &MfeMaeAggregates) -> RunDetail {
    RunDetail {
        summary: run.summary.clone(),
        regime_breakdown: run.regime_breakdown,
        skipped_entries: run.skipped_entries,
        mfe_mae: MfeMaeWire {
            mean_mfe_r: mfe_mae.avg_mfe_r,
            mean_mae_r: mfe_mae.avg_mae_r,
            count: mfe_mae.trade_count,
        },
        inputs: run.inputs.clone(),
        open_position: run.open_position.clone(),
        engine_fingerprint: run.engine_fingerprint.clone(),
        engine_target: run.engine_target.clone(),
        result_content_hash: run.result_content_hash.clone(),
        starting_equity: run.starting_equity,
        walk_forward: run.walk_forward.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::parse_indicator_specs;
    use crate::{CandlePatternName, IndicatorSpec, SweepableValue};

    /// The pattern a `CandlePattern` spec carries (schema 1.4.0, r5.s1.w3) —
    /// the `cdl:<name>` token's own accessor, since a pattern carries no period.
    fn pattern_name(spec: &IndicatorSpec) -> CandlePatternName {
        match spec {
            IndicatorSpec::CandlePattern { pattern } => *pattern,
            other => panic!("not a candle pattern: {other:?}"),
        }
    }

    fn fixed_period(spec: &IndicatorSpec) -> u32 {
        match spec {
            IndicatorSpec::Rsi { period }
            | IndicatorSpec::Ema { period }
            | IndicatorSpec::Adx { period }
            | IndicatorSpec::Atr { period }
            | IndicatorSpec::Highest { period, .. }
            | IndicatorSpec::Lowest { period, .. } => match period {
                SweepableValue::Fixed(period) => *period,
                SweepableValue::Sweep { .. } => panic!("CLI specs must be fixed"),
            },
            // r4.s6.w1: the structure variants carry their own leaf names; the
            // pivot span's left is the stand-in this helper reads.
            IndicatorSpec::PivotHigh { left, .. } | IndicatorSpec::PivotLow { left, .. } => {
                match left {
                    SweepableValue::Fixed(left) => *left,
                    SweepableValue::Sweep { .. } => panic!("CLI specs must be fixed"),
                }
            }
            IndicatorSpec::Resistance { min_touches, .. }
            | IndicatorSpec::Support { min_touches, .. } => match min_touches {
                SweepableValue::Fixed(min_touches) => *min_touches,
                SweepableValue::Sweep { .. } => panic!("CLI specs must be fixed"),
            },
            // r4.s6.w3: the pattern variants carry their own leaves; the span's
            // left is the stand-in this helper reads.
            IndicatorSpec::DoubleTopNeckline { left, .. }
            | IndicatorSpec::DoubleBottomNeckline { left, .. }
            | IndicatorSpec::TrendlineLows { left, .. }
            | IndicatorSpec::TrendlineHighs { left, .. } => match left {
                SweepableValue::Fixed(left) => *left,
                SweepableValue::Sweep { .. } => panic!("CLI specs must be fixed"),
            },
            IndicatorSpec::Macd { .. } => {
                panic!("MACD is not part of kind:period parsing")
            }
            IndicatorSpec::Body
            | IndicatorSpec::UpperWick
            | IndicatorSpec::LowerWick
            | IndicatorSpec::Range => panic!("the anatomy kinds take no period"),
            // r5.s1.w1 (schema 1.4.0) / r5.s1.w3: the named candle patterns take
            // no period either — the `cdl:<name>` token builds them, and the
            // `cdl` tests read the pattern through `pattern_name`.
            IndicatorSpec::CandlePattern { .. } => {
                panic!("the candle-pattern kinds take no period; read it with `pattern_name`")
            }
        }
    }

    #[test]
    fn parses_indicator_flag_kind_and_period() {
        let one = parse_indicator_specs(&["rsi:14".to_owned()]).expect("rsi:14 parses");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].label, "rsi:14");
        assert!(matches!(one[0].spec, IndicatorSpec::Rsi { .. }));
        assert_eq!(fixed_period(&one[0].spec), 14);

        let repeated =
            parse_indicator_specs(&["ema:50".to_owned(), "adx:14".to_owned()]).expect("parse");
        assert_eq!(repeated.len(), 2);
        assert_eq!(repeated[0].label, "ema:50");
        assert!(matches!(repeated[0].spec, IndicatorSpec::Ema { .. }));
        assert_eq!(fixed_period(&repeated[0].spec), 50);
        assert_eq!(repeated[1].label, "adx:14");
        assert!(matches!(repeated[1].spec, IndicatorSpec::Adx { .. }));
        assert_eq!(fixed_period(&repeated[1].spec), 14);

        // `atr:<period>` parses too (r2.s2 round-1 fix F5): the engine builds
        // `IndicatorSpec::Atr`, so `pulse indicators` / MCP `export_indicators`
        // can inspect the ATR an `AtrStop` sizes against.
        let atr = parse_indicator_specs(&["atr:14".to_owned()]).expect("atr:14 parses");
        assert_eq!(atr.len(), 1);
        assert_eq!(atr[0].label, "atr:14");
        assert!(matches!(atr[0].spec, IndicatorSpec::Atr { .. }));
        assert_eq!(fixed_period(&atr[0].spec), 14);

        let defaults = parse_indicator_specs(&[]).expect("defaults parse");
        assert_eq!(
            defaults
                .iter()
                .map(|column| column.label.as_str())
                .collect::<Vec<_>>(),
            ["rsi:14", "ema:50", "adx:14"]
        );

        // Repeated / case-variant specs dedup to one column each, order preserved
        // (the viewer renders one column per surviving spec).
        let deduped = parse_indicator_specs(&[
            "rsi:14".to_owned(),
            "RSI:14".to_owned(),
            "ema:50".to_owned(),
            "rsi:14".to_owned(),
        ])
        .expect("dedup parses");
        assert_eq!(
            deduped
                .iter()
                .map(|column| column.label.as_str())
                .collect::<Vec<_>>(),
            ["rsi:14", "ema:50"],
            "duplicate / case-variant --indicator flags dedup, order preserved"
        );

        assert!(parse_indicator_specs(&["macd:12".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["bogus:14".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["rsi".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["rsi:0".to_owned()]).is_err());

        // r4.s6.w3: the pattern tokens — the necklines carry the tolerance
        // fraction (normalized in the label, the `resistance` precedent), the
        // trendlines the span alone.
        let neckline = parse_indicator_specs(&["double_top_neckline:3-3-0.0030".to_owned()])
            .expect("double_top_neckline:3-3-0.0030 parses");
        assert_eq!(neckline.len(), 1);
        assert_eq!(neckline[0].label, "double_top_neckline:3-3-0.003");
        assert!(matches!(
            neckline[0].spec,
            IndicatorSpec::DoubleTopNeckline { .. }
        ));
        assert_eq!(fixed_period(&neckline[0].spec), 3);

        let bottom = parse_indicator_specs(&["double_bottom_neckline:2-2-0.05".to_owned()])
            .expect("double_bottom_neckline:2-2-0.05 parses");
        assert_eq!(bottom[0].label, "double_bottom_neckline:2-2-0.05");
        assert!(matches!(
            bottom[0].spec,
            IndicatorSpec::DoubleBottomNeckline { .. }
        ));
        assert_eq!(fixed_period(&bottom[0].spec), 2);

        let lows = parse_indicator_specs(&["trendline_lows:3-3".to_owned()])
            .expect("trendline_lows:3-3 parses");
        assert_eq!(lows[0].label, "trendline_lows:3-3");
        assert!(matches!(lows[0].spec, IndicatorSpec::TrendlineLows { .. }));
        assert_eq!(fixed_period(&lows[0].spec), 3);

        let highs = parse_indicator_specs(&["trendline_highs:1-1".to_owned()])
            .expect("trendline_highs:1-1 parses");
        assert_eq!(highs[0].label, "trendline_highs:1-1");
        assert!(matches!(
            highs[0].spec,
            IndicatorSpec::TrendlineHighs { .. }
        ));
        assert_eq!(fixed_period(&highs[0].spec), 1);

        // The malformed pattern tokens refuse, never silently coerce.
        assert!(parse_indicator_specs(&["trendline_lows:3".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["trendline_lows:3-0".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["double_top_neckline:3-3".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["double_top_neckline:3-3-0".to_owned()]).is_err());
    }

    /// The 17 names and their two spellings, in catalog order (r5.s1.w3).
    const CDL_NAMES: [(CandlePatternName, &str, &str); 17] = [
        (CandlePatternName::Doji, "doji", "Doji"),
        (
            CandlePatternName::DragonflyDoji,
            "dragonfly_doji",
            "DragonflyDoji",
        ),
        (
            CandlePatternName::GravestoneDoji,
            "gravestone_doji",
            "GravestoneDoji",
        ),
        (CandlePatternName::Hammer, "hammer", "Hammer"),
        (
            CandlePatternName::InvertedHammer,
            "inverted_hammer",
            "InvertedHammer",
        ),
        (CandlePatternName::HangingMan, "hanging_man", "HangingMan"),
        (
            CandlePatternName::ShootingStar,
            "shooting_star",
            "ShootingStar",
        ),
        (CandlePatternName::Marubozu, "marubozu", "Marubozu"),
        (
            CandlePatternName::SpinningTop,
            "spinning_top",
            "SpinningTop",
        ),
        (CandlePatternName::Engulfing, "engulfing", "Engulfing"),
        (CandlePatternName::Harami, "harami", "Harami"),
        (CandlePatternName::Piercing, "piercing", "Piercing"),
        (
            CandlePatternName::DarkCloudCover,
            "dark_cloud_cover",
            "DarkCloudCover",
        ),
        (
            CandlePatternName::MorningStar,
            "morning_star",
            "MorningStar",
        ),
        (
            CandlePatternName::EveningStar,
            "evening_star",
            "EveningStar",
        ),
        (
            CandlePatternName::ThreeWhiteSoldiers,
            "three_white_soldiers",
            "ThreeWhiteSoldiers",
        ),
        (
            CandlePatternName::ThreeBlackCrows,
            "three_black_crows",
            "ThreeBlackCrows",
        ),
    ];

    /// r5.s1.w3: every one of the 17 named candle patterns resolves from
    /// `cdl:<name>`, whatever the spelling — the name matches ignoring case and
    /// underscores, so `cdl:MorningStar`, `cdl:morning_star` and
    /// `cdl:morningstar` are one pattern — and the parsed spec carries the DSL
    /// variant.
    #[test]
    fn cdl_token_parses_all_seventeen_names_in_both_spellings() {
        for (pattern, snake, pascal) in CDL_NAMES {
            let upper = snake.to_ascii_uppercase();
            let lower = pascal.to_ascii_lowercase();
            for spelling in [snake, pascal, upper.as_str(), lower.as_str()] {
                let columns = parse_indicator_specs(&[format!("cdl:{spelling}")])
                    .unwrap_or_else(|e| panic!("cdl:{spelling} parses: {e}"));
                assert_eq!(columns.len(), 1, "one column for cdl:{spelling}");
                assert_eq!(
                    columns[0].label,
                    format!("cdl:{snake}"),
                    "the label is always the snake_case form"
                );
                assert_eq!(pattern_name(&columns[0].spec), pattern);
            }
        }
    }

    /// The label is the fixed `cdl:<snake_case>`, so case and underscore
    /// variants of one pattern dedup to a single column (order preserved), and
    /// the token mixes freely with the other kinds.
    #[test]
    fn cdl_token_label_is_snake_case_and_dedups() {
        let columns = parse_indicator_specs(&[
            "cdl:Engulfing".to_owned(),
            "rsi:14".to_owned(),
            "cdl:engulfing".to_owned(),
            "cdl:MORNING_STAR".to_owned(),
            "cdl:morning_star".to_owned(),
        ])
        .expect("the cdl columns parse");
        assert_eq!(
            columns
                .iter()
                .map(|column| column.label.as_str())
                .collect::<Vec<_>>(),
            ["cdl:engulfing", "rsi:14", "cdl:morning_star"],
            "case/underscore variants of one pattern collapse to one column"
        );
    }

    /// An unknown name — and the bare `cdl`, the empty name and the
    /// second-`:` forms — are refused with one error that names the token and
    /// lists all 17 `snake_case` names.
    #[test]
    fn cdl_token_refuses_unknown_name_and_lists_the_seventeen() {
        for bad in ["cdl:bogus", "cdl:", "cdl", "cdl:engulfing:1"] {
            let error = parse_indicator_specs(&[bad.to_owned()])
                .expect_err("a bad cdl token must be refused");
            let text = error.to_string();
            assert!(text.contains(bad), "the error names the token: {text}");
            for (_, snake, _) in CDL_NAMES {
                assert!(text.contains(snake), "the error lists {snake}: {text}");
            }
        }
    }

    /// r5.s2.w1 (SPINE G2/AC-1): every spec a compiled rule can carry has a
    /// label the token parser accepts, and `parse(label(spec)) == spec` — so the
    /// chart read's overlays are always addressable through `export_indicators`.
    #[test]
    #[allow(clippy::too_many_lines)] // one table of every spec kind, read in one pass
    fn indicator_label_round_trips_through_the_token_parser() {
        use super::{CANDLE_PATTERNS, indicator_label, parse_indicator_specs};
        use crate::domain::{IndicatorSpec, MacdOutput, PriceField, SweepableValue};
        use rust_decimal::Decimal;

        /// A fixed period leaf.
        fn fixed(value: u32) -> SweepableValue<u32> {
            SweepableValue::Fixed(value)
        }

        /// A fixed decimal leaf.
        fn fixed_decimal(value: Decimal) -> SweepableValue<Decimal> {
            SweepableValue::Fixed(value)
        }

        let mut specs = vec![
            IndicatorSpec::Rsi { period: fixed(14) },
            IndicatorSpec::Ema { period: fixed(50) },
            IndicatorSpec::Adx { period: fixed(14) },
            IndicatorSpec::Atr { period: fixed(14) },
            IndicatorSpec::Highest {
                period: fixed(20),
                source: PriceField::High,
            },
            IndicatorSpec::Highest {
                period: fixed(20),
                source: PriceField::Close,
            },
            IndicatorSpec::Lowest {
                period: fixed(20),
                source: PriceField::Low,
            },
            IndicatorSpec::Lowest {
                period: fixed(20),
                source: PriceField::Volume,
            },
            IndicatorSpec::Macd {
                fast: fixed(12),
                slow: fixed(26),
                signal: fixed(9),
                output: MacdOutput::Line,
            },
            IndicatorSpec::Macd {
                fast: fixed(12),
                slow: fixed(26),
                signal: fixed(9),
                output: MacdOutput::Signal,
            },
            IndicatorSpec::Macd {
                fast: fixed(12),
                slow: fixed(26),
                signal: fixed(9),
                output: MacdOutput::Histogram,
            },
            IndicatorSpec::PivotHigh {
                left: fixed(3),
                right: fixed(3),
            },
            IndicatorSpec::PivotLow {
                left: fixed(1),
                right: fixed(1),
            },
            IndicatorSpec::Resistance {
                left: fixed(3),
                right: fixed(3),
                tolerance_pct: fixed_decimal(Decimal::new(30, 4)),
                min_touches: fixed(2),
                lookback: fixed(100),
            },
            IndicatorSpec::Support {
                left: fixed(3),
                right: fixed(3),
                tolerance_pct: fixed_decimal(Decimal::new(5, 3)),
                min_touches: fixed(2),
                lookback: fixed(100),
            },
            IndicatorSpec::DoubleTopNeckline {
                left: fixed(3),
                right: fixed(3),
                tolerance_pct: fixed_decimal(Decimal::new(3, 3)),
            },
            IndicatorSpec::DoubleBottomNeckline {
                left: fixed(2),
                right: fixed(2),
                tolerance_pct: fixed_decimal(Decimal::new(5, 2)),
            },
            IndicatorSpec::TrendlineLows {
                left: fixed(3),
                right: fixed(3),
            },
            IndicatorSpec::TrendlineHighs {
                left: fixed(1),
                right: fixed(1),
            },
            IndicatorSpec::Body,
            IndicatorSpec::UpperWick,
            IndicatorSpec::LowerWick,
            IndicatorSpec::Range,
        ];
        // Every one of the 17 named patterns, through the one catalog table.
        specs.extend(
            CANDLE_PATTERNS
                .iter()
                .map(|(pattern, _)| IndicatorSpec::CandlePattern { pattern: *pattern }),
        );

        for spec in specs {
            let label = indicator_label(&spec);
            let columns = parse_indicator_specs(std::slice::from_ref(&label))
                .unwrap_or_else(|e| panic!("{label} must parse: {e}"));
            assert_eq!(columns.len(), 1, "one column for {label}");
            assert_eq!(columns[0].label, label, "the label is canonical");
            assert_eq!(
                columns[0].spec, spec,
                "parse(label(spec)) == spec for {label}"
            );
        }
    }

    /// The two spellings of a pattern's name live in one place each; this is what
    /// keeps `pattern_snake` and the parser's own catalog table from drifting.
    #[test]
    fn the_pattern_names_agree_with_the_catalog_table() {
        use super::{CANDLE_PATTERNS, pattern_snake};
        for (pattern, snake) in CANDLE_PATTERNS {
            assert_eq!(pattern_snake(pattern), snake);
        }
    }
}
