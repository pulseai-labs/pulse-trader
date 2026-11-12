//! r6.s2.w5 — Cipher Pullback Engulf v1 (STR-0003) as one two-sided document, proven
//! on STR-0003's design examples 1 and 3.
//!
//! `examples/strategies/cipher-v1.json` is a schema-1.6.0 strategy with a long side
//! and a short side, a `StructureStop` of 0.05% and a 3R take-profit. This suite is
//! STR-0003's "before any run" check:
//!
//! 1. the document loads, validates and compiles (`cipher_v1_document_loads`);
//! 2. on SOLUSDT the long side arms on the 4H signal bar of each example and
//!    triggers at or before the example's entry bar
//!    (`v1_arms_and_triggers_on_examples_1_and_3`);
//! 3. the trades from those triggers carry the structure stop and the 3R target
//!    (`v1_trades_carry_the_structure_stop`);
//! 4. on BTCUSDT the strategy runs twice to the same hash
//!    (`btcusdt_runs_deterministically`);
//! 5. `explain_marks` shows the setup state at the example entry bars
//!    (`explain_reports_the_setup_at_the_example_entry_bars`);
//! 6. the document round-trips over MCP (`cipher_v1_round_trips_over_mcp`).
//!
//! Every mark and every bar is before freeze `30f98b21`'s holdout (2026-10-10).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::path::{Path, PathBuf};

use pulse::{
    BacktestConfig, BacktestResult, BinanceAdapter, Candle, CandleSeries, CandleSeriesRepository,
    CandleStore, CompiledExit, CompiledStrategy, ConjunctReport, Direction, EngineSession,
    ExampleMarkDraft, ExampleMarkRepository, ExchangeAdapter, ExitReason, ExplainReport,
    ExplainRequest, MarkSelection, MarkSetup, MarkSide, MarkWriter, MarkWriterKind, Migrator, Pair,
    RuleRequest, SeriesEnd, SessionTimeframes, SetupEvent, SetupReport, SideState,
    SqliteExampleMarkRepo, SqliteStrategyRepo, StrategyDsl, SystemClock, Timeframe, Trade, compile,
    explain_marks, run_backtest, step_series, validate,
};
use rmcp::model::{ReadResourceRequestParams, ResourceContents};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tempfile::TempDir;

/// The checked-in document this suite pins.
fn document_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/strategies/cipher-v1.json")
}

/// The document's raw JSON text.
fn document_json() -> String {
    std::fs::read_to_string(document_path()).expect("read the checked-in document")
}

