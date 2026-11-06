//! r6.s1.w2 (AC-2): `WaveTrend{9, 12}` (WT1) against STR-0003's `TradingView`
//! ledger.
//!
//! The committed SOLUSDT 4h fixture (`tests/fixtures/solusdt-cipher-store`)
//! starts 2026-06-01, so every ledger bar is more than 400 4h bars past WT1's
//! warm-up (`2·9 + 12 − 3 = 27` candles). `TradingView` seeds its EMAs with an
//! SMA and the engine seeds with the first value; the difference has decayed
//! well below the tolerance by then. The engine's WT1 at each ledger bar must
//! agree with `TradingView`'s two-decimal reading to within 0.05.
//!
//! The ledger readings are `TradingView`'s, typed from STR-0003: they are the
//! outside reference, so they are NOT derived from the fixture or the engine.
//! If a bar misses the tolerance, do not tune the formula: the failure names
//! the bar, the engine value and the ledger value.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{path::PathBuf, str::FromStr};

use pulse::{
    CandleStore, CompiledValue, EvalContext, IndicatorEngine, IndicatorSpec, Pair, Series,
    SweepableValue, Timeframe,
};
use rust_decimal::Decimal;

/// One row per ledger bar: its `open_time` in ms UTC, a label, and the
/// `TradingView` WT1 reading.
const LEDGER: &[(i64, &str, &str)] = &[
    (1_787_745_600_000, "2026-08-26 12:00", "-2.57"),
    (1_787_760_000_000, "2026-08-26 16:00", "-7.80"),
    (1_787_774_400_000, "2026-08-26 20:00", "16.39"),
    (1_790_208_000_000, "2026-09-24 00:00", "-10.96"),
    (1_790_222_400_000, "2026-09-24 04:00", "-12.52"),
    (1_790_236_800_000, "2026-09-24 08:00", "-22.83"),
    (1_790_251_200_000, "2026-09-24 12:00", "-19.21"),
    (1_790_280_000_000, "2026-09-24 20:00", "5.92"),
];

/// The largest allowed `|engine − ledger|` (the ledger is rounded to 2 dp).
const TOLERANCE: &str = "0.05";

fn wavetrend_spec() -> IndicatorSpec {
    IndicatorSpec::WaveTrend {
        channel: SweepableValue::Fixed(9),
        average: SweepableValue::Fixed(12),
    }
}

#[test]
fn wavetrend_9_12_matches_the_str_0003_ledger_on_solusdt_4h() {
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

    let spec = wavetrend_spec();
    let mut engine =
        IndicatorEngine::from_specs(std::slice::from_ref(&spec)).expect("build engine");
    let operand = CompiledValue::Indicator {
        series: Series::Primary,
        spec,
        lag: 0,
    };
    let tolerance = Decimal::from_str(TOLERANCE).unwrap();

    // Every 4h bar from the first, so the EMAs warm exactly as they would in a
    // run.
    let mut readings: Vec<(i64, Option<Decimal>)> = Vec::with_capacity(series.candles.len());
    for candle in &series.candles {
        engine.step(candle);
        readings.push((candle.open_time, engine.current(&operand)));
    }

    // Collect every miss, so one run names all of them.
    let mut misses: Vec<String> = Vec::new();
    for (open_time, label, ledger) in LEDGER {
        let Some((index, (_, reading))) = readings
            .iter()
            .enumerate()
            .find(|(_, (t, _))| t == open_time)
        else {
            misses.push(format!(
                "{label} UTC: the fixture has no 4h bar at that open"
            ));
            continue;
        };
        assert!(
            index > 400,
            "{label} UTC is bar {index}: the parity claim needs it past 400 bars of warm-up"
        );
        let ledger = Decimal::from_str(ledger).unwrap();
        match reading {
            None => misses.push(format!("{label} UTC: WT1 has no value (ledger {ledger})")),
            Some(engine_value) => {
                let diff = (*engine_value - ledger).abs();
                if diff > tolerance {
                    misses.push(format!(
                        "{label} UTC: engine WT1 = {engine_value}, TradingView = {ledger}, \
                         |diff| = {diff} > {tolerance}"
                    ));
                }
            }
        }
    }
    assert!(
        misses.is_empty(),
        "WT1(9, 12) parity misses:\n{}",
        misses.join("\n")
    );
}
