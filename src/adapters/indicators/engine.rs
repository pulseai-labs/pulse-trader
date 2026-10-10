//! Streaming indicator engine over the VS-1.1.3 indicator adapters.

use std::collections::VecDeque;

use super::{
    adx::Adx,
    atr::Atr,
    ema::Ema,
    macd::Macd,
    patterns::{NecklineIndicator, NecklineKind, TrendlineIndicator},
    rolling::RollingExtremes,
    rsi::Rsi,
    structure::{Anatomy, AnatomyKind, Extreme, LevelIndicator, PivotIndicator},
};
use crate::{
    Candle, CompiledStrategy, CompiledValue, EvalContext, Indicator, IndicatorSpec, PriceField,
    Series, SweepableValue,
};
use rust_decimal::Decimal;

/// Errors produced while building an [`IndicatorEngine`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// A future parameter sweep reached the fixed-only indicator factory.
    #[error("unexpected Sweep value at {field}: an indicator engine requires only Fixed values")]
    UnexpectedSweep {
        /// A human-readable field hint for where the stray sweep was found.
        field: String,
    },
    /// An adapter rejected a fixed period tuple.
    ///
    /// The spec is BOXED (r4.s6.w1): the structure variants took `IndicatorSpec`
    /// past the size where carrying it inline in the error makes every
    /// `Result`'s `Ok` path pay for the `Err` (`clippy::result_large_err`).
    /// An invalid-period error is rare and constructed once per refusal.
    #[error("invalid period for {spec:?}: {detail}")]
    InvalidPeriod {
        /// The spec whose fixed parameters were rejected.
        spec: Box<IndicatorSpec>,
        /// Adapter/factory detail explaining the invalid parameter tuple.
        detail: String,
    },
    /// A grammatically-valid spec whose computation lands in a later work item.
    #[error("{0}")]
    Unsupported(String),
}

/// Composes the concrete indicator adapters needed by one compiled strategy.
///
/// The caller must drive [`IndicatorEngine::step`] with a gap-free candle series
/// in ascending `open_time` order. The engine does not detect or fill missing
/// bars. A strategy driver must also gate entry evaluation on
/// [`IndicatorEngine::is_warm`]; the frozen boolean evaluator is not warmup-safe
/// for `Not`/`Or` over unavailable indicator values.
pub struct IndicatorEngine {
    indicators: Vec<IndicatorSlot>,
    /// The candle ring for lagged `Price` leaves (r3.s2.w3): the last
    /// `price_lag + 2` stepped candles, newest at the back. Lag `0` reads the
    /// back (today), lag `1` the one before it — the old
    /// `previous_candle`/`current_candle` pair, generalized.
    candles: VecDeque<Candle>,
    /// The deepest lag any `Price` leaf of this engine's series carries;
    /// bounds [`IndicatorEngine::candles`]. `0` when no lagged price leaf
    /// exists — the ring then keeps two candles (today + previous), which is
    /// exactly the pre-r3.s2.w3 pair.
    price_lag: u32,
}

struct IndicatorSlot {
    spec: IndicatorSpec,
    indicator: Box<dyn Indicator>,
    /// The value ring (r3.s2.w3, b4): the last `max_lag + 2` values this slot
    /// produced, newest at the back — today's value, the previous one, and
    /// every lagged read up to the slot's deepest lag.
    history: VecDeque<Option<Decimal>>,
    /// The deepest lag any compiled leaf using this slot's spec carries;
    /// bounds [`IndicatorSlot::history`].
    max_lag: u32,
}

impl IndicatorSlot {
    /// The lag-`n` read: `n` bars back on this slot's own series (`0` =
    /// today). `None` before the ring holds bar `t − n`.
    fn lagged(&self, n: u32) -> Option<Decimal> {
        let n = n as usize;
        let len = self.history.len();
        if n >= len {
            return None;
        }
        self.history[len - 1 - n]
    }
}

