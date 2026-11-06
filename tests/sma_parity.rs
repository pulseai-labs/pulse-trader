//! r6.s1.w1 (AC-2): `Sma{50, close}` against STR-0003's `TradingView` ledger.
//!
//! The committed SOLUSDT 4h fixture (`tests/fixtures/solusdt-cipher-store`)
//! holds the candles around STR-0003's two design examples. The engine's
//! SMA(50, close) at each ledger bar must agree with `TradingView`'s two-decimal
//! reading to within 0.01.
//!
//! The ledger readings are `TradingView`'s, typed from STR-0003: they are the
//! outside reference, so they are NOT derived from the fixture.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{path::PathBuf, str::FromStr};

use pulse::{
    CandleStore, CompiledValue, EvalContext, IndicatorEngine, IndicatorSpec, Pair, PriceField,
    Series, SweepableValue, Timeframe,
};
use rust_decimal::Decimal;

/// One row per ledger bar: its `open_time` in ms UTC, a label, and the
/// `TradingView` reading.
const LEDGER: &[(i64, &str, &str)] = &[
    // 2026-08-26 20:00 UTC
    (1_787_774_400_000, "2026-08-26 20:00", "91.76"),
    // 2026-09-24 12:00 UTC
    (1_790_251_200_000, "2026-09-24 12:00", "110.65"),
];

/// The largest allowed `|engine − ledger|` (the ledger is rounded to 2 dp).
const TOLERANCE: &str = "0.01";

fn sma_spec() -> IndicatorSpec {
    IndicatorSpec::Sma {
        period: SweepableValue::Fixed(50),
        source: PriceField::Close,
    }
}

#[test]
fn sma_50_close_matches_the_str_0003_ledger_on_solusdt_4h() {
    let base =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/solusdt-cipher-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("SOLUSDT");
    let head = store
        .read_head(&pair, Timeframe::H4)
        .expect("read the 4h HEAD")
        .expect("the fixture store has a SOLUSDT 4h HEAD");
    let series = store
        .read_snapshot(&pair, Timeframe::H4, &head)
        .expect("read the 4h snapshot");

    let spec = sma_spec();
    let mut engine =
        IndicatorEngine::from_specs(std::slice::from_ref(&spec)).expect("build engine");
    let operand = CompiledValue::Indicator {
        series: Series::Primary,
        spec,
        lag: 0,
    };
    let tolerance = Decimal::from_str(TOLERANCE).unwrap();

    let mut readings: Vec<(i64, Option<Decimal>)> = Vec::with_capacity(series.candles.len());
    for candle in &series.candles {
        engine.step(candle);
        readings.push((candle.open_time, engine.current(&operand)));
    }

    for (open_time, label, ledger) in LEDGER {
        let reading = readings
            .iter()
            .find(|(t, _)| t == open_time)
            .unwrap_or_else(|| panic!("the fixture has no 4h bar opening {label} UTC"));
        let engine_value = reading
            .1
            .unwrap_or_else(|| panic!("SMA(50) has no value at the {label} UTC bar"));
        let ledger = Decimal::from_str(ledger).unwrap();
        let diff = (engine_value - ledger).abs();
        assert!(
            diff <= tolerance,
            "{label} UTC: engine SMA(50, close) = {engine_value}, TradingView = {ledger}, \
             |diff| = {diff} > {tolerance}"
        );
    }
}
