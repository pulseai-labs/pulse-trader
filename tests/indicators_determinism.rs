//! Single-process self-determinism proof for the WI-3.04 indicator engine.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

use pulse::{
    Candle, CandleStore, CompiledValue, EvalContext, IndicatorEngine, IndicatorSpec, Pair, Series,
    SweepableValue, Timeframe,
};
use rust_decimal::Decimal;

const RUNS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndicatorSnapshot {
    rsi_14: Option<Decimal>,
    ema_50: Option<Decimal>,
    adx_14: Option<Decimal>,
    macd_12_26_9: Option<Decimal>,
}

fn fixed(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
}

fn specs() -> [IndicatorSpec; 4] {
    [
        IndicatorSpec::Rsi { period: fixed(14) },
        IndicatorSpec::Ema { period: fixed(50) },
        IndicatorSpec::Adx { period: fixed(14) },
        IndicatorSpec::Macd {
            fast: fixed(12),
            slow: fixed(26),
            signal: fixed(9),
        },
    ]
}

fn manifest_path(parts: &[&str]) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.extend(parts);
    path
}

fn load_candles() -> Vec<Candle> {
    let store =
        CandleStore::with_base_dir(manifest_path(&["tests", "fixtures", "btcusdt-1m-store"]));
    let pair = Pair::new("BTCUSDT");
    let version = store
        .read_head(&pair, Timeframe::M15)
        .expect("read fixture HEAD")
        .expect("fixture HEAD exists");
    store
        .read_snapshot(&pair, Timeframe::M15, &version)
        .expect("read fixture snapshot")
        .candles
}

fn current(engine: &IndicatorEngine, spec: &IndicatorSpec) -> Option<Decimal> {
    engine.current(&CompiledValue::Indicator {
        series: Series::Primary,
        spec: spec.clone(),
    })
}

fn run_once(candles: &[Candle]) -> Vec<IndicatorSnapshot> {
    let specs = specs();
    let mut engine = IndicatorEngine::from_specs(&specs).expect("build engine");
    candles
        .iter()
        .map(|candle| {
            engine.step(candle);
            IndicatorSnapshot {
                rsi_14: current(&engine, &specs[0]),
                ema_50: current(&engine, &specs[1]),
                adx_14: current(&engine, &specs[2]),
                macd_12_26_9: current(&engine, &specs[3]),
            }
        })
        .collect()
}

#[test]
fn indicators_are_identical_across_100_fresh_engine_runs() {
    let candles = load_candles();
    let baseline = run_once(&candles);

    for run in 1..=RUNS {
        let next = run_once(&candles);
        assert_eq!(
            next, baseline,
            "NFR-2 single-process self-equality failed on run {run}"
        );
    }
}