/// Whether `spec` is one of the six level/pattern variants whose values are
/// legitimately `None` on some bars after the indicator is ready — a broken
/// zone, a within-tolerance neckline not yet paired, a wrong-way trendline.
/// Their slots are warmed by their LATCHED `is_ready()` plus the ring depth,
/// without the all-history-`Some` rule (see [`IndicatorEngine::is_warm`]);
/// every other slot keeps the all-`Some` rule.
fn is_level_or_pattern(spec: &IndicatorSpec) -> bool {
    matches!(
        spec,
        IndicatorSpec::Resistance { .. }
            | IndicatorSpec::Support { .. }
            | IndicatorSpec::DoubleTopNeckline { .. }
            | IndicatorSpec::DoubleBottomNeckline { .. }
            | IndicatorSpec::TrendlineLows { .. }
            | IndicatorSpec::TrendlineHighs { .. }
    )
}

impl IndicatorEngine {
    /// Build the engine from a compiled strategy's required indicator list.
    ///
    /// The engine is the **primary-series** engine: each slot's ring depth is
    /// the deepest lag of the compiled leaves that use the slot's spec, and the
    /// candle ring depth is the deepest lagged `Price` leaf on the primary
    /// series (r3.s2.w3, b4). A strategy with no lag keeps the exact ring
    /// depths of the pre-expression engine, so its warm point does not move.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] if any required indicator spec is non-fixed or
    /// has invalid fixed periods.
    pub fn new(strategy: &CompiledStrategy) -> Result<Self, EngineError> {
        Self::for_series(strategy, Series::Primary)
    }

    /// Build the engine for one series of a compiled strategy: the primary
    /// series via [`IndicatorEngine::new`], the higher-timeframe one via
    /// `Series::Htf` — the backtest adapter builds its HTF engine through this
    /// constructor, so an `h4:` lag counts H4 bars by construction (the HTF
    /// engine steps once per closed H4 candle; Q2's own-series rule needs no
    /// special case) — and the fixed daily one via `Series::D1` (r3.s2.w4),
    /// stepped once per closed UTC-midnight candle for the same reason.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] if any required indicator spec is non-fixed or
    /// has invalid fixed periods.
    pub fn for_series(strategy: &CompiledStrategy, series: Series) -> Result<Self, EngineError> {
        let (slot_lags, price_lag) = strategy.series_lags(series);
        let specs = match series {
            Series::Primary => strategy.required_indicators(),
            Series::Htf => strategy.required_htf_indicators(),
            Series::D1 => strategy.required_d1_indicators(),
        };
        Self::from_specs_with_lags(specs, &slot_lags, price_lag)
    }

