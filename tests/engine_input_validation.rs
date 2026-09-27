//! r3.s1.w3 — engine input validation (the `engine_input_validation` target).
//!
//! The engine must refuse input it would compute wrong money on (#47, #65, #45):
//!
//! - a series that is not strictly ascending by `open_time`, or that repeats an
//!   `open_time`, is refused with a typed error;
//! - a series with a missing candle (adjacent spacing exceeds one timeframe
//!   duration) is refused with a typed error naming the expected and the found
//!   `open_time` — for the primary series and, when given, the htf series;
//! - a run whose counted span crosses an 8h funding boundary without a funding
//!   stamp on (or before) the candle containing it is refused with a typed
//!   error naming the uncovered segment — funding accrual must never silently
//!   count a missed event as zero (#45);
//! - a pair with no pinned funding interval is refused with a named error
//!   rather than defaulted;
//! - a windowed run's counted span is what the funding check covers — an
//!   unstamped lead-in before `count_from_ms` does not refuse the run;
//! - a valid, fully stamped series accrues the same funding the pre-change
//!   engine accrued (frozen-value regression control), and two cold runs of the
//!   same inputs are identical (determinism control);
//! - the `#[ignore]`d scan reports, per snapshot version of a real store, the
//!   candle count, the span, the funding-stamp count and every funding gap the
//!   check finds — the pre-implementation evidence gate for the funding guard.
//!
//! Every hand-built series here uses true M15/H4 spacing (900 000 /
//! 14 400 000 ms) with epoch-aligned 8h boundaries, and the refusal cases run
//! in both debug and release (`--release` re-runs this whole target) so the
//! guards cannot be compiled out (#65).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    BacktestConfig, BacktestError, BacktestResult, BinanceAdapter, Candle, CandleSeries,
    CandleStore, Comparator, CompiledStrategy, Condition, DataVersion, Direction, ExitRule, Pair,
    PriceField, RiskParams, SchemaVersion, Series, SeriesEnd, SeriesRole, StrategyDsl,
    SweepableValue, SymbolFilters, Timeframe, ValueSource, compile, funding_gaps, run_backtest,
    validate,
};
use rust_decimal::Decimal;

/// True M15 spacing — one bar per 15 minutes.
const M15_MS: i64 = 900_000;
/// True H4 spacing — one bar per 4 hours.
const H4_MS: i64 = 14_400_000;

// ---------------------------------------------------------------------------
// Hand-built candle series
// ---------------------------------------------------------------------------

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

/// A flat M15 candle at absolute M15 index `idx` — `open = close = 100`,
/// `high = 100.5`, `low = 99.5` — carrying NO funding rate. Scenarios pin
/// their stamp pattern exactly via [`stamped_m15`].
fn raw_m15(idx: i64) -> Candle {
    let open_time = idx * M15_MS;
    Candle {
        open_time,
        close_time: open_time + M15_MS - 1,
        open: dec(100, 0),
        high: dec(1005, 1),
        low: dec(995, 1),
        close: dec(100, 0),
        volume: dec(1, 0),
        funding_rate: None,
    }
}

/// [`raw_m15`] with an explicit funding rate.
fn stamped_m15(idx: i64, rate: Decimal) -> Candle {
    let mut candle = raw_m15(idx);
    candle.funding_rate = Some(rate);
    candle
}

/// A flat H4 candle at absolute H4 index `j` (the htf series is never
/// funding-checked, so it never carries a stamp).
fn raw_h4(j: i64) -> Candle {
    let open_time = j * H4_MS;
    Candle {
        open_time,
        close_time: open_time + H4_MS - 1,
        open: dec(100, 0),
        high: dec(1005, 1),
        low: dec(995, 1),
        close: dec(100, 0),
        volume: dec(1, 0),
        funding_rate: None,
    }
}

fn btcusdt() -> Pair {
    Pair::new("BTCUSDT")
}

fn m15_series(candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: btcusdt(),
        timeframe: Timeframe::M15,
        version: DataVersion::new("v-engine-input-validation"),
        candles,
    }
}

/// [`m15_series`] under a different pair — the unknown-funding-interval case.
fn series_for(pair: &str, candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new(pair),
        timeframe: Timeframe::M15,
        version: DataVersion::new("v-engine-input-validation"),
        candles,
    }
}

fn h4_series(candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: btcusdt(),
        timeframe: Timeframe::H4,
        version: DataVersion::new("v-engine-input-validation"),
        candles,
    }
}

// ---------------------------------------------------------------------------
// Strategy + run plumbing
// ---------------------------------------------------------------------------

