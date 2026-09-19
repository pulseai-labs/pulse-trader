//! `render` — the one DSL display formatter (r2.s2.w4, feature-map `b10`).
//!
//! Every surface that *describes* a strategy renders through this module: the
//! Library node's `dsl_summary` and the Designer card's `summarize_dsl` are
//! thin adapters over [`strategy`], so the two can never drift apart again —
//! the `stop loss 5%` vs `stop_loss 5%` (and `atr stop 14 x2` vs
//! `atr_stop 14 x2`) disagreement schema 1.1.0 inherited is the bug this file
//! exists to delete.
//!
//! Pure `String` building over the grammar types — no I/O, no `serde_json`, no
//! adapter imports; the dependency arrow points INWARD (adapters may depend on
//! this module; it names nothing outside `dsl`). The vocabulary is pinned
//! token-for-token by `tests/dsl_render.rs`, including the two schema-1.1.0
//! strings ledger line `d18` reads off the Library at the walk: an `htf`
//! operand's `h4:` prefix (`h4:ema(200)`) and the ATR stop `atr(14)×2`
//! (U+00D7, never `x`).

use rust_decimal::Decimal;

use super::condition::{Comparator, Condition};
use super::exit::ExitRule;
use super::risk::{Direction, RiskParams};
use super::strategy::StrategyDsl;
use super::sweepable::SweepableValue;
use super::value::{IndicatorSpec, PriceField, Series, ValueSource};

/// A [`StrategyDsl`] rendered to summary lines — the fields both wire DTOs
/// carry (`DslSummary` wraps `entry` in a one-element `Vec`; the compact card
/// DTO takes it bare).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// The trade side, `"long"` / `"short"`.
    pub direction: String,
    /// The required entry trigger, one line (e.g. `rsi(14) < 30`).
    pub entry: String,
    /// The gating filters, one line each.
    pub filters: Vec<String>,
    /// The exit rules, one line each (e.g. `stop 5%`, `atr(14)×2`).
    pub exits: Vec<String>,
    /// The risk parameters, one line each.
    pub risk: Vec<String>,
}

/// The trade side as its display word: `"long"` / `"short"`.
#[must_use]
pub fn direction(d: Direction) -> &'static str {
    match d {
        Direction::Long => "long",
        Direction::Short => "short",
    }
}

/// An indicator reference as call text — lowercase name, comma-separated
/// parameters, no spaces: `rsi(14)`, `ema(200)`, `adx(14)`, `macd(12,26,9)`,
/// `atr(14)`.
#[must_use]
pub fn indicator(spec: &IndicatorSpec) -> String {
    match spec {
        IndicatorSpec::Rsi { period } => format!("rsi({})", u32_leaf(period)),
        IndicatorSpec::Ema { period } => format!("ema({})", u32_leaf(period)),
        IndicatorSpec::Adx { period } => format!("adx({})", u32_leaf(period)),
        IndicatorSpec::Macd { fast, slow, signal } => format!(
            "macd({},{},{})",
            u32_leaf(fast),
            u32_leaf(slow),
            u32_leaf(signal)
        ),
        IndicatorSpec::Atr { period } => format!("atr({})", u32_leaf(period)),
    }
}

/// Where a compared scalar comes from: a normalized constant (`30`, `0.5`), a
/// price field's name (`close`), or an indicator call (`ema(200)`) — each
/// `htf`-series operand prefixed `h4:` (`h4:close`, `h4:ema(200)`).
#[must_use]
pub fn value(source: &ValueSource) -> String {
    match source {
        ValueSource::Constant { value } => value.normalize().to_string(),
        ValueSource::Price { series, field } => series_tag(
            *series,
            match field {
                PriceField::Open => "open".to_owned(),
                PriceField::High => "high".to_owned(),
                PriceField::Low => "low".to_owned(),
                PriceField::Close => "close".to_owned(),
                PriceField::Volume => "volume".to_owned(),
            },
        ),
        ValueSource::Indicator { series, spec } => series_tag(*series, indicator(spec)),
    }
}

/// A comparator as its symbol: `<`, `<=`, `>`, `>=`, `=`. (The two cross
/// operators are [`Condition`] variants — they render inside [`condition`].)
#[must_use]
pub fn comparator(op: Comparator) -> &'static str {
    match op {
        Comparator::Gt => ">",
        Comparator::Gte => ">=",
        Comparator::Lt => "<",
        Comparator::Lte => "<=",
        Comparator::Eq => "=",
    }
}