    /// Build the engine from raw indicator specs (no lags — every ring keeps
    /// today + previous, the pre-r3.s2.w3 behaviour).
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] if any spec is non-fixed or has invalid fixed
    /// periods.
    pub fn from_specs(specs: &[IndicatorSpec]) -> Result<Self, EngineError> {
        Self::from_specs_with_lags(specs, &[], 0)
    }

    /// The ring-depth-aware factory: slot `i` keeps `slot_lags[i] + 2` values
    /// (aligned with `specs`; a missing entry means lag 0) and the candle ring
    /// keeps `price_lag + 2` candles.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] if any spec is non-fixed or has invalid fixed
    /// periods.
    pub fn from_specs_with_lags(
        specs: &[IndicatorSpec],
        slot_lags: &[u32],
        price_lag: u32,
    ) -> Result<Self, EngineError> {
        let mut indicators = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            if indicators
                .iter()
                .any(|slot: &IndicatorSlot| slot.spec == *spec)
            {
                continue;
            }
            indicators.push(IndicatorSlot {
                spec: spec.clone(),
                indicator: build_indicator(spec)?,
                history: VecDeque::with_capacity(
                    slot_lags.get(index).copied().unwrap_or(0) as usize + 2,
                ),
                max_lag: slot_lags.get(index).copied().unwrap_or(0),
            });
        }
        Ok(Self {
            indicators,
            candles: VecDeque::with_capacity(price_lag as usize + 2),
            price_lag,
        })
    }

    /// Number of distinct indicator instances owned by the engine.
    #[must_use]
    pub fn indicator_count(&self) -> usize {
        self.indicators.len()
    }

    /// Whether every owned indicator is fully warm: its ring holds `max_lag + 2`
    /// values (since r3.s2.w3 (b4)), its readiness window is satisfied, and —
    /// for every slot except the six level/pattern variants (the carve-out
    /// below) — it has both a **current** and a **previous** value. The candle
    /// ring holds `price_lag + 2` candles when a lagged `Price` leaf exists.
    ///
    /// A driver must not evaluate or fire strategy entry while
    /// [`IndicatorEngine::is_warm`] is false. Since #36 the guarantee includes
    /// a previous value for those slots: on the bar right after an indicator's
    /// first value, `previous` is still `None`, and a `CrossesAbove`/`CrossesBelow`
    /// leaf evaluates `false` there — which a `Not`/`Or` composition would otherwise
    /// turn into an entry on the first warm bar (#16, spine ruling a1). A
    /// strategy with no lag has `max_lag = 0`, so its warm point is exactly
    /// the pre-expression one.
    ///
    /// **The level/pattern carve-out (r4.s6 PR-362 fix rounds 1-2, Codex
    /// 4238952612/4239217700/4239217701 + close review R1).** The six
    /// level/pattern variants (`Resistance`, `Support`, `DoubleTopNeckline`,
    /// `DoubleBottomNeckline`, `TrendlineLows`, `TrendlineHighs`) yield `None`
    /// on a bar where no level exists after they are ready — their `is_ready()`
    /// reports "a value is possible at all", never "the next call returns
    /// `Some`" — and a clean break of the only zone is exactly such a bar. The
    /// taught break `CrossesAbove(close, lag(Resistance, 1))` reads the PREVIOUS
    /// bars' values, so the breakout bar's own `None` must not hold the whole
    /// entry gate cold. Such a slot therefore counts as warm once its
    /// `is_ready()` is true AND its ring holds `max_lag + 2` values (rule B:
    /// the ring-depth requirement stays — a lagged leaf never reads past the
    /// history that exists, so a `Not` over an unavailable lag cannot fire
    /// hundreds of bars early); a `None` from it is read by the no-value rule
    /// (a comparison over it is false), never as "not warm". Its readiness is
    /// LATCHED by the adapters (rule A): once true it never reverts, so a pivot
    /// aging out of `lookback` cannot make the slot cold and freeze every
    /// unrelated entry branch behind it. Every other slot keeps the
    /// all-history-`Some` rule unchanged.
    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.indicators.iter().all(|slot| {
            if is_level_or_pattern(&slot.spec) {
                slot.history.len() == slot.max_lag as usize + 2 && slot.indicator.is_ready()
            } else {
                slot.history.len() == slot.max_lag as usize + 2
                    && slot.history.iter().all(Option::is_some)
                    && slot.indicator.is_ready()
            }
        }) && (self.price_lag == 0 || self.candles.len() == self.price_lag as usize + 2)
    }

    /// Advance every owned indicator by one contiguous candle.
    ///
    /// The caller must pass gap-free, ascending candles. This method is the only
    /// mutator and never looks ahead. A driver must not evaluate/fire strategy
    /// entry while [`IndicatorEngine::is_warm`] is false.
    pub fn step(&mut self, candle: &Candle) {
        for slot in &mut self.indicators {
            let value = slot.indicator.next(candle);
            slot.history.push_back(value);
            let bound = slot.max_lag as usize + 2;
            while slot.history.len() > bound {
                slot.history.pop_front();
            }
        }
        self.candles.push_back(candle.clone());
        let bound = self.price_lag as usize + 2;
        while self.candles.len() > bound {
            self.candles.pop_front();
        }
    }

    /// The lag-`n` value of the slot with the given spec (`n` = 0 is today).
    fn lagged_indicator(&self, spec: &IndicatorSpec, lag: u32) -> Option<Decimal> {
        self.indicators
            .iter()
            .find(|slot| slot.spec == *spec)
            .and_then(|slot| slot.lagged(lag))
    }

    /// The lag-`n` price read (`n` = 0 is the current candle's field).
    fn lagged_price(&self, field: PriceField, lag: u32) -> Option<Decimal> {
        let len = self.candles.len();
        let n = lag as usize;
        if n >= len {
            return None;
        }
        let candle = &self.candles[len - 1 - n];
        Some(price(candle, field))
    }
}

impl EvalContext for IndicatorEngine {
    fn current(&self, value: &CompiledValue) -> Option<Decimal> {
        match value {
            CompiledValue::Const(value) => Some(*value),
            CompiledValue::Price { field, lag, .. } => self.lagged_price(*field, *lag),
            CompiledValue::Indicator { spec, lag, .. } => self.lagged_indicator(spec, *lag),
            // A compound value evaluates its tree through this same context
            // (the Q2 pointwise semantics live in `CompiledValue::eval`).
            value @ CompiledValue::Arith { .. } => value.eval(self, false),
        }
    }