/// Load the document through the read path the engine consumes:
/// `Migrator::v1().load` (version-safe) -> `validate` -> `compile`.
fn compiled_document() -> CompiledStrategy {
    let dsl: StrategyDsl = Migrator::v1()
        .load(&document_json())
        .expect("the document loads through Migrator::v1")
        .dsl;
    compile(&validate(&dsl).expect("the document validates")).expect("the document compiles")
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

/// The span of one 15m bar.
const M15_MS: i64 = 900_000;

/// One design example of STR-0003, all times UTC.
struct Example {
    /// `1` or `3`, STR-0003's numbering.
    number: u8,
    /// The 15m bar that opens as the 4H signal bar closes: the arm bar.
    arm_bar_open: &'static str,
    /// The arm bar's close time: the setup's `arm_time`.
    arm_time: &'static str,
    /// The example's entry bar (15m, open).
    entry_bar_open: &'static str,
}

const EXAMPLES: [Example; 2] = [
    // 4H signal bar: 2026-08-26 20:00 (the pole).
    Example {
        number: 1,
        arm_bar_open: "2026-08-26T23:45:00Z",
        arm_time: "2026-08-26T23:59:59.999Z",
        entry_bar_open: "2026-08-27T05:45:00Z",
    },
    // 4H signal bar: 2026-09-24 12:00 (the engulfing).
    Example {
        number: 3,
        arm_bar_open: "2026-09-24T15:45:00Z",
        arm_time: "2026-09-24T15:59:59.999Z",
        entry_bar_open: "2026-09-25T08:15:00Z",
    },
];

impl Example {
    fn arm_bar_open_ms(&self) -> i64 {
        ms(self.arm_bar_open)
    }

    fn arm_time_ms(&self) -> i64 {
        ms(self.arm_time)
    }

    fn entry_bar_open_ms(&self) -> i64 {
        ms(self.entry_bar_open)
    }
}

/// The SOLUSDT fixture's 15m and 4h series.
fn solusdt_series() -> (CandleSeries, CandleSeries) {
    (
        load_series(&solusdt_store(), "SOLUSDT", Timeframe::M15),
        load_series(&solusdt_store(), "SOLUSDT", Timeframe::H4),
    )
}

fn filters_of(pair: &str) -> pulse::SymbolFilters {
    BinanceAdapter::new()
        .symbol_filters(&Pair::new(pair))
        .expect("the pair's filters resolve through the port")
}

/// Drive an `EngineSession` over the SOLUSDT fixture with `step_series`, recording
/// the long side's state at every bar: `(bar open, state after the bar)`.
fn long_side_states(compiled: &CompiledStrategy) -> Vec<(i64, SideState)> {
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
    let mut states: Vec<(i64, SideState)> = Vec::new();
    step_series(
        &mut session,
        &primary,
        Some(&htf),
        None,
        |open_ms, probe| {
            let view = probe
                .setup_view()
                .expect("the document has a setup, so the probe has a view");
            states.push((open_ms, view.long.expect("the document has a long side")));
        },
    )
    .expect("the series steps");
    states
}

/// The trigger time of an example: the close time of the first bar after the arm bar
/// and at or before the entry bar whose event is `Triggered`.
fn trigger_time_of(example: &Example, states: &[(i64, SideState)]) -> i64 {
    let arm_bar = example.arm_bar_open_ms();
    let entry_bar = example.entry_bar_open_ms();
    let fired: Vec<&(i64, SideState)> = states
        .iter()
        .filter(|(open_ms, state)| {
            *open_ms > arm_bar
                && *open_ms <= entry_bar
                && state.event == Some(SetupEvent::Triggered)
        })
        .collect();
    let (_, state) = fired.first().unwrap_or_else(|| {
        panic!(
            "example {}: the long side never triggers after {} and by {}",
            example.number, example.arm_bar_open, example.entry_bar_open
        )
    });
    let fire = state
        .last_trigger
        .expect("a Triggered event leaves a last_trigger");
    assert_eq!(
        fire.arm_time,
        example.arm_time_ms(),
        "example {}: the trigger belongs to a different arm",
        example.number
    );
    fire.trigger_time
}

// ---------------------------------------------------------------------------
// 1. The document
// ---------------------------------------------------------------------------

/// The document loads through `Migrator::v1().load`, validates and compiles; both
/// sides are present and the exits and expiry are STR-0003's.
#[test]
fn cipher_v1_document_loads() {
    let compiled = compiled_document();
    let setup = compiled.setup().expect("the document declares a setup");
    assert!(setup.long.is_some(), "the long side is declared");
    assert!(setup.short.is_some(), "the short side is declared");
    assert_eq!(setup.expiry_bars, 96, "expiry_bars is 96");

    let buffer = Decimal::from_str_exact("0.0005").expect("a decimal literal");
    let target = Decimal::from(3);
    assert!(
        compiled
            .exits()
            .iter()
            .any(|exit| matches!(exit, CompiledExit::StructureStop { buffer_pct } if *buffer_pct == buffer)),
        "a StructureStop of 0.0005 is declared: {:?}",
        compiled.exits()
    );
    assert!(
        compiled.exits().iter().any(
            |exit| matches!(exit, CompiledExit::TakeProfit { target_r } if *target_r == target)
        ),
        "a TakeProfit of 3R is declared: {:?}",
        compiled.exits()
    );
}

// ---------------------------------------------------------------------------
// 2. Arm and trigger on examples 1 and 3
// ---------------------------------------------------------------------------

/// On SOLUSDT the long side arms on each example's arm bar, with that `arm_time`, and
/// triggers on some later bar at or before the example's entry bar.
#[test]
fn v1_arms_and_triggers_on_examples_1_and_3() {
    let compiled = compiled_document();
    let states = long_side_states(&compiled);
    for example in &EXAMPLES {
        // The arm bar's own close time is the table's `arm_time`.
        assert_eq!(
            example.arm_time_ms(),
            example.arm_bar_open_ms() + M15_MS - 1,
            "example {}: the table's arm_time is the arm bar's close",
            example.number
        );
        let (_, at_arm) = states
            .iter()
            .find(|(open_ms, _)| *open_ms == example.arm_bar_open_ms())
            .unwrap_or_else(|| panic!("example {}: the arm bar is in the fixture", example.number));
        assert_eq!(
            at_arm.event,
            Some(SetupEvent::Armed),
            "example {}: the long side must arm on {}",
            example.number,
            example.arm_bar_open
        );
        assert_eq!(
            at_arm.arm_time,
            example.arm_time_ms(),
            "example {}: arm_time",
            example.number
        );

        // The trigger: `trigger_time_of` asserts the first fire in the window
        // belongs to this arm, so a re-arm before the trigger fails there.
        let trigger_time = trigger_time_of(example, &states);
        let entry_bar_close = example.entry_bar_open_ms() + M15_MS - 1;
        assert!(
            trigger_time > example.arm_time_ms() && trigger_time <= entry_bar_close,
            "example {}: trigger at {trigger_time} is outside ({}, {entry_bar_close}]",
            example.number,
            example.arm_time_ms()
        );
    }
}

// ---------------------------------------------------------------------------
// 3. The trades carry the structure stop
// ---------------------------------------------------------------------------

/// Run the SOLUSDT fixture with `config`.
fn run_solusdt(compiled: &CompiledStrategy, config: &BacktestConfig) -> BacktestResult {
    let (primary, htf) = solusdt_series();
    run_backtest(
        compiled,
        &primary,
        Some(&htf),
        None,
        config,
        &filters_of("SOLUSDT"),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("the backtest runs over the fixture")
}

/// The trade whose `entry_signal_time` is `trigger_time`.
fn trade_at(result: &BacktestResult, trigger_time: i64, example: &Example) -> Trade {
    result
        .trades
        .iter()
        .find(|trade| trade.entry_signal_time == trigger_time)
        .unwrap_or_else(|| {
            panic!(
                "example {}: no trade with entry_signal_time {trigger_time} \
                 (a position may have been open); the run has {} trades",
                example.number,
                result.trades.len()
            )
        })
        .clone()
}

/// The structure stop, computed from the fixture's 15m candles alone: the anchor is
/// the first bar after the arm bar, up to the trigger bar, whose close is below the
/// mean of the last 50 closes including that bar; the extreme is the lowest low from
/// the anchor (or from the bar after the arm bar, if none) to the trigger bar
/// inclusive; the stop is the extreme times 0.9995.
fn oracle_stop(primary: &CandleSeries, arm_time: i64, trigger_time: i64) -> Decimal {
    let candles: &[Candle] = &primary.candles;
    let arm = candles
        .iter()
        .position(|candle| candle.close_time == arm_time)
        .expect("the arm bar is in the fixture");
    let trigger = candles
        .iter()
        .position(|candle| candle.close_time == trigger_time)
        .expect("the trigger bar is in the fixture");
    assert!(trigger > arm, "the trigger bar follows the arm bar");

    let anchor = ((arm + 1)..=trigger).find(|&index| {
        assert!(index >= 49, "50 closes of history are in the fixture");
        let sum: Decimal = candles[index - 49..=index]
            .iter()
            .map(|candle| candle.close)
            .sum();
        // close < sum / 50, without a division.
        candles[index].close * Decimal::from(50) < sum
    });
    let from = anchor.unwrap_or(arm + 1);
    let extreme = candles[from..=trigger]
        .iter()
        .map(|candle| candle.low)
        .min()
        .expect("the window holds at least the trigger bar");
    extreme * Decimal::from_str_exact("0.9995").expect("a decimal literal")
}

/// The trades from the two triggers are long, carry the arm time and the structure
/// stop, and, on a take-profit exit, the 3R target.
#[test]
fn v1_trades_carry_the_structure_stop() {
    let compiled = compiled_document();
    let states = long_side_states(&compiled);
    let (primary, _) = solusdt_series();

    let result = run_solusdt(&compiled, &BacktestConfig::default());
    let no_slippage = BacktestConfig {
        slippage_bps: Decimal::ZERO,
        ..BacktestConfig::default()
    };
    let free = run_solusdt(&compiled, &no_slippage);

    for example in &EXAMPLES {
        let trigger_time = trigger_time_of(example, &states);
        let trade = trade_at(&result, trigger_time, example);
        assert_eq!(
            trade.direction,
            Direction::Long,
            "example {}: direction",
            example.number
        );
        let setup_trace = trade
            .setup
            .unwrap_or_else(|| panic!("example {}: the trade has no setup trace", example.number));
        assert_eq!(
            setup_trace.arm_time,
            example.arm_time_ms(),
            "example {}: setup.arm_time",
            example.number
        );

        let expected = oracle_stop(&primary, example.arm_time_ms(), trigger_time);
        assert_eq!(
            trade.stop_price,
            Some(expected),
            "example {}: the stop is the window's lowest low times 0.9995",
            example.number
        );

        // With zero slippage the take-profit sits exactly 3R above the entry.
        let free_trade = trade_at(&free, trigger_time, example);
        let stop = free_trade.stop_price.expect("the trade records its stop");
        assert_eq!(
            free_trade.stop_price,
            Some(expected),
            "example {}: the stop does not depend on slippage",
            example.number
        );
        if free_trade.exit_reason == ExitReason::TakeProfit {
            assert_eq!(
                free_trade.exit_price,
                free_trade.entry_price + Decimal::from(3) * (free_trade.entry_price - stop),
                "example {}: the take-profit is entry + 3R",
                example.number
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 4. BTCUSDT determinism
// ---------------------------------------------------------------------------

/// Frozen golden: the number of completed trades v1 produces on the committed
/// BTCUSDT fixture (15m primary, 4h htf) under the default cost model.
const GOLDEN_TRADE_COUNT: usize = 5;

/// Frozen golden: the exact net P&L (quote currency, net of costs) of that run.
const GOLDEN_NET_PNL: &str = "-552.2652387762507962";

/// One run of v1 over the BTCUSDT fixture's M15 primary + H4 htf.
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
fn btcusdt_runs_deterministically() {
    let compiled = compiled_document();
    let first = run_btcusdt(&compiled);
    let second = run_btcusdt(&compiled);

    assert_eq!(
        first.result_content_hash(),
        second.result_content_hash(),
        "two runs of v1 gave different result hashes"
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
// 5. Explain shows the setup
// ---------------------------------------------------------------------------

/// A migrated database plus a writable copy of the SOLUSDT fixture store, with one
/// 15m long mark written at each example's entry bar through the repository.
struct World {
    _tmp_db: TempDir,
    _tmp_store: TempDir,
    db: pulse::Db,
    store_dir: PathBuf,
    /// The two mark ids: example 1, then example 3.
    mark_ids: Vec<String>,
}

async fn world() -> World {
    let tmp_db = TempDir::new().expect("tempdir");
    let (_, db) = support::mcp::migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().expect("tempdir");
    let store_dir = tmp_store.path().join("store");
    support::mcp::copy_tree(&solusdt_store(), &store_dir);

    let data_version = CandleStore::with_base_dir(store_dir.clone())
        .load_head(&Pair::new("SOLUSDT"), Timeframe::M15)
        .expect("the 15m HEAD reads")
        .expect("the fixture has a SOLUSDT 15m HEAD")
        .series
        .version
        .as_str()
        .to_owned();
    let repo = SqliteExampleMarkRepo::with_deps(db.pool().clone(), SystemClock);
    let mut mark_ids = Vec::new();
    for example in &EXAMPLES {
        let mark = repo
            .insert(&ExampleMarkDraft {
                pair: Pair::new("SOLUSDT"),
                timeframe: Timeframe::M15,
                bar_open_ms: example.entry_bar_open_ms(),
                side: MarkSide::Long,
                setup: MarkSetup::parse("cipher-v1-example").expect("a setup label"),
                note: None,
                writer: MarkWriter {
                    kind: MarkWriterKind::Trader,
                    name: "r6-s2-w5".to_owned(),
                },
                data_version: data_version.clone(),
            })
            .await
            .expect("the mark inserts");
        mark_ids.push(mark.id);
    }
    World {
        _tmp_db: tmp_db,
        _tmp_store: tmp_store,
        db,
        store_dir,
        mark_ids,
    }
}

impl World {
    /// Explain both marks against one inline rule, as the product does.
    async fn explain(&self, rule: &Value) -> ExplainReport {
        let marks = SqliteExampleMarkRepo::with_deps(self.db.pool().clone(), SystemClock);
        let strategies = SqliteStrategyRepo::new(self.db.pool().clone());
        let candles = CandleStore::with_base_dir(self.store_dir.clone());
        explain_marks(
            &marks,
            &candles,
            &strategies,
            &BinanceAdapter,
            None,
            ExplainRequest {
                marks: MarkSelection::Ids(self.mark_ids.clone()),
                rule: RuleRequest::Inline(Box::new(rule.clone())),
            },
        )
        .await
        .expect("explain_marks answers")
    }
}

/// The report's rows for one setup source, in index order.
fn rows<'a>(setup: &'a SetupReport, source: &str) -> Vec<&'a ConjunctReport> {
    setup
        .conditions
        .iter()
        .filter(|row| row.source == source)
        .collect()
}

/// Every operand of the row carries a value.
fn assert_operands(row: &ConjunctReport, what: &str) {
    assert!(!row.operands.is_empty(), "{what}: the row has no operands");
    for operand in &row.operands {
        assert!(
            operand.value.is_some(),
            "{what}: operand {} has a null value",
            operand.label
        );
    }
}

/// `explain_marks` shows the long side's setup at each example's entry bar: where the
/// side stands, what happened, the fire that led to the entry, and each condition
/// with its operand values.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explain_reports_the_setup_at_the_example_entry_bars() {
    let world = world().await;
    let document: Value = serde_json::from_str(&document_json()).expect("the document is JSON");
    let report = world.explain(&document).await;
    assert_eq!(report.marks_explained, 2, "both marks explained");
    assert!(report.refusals.is_empty(), "{:?}", report.refusals);

    for (index, example) in EXAMPLES.iter().enumerate() {
        let mark = &report.marks[index];
        let setup = mark
            .setup
            .as_ref()
            .unwrap_or_else(|| panic!("example {}: the mark has no setup report", example.number));
        assert_eq!(setup.side, "long", "example {}: side", example.number);
        assert!(
            mark.side_matches,
            "example {}: side_matches",
            example.number
        );

        // Either the side fires on the mark's own bar, or the fire that led to the
        // entry is the side's last trigger, from this example's arm.
        let bar_close = rfc3339(example.entry_bar_open_ms() + M15_MS - 1);
        if setup.event == Some(SetupEvent::Triggered) {
            assert!(
                mark.all_held,
                "example {}: all_held on a fire",
                example.number
            );
        } else {
            let last = setup.last_trigger.as_ref().unwrap_or_else(|| {
                panic!(
                    "example {}: no fire led to the mark (phase {:?}, event {:?})",
                    example.number, setup.phase, setup.event
                )
            });
            assert_eq!(
                last.armed_at,
                rfc3339(example.arm_time_ms()),
                "example {}: last_trigger.armed_at",
                example.number
            );
            assert!(
                last.triggered_at <= bar_close,
                "example {}: triggered_at {} is after the mark bar's close {bar_close}",
                example.number,
                last.triggered_at
            );
            assert!(
                !mark.all_held,
                "example {}: all_held needs a fire on the mark's own bar",
                example.number
            );
        }

        // The four condition groups, each with a state on every row.
        let arm = rows(setup, "setup.long.arm");
        let dip = rows(setup, "setup.long.dip");
        let trigger = rows(setup, "setup.long.trigger");
        let anchor = rows(setup, "setup.long.stop_anchor");
        assert_eq!(arm.len(), 3, "arm: CONTEXT, PULLBACK, SIGNAL");
        assert_eq!(dip.len(), 1, "dip");
        assert_eq!(trigger.len(), 2, "trigger: the SMA 50 and the band top");
        assert_eq!(anchor.len(), 1, "stop_anchor");
        for (position, row) in setup.conditions.iter().enumerate() {
            assert_eq!(row.index, position, "index counts across the list");
            assert!(
                ["held", "not_held", "not_warm"].contains(&row.state),
                "example {}: row {position} state {}",
                example.number,
                row.state
            );
        }

        // The comparison rows carry operand values. PULLBACK and SIGNAL are nested
        // `Or`s, which explain reports by state only.
        assert_operands(arm[0], "arm CONTEXT");
        assert!(arm[1].operands.is_empty(), "arm PULLBACK is state only");
        assert!(arm[2].operands.is_empty(), "arm SIGNAL is state only");
        assert_operands(dip[0], "dip");
        assert_operands(trigger[0], "trigger SMA 50");
        assert_operands(trigger[1], "trigger band top");
        assert_operands(anchor[0], "stop_anchor");
    }

    // A rule without a setup explains as it did before: no `setup` member at all.
    let building_blocks: Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("examples/strategies/cipher-building-blocks.json"),
        )
        .expect("read the building-blocks example"),
    )
    .expect("the building-blocks example is JSON");
    let plain = world.explain(&building_blocks).await;
    for mark in &plain.marks {
        assert!(mark.setup.is_none(), "a rule without a setup has no report");
        let wire = serde_json::to_value(mark).expect("the explanation serializes");
        assert!(
            wire.get("setup").is_none(),
            "a rule without a setup serializes no `setup` member"
        );
    }
}

/// An epoch-ms instant as RFC 3339 UTC milliseconds, explain's spelling.
fn rfc3339(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .expect("a representable instant")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// 6. MCP round trip
// ---------------------------------------------------------------------------

/// `submit_strategy_version` accepts the document, and `pulse://dsl/schema` lists
/// `setup`, `StructureStop` and `expiry_bars`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cipher_v1_round_trips_over_mcp() {
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
    for spelling in ["\"setup\"", "\"StructureStop\"", "\"expiry_bars\""] {
        assert!(
            schema.contains(spelling),
            "pulse://dsl/schema does not list {spelling}"
        );
    }

    let document: Value = serde_json::from_str(&document_json()).expect("the document is JSON");
    let submitted = support::mcp::call(
        &client,
        "submit_strategy_version",
        json!({
            "strategy_name": "Cipher Pullback Engulf v1",
            "dsl": document,
            "hypothesis": "r6.s2.w5: STR-0003's v1 rules as one two-sided document",
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
