//! Cross-validates the streaming indicator engine against the committed
//! pandas-ta reference fixture generated offline for WI-3.04.
#![allow(
    clippy::cast_precision_loss,
    clippy::expect_used,
    clippy::too_many_lines,
    clippy::unwrap_used
)]

use std::{path::PathBuf, str::FromStr};

use pulse::{
    Candle, CandleStore, CompiledValue, EvalContext, IndicatorEngine, IndicatorSpec, Pair, Series,
    SweepableValue, Timeframe,
};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::Deserialize;

const REL_EPS: f64 = 1e-6;
const ABS_FLOOR: f64 = 1e-9;
const MIN_COMPARED_ROWS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndicatorName {
    Rsi,
    Ema,
    Adx,
    Macd,
    Atr,
}

impl IndicatorName {
    fn label(self) -> &'static str {
        match self {
            Self::Rsi => "RSI(14)",
            Self::Ema => "EMA(50)",
            Self::Adx => "ADX(14)",
            Self::Macd => "MACD(12,26,9)",
            Self::Atr => "ATR(14)",
        }
    }

    fn spec(self) -> IndicatorSpec {
        match self {
            Self::Rsi => IndicatorSpec::Rsi { period: fixed(14) },
            Self::Ema => IndicatorSpec::Ema { period: fixed(50) },
            Self::Adx => IndicatorSpec::Adx { period: fixed(14) },
            Self::Macd => IndicatorSpec::Macd {
                fast: fixed(12),
                slow: fixed(26),
                signal: fixed(9),
            },
            Self::Atr => IndicatorSpec::Atr { period: fixed(14) },
        }
    }

    fn settle_bars(self) -> usize {
        match self {
            // The generator pins recursive EMA with sma=false for these three,
            // so they should agree immediately once warm. ATR shares the
            // SMA-seeded Wilder RMA with ADX (same `wilder.rs` core), and
            // pandas-ta-classic's `atr(mamode="rma")` turns out to SMA-seed
            // identically — measured at regen (r2.s2.w2): the worst relative
            // delta over post-warmup rows is ≈2.7e-12, already below REL_EPS,
            // so no settling window is needed at all.
            Self::Rsi | Self::Ema | Self::Macd | Self::Atr => 0,
            // ADX uses the same Wilder alpha, but the adapter is SMA-seeded
            // while pandas-ta's RMA is recursively seeded. The first 280
            // post-warmup rows let the seed delta decay below REL_EPS.
            Self::Adx => 280,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReferenceRow {
    open_time: i64,
    rsi_14: String,
    ema_50: String,
    adx_14: String,
    macd_12_26_9: String,
    atr_14: String,
}

impl ReferenceRow {
    fn value(&self, indicator: IndicatorName) -> Option<f64> {
        let raw = match indicator {
            IndicatorName::Rsi => &self.rsi_14,
            IndicatorName::Ema => &self.ema_50,
            IndicatorName::Adx => &self.adx_14,
            IndicatorName::Macd => &self.macd_12_26_9,
            IndicatorName::Atr => &self.atr_14,
        };
        if raw.is_empty() {
            None
        } else {
            Some(raw.parse::<f64>().expect("reference value is numeric"))
        }
    }
}

fn fixed(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
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

fn load_reference() -> Vec<ReferenceRow> {
    let path = manifest_path(&[
        "tests",
        "fixtures",
        "indicators",
        "btcusdt-m15-reference.csv",
    ]);
    let mut reader = csv::Reader::from_path(path).expect("open pandas-ta reference CSV");
    reader
        .deserialize()
        .map(|row| row.expect("parse reference CSV row"))
        .collect()
}

fn indicator_names() -> [IndicatorName; 5] {
    [
        IndicatorName::Rsi,
        IndicatorName::Ema,
        IndicatorName::Adx,
        IndicatorName::Macd,
        IndicatorName::Atr,
    ]
}

fn all_specs() -> Vec<IndicatorSpec> {
    indicator_names()
        .into_iter()
        .map(IndicatorName::spec)
        .collect()
}

fn current_value(engine: &IndicatorEngine, indicator: IndicatorName) -> Option<Decimal> {
    engine.current(&CompiledValue::Indicator {
        series: Series::Primary,
        spec: indicator.spec(),
    })
}

fn decimal_to_f64(value: Decimal) -> f64 {
    value.to_f64().expect("indicator Decimal fits f64")
}

fn close_enough(engine: Decimal, reference: f64) -> bool {
    let engine = decimal_to_f64(engine);
    let diff = (engine - reference).abs();
    let scale = engine.abs().max(reference.abs()).max(ABS_FLOOR);
    diff <= REL_EPS * scale
}

fn first_reference_index(rows: &[ReferenceRow], indicator: IndicatorName) -> usize {
    rows.iter()
        .position(|row| row.value(indicator).is_some())
        .unwrap_or_else(|| panic!("{} reference has at least one value", indicator.label()))
}

fn first_engine_index(candles: &[Candle], indicator: IndicatorName) -> usize {
    let mut engine = IndicatorEngine::from_specs(&[indicator.spec()]).expect("build engine");
    candles
        .iter()
        .position(|candle| {
            engine.step(candle);
            current_value(&engine, indicator).is_some()
        })
        .unwrap_or_else(|| panic!("{} engine has at least one value", indicator.label()))
}

#[test]
fn warmup_boundary_aligns_with_reference() {
    let candles = load_candles();
    let reference = load_reference();
    assert_eq!(candles.len(), reference.len(), "same row count");

    for indicator in indicator_names() {
        let expected = first_reference_index(&reference, indicator);
        let got = first_engine_index(&candles, indicator);
        assert_eq!(
            got,
            expected,
            "{} first engine value must align with first reference value",
            indicator.label()
        );
    }
}

#[test]
fn cross_validation_uses_relative_tolerance() {
    let engine = Decimal::from_str("100000.00000000").unwrap();
    let reference = 100_000.05_f64;

    assert!(
        (decimal_to_f64(engine) - reference).abs() > 1e-6,
        "crafted pair would fail a fixed absolute 1e-6 tolerance"
    );
    assert!(
        close_enough(engine, reference),
        "crafted EMA-scale pair passes the relative tolerance"
    );
}

#[test]
fn indicators_match_pandas_ta_reference_after_settling() {
    let candles = load_candles();
    let reference = load_reference();
    assert_eq!(candles.len(), reference.len(), "same row count");

    let specs = all_specs();
    let mut engine = IndicatorEngine::from_specs(&specs).expect("build engine");
    // One (indicator, compared-row-count) slot per `indicator_names()` entry, in
    // the same order the loop below enumerates — so a vacuous-count failure names
    // the right indicator (every slot was previously mislabelled `Rsi`).
    let mut compared = indicator_names().map(|name| (name, 0usize));

    for (row_idx, (candle, row)) in candles.iter().zip(reference.iter()).enumerate() {
        assert_eq!(candle.open_time, row.open_time, "row {row_idx} open_time");
        engine.step(candle);

        for (slot, indicator) in indicator_names().into_iter().enumerate() {
            let got = current_value(&engine, indicator);
            let expected = row.value(indicator);
            assert_eq!(
                got.is_some(),
                expected.is_some(),
                "{} definition mismatch at row {row_idx}",
                indicator.label()
            );

            let first = first_reference_index(&reference, indicator);
            if row_idx < first + indicator.settle_bars() {
                continue;
            }

            if let (Some(got), Some(expected)) = (got, expected) {
                assert!(
                    close_enough(got, expected),
                    "{} row {row_idx}: engine={got} reference={expected}",
                    indicator.label()
                );
                compared[slot].1 += 1;
            }
        }
    }

    for (indicator, count) in compared {
        assert!(
            count >= MIN_COMPARED_ROWS,
            "{} compared row count is non-vacuous: {count}",
            indicator.label()
        );
    }
}