    fn previous(&self, value: &CompiledValue) -> Option<Decimal> {
        match value {
            CompiledValue::Const(value) => Some(*value),
            CompiledValue::Price { field, lag, .. } => {
                self.lagged_price(*field, lag.saturating_add(1))
            }
            CompiledValue::Indicator { spec, lag, .. } => {
                self.lagged_indicator(spec, lag.saturating_add(1))
            }
            value @ CompiledValue::Arith { .. } => value.eval(self, true),
        }
    }
}

fn build_indicator(spec: &IndicatorSpec) -> Result<Box<dyn Indicator>, EngineError> {
    match spec {
        IndicatorSpec::Rsi { period } => {
            let period = fixed_u32(period, "rsi.period")?;
            Rsi::new(period)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("RSI period {period} is invalid")))
        }
        IndicatorSpec::Ema { period } => {
            let period = fixed_u32(period, "ema.period")?;
            Ema::new(period)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("EMA period {period} is invalid")))
        }
        IndicatorSpec::Adx { period } => {
            let period = fixed_u32(period, "adx.period")?;
            Adx::new(period)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("ADX period {period} is invalid")))
        }
        IndicatorSpec::Macd {
            fast,
            slow,
            signal,
            output,
        } => {
            let fast = fixed_u32(fast, "macd.fast")?;
            let slow = fixed_u32(slow, "macd.slow")?;
            let signal = fixed_u32(signal, "macd.signal")?;
            if fast >= slow {
                return Err(invalid_period(
                    spec,
                    format!("MACD fast period {fast} must be less than slow period {slow}"),
                ));
            }
            Macd::new(fast, slow, signal, *output)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| {
                    invalid_period(
                        spec,
                        format!(
                            "MACD periods fast={fast}, slow={slow}, signal={signal} are invalid"
                        ),
                    )
                })
        }
        IndicatorSpec::Atr { period } => {
            let period = fixed_u32(period, "atr.period")?;
            Atr::new(period)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("ATR period {period} is invalid")))
        }
        IndicatorSpec::Highest { period, source } => {
            let period = fixed_u32(period, "highest.period")?;
            RollingExtremes::new(period, *source, true)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("Highest period {period} is invalid")))
        }
        IndicatorSpec::Lowest { period, source } => {
            let period = fixed_u32(period, "lowest.period")?;
            RollingExtremes::new(period, *source, false)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("Lowest period {period} is invalid")))
        }
        // The structure indicators (schema 1.3.0, r4.s6.w1) and the pattern
        // indicators (r4.s6.w3): twelve variants over the `structure` and
        // `patterns` adapters, built by their own factory. They are listed here
        // BY NAME — never matched with a wildcard — so a new variant still
        // breaks this match and has to be routed by hand.
        IndicatorSpec::PivotHigh { .. }
        | IndicatorSpec::PivotLow { .. }
        | IndicatorSpec::Resistance { .. }
        | IndicatorSpec::Support { .. }
        | IndicatorSpec::DoubleTopNeckline { .. }
        | IndicatorSpec::DoubleBottomNeckline { .. }
        | IndicatorSpec::TrendlineLows { .. }
        | IndicatorSpec::TrendlineHighs { .. }
        | IndicatorSpec::Body
        | IndicatorSpec::UpperWick
        | IndicatorSpec::LowerWick
        | IndicatorSpec::Range => build_structure_indicator(spec),
    }
}

