//! r6.s3.w3 — Cipher Pullback Engulf v2 (STR-0003) as one two-sided document.
//!
//! `examples/strategies/cipher-v2.json` is `cipher-v1.json` with the 4H swing-trend
//! rule added to each side's `arm`: the long side arms only when, of the last two
//! confirmed 4H swing highs, the later is higher **and** of the last two confirmed
//! 4H swing lows, the later is higher; the short side only when both are lower.
//! Anything else is ranging and nothing arms (STR-0003 v2; ADR-0033 decision 2 and
//! PDR-0046 decision 3 put the check on the 15m bar where a 4H bar closed). This
//! suite is STR-0003's "before any run" check for v2:
//!
//! 1. the document is v1 plus two conjuncts per side
//!    (`cipher_v2_document_loads`);
//! 2. on SOLUSDT v2 arms only where v1 arms and the swings trend
//!    (`v2_arms_only_where_v1_arms_and_the_swings_trend`);
//! 3. the v2 arm rows at the design examples, pinned as observed
//!    (`v2_on_the_design_examples`);
//! 4. on BTCUSDT v2 runs twice to the same hash
//!    (`btcusdt_v2_runs_deterministically`);
//! 5. the two swing conjuncts, lifted from the document, do what the rule says on
//!    hand-built swings (`v2_swing_conjuncts_on_hand_built_swings`);
//! 6. the document round-trips over MCP (`cipher_v2_round_trips_over_mcp`).
//!
//! Every bar is before freeze `30f98b21`'s holdout (2026-10-10).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::path::{Path, PathBuf};

use pulse::{
    BacktestConfig, BacktestResult, BinanceAdapter, Candle, CandleSeries, CandleStore, Comparator,
    CompiledStrategy, CompiledValue, Condition, DataVersion, Direction, EngineSession, EvalProbe,
    ExchangeAdapter, ExitRule, IndicatorSpec, Migrator, Pair, PriceField, RiskParams,
    SchemaVersion, Series, SeriesEnd, SessionTimeframes, SetupEvent, SetupSide, StrategyDsl,
    SweepableValue, SymbolFilters, Timeframe, ValueSource, compile, run_backtest, step_series,
    validate,
};
use rmcp::model::{ReadResourceRequestParams, ResourceContents};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (copied from tests/cipher_v1_examples.rs, tests/setup_sequence.rs and
// tests/previous_pivots.rs)
// ---------------------------------------------------------------------------

/// The checked-in v2 document this suite pins.
fn v2_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/strategies/cipher-v2.json")
}

/// The checked-in v1 document v2 is compared against.
fn v1_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/strategies/cipher-v1.json")
}

fn json_text(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read the checked-in document")
}

/// Load a document through the read path the engine consumes:
/// `Migrator::v1().load` (version-safe).
fn dsl_of(path: &Path) -> StrategyDsl {
    Migrator::v1()
        .load(&json_text(path))
        .expect("the document loads through Migrator::v1")
        .dsl
}

/// `Migrator::v1().load` -> `validate` -> `compile`.
fn compiled_of(path: &Path) -> CompiledStrategy {
    compile(&validate(&dsl_of(path)).expect("the document validates"))
        .expect("the document compiles")
}

/// One committed fixture series for `pair` (no network / LLM / `SQLite`).
fn load_series(store_dir: &Path, pair: &str, timeframe: Timeframe) -> CandleSeries {
    let store = CandleStore::with_base_dir(store_dir.to_path_buf());
    let pair = Pair::new(pair);
    let head = store
        .read_head(&pair, timeframe)
        .expect("read HEAD")
        .expect("HEAD present in the fixture store");
    store
        .read_snapshot(&pair, timeframe, &head)
        .expect("read snapshot")
}

fn solusdt_store() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/solusdt-cipher-store")
}

fn btcusdt_store() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store")
}

/// An RFC 3339 instant as epoch milliseconds.
fn ms(rfc3339: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .unwrap_or_else(|error| panic!("{rfc3339}: {error}"))
        .timestamp_millis()
}

