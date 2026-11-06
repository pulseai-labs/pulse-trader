//! r6.s1.w3 — the Cipher building blocks composed in one rule and proven on
//! real candles.
//!
//! `examples/strategies/cipher-building-blocks.json` is a schema-1.5.0, long-only
//! composition proof, **not** the v1 strategy and not a profitable one. It
//! stitches w1's and w2's operands (SMA 50, the depth-3 `max` ribbon band top,
//! `WaveTrend{9,12}`) together with the existing `Compare`, `And`, `Or`, `Arith`
//! and `Lag`: a 4H context (close above SMA 50), a 4H pullback (WT1 below 0 on
//! one of the 3 bars before the signal), a 4H signal (STR-0003's engulfing or the
//! pole) and a 15m trigger (close above the ribbon band top and the 15m SMA 50,
//! a recent 15m WT1 dip). The setup sequence (arm, dip, trigger, expiry) is
//! r6.s2's, not this item's.
//!
//! Three things are proven here:
//!
//! 1. the example backtests deterministically on the committed BTCUSDT fixture
//!    (`btcusdt_backtest_is_deterministic`);
//! 2. on the SOLUSDT fixture the 4H conditions hold on the signal bars of
//!    STR-0003's examples 1 and 3 (`solusdt_signal_holds_on_the_example_bars`);
//! 3. `explain_marks` reports the engine value of every operand at a marked bar
//!    (`explain_reports_every_operand_value`).
//!
//! Every mark is on 2026-08-27 or 2026-09-24, outside freeze `30f98b21`'s
//! holdout (starts 2026-10-10).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::path::{Path, PathBuf};

use pulse::{
    BacktestConfig, BacktestResult, BinanceAdapter, CandleSeries, CandleSeriesRepository,
    CandleStore, CompiledStrategy, ExampleMarkDraft, ExampleMarkRepository, ExchangeAdapter,
    ExplainReport, ExplainRequest, MarkSelection, MarkSetup, MarkSide, MarkWriter, MarkWriterKind,
    Migrator, Pair, RuleRequest, SqliteExampleMarkRepo, SqliteStrategyRepo, StrategyDsl,
    SystemClock, Timeframe, compile, explain_marks, run_backtest, validate,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Frozen golden: the number of completed trades the example produces on the
/// committed BTCUSDT fixture (15m primary, 4h htf). Observed: 0, with a net P&L
/// of 0. The rule is a composition proof, not a strategy, and the spec allows a
/// count of 0; the observed pair is pinned so a silent engine drift fails
/// loudly.
const GOLDEN_TRADE_COUNT: usize = 0;

/// Frozen golden: the exact net P&L (quote currency, net of costs) of that run.
const GOLDEN_NET_PNL: &str = "0";

/// The checked-in example this suite pins.
fn example_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("examples/strategies/cipher-building-blocks.json")
}

/// The example's raw JSON text.
fn example_json() -> String {
    std::fs::read_to_string(example_path()).expect("read the checked-in example json")
}

/// Load the example through the read path the engine consumes:
/// `Migrator::v1().load` (version-safe) -> `validate` -> `compile`.
fn compiled_example() -> CompiledStrategy {
    let dsl: StrategyDsl = Migrator::v1()
        .load(&example_json())
        .expect("the example loads through Migrator::v1")
        .dsl;
    compile(&validate(&dsl).expect("the example validates")).expect("the example compiles")
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

fn btcusdt_store() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store")
}

/// One run of the example over the BTCUSDT fixture's M15 primary + H4 htf, with
/// the real BTCUSDT filters and the default cost model.
fn run_btcusdt(compiled: &CompiledStrategy) -> BacktestResult {
    let primary = load_series(&btcusdt_store(), "BTCUSDT", Timeframe::M15);
    let htf = load_series(&btcusdt_store(), "BTCUSDT", Timeframe::H4);
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port");
    run_backtest(
        compiled,
        &primary,
        Some(&htf),
        None,
        &BacktestConfig::default(),
        &filters,
        pulse::SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("backtest runs over the fixture")
}

/// Two runs agree on every trade and on the P&L, and the observed trade count
/// and net P&L are the pinned goldens.
#[test]
fn btcusdt_backtest_is_deterministic() {
    let compiled = compiled_example();
    let first = run_btcusdt(&compiled);
    let second = run_btcusdt(&compiled);

    assert_eq!(
        first.trades, second.trades,
        "two runs of the example gave different trades"
    );
    assert_eq!(
        first.net_pnl, second.net_pnl,
        "two runs of the example gave different net P&L"
    );
    assert_eq!(
        first.trades.len(),
        GOLDEN_TRADE_COUNT,
        "golden trade count drifted: got {} (net P&L {})",
        first.trades.len(),
        first.net_pnl
    );
    let expected = rust_decimal::Decimal::from_str_exact(GOLDEN_NET_PNL)
        .unwrap_or_else(|_| panic!("GOLDEN_NET_PNL unset: observed net P&L {}", first.net_pnl));
    assert_eq!(first.net_pnl, expected, "golden net P&L drifted");
}

// ---------------------------------------------------------------------------
// The SOLUSDT marks (STR-0003's examples 1 and 3)
// ---------------------------------------------------------------------------

/// Example 1: the 15m bar opening 2026-08-27 00:00 UTC. The pole bar (4h,
/// 2026-08-26 20:00) has just closed.
const EXAMPLE_1_MS: i64 = 1_787_788_800_000;

/// Example 3: the 15m bar opening 2026-09-24 16:00 UTC. The engulfing bar (4h,
/// 2026-09-24 12:00) has just closed.
const EXAMPLE_3_MS: i64 = 1_790_265_600_000;

fn solusdt_store() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/solusdt-cipher-store")
}