/// A condition as one line of text: `lhs op rhs`; `and`/`or` groups render
/// each member in parentheses joined by ` and ` / ` or `; `not` wraps its
/// operand — `rsi(14) < 30`, `(a) and (b)`, `not (…)`.
#[must_use]
pub fn condition(c: &Condition) -> String {
    match c {
        Condition::Compare { lhs, op, rhs } => {
            format!("{} {} {}", value(lhs), comparator(*op), value(rhs))
        }
        Condition::CrossesAbove { lhs, rhs } => {
            format!("{} crosses above {}", value(lhs), value(rhs))
        }
        Condition::CrossesBelow { lhs, rhs } => {
            format!("{} crosses below {}", value(lhs), value(rhs))
        }
        Condition::And { conditions } => joined(conditions, "and"),
        Condition::Or { conditions } => joined(conditions, "or"),
        Condition::Not { condition } => format!("not ({})", self::condition(condition)),
    }
}

/// An exit rule as one line: `stop 1.5%`, `take profit 2R`, `trailing 1%`,
/// `time stop 20 bars`, `signal exit (<condition>)`, and — schema 1.1.0 —
/// `atr(14)×2` for an `AtrStop` (U+00D7; `multiple` normalized: `2.0` → `2`,
/// `2.5` → `2.5`).
#[must_use]
pub fn exit(rule: &ExitRule) -> String {
    match rule {
        ExitRule::StopLoss { distance_pct } => format!("stop {}", pct_leaf(distance_pct)),
        ExitRule::TakeProfit { target_r } => {
            format!("take profit {}R", decimal_leaf(target_r))
        }
        ExitRule::TrailingStop { trail_pct } => format!("trailing {}", pct_leaf(trail_pct)),
        ExitRule::TimeStop { max_bars } => format!("time stop {} bars", u32_leaf(max_bars)),
        ExitRule::SignalExit { condition } => {
            format!("signal exit ({})", self::condition(condition))
        }
        ExitRule::AtrStop { period, multiple } => {
            format!("atr({})\u{d7}{}", u32_leaf(period), decimal_leaf(multiple))
        }
    }
}

/// The risk inputs as their two summary lines: `risk 1% per trade`,
/// `max leverage 3x`.
#[must_use]
pub fn risk(risk: &RiskParams) -> Vec<String> {
    vec![
        format!("risk {} per trade", pct_leaf(&risk.risk_per_trade_pct)),
        format!("max leverage {}x", decimal_leaf(&risk.max_leverage)),
    ]
}

/// Render a whole [`StrategyDsl`] — the one call both wire DTOs delegate to.
#[must_use]
pub fn strategy(dsl: &StrategyDsl) -> Rendered {
    Rendered {
        direction: direction(dsl.direction).to_owned(),
        entry: condition(&dsl.entry),
        filters: dsl.filters.iter().map(condition).collect(),
        exits: dsl.exits.iter().map(exit).collect(),
        risk: risk(&dsl.risk),
    }
}

/// A conjoined/disjoined list: each member parenthesized, joined by
/// ` <joiner> ` — `(a) and (b)`.
fn joined(conditions: &[Condition], joiner: &str) -> String {
    conditions
        .iter()
        .map(|c| format!("({})", condition(c)))
        .collect::<Vec<_>>()
        .join(&format!(" {joiner} "))
}

/// The `series` tag as a text prefix — schema 1.1.0's only higher timeframe is
/// H4, so an `htf` operand reads `h4:…`; `primary` renders bare.
fn series_tag(series: Series, text: String) -> String {
    match series {
        Series::Primary => text,
        Series::Htf => format!("h4:{text}"),
    }
}

/// A fixed-or-sweepable `u32` leaf: `14`, or `{5..20}` for a sweep (the
/// `{lo..hi}` slot; validation rejects sweeps before persist, so the sweep arm
/// is a faithful rendering of a shape a stored version will not carry).
fn u32_leaf(value: &SweepableValue<u32>) -> String {
    match value {
        SweepableValue::Fixed(v) => v.to_string(),
        SweepableValue::Sweep { start, end, .. } => format!("{{{start}..{end}}}"),
    }
}

/// A fixed-or-sweepable `Decimal` leaf, normalized (trailing zeros trimmed):
/// `2`, `2.5`, or `{2..3}` for a sweep.
fn decimal_leaf(value: &SweepableValue<Decimal>) -> String {
    match value {
        SweepableValue::Fixed(v) => v.normalize().to_string(),
        SweepableValue::Sweep { start, end, .. } => {
            format!("{{{}..{}}}", start.normalize(), end.normalize())
        }
    }
}

/// A decimal-fraction leaf as a percentage (`0.05` → `5%`) — the DSL stores
/// fractions; the summary speaks the human unit. A sweep fills the slot:
/// `{1..5}%`.
fn pct_leaf(value: &SweepableValue<Decimal>) -> String {
    match value {
        SweepableValue::Fixed(fraction) => {
            format!("{}%", (*fraction * Decimal::from(100)).normalize())
        }
        SweepableValue::Sweep { start, end, .. } => format!(
            "{{{}..{}}}%",
            (*start * Decimal::from(100)).normalize(),
            (*end * Decimal::from(100)).normalize()
        ),
    }
}