/// The 15m arm bars of STR-0003's design examples 1 and 3 (r6.s2.w5's table): the
/// 15m bar that opens as the 4H signal bar closes.
const EXAMPLE_ARM_BARS: [(u8, &str); 2] = [
    // 4H signal bar: 2026-08-26 20:00 (the pole).
    (1, "2026-08-26T23:45:00Z"),
    // 4H signal bar: 2026-09-24 12:00 (the engulfing).
    (3, "2026-09-24T15:45:00Z"),
];

/// The SOLUSDT fixture's 15m and 4h series.
fn solusdt_series() -> (CandleSeries, CandleSeries) {
    (
        load_series(&solusdt_store(), "SOLUSDT", Timeframe::M15),
        load_series(&solusdt_store(), "SOLUSDT", Timeframe::H4),
    )
}

fn filters_of(pair: &str) -> SymbolFilters {
    BinanceAdapter::new()
        .symbol_filters(&Pair::new(pair))
        .expect("the pair's filters resolve through the port")
}

fn dec(text: &str) -> Decimal {
    text.parse::<Decimal>().expect("decimal literal")
}

fn fixed_u32(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
}

// ---------------------------------------------------------------------------
// The four swing operands, as the probe reads them
// ---------------------------------------------------------------------------

/// The four 4H swing prices v2's arm compares, `left = right = 3`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Swings {
    /// `PivotHigh`: the latest confirmed swing high.
    high: Option<Decimal>,
    /// `PreviousPivotHigh`: the one before it.
    previous_high: Option<Decimal>,
    /// `PivotLow`: the latest confirmed swing low.
    low: Option<Decimal>,
    /// `PreviousPivotLow`: the one before it.
    previous_low: Option<Decimal>,
}

impl Swings {
    /// The long side's rule: the later high is higher and the later low is higher.
    /// A missing operand is false.
    fn rise(&self) -> bool {
        matches!((self.high, self.previous_high), (Some(a), Some(b)) if a > b)
            && matches!((self.low, self.previous_low), (Some(a), Some(b)) if a > b)
    }

    /// The short side's rule: both later swings are lower. A missing operand is false.
    fn fall(&self) -> bool {
        matches!((self.high, self.previous_high), (Some(a), Some(b)) if a < b)
            && matches!((self.low, self.previous_low), (Some(a), Some(b)) if a < b)
    }
}

/// One `htf` swing operand.
fn htf_swing(spec: IndicatorSpec) -> CompiledValue {
    CompiledValue::Indicator {
        series: Series::Htf,
        spec,
        lag: 0,
    }
}

/// Read the four swing operands through the probe at the bar just stepped.
fn read_swings(probe: &EvalProbe<'_>) -> Swings {
    let (left, right) = (fixed_u32(3), fixed_u32(3));
    let read = |spec: IndicatorSpec| probe.operand(&htf_swing(spec), false);
    Swings {
        high: read(IndicatorSpec::PivotHigh {
            left: left.clone(),
            right: right.clone(),
        }),
        previous_high: read(IndicatorSpec::PreviousPivotHigh {
            left: left.clone(),
            right: right.clone(),
        }),
        low: read(IndicatorSpec::PivotLow {
            left: left.clone(),
            right: right.clone(),
        }),
        previous_low: read(IndicatorSpec::PreviousPivotLow { left, right }),
    }
}

/// One stepped bar: its open time, each side's event, and (for a session that holds
/// the swing operands) the four swing prices.
#[derive(Debug, Clone, Copy)]
struct Bar {
    open_ms: i64,
    long: Option<SetupEvent>,
    short: Option<SetupEvent>,
    swings: Option<Swings>,
}

/// Drive an `EngineSession` over the SOLUSDT fixture with `step_series`, recording
/// both sides' events at every bar. `with_swings` also reads the four swing
/// operands through the probe; it is for v2, whose engine holds those specs.
fn solusdt_bars(compiled: &CompiledStrategy, with_swings: bool) -> Vec<Bar> {
    let (primary, htf) = solusdt_series();
    let mut session = EngineSession::new(
        compiled,
        &Pair::new("SOLUSDT"),
        SessionTimeframes {
            primary: Timeframe::M15,
            htf: Some(Timeframe::H4),
            d1: None,
        },
        BacktestConfig::default(),
        filters_of("SOLUSDT"),
        None,
    )
    .expect("the session builds");
    let mut bars: Vec<Bar> = Vec::new();
    step_series(
        &mut session,
        &primary,
        Some(&htf),
        None,
        |open_ms, probe| {
            let view = probe
                .setup_view()
                .expect("the document has a setup, so the probe has a view");
            bars.push(Bar {
                open_ms,
                long: view.long.expect("the document has a long side").event,
                short: view.short.expect("the document has a short side").event,
                swings: with_swings.then(|| read_swings(probe)),
            });
        },
    )
    .expect("the series steps");
    bars
}