/// A migrated database plus a writable copy of the SOLUSDT fixture store, with
/// one 15m long mark written at each example bar through the repository.
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
    for bar_open_ms in [EXAMPLE_1_MS, EXAMPLE_3_MS] {
        let mark = repo
            .insert(&ExampleMarkDraft {
                pair: Pair::new("SOLUSDT"),
                timeframe: Timeframe::M15,
                bar_open_ms,
                side: MarkSide::Long,
                setup: MarkSetup::parse("cipher-building-blocks").expect("a setup label"),
                note: None,
                writer: MarkWriter {
                    kind: MarkWriterKind::Trader,
                    name: "r6-s1-w3".to_owned(),
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

// ---------------------------------------------------------------------------
// The rules: the example, and the probes cut from it
// ---------------------------------------------------------------------------

/// The example as a JSON document.
fn example_doc() -> Value {
    serde_json::from_str(&example_json()).expect("the example is JSON")
}

/// The children of a JSON `And`/`Or` node.
fn children(node: &Value) -> Vec<Value> {
    node["conditions"]
        .as_array()
        .expect("an And/Or node carries conditions")
        .clone()
}

/// The example with its `filters` replaced: every probe keeps the example's
/// entry (band top and 15m SMA 50) and its name, schema and exits.
fn probe(filters: Vec<Value>) -> Value {
    let mut doc = example_doc();
    doc["filters"] = Value::Array(filters);
    doc
}

/// Filter 1 (the 4H context), the example's first filter.
fn context_filter() -> Value {
    example_doc()["filters"][0].clone()
}

/// The 4H signal filter's two branches: `(ENGULFING, POLE)`, each an `And`.
fn signal_branches() -> (Value, Value) {
    let doc = example_doc();
    let signal = children(&doc["filters"][2]);
    assert_eq!(signal.len(), 2, "the signal is an Or of ENGULFING and POLE");
    (signal[0].clone(), signal[1].clone())
}

/// The state of every filter-sourced conjunct of one mark, in order.
fn filter_states(report: &ExplainReport, mark: usize) -> Vec<(&str, usize)> {
    report.marks[mark]
        .conjuncts
        .iter()
        .filter(|conjunct| conjunct.source == "filter")
        .map(|conjunct| (conjunct.state, conjunct.index))
        .collect()
}

/// Assert that the conjuncts at these positions among the FILTER conjuncts are
/// all `held` at this mark.
fn assert_filters_held(report: &ExplainReport, mark: usize, which: &[usize], what: &str) {
    let states = filter_states(report, mark);
    for position in which {
        let (state, index) = states[*position];
        assert_eq!(
            state, "held",
            "{what}: filter conjunct {position} (conjunct {index}) is {state} at mark {mark} \
             (evaluation bar {}); condition: {}",
            report.marks[mark].evaluation_bar, report.marks[mark].conjuncts[index].condition
        );
    }
}

/// On the SOLUSDT fixture the 4H conditions hold on the signal bars of STR-0003's
/// examples 1 and 3. Explain reports a nested `Or` by state only, so the branch
/// detail comes from two probes with the branches flattened to top level.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn solusdt_signal_holds_on_the_example_bars() {
    let world = world().await;

    // The example itself: filters 1, 2 and 3 (context, pullback, signal) held at
    // both marks. Filter 4 (the 15m dip) is not asserted: it is a 15m trigger.
    let report = world.explain(&example_doc()).await;
    assert_eq!(report.marks_explained, 2, "both marks explained");
    assert!(report.refusals.is_empty(), "{:?}", report.refusals);
    for mark in 0..2 {
        assert_filters_held(&report, mark, &[0, 1, 2], "the example");
    }

    let (engulfing, pole) = signal_branches();

    // The pole probe: filter 1 plus POLE's three compares, top level, no `Or`.
    // Example 1 is the pole bar: every POLE conjunct held.
    let mut filters = vec![context_filter()];
    filters.extend(children(&pole));
    let report = world.explain(&probe(filters)).await;
    assert_eq!(report.marks_explained, 2);
    assert_filters_held(&report, 0, &[0, 1, 2, 3], "the pole probe at example 1");

    // The engulfing probe: filter 1 plus ENGULFING's four compares. Example 3 is
    // the engulfing bar: every ENGULFING conjunct held.
    let mut filters = vec![context_filter()];
    filters.extend(children(&engulfing));
    let report = world.explain(&probe(filters)).await;
    assert_eq!(report.marks_explained, 2);
    assert_filters_held(
        &report,
        1,
        &[0, 1, 2, 3, 4],
        "the engulfing probe at example 3",
    );
}

/// `TradingView`'s readings on each mark's signal bar (STR-0003's ledger): the 4H
/// SMA 50 and the 4H WT1 on the pole bar (2026-08-26 20:00) and on the engulfing
/// bar (2026-09-24 12:00). Typed from the ledger, not derived from the engine.
const LEDGER_AT_MARK: [(&str, &str); 2] = [("91.76", "16.39"), ("110.65", "-19.21")];

/// The operand probe's four top-level compares (no `Or`, so explain reports the
/// operands of each): 4H close above the 4H SMA 50, the 4H WT1 below 0, the 4H
/// WT1 one bar back below 0, and the 15m WT1 below 0. Together with the
/// example's two entry compares (band top and 15m SMA 50), every operand the
/// example reads is reported.
fn operand_probe() -> Value {
    let wt = |series: &str| {
        json!({"type": "Indicator", "series": series,
               "spec": {"indicator": "WaveTrend", "channel": 9, "average": 12}})
    };
    let below_zero = |lhs: Value| {
        json!({"type": "Compare", "lhs": lhs, "op": "Lt",
                                         "rhs": {"type": "Constant", "value": "0"}})
    };
    probe(vec![
        json!({"type": "Compare",
               "lhs": {"type": "Price", "series": "htf", "field": "Close"},
               "op": "Gt",
               "rhs": {"type": "Indicator", "series": "htf",
                       "spec": {"indicator": "Sma", "period": 50}}}),
        below_zero(wt("htf")),
        below_zero(json!({"type": "Lag", "value": wt("htf"), "bars": 1})),
        below_zero(wt("primary")),
    ])
}

/// Explain reports the engine value of every operand at a marked bar: each
/// `OperandReport` has a non-null value at both marks, and the labels name the
/// 4H SMA, the 4H and 15m WT1 and the ribbon band top.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explain_reports_every_operand_value() {
    let world = world().await;
    let report = world.explain(&operand_probe()).await;
    assert_eq!(report.marks_explained, 2, "both marks explained");

    for (index, mark) in report.marks.iter().enumerate() {
        let operands: Vec<_> = mark
            .conjuncts
            .iter()
            .flat_map(|conjunct| conjunct.operands.iter())
            .collect();
        assert!(!operands.is_empty(), "mark {index} reports no operands");
        for operand in &operands {
            assert!(
                operand.value.is_some(),
                "mark {index}: operand {} has a null value at {}",
                operand.label,
                mark.evaluation_bar
            );
        }

        let labels: Vec<&str> = operands.iter().map(|o| o.label.as_str()).collect();
        assert!(
            labels.contains(&"h4:sma:50"),
            "mark {index}: no h4:sma:50 label in {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("h4:wavetrend:9:12")),
            "mark {index}: no h4:wavetrend:9:12 label in {labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|l| l.contains("wavetrend:9:12") && !l.contains("h4:")),
            "mark {index}: no 15m wavetrend:9:12 label (without h4:) in {labels:?}"
        );
        assert!(
            labels.iter().any(|l| l.contains("max(")),
            "mark {index}: no max( label for the band top in {labels:?}"
        );

        // The values are the engine's, read on the signal bar: they sit on
        // TradingView's two-decimal readings in STR-0003's ledger (the same
        // readings `sma_parity` and `wavetrend_parity` pin).
        let (sma_ledger, wt1_ledger) = LEDGER_AT_MARK[index];
        let value_of = |label: &str| -> Decimal {
            let operand = operands
                .iter()
                .find(|o| o.label == label)
                .unwrap_or_else(|| panic!("mark {index}: no {label} operand"));
            Decimal::from_str_exact(operand.value.as_deref().expect("non-null above"))
                .expect("a decimal value")
        };
        let tolerance = Decimal::from_str_exact("0.05").expect("a decimal literal");
        for (label, ledger) in [("h4:sma:50", sma_ledger), ("h4:wavetrend:9:12", wt1_ledger)] {
            let ledger = Decimal::from_str_exact(ledger).expect("a decimal literal");
            let diff = (value_of(label) - ledger).abs();
            assert!(
                diff <= tolerance,
                "mark {index}: {label} = {}, TradingView = {ledger}, |diff| = {diff}",
                value_of(label)
            );
        }
    }
}
