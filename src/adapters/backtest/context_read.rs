//! The indicator reads behind `loss_context` (r6.s3.w4, STR-0003).
//!
//! The application ring may import `crate::adapters::backtest` and nothing else from
//! the adapters (ADR-0015), and the indicator engine lives behind it. So the two
//! engine passes the loss read needs live here, beside [`indicator_values_at`]:
//!
//! - one engine over the primary candles for `ATR(period)`;
//! - one engine over the higher-timeframe candles for the four swing pivots, fed each
//!   HTF candle only once its `close_time` is at or before the primary bar's close
//!   (the backtest's own alignment, ADR-0024, as [`step_series`] does it).
//!
//! Both passes only read. Neither writes a snapshot, a run or a hash.

use std::collections::{BTreeMap, BTreeSet};

use rust_decimal::Decimal;

use crate::adapters::indicators::engine::IndicatorEngine;
use crate::domain::{
    Candle, CandleSeries, CompiledValue, EvalContext, IndicatorSpec, Series, SweepableValue,
};

/// The four confirmed swing prices of one higher-timeframe instant. `None` before
/// that pivot is confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwingPivots {
    /// `PivotHigh`: the latest confirmed swing high.
    pub high: Option<Decimal>,
    /// `PreviousPivotHigh`: the swing high before it.
    pub previous_high: Option<Decimal>,
    /// `PivotLow`: the latest confirmed swing low.
    pub low: Option<Decimal>,
    /// `PreviousPivotLow`: the swing low before it.
    pub previous_low: Option<Decimal>,
}

/// One primary bar's reading: its close, its ATR, and the 4H swings closed by then.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarReading {
    /// The primary bar's close price.
    pub close: Decimal,
    /// `ATR(period)` at the bar; `None` while it warms.
    pub atr: Option<Decimal>,
    /// The swings closed at or before the bar's close; `None` when no HTF was given.
    pub pivots: Option<SwingPivots>,
}

fn fixed(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
}

/// The engine value of `spec` at the last stepped candle.
fn read(engine: &IndicatorEngine, spec: &IndicatorSpec) -> Option<Decimal> {
    engine.current(&CompiledValue::Indicator {
        series: Series::Primary,
        spec: spec.clone(),
        lag: 0,
    })
}

/// The four pivot specs for a `left`/`right` span, in [`SwingPivots`] field order.
fn pivot_specs(left: u32, right: u32) -> [IndicatorSpec; 4] {
    [
        IndicatorSpec::PivotHigh {
            left: fixed(left),
            right: fixed(right),
        },
        IndicatorSpec::PreviousPivotHigh {
            left: fixed(left),
            right: fixed(right),
        },
        IndicatorSpec::PivotLow {
            left: fixed(left),
            right: fixed(right),
        },
        IndicatorSpec::PreviousPivotLow {
            left: fixed(left),
            right: fixed(right),
        },
    ]
}

fn pivots_of(engine: &IndicatorEngine, specs: &[IndicatorSpec; 4]) -> SwingPivots {
    SwingPivots {
        high: read(engine, &specs[0]),
        previous_high: read(engine, &specs[1]),
        low: read(engine, &specs[2]),
        previous_low: read(engine, &specs[3]),
    }
}

/// The primary bars whose `close_time` is in `wanted`, each read with `ATR(atr_period)`
/// and the HTF swings of span `pivot_span` (`left`, `right`) closed at or before that
/// bar's close. Keyed by the bar's `close_time`.
///
/// # Errors
///
/// `Err` when an engine cannot be built from the periods.
pub fn bar_readings(
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    wanted: &BTreeSet<i64>,
    atr_period: u32,
    pivot_span: (u32, u32),
) -> Result<BTreeMap<i64, BarReading>, String> {
    let atr_spec = IndicatorSpec::Atr {
        period: fixed(atr_period),
    };
    let mut atr_engine = IndicatorEngine::from_specs(std::slice::from_ref(&atr_spec))
        .map_err(|error| error.to_string())?;
    let specs = pivot_specs(pivot_span.0, pivot_span.1);
    let mut htf_engine = htf
        .map(|_| IndicatorEngine::from_specs(&specs).map_err(|error| error.to_string()))
        .transpose()?;
    let htf_candles: &[Candle] = htf.map_or(&[][..], |series| series.candles.as_slice());
    // The next HTF candle not yet fed: a cursor over the source series hands every
    // closed candle over exactly once, as `step_series` does.
    let mut cursor = 0_usize;

    let mut found = BTreeMap::new();
    for candle in &primary.candles {
        atr_engine.step(candle);
        if let Some(engine) = htf_engine.as_mut() {
            while let Some(closed) = htf_candles.get(cursor)
                && closed.close_time <= candle.close_time
            {
                engine.step(closed);
                cursor += 1;
            }
        }
        if wanted.contains(&candle.close_time) {
            found.insert(
                candle.close_time,
                BarReading {
                    close: candle.close,
                    atr: read(&atr_engine, &atr_spec),
                    pivots: htf_engine.as_ref().map(|engine| pivots_of(engine, &specs)),
                },
            );
        }
    }
    Ok(found)
}

/// The four swings of span `pivot_span` as the HTF series stood at `at_close_ms`: every
/// HTF candle whose `close_time` is at or before it, stepped in order.
///
/// # Errors
///
/// `Err` when the engine cannot be built from the span.
pub fn swing_pivots_at(
    htf: &CandleSeries,
    at_close_ms: i64,
    pivot_span: (u32, u32),
) -> Result<SwingPivots, String> {
    let specs = pivot_specs(pivot_span.0, pivot_span.1);
    let mut engine = IndicatorEngine::from_specs(&specs).map_err(|error| error.to_string())?;
    for candle in htf
        .candles
        .iter()
        .take_while(|candle| candle.close_time <= at_close_ms)
    {
        engine.step(candle);
    }
    Ok(pivots_of(&engine, &specs))
}