// ---------------------------------------------------------------------------
// 1. The document
// ---------------------------------------------------------------------------

/// STR-0003 v2's two added conjuncts for one side, as JSON: every operand `htf`,
/// `left = right = 3`, the comparator `op`.
fn expected_conjuncts(op: &str) -> Vec<Value> {
    let operand = |indicator: &str| {
        json!({
            "type": "Indicator",
            "series": "htf",
            "spec": { "indicator": indicator, "left": 3, "right": 3 }
        })
    };
    vec![
        json!({
            "type": "Compare",
            "lhs": operand("PivotHigh"),
            "op": op,
            "rhs": operand("PreviousPivotHigh")
        }),
        json!({
            "type": "Compare",
            "lhs": operand("PivotLow"),
            "op": op,
            "rhs": operand("PreviousPivotLow")
        }),
    ]
}

/// The document loads, validates and compiles; each side's `arm` has v1's three
/// conjuncts plus the two swing conjuncts; every other field equals v1's.
#[test]
fn cipher_v2_document_loads() {
    let v1: Value = serde_json::from_str(&json_text(&v1_path())).expect("v1 is JSON");
    let v2: Value = serde_json::from_str(&json_text(&v2_path())).expect("v2 is JSON");
    assert_eq!(v2["schema_version"], "1.7.0", "v2 is a 1.7.0 document");
    assert_eq!(v2["name"], "Cipher Pullback Engulf v2 (STR-0003)");

    // Remove the two conjuncts from each side, then set the name and the schema
    // version equal: what remains must be v1, field for field.
    let mut rest = v2.clone();
    for (side, op) in [("long", "Gt"), ("short", "Lt")] {
        let v1_arm = &v1["setup"][side]["arm"];
        let arm = &mut rest["setup"][side]["arm"];
        assert_eq!(arm["type"], "And", "{side}: the arm is an And");
        let conjuncts = arm["conditions"]
            .as_array_mut()
            .expect("the arm's conditions are an array");
        let v1_conjuncts = v1_arm["conditions"]
            .as_array()
            .expect("v1's arm conditions are an array");
        assert_eq!(
            v1_conjuncts.len(),
            3,
            "{side}: v1's CONTEXT, PULLBACK, SIGNAL"
        );
        assert_eq!(conjuncts.len(), 5, "{side}: v1's three plus the two swings");
        assert_eq!(
            conjuncts[..3],
            v1_conjuncts[..],
            "{side}: the first three conjuncts are v1's, in order"
        );
        let added = conjuncts.split_off(3);
        assert_eq!(
            added,
            expected_conjuncts(op),
            "{side}: conjuncts 4 and 5 are the swing comparisons, top level"
        );
    }
    rest["name"] = v1["name"].clone();
    rest["schema_version"] = v1["schema_version"].clone();
    assert_eq!(
        rest, v1,
        "v2 differs from v1 only in name, schema version and the two conjuncts per side"
    );

    // And it is a document the engine accepts.
    let compiled = compiled_of(&v2_path());
    let setup = compiled.setup().expect("the document declares a setup");
    assert!(setup.long.is_some(), "the long side is declared");
    assert!(setup.short.is_some(), "the short side is declared");
    assert_eq!(setup.expiry_bars, 96, "expiry_bars is 96");
}

// ---------------------------------------------------------------------------
// 2. v2 arms only where v1 arms and the swings trend
// ---------------------------------------------------------------------------

/// Count of bars on SOLUSDT where v1's long side arms and v2's does not.
const GOLDEN_REFUSED_LONG: usize = 11;