/// Build the structure and pattern families' adapters (schema 1.3.0, r4.s6.w1
/// and r4.s6.w3) — the two groups whose math lives in `structure`/`patterns`
/// rather than behind `convert`.
///
/// `build_indicator` routes exactly the twelve variants here, so the trailing
/// arm is a contract guard, unreachable by construction (the house shape at
/// `agent/tools.rs`'s `Op` dispatch): a new variant is a compile error in the
/// factory's own match first, and that match is where its builder is chosen.
fn build_structure_indicator(spec: &IndicatorSpec) -> Result<Box<dyn Indicator>, EngineError> {
    match spec {
        IndicatorSpec::PivotHigh { left, right } => {
            build_pivot(spec, left, right, Extreme::High, "PivotHigh")
        }
        IndicatorSpec::PivotLow { left, right } => {
            build_pivot(spec, left, right, Extreme::Low, "PivotLow")
        }
        IndicatorSpec::Resistance {
            left,
            right,
            tolerance_pct,
            min_touches,
            lookback,
        } => build_level(
            spec,
            (left, right, tolerance_pct, min_touches, lookback),
            true,
            "Resistance",
        ),
        IndicatorSpec::Support {
            left,
            right,
            tolerance_pct,
            min_touches,
            lookback,
        } => build_level(
            spec,
            (left, right, tolerance_pct, min_touches, lookback),
            false,
            "Support",
        ),
        IndicatorSpec::DoubleTopNeckline {
            left,
            right,
            tolerance_pct,
        } => build_neckline(
            spec,
            (left, right, tolerance_pct),
            NecklineKind::DoubleTop,
            "DoubleTopNeckline",
        ),
        IndicatorSpec::DoubleBottomNeckline {
            left,
            right,
            tolerance_pct,
        } => build_neckline(
            spec,
            (left, right, tolerance_pct),
            NecklineKind::DoubleBottom,
            "DoubleBottomNeckline",
        ),
        IndicatorSpec::TrendlineLows { left, right } => {
            build_trendline(spec, left, right, true, "TrendlineLows")
        }
        IndicatorSpec::TrendlineHighs { left, right } => {
            build_trendline(spec, left, right, false, "TrendlineHighs")
        }
        // The four anatomy variants take no parameters and are defined on every
        // bar, including the first — nothing can refuse them.
        IndicatorSpec::Body => Ok(Box::new(Anatomy::new(AnatomyKind::Body))),
        IndicatorSpec::UpperWick => Ok(Box::new(Anatomy::new(AnatomyKind::UpperWick))),
        IndicatorSpec::LowerWick => Ok(Box::new(Anatomy::new(AnatomyKind::LowerWick))),
        IndicatorSpec::Range => Ok(Box::new(Anatomy::new(AnatomyKind::Range))),
        other => unreachable!(
            "build_indicator routes only the structure/pattern variants here, never {other:?}"
        ),
    }
}

/// Build a `PivotHigh`/`PivotLow` slot from its fixed span.
fn build_pivot(
    spec: &IndicatorSpec,
    left: &SweepableValue<u32>,
    right: &SweepableValue<u32>,
    extreme: Extreme,
    name: &str,
) -> Result<Box<dyn Indicator>, EngineError> {
    let left = fixed_u32(left, &format!("{}.left", name.to_ascii_lowercase()))?;
    let right = fixed_u32(right, &format!("{}.right", name.to_ascii_lowercase()))?;
    PivotIndicator::new(left, right, extreme)
        .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
        .ok_or_else(|| {
            invalid_period(
                spec,
                format!("{name} left={left}, right={right} is invalid"),
            )
        })
}

/// The five borrowed `SweepableValue` leaves of a `Resistance`/`Support` spec,
/// in declaration order: `(left, right, tolerance_pct, min_touches, lookback)`.
/// Named so the factory can take them as one value without tripping the
/// pedantic type-complexity budget.
type LevelLeaves<'a> = (
    &'a SweepableValue<u32>,
    &'a SweepableValue<u32>,
    &'a SweepableValue<Decimal>,
    &'a SweepableValue<u32>,
    &'a SweepableValue<u32>,
);

/// Build a `Resistance`/`Support` slot from its fixed parameter tuple.
fn build_level(
    spec: &IndicatorSpec,
    leaves: LevelLeaves<'_>,
    above: bool,
    name: &str,
) -> Result<Box<dyn Indicator>, EngineError> {
    let (left, right, tolerance_pct, min_touches, lookback) = leaves;
    let field = name.to_ascii_lowercase();
    let left = fixed_u32(left, &format!("{field}.left"))?;
    let right = fixed_u32(right, &format!("{field}.right"))?;
    let tolerance_pct = fixed_decimal(tolerance_pct, &format!("{field}.tolerance_pct"))?;
    let min_touches = fixed_u32(min_touches, &format!("{field}.min_touches"))?;
    let lookback = fixed_u32(lookback, &format!("{field}.lookback"))?;
    LevelIndicator::new(left, right, tolerance_pct, min_touches, lookback, above)
        .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
        .ok_or_else(|| {
            invalid_period(
                spec,
                format!(
                    "{name} parameters left={left}, right={right}, tolerance_pct={tolerance_pct}, \
                     min_touches={min_touches}, lookback={lookback} are invalid"
                ),
            )
        })
}