/// Long `close > 99` on the flat-100 series: the entry signal fires on the
/// first bar and fills at the next bar's open; the 5% stop (at 95) is never
/// reached on a flat series, so the run ends in the snapshot-end force-close.
fn always_in_dsl() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "engine-input-validation fixture".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant { value: dec(99, 0) },
        },
        filters: vec![],
        exits: vec![ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(dec(5, 2)), // 0.05 = 5%
        }],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec(1, 2)),
            max_leverage: SweepableValue::Fixed(dec(3, 0)),
        },
    }
}

fn compiled() -> CompiledStrategy {
    compile(&validate(&always_in_dsl()).expect("fixture validates")).expect("fixture compiles")
}

fn zero_slippage() -> BacktestConfig {
    BacktestConfig {
        starting_equity: dec(10_000, 0),
        taker_fee_bps: dec(0, 0),
        slippage_bps: dec(0, 0),
    }
}

fn run(
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    count_from_ms: Option<i64>,
) -> Result<BacktestResult, BacktestError> {
    run_backtest(
        &compiled(),
        primary,
        htf,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        count_from_ms,
    )
}

// ---------------------------------------------------------------------------
// (i) structural refusals — primary and htf
// ---------------------------------------------------------------------------

/// A primary series missing one M15 candle is refused, naming the expected and
/// the found `open_time`.
#[test]
fn gapped_primary_series_is_refused_with_a_typed_gap() {
    // bars 0, 1, 2, 4 — the bar at index 3 is missing.
    let primary = m15_series(vec![raw_m15(0), raw_m15(1), raw_m15(2), raw_m15(4)]);
    let err = run(&primary, None, None)
        .expect_err("a primary series with a missing candle must be refused");
    assert!(
        matches!(
            err,
            BacktestError::SeriesGap {
                series: SeriesRole::Primary,
                expected: 2_700_000, // bar 3's open_time
                found: 3_600_000,    // bar 4's open_time
            }
        ),
        "the refusal must be the typed gap naming expected and found; got {err:?}"
    );
}

/// An htf series missing one H4 bar is refused with the same typed gap, even
/// for a strategy that never reads the htf series — the engine refuses input
/// it was handed, not input it happened to use.
#[test]
fn gapped_htf_series_is_refused_with_a_typed_gap() {
    let primary = m15_series(vec![
        raw_m15(0),
        raw_m15(1),
        raw_m15(2),
        raw_m15(3),
        raw_m15(4),
        raw_m15(5),
    ]);
    // H4 bars 0, 1, 3 — bar j=2 is missing.
    let htf = h4_series(vec![raw_h4(0), raw_h4(1), raw_h4(3)]);
    let err = run(&primary, Some(&htf), None)
        .expect_err("an htf series with a missing bar must be refused");
    assert!(
        matches!(
            err,
            BacktestError::SeriesGap {
                series: SeriesRole::Htf,
                expected: 28_800_000, // H4 bar 2's open_time
                found: 43_200_000,    // H4 bar 3's open_time
            }
        ),
        "the refusal must be the typed gap naming expected and found; got {err:?}"
    );
}

/// A primary series that is not strictly ascending by `open_time` is refused —
/// the out-of-order bar itself, not some downstream consequence.
#[test]
fn unsorted_primary_series_is_refused() {
    // Bars 0, 1, 3, 2, 4 in that order: the candle at index 3 comes before the
    // one at index 2.
    let primary = m15_series(vec![
        raw_m15(0),
        raw_m15(1),
        raw_m15(3),
        raw_m15(2),
        raw_m15(4),
    ]);
    let err = run(&primary, None, None).expect_err("an unsorted primary series must be refused");
    assert!(
        matches!(
            err,
            BacktestError::SeriesUnsorted {
                series: SeriesRole::Primary,
                at: 1_800_000, // the out-of-order / duplicated open_time
            }
        ),
        "the refusal must name the out-of-order open_time; got {err:?}"
    );
}