/// Count of bars on SOLUSDT where v1's short side arms and v2's does not.
const GOLDEN_REFUSED_SHORT: usize = 11;

/// Step v1 and v2 over the SOLUSDT fixture. Where v2's side arms, v1's side arms.
/// Where v1's side arms and v2's does not, the swings do not trend (or an operand is
/// missing). At least one such refused bar exists.
#[test]
fn v2_arms_only_where_v1_arms_and_the_swings_trend() {
    let v1 = solusdt_bars(&compiled_of(&v1_path()), false);
    let v2 = solusdt_bars(&compiled_of(&v2_path()), true);
    assert_eq!(v1.len(), v2.len(), "both runs step the same bars");
    assert!(!v1.is_empty(), "the fixture has bars");

    let armed = Some(SetupEvent::Armed);
    let mut refused_long: Vec<i64> = Vec::new();
    let mut refused_short: Vec<i64> = Vec::new();
    let mut v1_arms = (0_usize, 0_usize);
    let mut v2_arms = (0_usize, 0_usize);
    for (a, b) in v1.iter().zip(&v2) {
        assert_eq!(a.open_ms, b.open_ms, "the runs are bar for bar");
        let swings = b.swings.expect("v2's bars carry the swing prices");

        // v2 arms only where v1 arms.
        assert!(
            b.long != armed || a.long == armed,
            "bar {}: v2's long side arms where v1's does not; swings {swings:?}",
            b.open_ms
        );
        assert!(
            b.short != armed || a.short == armed,
            "bar {}: v2's short side arms where v1's does not; swings {swings:?}",
            b.open_ms
        );
        v1_arms.0 += usize::from(a.long == armed);
        v1_arms.1 += usize::from(a.short == armed);
        v2_arms.0 += usize::from(b.long == armed);
        v2_arms.1 += usize::from(b.short == armed);

        // A refusal is a bar where the swings do not trend.
        if a.long == armed && b.long != armed {
            assert!(
                !swings.rise(),
                "bar {}: v1 arms long, v2 refuses, yet the swings rise: {swings:?}",
                b.open_ms
            );
            refused_long.push(b.open_ms);
        }
        if a.short == armed && b.short != armed {
            assert!(
                !swings.fall(),
                "bar {}: v1 arms short, v2 refuses, yet the swings fall: {swings:?}",
                b.open_ms
            );
            refused_short.push(b.open_ms);
        }
    }

    eprintln!(
        "v1 arms: long {} short {}; v2 arms: long {} short {}; refused: long {} short {}",
        v1_arms.0,
        v1_arms.1,
        v2_arms.0,
        v2_arms.1,
        refused_long.len(),
        refused_short.len()
    );
    assert!(
        !refused_long.is_empty() || !refused_short.is_empty(),
        "no bar in the fixture shows v2 refusing where v1 arms"
    );
    assert_eq!(
        (refused_long.len(), refused_short.len()),
        (GOLDEN_REFUSED_LONG, GOLDEN_REFUSED_SHORT),
        "the refused-bar counts drifted (long, short)"
    );
}

// ---------------------------------------------------------------------------
// 3. v2 on the design examples
// ---------------------------------------------------------------------------