/// The three borrowed `SweepableValue` leaves of a `DoubleTopNeckline` /
/// `DoubleBottomNeckline` spec, in declaration order:
/// `(left, right, tolerance_pct)`.
type NecklineLeaves<'a> = (
    &'a SweepableValue<u32>,
    &'a SweepableValue<u32>,
    &'a SweepableValue<Decimal>,
);

/// Build a `DoubleTopNeckline`/`DoubleBottomNeckline` slot from its fixed
/// parameter tuple.
fn build_neckline(
    spec: &IndicatorSpec,
    leaves: NecklineLeaves<'_>,
    kind: NecklineKind,
    name: &str,
) -> Result<Box<dyn Indicator>, EngineError> {
    let (left, right, tolerance_pct) = leaves;
    let field = name.to_ascii_lowercase();
    let left = fixed_u32(left, &format!("{field}.left"))?;
    let right = fixed_u32(right, &format!("{field}.right"))?;
    let tolerance_pct = fixed_decimal(tolerance_pct, &format!("{field}.tolerance_pct"))?;
    NecklineIndicator::new(left, right, tolerance_pct, kind)
        .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
        .ok_or_else(|| {
            invalid_period(
                spec,
                format!(
                    "{name} parameters left={left}, right={right}, tolerance_pct={tolerance_pct} \
                     are invalid"
                ),
            )
        })
}

/// Build a `TrendlineLows`/`TrendlineHighs` slot from its fixed span.
fn build_trendline(
    spec: &IndicatorSpec,
    left: &SweepableValue<u32>,
    right: &SweepableValue<u32>,
    rising: bool,
    name: &str,
) -> Result<Box<dyn Indicator>, EngineError> {
    let field = name.to_ascii_lowercase();
    let left = fixed_u32(left, &format!("{field}.left"))?;
    let right = fixed_u32(right, &format!("{field}.right"))?;
    TrendlineIndicator::new(left, right, rising)
        .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
        .ok_or_else(|| {
            invalid_period(
                spec,
                format!("{name} left={left}, right={right} is invalid"),
            )
        })
}

fn fixed_u32(value: &SweepableValue<u32>, field: &str) -> Result<u32, EngineError> {
    match value {
        SweepableValue::Fixed(value) => Ok(*value),
        SweepableValue::Sweep { .. } => Err(EngineError::UnexpectedSweep {
            field: field.to_owned(),
        }),
    }
}

fn fixed_decimal(value: &SweepableValue<Decimal>, field: &str) -> Result<Decimal, EngineError> {
    match value {
        SweepableValue::Fixed(value) => Ok(*value),
        SweepableValue::Sweep { .. } => Err(EngineError::UnexpectedSweep {
            field: field.to_owned(),
        }),
    }
}

fn invalid_period(spec: &IndicatorSpec, detail: String) -> EngineError {
    EngineError::InvalidPeriod {
        spec: Box::new(spec.clone()),
        detail,
    }
}