/// A repeated `open_time` is refused — two candles cannot occupy one bar.
#[test]
fn duplicate_open_time_is_refused() {
    let primary = m15_series(vec![
        raw_m15(0),
        raw_m15(1),
        raw_m15(2),
        raw_m15(2),
        raw_m15(4),
    ]);
    let err = run(&primary, None, None)
        .expect_err("a primary series with a repeated open_time must be refused");
    assert!(
        matches!(
            err,
            BacktestError::SeriesUnsorted {
                series: SeriesRole::Primary,
                at: 1_800_000, // the out-of-order / duplicated open_time
            }
        ),
        "the refusal must name the duplicated open_time; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// (iii)/(iv) funding-order refusals and the windowed counted span
// ---------------------------------------------------------------------------

/// A counted span that skips one 8h funding stamp is refused, naming the
/// uncovered segment; the same span with the stamp present runs.
#[test]
fn counted_span_skipping_one_funding_stamp_is_refused() {
    // 65 M15 bars (0..=64) span 16h+; the 8h boundaries sit at indices 0, 32, 64.
    // Stamps at 0 and 64 only: the segment between them holds a whole unstamped
    // 8h event at index 32.
    let missing = m15_series(
        (0..=64)
            .map(|i| match i {
                0 | 64 => stamped_m15(i, dec(1, 3)),
                _ => raw_m15(i),
            })
            .collect(),
    );
    let err = run(&missing, None, None)
        .expect_err("a counted span whose middle 8h boundary carries no stamp must be refused");
    assert!(
        matches!(
            err,
            BacktestError::FundingGap {
                from: 0,
                to: 57_600_000 // the index-64 stamp's open_time: the uncovered segment's later anchor
            }
        ),
        "the refusal must name the uncovered segment; got {err:?}"
    );

    // The same span with the index-32 stamp present runs.
    let stamped = m15_series(
        (0..=64)
            .map(|i| match i {
                0 | 32 | 64 => stamped_m15(i, dec(1, 3)),
                _ => raw_m15(i),
            })
            .collect(),
    );
    let result = run(&stamped, None, None).expect("the fully stamped span must run");
    assert_eq!(
        result.trades.len(),
        1,
        "the flat-100 fixture still enters once"
    );
}

/// A windowed run's funding check covers exactly the counted span: a stampless
/// lead-in before `count_from_ms` runs, while the same series counted from its
/// start — whose counted span crosses an unstamped boundary — is refused.
#[test]
fn windowed_counted_span_ignores_unstamped_lead_in_but_refuses_its_own_gaps() {
    // Stamps only at index 64; indices 0 and 32 carry nothing.
    let candles: Vec<Candle> = (0..=64)
        .map(|i| {
            if i == 64 {
                stamped_m15(i, dec(1, 3))
            } else {
                raw_m15(i)
            }
        })
        .collect();
    let series = m15_series(candles);

    // Counted from index 32: the counted span (32..=64) has its first boundary
    // at its own start and its second at 64 — one interval apart, fully stamped.
    let windowed = run(&series, None, Some(32 * M15_MS))
        .expect("an unstamped lead-in must not refuse a windowed run");
    assert_eq!(windowed.trades.len(), 1);

    // The same series counted from the start refuses: the segment from the span
    // start to the only stamp spans two 8h events with nothing in between.
    let err = run(&series, None, None)
        .expect_err("the same series counted from its start must be refused");
    assert!(
        matches!(
            err,
            BacktestError::FundingGap {
                from: 0,
                to: 57_600_000 // the index-64 stamp's open_time
            }
        ),
        "the refusal must name the uncovered segment; got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Unknown funding interval
// ---------------------------------------------------------------------------

/// A pair with no pinned funding interval is refused with a named error rather
/// than silently defaulted. The series itself is structurally sound and fully
/// stamped, so the refusal that fires is the interval one.
#[test]
fn unknown_pair_is_refused_with_a_named_error() {
    let primary = series_for(
        "ETHUSDT",
        vec![raw_m15(0), raw_m15(1), raw_m15(2), raw_m15(3)],
    );
    let err = run(&primary, None, None)
        .expect_err("a pair with no pinned funding interval must be refused");
    assert!(
        matches!(err, BacktestError::FundingIntervalUnknown { .. }),
        "the refusal must be the typed unknown-interval error; got {err:?}"
    );
    assert!(
        err.to_string().contains("ETHUSDT"),
        "the refusal must name the pair; got {err}"
    );
}

// ---------------------------------------------------------------------------
// Regression controls — funding value frozen from the pre-change engine, and
// cold-run determinism
// ---------------------------------------------------------------------------

/// On a valid, fully stamped series the per-trade funding equals the value the
/// pre-change engine accrued — the guards refuse input, they never re-price
/// output. The frozen value was observed from the guard-free engine on exactly
/// this series before the guards landed (placeholder run, then frozen).
#[test]
fn funding_accrual_matches_the_pre_change_engine_value() {
    // 40 bars (~10h): explicit non-zero stamps at the 8h boundaries 0 and 32.
    let primary = m15_series(
        (0..40)
            .map(|i| match i {
                0 | 32 => stamped_m15(i, dec(1, 3)), // 0.001
                _ => raw_m15(i),
            })
            .collect(),
    );
    let result = run(&primary, None, None).expect("a fully stamped series must run");
    assert_eq!(result.trades.len(), 1);
    // Frozen from the pre-change (guard-free) engine on exactly this series:
    // authored with a print + nonzero assert, observed `frozen funding_total =
    // -2.000` (qty 20 x entry 100 at a 5% stop under 1% risk; one stamped 8h
    // boundary inside the holding window at rate 0.001), then frozen.
    assert_eq!(
        result.trades[0].funding_total,
        dec(-2, 0),
        "per-trade funding must equal the pre-change engine's accrued value"
    );
    assert_eq!(result.funding_total, dec(-2, 0));
}

/// Two cold runs of the same inputs produce identical results — the guards add
/// no nondeterminism.
#[test]
fn two_cold_runs_of_the_same_inputs_are_identical() {
    let build = || {
        m15_series(
            (0..40)
                .map(|i| match i {
                    0 | 32 => stamped_m15(i, dec(1, 3)),
                    _ => raw_m15(i),
                })
                .collect(),
        )
    };
    let first = run(&build(), None, None).expect("first cold run");
    let second = run(&build(), None, None).expect("second cold run");
    assert_eq!(first, second, "two cold runs must be identical");
    assert_eq!(
        first.result_content_hash(),
        second.result_content_hash(),
        "the content hash must agree across cold runs"
    );
}

// ---------------------------------------------------------------------------
// Step 0 — the pre-implementation scan over a real snapshot store (#[ignore]d)
// ---------------------------------------------------------------------------

/// Walks every BTCUSDT snapshot version a real store holds and prints, per
/// `data_version`: the candle count, the span, the funding-stamp count, and
/// every funding gap the check finds (M15), plus the structural validation
/// result for both timeframes. Observational by design: it never asserts, it
/// reports — the run's operator reads the output and rules.
///
/// Run with:
/// `PULSE_FUNDING_SCAN_DIR=<store root> cargo nextest run --test engine_input_validation scan_real --ignored --nocapture`
#[test]
#[ignore = "observational scan over a real store; set PULSE_FUNDING_SCAN_DIR"]
fn scan_real_snapshots_report_counts_spans_stamps_and_gaps() {
    let root = std::env::var("PULSE_FUNDING_SCAN_DIR")
        .expect("set PULSE_FUNDING_SCAN_DIR to the candle store root");
    let store = CandleStore::with_base_dir(PathBuf::from(root));
    let interval = BinanceAdapter::new()
        .funding_interval_ms(&btcusdt())
        .expect("the pair must have a pinned funding interval");

    for timeframe in [Timeframe::M15, Timeframe::H4] {
        let tf_dir = store
            .snapshot_path(&btcusdt(), timeframe, &DataVersion::new("probe"))
            .parent()
            .expect("snapshot path always has a parent")
            .to_path_buf();
        let mut versions: Vec<String> = std::fs::read_dir(&tf_dir)
            .expect("timeframe directory exists")
            .filter_map(|entry| {
                let path = entry.expect("readable entry").path();
                if path.extension()?.to_str()? == "parquet" {
                    path.file_stem()?.to_str().map(str::to_owned)
                } else {
                    None
                }
            })
            .collect();
        versions.sort();
        for version in versions {
            let data_version = DataVersion::new(version.as_str());
            let series = store
                .read_snapshot(&btcusdt(), timeframe, &data_version)
                .unwrap_or_else(|err| panic!("{version}: snapshot read failed: {err}"));
            let first = series.candles.first().map(|c| c.open_time);
            let last = series.candles.last().map(|c| c.close_time);
            let stamps = series
                .candles
                .iter()
                .filter(|candle| candle.funding_rate.is_some())
                .count();
            println!(
                "version={version} tf={timeframe:?} candles={} span=[{:?}..={:?}] stamps={stamps}",
                series.candles.len(),
                first,
                last
            );
            match series.validate() {
                Ok(gaps) => {
                    if gaps.is_empty() {
                        println!("  structural: contiguous");
                    } else {
                        for gap in &gaps {
                            println!(
                                "  structural gap: expected open_time {} found {}",
                                gap.expected, gap.found
                            );
                        }
                    }
                }
                Err(err) => println!("  structural: INVALID ({err})"),
            }
            if timeframe == Timeframe::M15 {
                let gaps = funding_gaps(&series, None, interval);
                if gaps.is_empty() {
                    println!("  funding: fully stamped over the whole span");
                } else {
                    for gap in &gaps {
                        println!(
                            "  funding gap: ({}, {}] uncovered (~{}h)",
                            gap.from,
                            gap.to,
                            (gap.to - gap.from) / 3_600_000
                        );
                    }
                }
            }
        }
    }
}