/// What v2's long side and the four swing prices read at an example's arm bar,
/// pinned as observed: `(arms, [PivotHigh, PreviousPivotHigh, PivotLow,
/// PreviousPivotLow])`. STR-0003 does not require v2 to take the design examples,
/// so `arms` is a record, not a requirement.
type ExampleGolden = (bool, [Option<&'static str>; 4]);

const GOLDEN_EXAMPLES: [ExampleGolden; 2] = [
    (
        true,
        [
            Some("103.2600"),
            Some("100.5400"),
            Some("95.2100"),
            Some("93.2200"),
        ],
    ),
    (
        false,
        [
            Some("119.6800"),
            Some("119.9900"),
            Some("112.8000"),
            Some("115.5300"),
        ],
    ),
];

/// At the arm bars of examples 1 and 3, v2's long side and the four swing prices
/// match the pinned record.
#[test]
fn v2_on_the_design_examples() {
    let bars = solusdt_bars(&compiled_of(&v2_path()), true);
    for ((number, arm_bar_open), golden) in EXAMPLE_ARM_BARS.iter().zip(GOLDEN_EXAMPLES) {
        let at = ms(arm_bar_open);
        let bar = bars
            .iter()
            .find(|bar| bar.open_ms == at)
            .unwrap_or_else(|| panic!("example {number}: the arm bar is in the fixture"));
        let swings = bar.swings.expect("v2's bars carry the swing prices");
        let observed = (
            bar.long == Some(SetupEvent::Armed),
            [
                swings.high,
                swings.previous_high,
                swings.low,
                swings.previous_low,
            ],
        );
        let expected = (golden.0, golden.1.map(|price| price.map(dec)));
        assert_eq!(
            observed, expected,
            "example {number}: v2's arm bar {arm_bar_open} (arms, [PivotHigh, \
             PreviousPivotHigh, PivotLow, PreviousPivotLow]) drifted from the record"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. BTCUSDT determinism
// ---------------------------------------------------------------------------

/// Frozen golden: the number of completed trades v2 produces on the committed
/// BTCUSDT fixture (15m primary, 4h htf) under the default cost model.
const GOLDEN_TRADE_COUNT: usize = 0;

/// Frozen golden: the exact net P&L (quote currency, net of costs) of that run.
const GOLDEN_NET_PNL: &str = "0";

/// One run of v2 over the BTCUSDT fixture's M15 primary + H4 htf.
fn run_btcusdt(compiled: &CompiledStrategy) -> BacktestResult {
    let primary = load_series(&btcusdt_store(), "BTCUSDT", Timeframe::M15);
    let htf = load_series(&btcusdt_store(), "BTCUSDT", Timeframe::H4);
    run_backtest(
        compiled,
        &primary,
        Some(&htf),
        None,
        &BacktestConfig::default(),
        &filters_of("BTCUSDT"),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("backtest runs over the fixture")
}

/// Two runs succeed and give the same `result_content_hash`; the trade count and
/// net P&L are the pinned goldens.
#[test]
fn btcusdt_v2_runs_deterministically() {
    let compiled = compiled_of(&v2_path());
    let first = run_btcusdt(&compiled);
    let second = run_btcusdt(&compiled);

    assert_eq!(
        first.result_content_hash(),
        second.result_content_hash(),
        "two runs of v2 gave different result hashes"
    );
    assert_eq!(
        first.trades.len(),
        GOLDEN_TRADE_COUNT,
        "golden trade count drifted: got {} (net P&L {})",
        first.trades.len(),
        first.net_pnl
    );
    let expected = Decimal::from_str_exact(GOLDEN_NET_PNL)
        .unwrap_or_else(|_| panic!("GOLDEN_NET_PNL unset: observed net P&L {}", first.net_pnl));
    assert_eq!(first.net_pnl, expected, "golden net P&L drifted");
}

// ---------------------------------------------------------------------------
// 5. The swing conjuncts on hand-built swings
// ---------------------------------------------------------------------------

/// 2025-01-01T00:00:00Z in epoch milliseconds (a multiple of the 8h funding interval).
const START: i64 = 1_735_689_600_000;
const M15_MS: i64 = 900_000;
const H4_MS: i64 = 14_400_000;

/// The zero-rate funding stamps on 8h boundaries (the setup suite's policy).
fn funding_stamp(open_time: i64) -> Option<Decimal> {
    if open_time % 28_800_000 == 0 {
        Some(Decimal::ZERO)
    } else {
        None
    }
}

/// One 15m candle at absolute index `i`, flat at 100.
fn flat_m15(i: i64) -> Candle {
    let open_time = START + i * M15_MS;
    Candle {
        open_time,
        close_time: open_time + M15_MS - 1,
        open: dec("100"),
        high: dec("100"),
        low: dec("100"),
        close: dec("100"),
        volume: dec("1"),
        funding_rate: funding_stamp(open_time),
    }
}

/// One 4H candle at absolute index `j` that carries its `high` and `low` exactly;
/// open and close sit inside.
fn h4_bar(j: i64, high: i64, low: i64) -> Candle {
    let open_time = START + j * H4_MS;
    Candle {
        open_time,
        close_time: open_time + H4_MS - 1,
        open: Decimal::from(high - 1),
        high: Decimal::from(high),
        low: Decimal::from(low),
        close: Decimal::from(high - 1),
        volume: dec("1"),
        funding_rate: funding_stamp(open_time),
    }
}

fn series_of(timeframe: Timeframe, candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe,
        version: DataVersion::new("v-cipher-v2-swings"),
        candles,
    }
}

/// 15 4H bars, indices 0..=14. The shape has a swing at index 3 and, unless the
/// shape says otherwise, one at index 10; with `left = right = 3` they confirm
/// when bars 6 and 13 close. Hand-checked: no other bar is a swing (every other
/// bar sits on a ramp and loses the left or the right test).
///
/// A swing HIGH shape is read off the highs column; the lows column of a swing LOW
/// shape is `190 - shape`, so a peak of the shape is a trough of the lows.
type Shape = [i64; 15];

/// Peaks 110 @3 then 115 @10: the later peak is higher.
const PEAKS_UP: Shape = [
    100, 102, 104, 110, 106, 104, 102, 104, 106, 108, 115, 109, 107, 105, 103,
];
/// Peaks 115 @3 then 110 @10: the later peak is lower.
const PEAKS_DOWN: Shape = [
    100, 102, 104, 115, 106, 104, 102, 104, 106, 108, 110, 107, 105, 103, 101,
];
/// Peaks 110 @3 and 110 @10: the same price.
const PEAKS_EQUAL: Shape = [
    100, 102, 104, 110, 106, 104, 102, 104, 106, 108, 110, 107, 105, 103, 101,
];
/// One peak, 110 @3, then a steady fall: no second swing ever confirms.
const PEAK_ALONE: Shape = [
    100, 102, 104, 110, 106, 104, 102, 101, 100, 99, 98, 97, 96, 95, 94,
];

/// The 4H series whose highs follow `highs` and whose lows are `190 - lows_shape`.
fn swing_h4(highs: &Shape, lows_shape: &Shape) -> CandleSeries {
    let candles = (0..15)
        .map(|j| {
            let at = usize::try_from(j).expect("small index");
            h4_bar(j, highs[at], 190 - lows_shape[at])
        })
        .collect();
    series_of(Timeframe::H4, candles)
}

/// 240 flat 15m bars: 4H bars 0..=14 close on 15m bars 15, 31, ..., 239.
fn swing_m15() -> CandleSeries {
    series_of(Timeframe::M15, (0..240).map(flat_m15).collect())
}

/// The 15m bar where 4H bar 14 closes: the arm bar of every scenario. Both second
/// swings confirmed when bar 13 closed; the engine's warm rule asks for a value on
/// the bar before as well (`IndicatorEngine::is_warm`), so the first bar the rule
/// can arm is the next 4H close.
const ARM_BAR: usize = 239;
/// The 15m bar where 4H bar 12 closes: the second swing is not yet confirmed.
const BAR_BEFORE_CONFIRMATION: usize = 207;

/// The two swing conjuncts (conjuncts 4 and 5) of one side's `arm`, taken from the
/// parsed document.
fn swing_conjuncts(dsl: &StrategyDsl, long: bool) -> Vec<Condition> {
    let setup = dsl.setup.as_ref().expect("the document has a setup");
    let side = if long { &setup.long } else { &setup.short };
    let side = side.as_ref().expect("the side is declared");
    let Condition::And { conditions } = &side.arm else {
        panic!("the arm is an And: {:?}", side.arm);
    };
    assert_eq!(conditions.len(), 5, "v1's three conjuncts plus the swings");
    conditions[3..].to_vec()
}

fn price(series: Series, field: PriceField) -> ValueSource {
    ValueSource::Price { series, field }
}

fn constant(text: &str) -> ValueSource {
    ValueSource::Constant { value: dec(text) }
}

fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

/// A probe strategy: each side's `arm` is exactly the given swing conjuncts; dip
/// `close < 99` / `close > 101`, trigger `close > 101` / `close < 99`, shared gate
/// `close > 0`, `StopLoss 0.05`.
fn probe_strategy(long_arm: Vec<Condition>, short_arm: Vec<Condition>) -> CompiledStrategy {
    let close = || price(Series::Primary, PriceField::Close);
    let dsl = StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "cipher v2 swing conjuncts".to_owned(),
        direction: Direction::Long,
        entry: compare(close(), Comparator::Gt, constant("0")),
        filters: vec![],
        exits: vec![ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(dec("0.05")),
        }],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
            max_leverage: SweepableValue::Fixed(dec("3")),
        },
        setup: Some(Box::new(pulse::Setup {
            expiry_bars: SweepableValue::Fixed(96),
            long: Some(SetupSide {
                arm: Condition::And {
                    conditions: long_arm,
                },
                dip: compare(close(), Comparator::Lt, constant("99")),
                trigger: compare(close(), Comparator::Gt, constant("101")),
                stop_anchor: None,
            }),
            short: Some(SetupSide {
                arm: Condition::And {
                    conditions: short_arm,
                },
                dip: compare(close(), Comparator::Gt, constant("101")),
                trigger: compare(close(), Comparator::Lt, constant("99")),
                stop_anchor: None,
            }),
        })),
    };
    compile(&validate(&dsl).expect("the probe strategy validates"))
        .expect("the probe strategy compiles")
}

/// Step `strategy` over the 15m and 4H series and return every bar.
fn step_swings(strategy: &CompiledStrategy, htf: &CandleSeries) -> Vec<Bar> {
    let primary = swing_m15();
    let mut session = EngineSession::new(
        strategy,
        &Pair::new("BTCUSDT"),
        SessionTimeframes {
            primary: Timeframe::M15,
            htf: Some(Timeframe::H4),
            d1: None,
        },
        BacktestConfig {
            starting_equity: dec("10000"),
            taker_fee_bps: dec("0"),
            slippage_bps: dec("0"),
        },
        SymbolFilters::unconstrained(),
        None,
    )
    .expect("the session builds");
    let mut bars: Vec<Bar> = Vec::new();
    step_series(&mut session, &primary, Some(htf), None, |open_ms, probe| {
        let view = probe.setup_view().expect("a setup strategy has a view");
        bars.push(Bar {
            open_ms,
            long: view.long.expect("the long side is declared").event,
            short: view.short.expect("the short side is declared").event,
            swings: Some(read_swings(probe)),
        });
    })
    .expect("the series steps");
    bars
}

/// A pinned price literal.
fn px(value: i64) -> Decimal {
    Decimal::from(value)
}

/// The two swing conjuncts of `cipher-v2.json`, driven over hand-built swings.
/// Each case names the four swing prices the conjuncts read at the arm bar, so a
/// refusal is shown to come from the swings and not from a cold engine.
#[test]
fn v2_swing_conjuncts_on_hand_built_swings() {
    let dsl = dsl_of(&v2_path());
    let strategy = probe_strategy(swing_conjuncts(&dsl, true), swing_conjuncts(&dsl, false));

    // (label, highs shape, lows shape, expected swings, long arms, short arms).
    // The lows column is `190 - lows shape`: a peak of 115 is a low of 75.
    let cases: [(&str, &Shape, &Shape, Swings, bool, bool); 6] = [
        (
            "later high higher, later low higher",
            &PEAKS_UP,
            &PEAKS_DOWN,
            Swings {
                high: Some(px(115)),
                previous_high: Some(px(110)),
                low: Some(px(80)),
                previous_low: Some(px(75)),
            },
            true,
            false,
        ),
        (
            "later high lower, later low lower",
            &PEAKS_DOWN,
            &PEAKS_UP,
            Swings {
                high: Some(px(110)),
                previous_high: Some(px(115)),
                low: Some(px(75)),
                previous_low: Some(px(80)),
            },
            false,
            true,
        ),
        (
            "equal highs, lows higher",
            &PEAKS_EQUAL,
            &PEAKS_DOWN,
            Swings {
                high: Some(px(110)),
                previous_high: Some(px(110)),
                low: Some(px(80)),
                previous_low: Some(px(75)),
            },
            false,
            false,
        ),
        (
            "equal lows, highs higher",
            &PEAKS_UP,
            &PEAKS_EQUAL,
            Swings {
                high: Some(px(115)),
                previous_high: Some(px(110)),
                low: Some(px(80)),
                previous_low: Some(px(80)),
            },
            false,
            false,
        ),
        (
            "highs higher, lows lower",
            &PEAKS_UP,
            &PEAKS_UP,
            Swings {
                high: Some(px(115)),
                previous_high: Some(px(110)),
                low: Some(px(75)),
                previous_low: Some(px(80)),
            },
            false,
            false,
        ),
        (
            "only one swing high confirmed",
            &PEAK_ALONE,
            &PEAKS_DOWN,
            Swings {
                high: Some(px(110)),
                previous_high: None,
                low: Some(px(80)),
                previous_low: Some(px(75)),
            },
            false,
            false,
        ),
    ];

    for (label, highs, lows_shape, expected, long_arms, short_arms) in cases {
        let bars = step_swings(&strategy, &swing_h4(highs, lows_shape));
        assert_eq!(bars.len(), 240, "{label}: the series steps every bar");

        // The state at the arm bar: the four swing prices, then each side's event.
        let at_arm = bars[ARM_BAR];
        assert_eq!(
            at_arm.swings,
            Some(expected),
            "{label}: the swing prices at the arm bar"
        );
        let armed = Some(SetupEvent::Armed);
        assert_eq!(
            at_arm.long == armed,
            long_arms,
            "{label}: long side event {:?}",
            at_arm.long
        );
        assert_eq!(
            at_arm.short == armed,
            short_arms,
            "{label}: short side event {:?}",
            at_arm.short
        );

        // Before the second swing of each kind confirms, nothing arms: the
        // previous swing prices are `None`.
        let early = bars[BAR_BEFORE_CONFIRMATION];
        let early_swings = early.swings.expect("the bar carries the swing prices");
        assert_eq!(
            (early_swings.previous_high, early_swings.previous_low),
            (None, None),
            "{label}: at most one swing of each kind is confirmed before bar 13"
        );
        assert!(
            early.long != armed && early.short != armed,
            "{label}: no side arms before two swings of each kind are confirmed"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. MCP round trip
// ---------------------------------------------------------------------------

/// `submit_strategy_version` accepts the document, and `pulse://dsl/schema` lists
/// `PreviousPivotHigh` and `PreviousPivotLow`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cipher_v2_round_trips_over_mcp() {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, _db) = support::mcp::migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    support::mcp::copy_tree(
        &support::mcp::manifest(support::mcp::FIXTURE_STORE),
        &store_dir,
    );
    let client = support::mcp::spawn_client(&db_path, &store_dir).await;

    let read = client
        .read_resource(ReadResourceRequestParams::new("pulse://dsl/schema"))
        .await
        .expect("resources/read pulse://dsl/schema");
    let ResourceContents::TextResourceContents { text, .. } = &read.contents[0] else {
        panic!(
            "dsl_schema must be text contents, got {:?}",
            read.contents[0]
        );
    };
    let doc: Value = serde_json::from_str(text).expect("dsl_schema body parses as JSON");
    let schema = doc["json_schema"].to_string();
    for spelling in ["\"PreviousPivotHigh\"", "\"PreviousPivotLow\""] {
        assert!(
            schema.contains(spelling),
            "pulse://dsl/schema does not list {spelling}"
        );
    }

    let document: Value = serde_json::from_str(&json_text(&v2_path())).expect("v2 is JSON");
    let submitted = support::mcp::call(
        &client,
        "submit_strategy_version",
        json!({
            "strategy_name": "Cipher Pullback Engulf v2",
            "dsl": document,
            "hypothesis": "r6.s3.w3: STR-0003's v2 rules as one two-sided document",
        }),
    )
    .await;
    let version_id = submitted["version_id"]
        .as_str()
        .expect("version_id reported")
        .to_owned();
    let version =
        support::mcp::call(&client, "get_version", json!({"version_id": version_id})).await;
    assert_eq!(version["dsl_schema_version"], "1.7.0");
    assert_eq!(version["dsl"]["name"], document["name"]);
    assert_eq!(version["dsl"]["setup"], document["setup"]);
    assert_eq!(version["dsl"]["exits"], document["exits"]);

    client.cancel().await.expect("cancel session");
}