fn price(candle: &Candle, field: PriceField) -> Decimal {
    match field {
        PriceField::Open => candle.open,
        PriceField::High => candle.high,
        PriceField::Low => candle.low,
        PriceField::Close => candle.close,
        PriceField::Volume => candle.volume,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{EngineError, IndicatorEngine};
    use crate::{
        Candle, Comparator, Condition, Direction, EvalContext, ExitRule, IndicatorSpec, PriceField,
        RiskParams, SchemaVersion, Series, StrategyDsl, SweepableValue, ValueSource, compile,
        validate,
    };
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn d(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn candle(idx: i64, close: &str) -> Candle {
        let close = d(close);
        Candle {
            open_time: idx * 60_000,
            close_time: idx * 60_000 + 59_999,
            open: close,
            high: close + Decimal::ONE,
            low: close - Decimal::ONE,
            close,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    fn stop_exit() -> ExitRule {
        ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(d("0.05")),
        }
    }

    fn risk() -> RiskParams {
        RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(d("0.01")),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        }
    }

    fn compiled(entry: Condition, filters: Vec<Condition>) -> crate::CompiledStrategy {
        let dsl = StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "engine fixture".to_owned(),
            direction: Direction::Long,
            entry,
            filters,
            exits: vec![stop_exit()],
            risk: risk(),
        };
        compile(&validate(&dsl).expect("fixture validates")).expect("fixture compiles")
    }

    fn indicator_value(spec: IndicatorSpec) -> ValueSource {
        ValueSource::Indicator {
            series: Series::Primary,
            spec,
        }
    }

    fn constant(value: &str) -> ValueSource {
        ValueSource::Constant { value: d(value) }
    }

    fn price(field: PriceField) -> ValueSource {
        ValueSource::Price {
            series: Series::Primary,
            field,
        }
    }

    fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
        Condition::Compare { lhs, op, rhs }
    }

    fn rsi(period: u32) -> IndicatorSpec {
        IndicatorSpec::Rsi {
            period: SweepableValue::Fixed(period),
        }
    }

    fn ema(period: u32) -> IndicatorSpec {
        IndicatorSpec::Ema {
            period: SweepableValue::Fixed(period),
        }
    }

    #[test]
    fn engine_builds_one_indicator_per_distinct_spec() {
        let rsi = rsi(2);
        let ema = ema(2);
        let strategy = compiled(
            compare(indicator_value(rsi.clone()), Comparator::Lt, constant("30")),
            vec![
                compare(indicator_value(rsi.clone()), Comparator::Gt, constant("10")),
                compare(
                    indicator_value(ema.clone()),
                    Comparator::Gt,
                    constant("100"),
                ),
            ],
        );

        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");

        assert_eq!(engine.indicator_count(), 2);
        for (idx, close) in (0_i64..).zip(["100", "99", "98"]) {
            engine.step(&candle(idx, close));
        }
        let first = engine.current(&crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec: rsi.clone(),
            lag: 0,
        });
        let second = engine.current(&crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec: rsi,
            lag: 0,
        });
        assert_eq!(first, second);
        assert!(
            engine
                .current(&crate::CompiledValue::Indicator {
                    series: Series::Primary,
                    spec: ema,
                    lag: 0,
                })
                .is_some()
        );
    }

    #[test]
    fn engine_current_and_previous_resolve_price_const_indicator() {
        let spec = ema(2);
        let strategy = compiled(
            compare(
                indicator_value(spec.clone()),
                Comparator::Gt,
                constant("100"),
            ),
            vec![],
        );
        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
        for (idx, close) in (0_i64..).zip(["100", "102", "104"]) {
            engine.step(&candle(idx, close));
        }

        let const_value = crate::CompiledValue::Const(d("7"));
        let close_value = crate::CompiledValue::Price {
            series: Series::Primary,
            field: PriceField::Close,
            lag: 0,
        };
        let indicator = crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec,
            lag: 0,
        };

        assert_eq!(engine.current(&const_value), Some(d("7")));
        assert_eq!(engine.previous(&const_value), Some(d("7")));
        assert_eq!(engine.current(&close_value), Some(d("104")));
        assert_eq!(engine.previous(&close_value), Some(d("102")));
        assert!(engine.current(&indicator).is_some());
        assert!(engine.previous(&indicator).is_some());
    }

    #[test]
    fn engine_previous_is_none_on_first_bar() {
        let spec = ema(2);
        let strategy = compiled(
            compare(
                indicator_value(spec.clone()),
                Comparator::Gt,
                constant("100"),
            ),
            vec![],
        );
        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
        engine.step(&candle(0, "100"));

        let close_value = crate::CompiledValue::Price {
            series: Series::Primary,
            field: PriceField::Close,
            lag: 0,
        };
        let indicator = crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec,
            lag: 0,
        };
        assert_eq!(engine.current(&close_value), Some(d("100")));
        assert_eq!(engine.previous(&close_value), None);
        assert_eq!(engine.previous(&indicator), None);
    }

    #[test]
    fn engine_indicator_is_none_during_warmup() {
        let spec = rsi(3);
        let strategy = compiled(
            compare(
                indicator_value(spec.clone()),
                Comparator::Lt,
                constant("30"),
            ),
            vec![],
        );
        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
        let value = crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec,
            lag: 0,
        };

        for (idx, close) in (0_i64..).zip(["100", "99", "98"]) {
            engine.step(&candle(idx, close));
            assert_eq!(engine.current(&value), None);
            assert!(!engine.is_warm());
        }
        engine.step(&candle(3, "97"));
        assert!(engine.current(&value).is_some());
        // #36: warm ALSO requires a previous value — on the bar right after
        // the first value exists, `previous` is still `None`.
        assert!(!engine.is_warm());
        engine.step(&candle(4, "96"));
        assert!(engine.is_warm());
    }

    #[test]
    fn entry_cannot_fire_during_warmup_then_can() {
        let spec = rsi(3);
        let entry = Condition::Not {
            condition: Box::new(compare(
                indicator_value(spec),
                Comparator::Gt,
                constant("70"),
            )),
        };
        let strategy = compiled(entry, vec![]);
        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");

        for (idx, close) in (0_i64..).zip(["100", "99", "98"]) {
            engine.step(&candle(idx, close));
            assert!(
                strategy.entry().eval(&engine),
                "pins the Not(warmup) hazard"
            );
            let gated_entry = engine.is_warm() && strategy.entry().eval(&engine);
            assert!(!gated_entry, "the readiness gate suppresses warmup entry");
        }

        engine.step(&candle(3, "97"));
        // #36: the gate holds one bar longer now — `previous` is `None` on the
        // bar right after the first value, so the entry still cannot fire.
        assert!(!engine.is_warm());
        assert!(
            !engine.is_warm() && strategy.entry().eval(&engine),
            "the readiness gate suppresses the first-warm-bar entry"
        );
        engine.step(&candle(4, "96"));
        assert!(engine.is_warm());
        assert!(strategy.entry().eval(&engine));
    }

    #[test]
    fn factory_rejects_sweep_payload() {
        let result = IndicatorEngine::from_specs(&[IndicatorSpec::Rsi {
            period: SweepableValue::Sweep {
                start: 2,
                end: 10,
                step: 1,
            },
        }]);
        let Err(err) = result else {
            panic!("sweep payload must be rejected");
        };

        assert!(matches!(err, EngineError::UnexpectedSweep { .. }));
    }

    #[test]
    fn engine_step_is_deterministic_across_repeated_runs() {
        let rsi = rsi(3);
        let ema = ema(2);
        let strategy = compiled(
            compare(indicator_value(rsi.clone()), Comparator::Lt, constant("30")),
            vec![compare(
                indicator_value(ema.clone()),
                Comparator::Gt,
                price(PriceField::Close),
            )],
        );
        let candles = ["100.5", "99.25", "98.75", "97.5", "99.0"];
        let run = || {
            let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
            let rsi_value = crate::CompiledValue::Indicator {
                series: Series::Primary,
                spec: rsi.clone(),
                lag: 0,
            };
            let ema_value = crate::CompiledValue::Indicator {
                series: Series::Primary,
                spec: ema.clone(),
                lag: 0,
            };
            (0_i64..)
                .zip(candles)
                .map(|(idx, close)| {
                    engine.step(&candle(idx, close));
                    (
                        engine.current(&rsi_value),
                        engine.previous(&rsi_value),
                        engine.current(&ema_value),
                        engine.previous(&ema_value),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(run(), run(), "NFR-2: repeated engine runs match exactly");
    }

    #[test]
    fn factory_rejects_invalid_period() {
        let result = IndicatorEngine::from_specs(&[IndicatorSpec::Rsi {
            period: SweepableValue::Fixed(0),
        }]);
        let Err(err) = result else {
            panic!("invalid period must be rejected");
        };

        assert!(matches!(err, EngineError::InvalidPeriod { .. }));
    }

    /// r2.s2.w2: the factory builds `Atr` — the Wilder true-range adapter —
    /// like every other fixed-period spec.
    #[test]
    fn factory_builds_atr() {
        let engine = IndicatorEngine::from_specs(&[IndicatorSpec::Atr {
            period: SweepableValue::Fixed(14),
        }])
        .expect("atr builds in r2.s2.w2");

        assert_eq!(engine.indicator_count(), 1);
    }
}
