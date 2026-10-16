//! r5.s3.w2 (PDR-0037 Q5, ADR-0031, d107) — the `explain_marks` contract.
//!
//! The one truth (SPINE G3): a condition `explain_marks` reports as held at a
//! mark's evaluation bar is exactly what the backtest's entry gate sees at that
//! bar for that rule, and on a backtest's OWN entry bars every condition reads
//! held. The fence (G4): no engine is stepped onto a candle at or after the open
//! freeze's holdout start, and a mark whose evaluation bar opens there is hidden
//! and counted, never explained.
//!
//! **The candles are the fixture's own.** Every bar time below is read out of the
//! committed `btcusdt-1m-store` at run time, never written down: the fixture's 15m
//! and 4h snapshots are the HEAD the store serves, and a hard-coded time would
//! silently stop testing anything the day the fixture moved.
//!
//! **Both edges are driven as the product drives them.** The use case runs over a
//! real store and a real migrated database; the `pulse mcp` stdio tool is driven
//! for the surface contract. Neither reaches behind the other's back, and each
//! edge reads the freeze inside the call it serves.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::path::{Path, PathBuf};

use chrono::SecondsFormat;
use pulse::{
    BacktestConfig, BinanceAdapter, CandleSeries, CandleSeriesRepository, CandleStore, Db,
    EvalContext, ExampleMark, ExampleMarkDraft, ExampleMarkRepository, ExchangeAdapter,
    ExplainError, ExplainReport, ExplainRequest, MarkSelection, MarkSetup, MarkSide, MarkWriter,
    MarkWriterKind, Pair, RuleRequest, SqliteExampleMarkRepo, StrategyRepository, SystemClock,
    Timeframe, compile, explain_marks, run_backtest, validate,
};
use serde_json::Value;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// The fixture's pair.
fn pair() -> Pair {
    Pair::new("BTCUSDT")
}

/// The holdout start every freeze case uses — inside the fixture's January 2025
/// range, a UTC midnight and therefore a 15m and a 4h boundary at once
/// (`tests/chart_read.rs`'s own constant).
const HOLDOUT_MS: i64 = 1_736_985_600_000;

/// Copy the committed fixture into a writable tempdir.
fn fixture_store(tmp: &TempDir) -> PathBuf {
    let store_dir = tmp.path().join("store");
    support::mcp::copy_tree(
        &support::mcp::manifest(support::mcp::FIXTURE_STORE),
        &store_dir,
    );
    store_dir
}

/// A store over a copied fixture data dir.
fn store(dir: &Path) -> CandleStore {
    CandleStore::with_base_dir(dir.to_path_buf())
}

/// The HEAD series for one timeframe, read straight from the fixture — the bars
/// the tests mark are chosen from it, never written down.
fn series(dir: &Path, timeframe: Timeframe) -> CandleSeries {
    store(dir)
        .load_head(&pair(), timeframe)
        .expect("the fixture HEAD reads")
        .expect("the fixture has a HEAD")
        .series
}

/// The whole world one case needs: a migrated db, a copied fixture store, and
/// the handles the use case takes.
struct World {
    _tmp_db: TempDir,
    _tmp_store: TempDir,
    db: Db,
    store_dir: PathBuf,
}

async fn world() -> World {
    let tmp_db = TempDir::new().expect("tempdir");
    let (_, db) = support::mcp::migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().expect("tempdir");
    let store_dir = fixture_store(&tmp_store);
    World {
        _tmp_db: tmp_db,
        _tmp_store: tmp_store,
        db,
        store_dir,
    }
}

impl World {
    /// Write one mark through the port (`SqliteExampleMarkRepo`), stamped with a
    /// version the store really holds — the write path is w1's contract (d106).
    async fn mark(&self, timeframe: Timeframe, bar_open_ms: i64, side: MarkSide) -> ExampleMark {
        let repo = SqliteExampleMarkRepo::with_deps(self.db.pool().clone(), SystemClock);
        repo.insert(&ExampleMarkDraft {
            pair: pair(),
            timeframe,
            bar_open_ms,
            side,
            setup: MarkSetup::parse("explain-fixture").expect("a setup label"),
            note: None,
            writer: MarkWriter {
                kind: MarkWriterKind::Trader,
                name: "w2-app".to_owned(),
            },
            // A real version string: the store requires one, and the use case
            // reads HEAD for the evaluation bars.
            data_version: series(&self.store_dir, timeframe)
                .version
                .as_str()
                .to_owned(),
        })
        .await
        .expect("the mark inserts")
    }

    /// Run the use case over this world.
    async fn explain(
        &self,
        marks: MarkSelection,
        rule: &str,
        freeze_start: Option<i64>,
    ) -> Result<ExplainReport, ExplainError> {
        let marks_repo = SqliteExampleMarkRepo::with_deps(self.db.pool().clone(), SystemClock);
        let strategies = pulse::SqliteStrategyRepo::new(self.db.pool().clone());
        let candles = store(&self.store_dir);
        let exchange = BinanceAdapter;
        explain_marks(
            &marks_repo,
            &candles,
            &strategies,
            &exchange,
            freeze_start.map(|holdout_start_ms| pulse::HoldoutFreeze { holdout_start_ms }),
            ExplainRequest {
                marks,
                rule: RuleRequest::Inline(Box::new(rule_json(rule))),
            },
        )
        .await
    }
}

// ---------------------------------------------------------------------------
// The rules
// ---------------------------------------------------------------------------

/// The rule most cases explain: four conjuncts — the entry, a primary filter, a
/// HIGHER-SERIES filter and a CANDLE PATTERN. The spec asks for "at least three
/// conjuncts across primary and HTF operands (one candle pattern among them)".
const PATTERN_RULE: &str = r#"{
  "schema_version": "1.4.0",
  "name": "r5.s3.w2 explain fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "100" }
  },
  "filters": [
    {
      "type": "Compare",
      "lhs": { "type": "Price", "field": "Close" },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "1" }
    },
    {
      "type": "Compare",
      "lhs": { "type": "Price", "series": "htf", "field": "Close" },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "1" }
    },
    {
      "type": "Compare",
      "lhs": {
        "type": "Indicator",
        "spec": { "indicator": "CandlePattern", "pattern": "Engulfing" }
      },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "-200" }
    }
  ],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A top-level `And` on BOTH halves — the shape the flattening rule exists for,
/// so the conjunct count is the LEAF count and not the node count.
const FLAT_RULE: &str = r#"{
  "schema_version": "1.4.0",
  "name": "flatten fixture",
  "direction": "long",
  "entry": {
    "type": "And",
    "conditions": [
      {
        "type": "Compare",
        "lhs": { "type": "Price", "field": "Close" },
        "op": "Gt",
        "rhs": { "type": "Constant", "value": "1" }
      },
      {
        "type": "Compare",
        "lhs": { "type": "Price", "field": "Open" },
        "op": "Gt",
        "rhs": { "type": "Constant", "value": "1" }
      }
    ]
  },
  "filters": [
    {
      "type": "And",
      "conditions": [
        {
          "type": "Compare",
          "lhs": { "type": "Price", "field": "High" },
          "op": "Gt",
          "rhs": { "type": "Constant", "value": "1" }
        },
        {
          "type": "Compare",
          "lhs": { "type": "Price", "field": "Low" },
          "op": "Gt",
          "rhs": { "type": "Constant", "value": "1" }
        }
      ]
    }
  ],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A SHORT-only rule: one direction apart from `PATTERN_RULE`'s shape.
const SHORT_RULE: &str = r#"{
  "schema_version": "1.4.0",
  "name": "short fixture",
  "direction": "short",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "1" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A cross whose operands both report their previous-bar values.
const CROSS_RULE: &str = r#"{
  "schema_version": "1.4.0",
  "name": "cross fixture",
  "direction": "long",
  "entry": {
    "type": "CrossesAbove",
    "lhs": { "type": "Price", "field": "Close" },
    "rhs": { "type": "Price", "field": "Open" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A rule whose single conjunct can never hold.
const NEVER_RULE: &str = r#"{
  "schema_version": "1.4.0",
  "name": "never fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "field": "Close" },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "0" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// An invalid rule: `risk.risk_per_trade_pct` is out of range — the validator
/// refuses it with a field error.
const INVALID_RULE: &str = r#"{
  "schema_version": "1.4.0",
  "name": "invalid fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "1" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "2.0", "max_leverage": "3" }
}"#;

/// No stop loss, so the ENGINE refuses it — explain must refuse with the same
/// reason, because a rule the backtest cannot run has no backtest truth.
const NO_STOP_RULE: &str = r#"{
  "schema_version": "1.4.0",
  "name": "no stop fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "1" }
  },
  "filters": [],
  "exits": [
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// A rule that reads the `d1` series, which the fixture store has no snapshot
/// for — the "a series the rule reads is missing" refusal.
const D1_RULE: &str = r#"{
  "schema_version": "1.4.0",
  "name": "d1 fixture",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "series": "d1", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "1" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

fn rule_json(text: &str) -> Value {
    serde_json::from_str(text).expect("the rule literal parses")
}

// ---------------------------------------------------------------------------
// Fixture-shaped helpers
// ---------------------------------------------------------------------------

/// An epoch-ms instant as RFC 3339 UTC seconds — the MCP tools' time spelling
/// for an ARGUMENT (`write_mark`'s `bar_time`).
fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("the instant is in range")
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// An epoch-ms instant as RFC 3339 UTC MILLISECONDS — the spelling the explain
/// response emits, matching `tools.rs`'s `window_ms` and `mark_explain.rs`'s
/// `rfc3339_ms`. Comparisons against an EXPLAIN field use this one, never the
/// seconds spelling above.
fn rfc3339_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("the instant is in range")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The fixture's 15m bar open times, in order.
fn m15_bars(dir: &Path) -> Vec<i64> {
    series(dir, Timeframe::M15)
        .candles
        .iter()
        .map(|candle| candle.open_time)
        .collect()
}

/// The first 4h bar whose CLOSE is itself a 15m open time — the bar a 4h mark
/// maps from. Returns `(4h open, 15m evaluation bar)`.
fn h4_bar_with_15m_evaluation(dir: &Path) -> (i64, i64) {
    let h4 = series(dir, Timeframe::H4);
    let m15 = series(dir, Timeframe::M15);
    for candle in &h4.candles {
        let close = candle.open_time + Timeframe::H4.duration_ms();
        if m15.candles.iter().any(|bar| bar.open_time == close) {
            return (candle.open_time, close);
        }
    }
    panic!("the fixture holds no 4h candle whose close is a 15m open time");
}

// ---------------------------------------------------------------------------
// AC-1 · the conjuncts, flattened, with their operands
// ---------------------------------------------------------------------------

/// The conjunct count is the LEAF count: a top-level `And` on the entry and on
/// a filter expands, so four leaves report as four conjuncts in the declared
/// order, each labelled with the half it came from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn top_level_ands_flatten_into_one_conjunct_per_leaf() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let mark = world.mark(Timeframe::M15, bars[200], MarkSide::Long).await;

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id.clone()]), FLAT_RULE, None)
        .await
        .expect("the rule explains");

    assert_eq!(report.marks_explained, 1, "one mark explained");
    let explained = &report.marks[0];
    let sources: Vec<&str> = explained
        .conjuncts
        .iter()
        .map(|conjunct| conjunct.source)
        .collect();
    assert_eq!(
        sources,
        ["entry", "entry", "filter", "filter"],
        "entry ∧ filters with top-level Ands flattened is four conjuncts, \
         the entry's two first"
    );
    let indexes: Vec<usize> = explained
        .conjuncts
        .iter()
        .map(|conjunct| conjunct.index)
        .collect();
    assert_eq!(indexes, [0, 1, 2, 3], "indexes are positional");
}

/// A conjunct's operands carry the values behind its answer, labelled in the
/// house DSL vocabulary, and a `Compare` reports both sides at the bar.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operands_name_the_values_behind_the_answer() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let bar = bars[300];
    let mark = world.mark(Timeframe::M15, bar, MarkSide::Long).await;

    let report = world
        .explain(
            MarkSelection::Ids(vec![mark.id.clone()]),
            PATTERN_RULE,
            None,
        )
        .await
        .expect("the rule explains");
    let explained = &report.marks[0];

    // The entry: rsi(14) < 100 — both sides labelled, both valued.
    let entry = &explained.conjuncts[0];
    assert_eq!(entry.index, 0);
    assert_eq!(entry.source, "entry");
    assert_eq!(entry.operands.len(), 2, "a Compare has two operands");
    assert_eq!(
        entry.operands[0].label, "rsi:14",
        "the indicator's own read-path token, not the prose rendering"
    );
    assert_eq!(
        entry.operands[1].label, "100",
        "a constant is its own value"
    );
    assert!(
        entry.operands[0].value.is_some(),
        "a warm bar holds the RSI value: {entry:?}"
    );
    assert_eq!(entry.operands[1].value.as_deref(), Some("100"));

    // The HTF filter names its series, the read path's own spelling.
    let htf = &explained.conjuncts[2];
    assert_eq!(htf.source, "filter");
    assert_eq!(
        htf.operands[0].label, "h4:close",
        "an htf operand is prefixed the way the read path spells it"
    );

    // The pattern conjunct names the pattern.
    let pattern = &explained.conjuncts[3];
    assert_eq!(pattern.operands[0].label, "cdl:engulfing");

    // The values are the engine's own: the primary close at this bar.
    let close = series(&world.store_dir, Timeframe::M15)
        .candles
        .iter()
        .find(|candle| candle.open_time == bar)
        .map(|candle| candle.close.normalize().to_string())
        .expect("the bar is in the fixture");
    assert_eq!(
        explained.conjuncts[1].operands[0].value.as_deref(),
        Some(close.as_str()),
        "the primary close operand reads the bar's own close"
    );
}

/// A cross reports both operands on the current AND the previous bar — the
/// comparison's other half.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cross_reports_its_previous_bar_operands() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let mark = world.mark(Timeframe::M15, bars[300], MarkSide::Long).await;
    let rule = PATTERN_RULE.replace(
        r#"{ "type": "Price", "field": "Close" },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "1" }"#,
        r#"{ "type": "Price", "field": "Close" },
        "op": "Gt",
        "rhs": { "type": "Constant", "value": "1" }"#,
    );
    let _ = rule; // The literal below is the readable form.

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id.clone()]), CROSS_RULE, None)
        .await
        .expect("the rule explains");
    let conjunct = &report.marks[0].conjuncts[0];
    assert_eq!(conjunct.operands.len(), 2, "a cross has two operands");
    for operand in &conjunct.operands {
        assert!(
            operand.previous.is_some(),
            "a cross reports the previous bar too: {operand:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// AC-1 · G3 — the same truth as the backtest
// ---------------------------------------------------------------------------

/// **G3, the load-bearing case.** Run a backtest of the fixture rule on the
/// fixture store, take the bars its OWN entry gate fired on, mark them, and
/// explain: every conjunct reads `held` and `allHeld` is true.
///
/// A stored trade's signal bar: `session.rs`'s `evaluate_blocks` records
/// `PendingEntry { signal_time: primary.close_time }`, `engine.rs` copies it to
/// `entry_signal_time`, and `Trade.entry_signal_time` carries it — so the signal
/// bar is the primary candle with `close_time == trade.entry_signal_time`, and
/// the candle to mark is that bar's `open_time`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g3_every_conjunct_holds_on_a_backtests_own_entry_bars() {
    let world = world().await;
    let m15 = series(&world.store_dir, Timeframe::M15);
    let h4 = series(&world.store_dir, Timeframe::H4);

    let loaded = pulse::Migrator::v1().load(PATTERN_RULE).expect("loads");
    let compiled = compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
    let exchange = BinanceAdapter;
    let filters = exchange
        .symbol_filters(&pair())
        .expect("the pair's filters");
    let result = run_backtest(
        &compiled,
        &m15,
        Some(&h4),
        None,
        &BacktestConfig::default(),
        &filters,
        pulse::SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("the fixture rule runs");

    assert!(
        result.trades.len() >= 3,
        "the fixture rule must fire at least three times to make this case \
         non-vacuous, got {}",
        result.trades.len()
    );

    // Each trade's signal bar, mapped by close_time -> the candle to mark.
    let mut marked: Vec<String> = Vec::new();
    for trade in &result.trades {
        let bar = m15
            .candles
            .iter()
            .find(|candle| candle.close_time == trade.entry_signal_time)
            .unwrap_or_else(|| {
                panic!(
                    "the trade's signal close_time {} names no 15m candle",
                    trade.entry_signal_time
                )
            });
        let mark = world
            .mark(Timeframe::M15, bar.open_time, MarkSide::Long)
            .await;
        marked.push(mark.id);
    }

    let report = world
        .explain(MarkSelection::Ids(marked.clone()), PATTERN_RULE, None)
        .await
        .expect("the entry bars explain");

    assert_eq!(
        report.marks_explained,
        marked.len(),
        "every marked entry bar is explained"
    );
    for explained in &report.marks {
        for conjunct in &explained.conjuncts {
            assert_eq!(
                conjunct.state, "held",
                "G3: on a backtest's own entry bar every condition reads held — \
                 mark {} conjunct {} ({}) read {}",
                explained.mark.mark_id, conjunct.index, conjunct.condition, conjunct.state
            );
        }
        assert!(
            explained.all_held,
            "G3: allHeld is true on an entry bar (mark {})",
            explained.mark.mark_id
        );
    }
    assert_eq!(
        report.held_by_every_mark,
        vec![0, 1, 2, 3],
        "every conjunct is held at every entry-bar mark"
    );
}

/// **G3's other half.** On a SAMPLE of other warm bars, `allHeld` equals an
/// independent evaluation of the compiled `And` on the same context, and each
/// conjunct's state equals an independent evaluation of that conjunct — the
/// check that the flattening, the ORDER and the labels are right.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g3_sample_of_other_bars_agrees_with_an_independent_evaluation() {
    let world = world().await;
    let loaded = pulse::Migrator::v1().load(PATTERN_RULE).expect("loads");
    let compiled = compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");

    // The independent evaluator: the SAME session seam, driven by the test with
    // the exported `step_series`, reading the probe directly. It shares no code
    // with the use case's marking/gathering/ordering path.
    let m15 = series(&world.store_dir, Timeframe::M15);
    let h4 = series(&world.store_dir, Timeframe::H4);
    let exchange = BinanceAdapter;
    let filters = exchange
        .symbol_filters(&pair())
        .expect("the pair's filters");
    let mut session = pulse::EngineSession::new(
        &compiled,
        &pair(),
        pulse::SessionTimeframes {
            primary: Timeframe::M15,
            htf: Some(Timeframe::H4),
            d1: None,
        },
        BacktestConfig::default(),
        filters,
        None,
    )
    .expect("the session builds");

    // A sample of warm bars, spread across the series.
    let all_bars = m15_bars(&world.store_dir);
    let sample: Vec<i64> = all_bars
        .iter()
        .skip(250)
        .step_by(97)
        .copied()
        .take(12)
        .collect();
    assert!(sample.len() >= 6, "the sample must be a real sample");

    // The test's own readings, one per sampled bar.
    let mut expected: std::collections::BTreeMap<i64, (bool, Vec<bool>)> =
        std::collections::BTreeMap::new();
    let conjuncts: Vec<pulse::CompiledCondition> = match compiled.entry() {
        pulse::CompiledCondition::And(children) => children.clone(),
        other => vec![other.clone()],
    };
    pulse::step_series(&mut session, &m15, Some(&h4), None, |open_ms, probe| {
        if !sample.contains(&open_ms) {
            return;
        }
        let warm = probe.is_warm();
        let states = conjuncts
            .iter()
            .map(|condition| warm && probe.eval(condition))
            .collect();
        expected.insert(open_ms, (warm, states));
    })
    .expect("the test's own step succeeds");

    // Mark the sampled bars and explain them.
    let mut ids = Vec::new();
    for bar in &sample {
        let mark = world.mark(Timeframe::M15, *bar, MarkSide::Long).await;
        ids.push(mark.id);
    }
    let report = world
        .explain(MarkSelection::Ids(ids), PATTERN_RULE, None)
        .await
        .expect("the sampled bars explain");

    for explained in &report.marks {
        let bar = chrono::DateTime::parse_from_rfc3339(&explained.mark.bar_time)
            .expect("the mark's bar time is RFC 3339")
            .timestamp_millis();
        let (warm, states) = expected
            .get(&bar)
            .unwrap_or_else(|| panic!("no test reading for bar {bar}"));
        assert_eq!(
            explained.conjuncts.len(),
            states.len(),
            "the report and the independent reading agree on the count at {bar}"
        );
        if !warm {
            for conjunct in &explained.conjuncts {
                assert_eq!(
                    conjunct.state, "not_warm",
                    "a cold gate reports not_warm at {bar}"
                );
            }
            assert!(!explained.all_held);
            continue;
        }
        for (index, conjunct) in explained.conjuncts.iter().enumerate() {
            let want = if states[index] { "held" } else { "not_held" };
            assert_eq!(
                conjunct.state, want,
                "at {bar} conjunct {index} ({}) disagrees with the independent \
                 evaluation",
                conjunct.condition
            );
        }
        let want_all = explained.side_matches && states.iter().all(|held| *held);
        assert_eq!(
            explained.all_held, want_all,
            "allHeld at {bar} must equal the compiled And's own value"
        );
    }
}

/// `not_warm` before the engines warm up, with NO operand values — a partial
/// history would report numbers the gate never saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn before_warmup_every_conjunct_is_not_warm_and_carries_no_values() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    // The fixture's second bar: `bars > 0` holds, but no indicator is warm yet.
    let mark = world.mark(Timeframe::M15, bars[1], MarkSide::Long).await;

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id]), PATTERN_RULE, None)
        .await
        .expect("the early bar explains");
    let explained = &report.marks[0];
    for conjunct in &explained.conjuncts {
        assert_eq!(
            conjunct.state, "not_warm",
            "the gate is cold at the fixture's second bar: {conjunct:?}"
        );
        assert!(
            conjunct.operands.is_empty(),
            "a not_warm conjunct reports no operand values"
        );
    }
    assert!(!explained.all_held);
}

/// `sideMatches` is false for a long mark under a short-only rule, and that
/// alone makes `allHeld` false even though the condition holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_mark_under_a_short_rule_does_not_side_match() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let bar = bars[300];
    let mark = world.mark(Timeframe::M15, bar, MarkSide::Long).await;

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id.clone()]), SHORT_RULE, None)
        .await
        .expect("the short rule explains");
    let explained = &report.marks[0];
    assert!(!explained.side_matches, "a long mark under a short rule");
    assert_eq!(
        explained.conjuncts[0].state, "held",
        "the condition itself still holds"
    );
    assert!(
        !explained.all_held,
        "the side mismatch alone makes allHeld false"
    );

    // The same mark under a LONG rule of the same shape does side-match.
    let report = world
        .explain(
            MarkSelection::Ids(vec![mark.id]),
            SHORT_RULE.replace("\"short\"", "\"long\"").as_str(),
            None,
        )
        .await
        .expect("the long mirror explains");
    let explained = &report.marks[0];
    assert!(explained.side_matches);
    assert!(explained.all_held, "a held condition on a matching side");
}

/// A rule nothing satisfies reports `not_held` with the values that show why,
/// and `heldByEveryMark` is empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_that_never_holds_reports_not_held_with_its_values() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let mark = world.mark(Timeframe::M15, bars[300], MarkSide::Long).await;

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id]), NEVER_RULE, None)
        .await
        .expect("the rule explains");
    let explained = &report.marks[0];
    assert_eq!(explained.conjuncts[0].state, "not_held");
    assert!(
        explained.conjuncts[0].operands[0].value.is_some(),
        "the value behind the answer is reported"
    );
    assert!(report.held_by_every_mark.is_empty());
}

// ---------------------------------------------------------------------------
// AC-1 · the evaluation bar (Q5) and the fence (G4)
// ---------------------------------------------------------------------------

/// A 4h mark is evaluated at the FIRST 15m bar opening at its candle's close —
/// and no 4h candle that closes after that bar's open is visible: the pattern
/// values at the mark's own candle and the 4h operand's value both come from
/// closed bars only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_4h_mark_is_evaluated_at_the_15m_bar_opening_at_its_close() {
    let world = world().await;
    let (h4_open, evaluation_bar) = h4_bar_with_15m_evaluation(&world.store_dir);
    let mark = world.mark(Timeframe::H4, h4_open, MarkSide::Long).await;

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id]), PATTERN_RULE, None)
        .await
        .expect("the 4h mark explains");
    let explained = &report.marks[0];
    assert_eq!(
        explained.evaluation_bar,
        rfc3339_ms(evaluation_bar),
        "a 4h mark is evaluated at the 15m bar opening at its close, crossing as RFC 3339 UTC ms"
    );
    assert_eq!(
        explained.mark.timeframe, "4h",
        "the mark itself is unchanged"
    );

    // The independent reading at that bar, from the test's own session: the htf
    // operand must be the 4h candle that had CLOSED by then, never a later one.
    let m15 = series(&world.store_dir, Timeframe::M15);
    let h4 = series(&world.store_dir, Timeframe::H4);
    let loaded = pulse::Migrator::v1().load(PATTERN_RULE).expect("loads");
    let compiled = compile(&validate(&loaded.dsl).expect("validates")).expect("compiles");
    let exchange = BinanceAdapter;
    let filters = exchange
        .symbol_filters(&pair())
        .expect("the pair's filters");
    let mut session = pulse::EngineSession::new(
        &compiled,
        &pair(),
        pulse::SessionTimeframes {
            primary: Timeframe::M15,
            htf: Some(Timeframe::H4),
            d1: None,
        },
        BacktestConfig::default(),
        filters,
        None,
    )
    .expect("the session builds");
    let mut htf_close_at_bar: Option<rust_decimal::Decimal> = None;
    pulse::step_series(&mut session, &m15, Some(&h4), None, |open_ms, probe| {
        if open_ms != evaluation_bar {
            return;
        }
        htf_close_at_bar = probe.operand(
            &pulse::CompiledValue::Price {
                series: pulse::Series::Htf,
                field: pulse::PriceField::Close,
                lag: 0,
            },
            false,
        );
    })
    .expect("the test's own step succeeds");

    let expected_htf_close = {
        // The last 4h candle whose close_time <= the evaluation bar's open.
        let last_closed = h4
            .candles
            .iter()
            .rev()
            .find(|candle| candle.close_time <= evaluation_bar)
            .expect("a 4h candle had closed by then");
        last_closed.close.normalize().to_string()
    };
    assert_eq!(
        htf_close_at_bar.map(|value| value.normalize().to_string()),
        Some(expected_htf_close.clone()),
        "the htf operand reads the last CLOSED 4h candle's close"
    );
    assert_eq!(
        explained.conjuncts[2].operands[0].value.as_deref(),
        Some(expected_htf_close.as_str()),
        "and the report agrees with that reading"
    );
    // The next 4h candle's close is NOT what was read — the proof it was not
    // visible: the bar that opens at the evaluation bar is still forming.
    let forming = h4
        .candles
        .iter()
        .find(|candle| candle.open_time == h4_open + Timeframe::H4.duration_ms());
    if let Some(forming) = forming {
        assert_ne!(
            explained.conjuncts[2].operands[0].value.as_deref(),
            Some(forming.close.normalize().to_string().as_str()),
            "a 4h candle that has not closed is never visible"
        );
    }
}

/// A mark whose evaluation bar opens AT the holdout start is hidden and counted,
/// never explained — and no engine is stepped onto a candle at or after the
/// start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mark_whose_evaluation_bar_opens_at_the_holdout_is_hidden_and_counted() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    // A 15m mark exactly at the holdout start, and one well before it.
    let before = *bars
        .iter()
        .find(|bar| **bar < HOLDOUT_MS)
        .expect("a bar before the holdout");
    let at_start = *bars
        .iter()
        .find(|bar| **bar == HOLDOUT_MS)
        .unwrap_or_else(|| panic!("the fixture has no 15m bar at {HOLDOUT_MS}"));

    let hidden_mark = world.mark(Timeframe::M15, at_start, MarkSide::Long).await;
    let visible_mark = world.mark(Timeframe::M15, before, MarkSide::Long).await;

    let report = world
        .explain(
            MarkSelection::Ids(vec![hidden_mark.id.clone(), visible_mark.id.clone()]),
            NEVER_RULE,
            Some(HOLDOUT_MS),
        )
        .await
        .expect("the case explains");

    assert_eq!(
        report.marks_explained, 1,
        "only the mark before the holdout is explained"
    );
    assert_eq!(
        report.marks[0].mark.mark_id, visible_mark.id,
        "the explained mark is the one before the start"
    );
    let hidden = report
        .hidden_by_holdout
        .expect("a freeze is open, so the count is present");
    assert_eq!(
        hidden.holdout_start,
        rfc3339_ms(HOLDOUT_MS),
        "the holdout start crosses as RFC 3339 UTC ms, as `list_marks` renders it"
    );
    assert!(
        hidden.count >= 1,
        "the mark whose evaluation bar opens at the holdout start is counted: {hidden:?}"
    );
    assert!(
        !report
            .marks
            .iter()
            .any(|mark| mark.mark.mark_id == hidden_mark.id),
        "the hidden mark is never explained"
    );
}

/// A `4h` mark whose candle CLOSES exactly at the holdout start has its
/// evaluation bar opening exactly there: the 15m bar is cut, so the mark is
/// hidden. G4 forces it — the bar it would be explained at is a holdout bar.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_4h_mark_closing_exactly_at_the_holdout_start_is_hidden() {
    let world = world().await;
    let h4 = series(&world.store_dir, Timeframe::H4);
    let closing_at_start: Vec<i64> = h4
        .candles
        .iter()
        .filter(|candle| candle.open_time + Timeframe::H4.duration_ms() == HOLDOUT_MS)
        .map(|candle| candle.open_time)
        .collect();
    let Some(&bar_open) = closing_at_start.first() else {
        panic!(
            "the fixture has no 4h candle closing exactly at {HOLDOUT_MS} — this case \
             needs one; the constant is the fixture's own chart_read boundary"
        );
    };
    let mark = world.mark(Timeframe::H4, bar_open, MarkSide::Long).await;

    let report = world
        .explain(
            MarkSelection::Ids(vec![mark.id.clone()]),
            NEVER_RULE,
            Some(HOLDOUT_MS),
        )
        .await
        .expect("the case explains");
    assert_eq!(
        report.marks_explained, 0,
        "the mark's evaluation bar opens exactly at the holdout start"
    );
    assert_eq!(
        report.hidden_by_holdout.expect("a freeze is open").count,
        1,
        "and it is counted as hidden"
    );
}

/// With no freeze open the use case is inert on the fence: the same mark that is
/// hidden above is explained, and `hiddenByHoldout` is null.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_freeze_the_fence_hides_nothing() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let at_start = *bars
        .iter()
        .find(|bar| **bar == HOLDOUT_MS)
        .expect("the fixture has a 15m bar at the holdout start");
    let mark = world.mark(Timeframe::M15, at_start, MarkSide::Long).await;

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id]), NEVER_RULE, None)
        .await
        .expect("the case explains");
    assert_eq!(report.marks_explained, 1, "no freeze: nothing is hidden");
    assert!(
        report.hidden_by_holdout.is_none(),
        "no freeze open means no count"
    );
}

// ---------------------------------------------------------------------------
// AC-1 · the 17 patterns
// ---------------------------------------------------------------------------

/// `patterns` lists all 17 values in catalog order, named `cdl:<snake_case>`,
/// on the MARK's own candle and timeframe — and they equal what an independent
/// pattern engine reads at that bar.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn patterns_list_seventeen_values_at_the_mark_s_own_candle() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let bar = bars[400];
    let mark = world.mark(Timeframe::M15, bar, MarkSide::Long).await;

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id]), NEVER_RULE, None)
        .await
        .expect("the case explains");
    let patterns = &report.marks[0].patterns;
    assert_eq!(patterns.len(), 17, "all 17 named patterns are listed");

    // The independent reading: the same 17 specs over the same bars, stepped to
    // the mark's own candle.
    let m15 = series(&world.store_dir, Timeframe::M15);
    let specs: Vec<pulse::IndicatorSpec> = patterns
        .iter()
        .map(|pattern| pulse::IndicatorSpec::CandlePattern {
            pattern: pattern_name(&pattern.name),
        })
        .collect();
    let mut engine = pulse::IndicatorEngine::from_specs(&specs).expect("the pattern engine builds");
    for candle in &m15.candles {
        engine.step(candle);
        if candle.open_time != bar {
            continue;
        }
        for (index, pattern) in patterns.iter().enumerate() {
            let value = engine
                .current(&pulse::CompiledValue::Indicator {
                    series: pulse::Series::Primary,
                    spec: specs[index].clone(),
                    lag: 0,
                })
                .map(|value| value.normalize().to_string());
            assert_eq!(
                pattern.value, value,
                "pattern {} at the mark's own candle",
                pattern.name
            );
        }
    }

    // The names are the read path's own `cdl:<snake_case>` spelling, in order.
    let names: Vec<&str> = patterns
        .iter()
        .map(|pattern| pattern.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "cdl:doji",
            "cdl:dragonfly_doji",
            "cdl:gravestone_doji",
            "cdl:hammer",
            "cdl:inverted_hammer",
            "cdl:hanging_man",
            "cdl:shooting_star",
            "cdl:marubozu",
            "cdl:spinning_top",
            "cdl:engulfing",
            "cdl:harami",
            "cdl:piercing",
            "cdl:dark_cloud_cover",
            "cdl:morning_star",
            "cdl:evening_star",
            "cdl:three_white_soldiers",
            "cdl:three_black_crows",
        ],
        "the 17 names in catalog order"
    );
    // The pattern values are TA-Lib's integers, so every one is an integer
    // string or null.
    for pattern in patterns {
        if let Some(value) = &pattern.value {
            assert!(
                value.parse::<i64>().is_ok(),
                "pattern {} reads an integer, got {value:?}",
                pattern.name
            );
        }
    }
}

/// The `cdl:<snake_case>` name back to its DSL pattern value — the same
/// vocabulary `parse_indicator_specs` accepts, read from the report's own name.
fn pattern_name(name: &str) -> pulse::CandlePatternName {
    use pulse::CandlePatternName as P;
    // The report's `name` is the READ PATH's token, `cdl:<snake_case>`.
    let name = name.strip_prefix("cdl:").unwrap_or(name);
    match name {
        "doji" => P::Doji,
        "dragonfly_doji" => P::DragonflyDoji,
        "gravestone_doji" => P::GravestoneDoji,
        "hammer" => P::Hammer,
        "inverted_hammer" => P::InvertedHammer,
        "hanging_man" => P::HangingMan,
        "shooting_star" => P::ShootingStar,
        "marubozu" => P::Marubozu,
        "spinning_top" => P::SpinningTop,
        "engulfing" => P::Engulfing,
        "harami" => P::Harami,
        "piercing" => P::Piercing,
        "dark_cloud_cover" => P::DarkCloudCover,
        "morning_star" => P::MorningStar,
        "evening_star" => P::EveningStar,
        "three_white_soldiers" => P::ThreeWhiteSoldiers,
        "three_black_crows" => P::ThreeBlackCrows,
        other => panic!("unknown pattern name {other}"),
    }
}

// ---------------------------------------------------------------------------
// AC-1 · refusals
// ---------------------------------------------------------------------------

/// An invalid DSL is refused with the validator's own errors.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_rule_is_refused_with_the_validators_errors() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let mark = world.mark(Timeframe::M15, bars[300], MarkSide::Long).await;

    let error = world
        .explain(MarkSelection::Ids(vec![mark.id]), INVALID_RULE, None)
        .await
        .expect_err("the invalid rule is refused");
    match error {
        ExplainError::Dsl { path, message } => {
            assert_eq!(path, "dsl");
            assert!(
                message.contains("risk_per_trade_pct"),
                "the validator's field path is carried: {message}"
            );
        }
        other => panic!("expected a DSL refusal, got {other:?}"),
    }
}

/// A rule the ENGINE refuses (no stop loss) is refused with the engine's own
/// reason — a rule the backtest cannot run has no backtest truth to report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_the_engine_refuses_is_refused_for_the_same_reason() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let mark = world.mark(Timeframe::M15, bars[300], MarkSide::Long).await;

    let error = world
        .explain(MarkSelection::Ids(vec![mark.id]), NO_STOP_RULE, None)
        .await
        .expect_err("the stop-less rule is refused");
    let text = error.to_string();
    assert!(
        text.to_lowercase().contains("stop"),
        "the refusal names the missing stop: {text}"
    );
}

/// A rule that reads a series the store has no snapshot for is refused, naming
/// the series.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rule_reading_a_missing_series_is_refused() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let mark = world.mark(Timeframe::M15, bars[300], MarkSide::Long).await;

    let error = world
        .explain(MarkSelection::Ids(vec![mark.id]), D1_RULE, None)
        .await
        .expect_err("the d1 rule is refused on a fixture with no daily snapshot");
    let text = error.to_string();
    assert!(
        text.contains("1d"),
        "the refusal names the missing series: {text}"
    );
}

/// An unknown mark id is a typed refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_mark_id_is_refused() {
    let world = world().await;
    let error = world
        .explain(
            MarkSelection::Ids(vec!["no-such-mark".to_owned()]),
            NEVER_RULE,
            None,
        )
        .await
        .expect_err("an unknown id is refused");
    assert!(
        matches!(error, ExplainError::Mark(_)),
        "the mark use case's own not-found refusal: {error:?}"
    );
}

/// An empty id list is a field refusal naming `markIds`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_id_selection_is_a_field_refusal() {
    let world = world().await;
    let error = world
        .explain(MarkSelection::Ids(Vec::new()), NEVER_RULE, None)
        .await
        .expect_err("an empty selection is refused");
    match error {
        ExplainError::Field { path, .. } => assert_eq!(path, "markIds"),
        other => panic!("expected a field refusal, got {other:?}"),
    }
}

/// A mark whose evaluation bar has no 15m candle is reported under `refusals`
/// as `no_evaluation_bar` — never silently dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mark_with_no_evaluation_bar_is_reported_not_dropped() {
    let world = world().await;
    let m15 = series(&world.store_dir, Timeframe::M15);
    let last = m15.candles.last().expect("the fixture is non-empty");
    // A 4h mark on the LAST 4h candle: its close is past the 15m snapshot's end,
    // so no 15m bar opens there.
    let h4 = series(&world.store_dir, Timeframe::H4);
    let last_h4 = h4.candles.last().expect("the fixture has 4h bars");
    let mark = world
        .mark(Timeframe::H4, last_h4.open_time, MarkSide::Long)
        .await;

    let report = world
        .explain(MarkSelection::Ids(vec![mark.id.clone()]), NEVER_RULE, None)
        .await
        .expect("the read completes");

    let _ = last;
    if report.marks_explained == 0 {
        let refusal = report
            .refusals
            .iter()
            .find(|refusal| refusal.mark_id == mark.id)
            .expect("the mark is reported as a refusal");
        assert_eq!(refusal.reason, "no_evaluation_bar");
    }
}

// ---------------------------------------------------------------------------
// AC-1 · inline DSL and version_id agree; the MCP tool over an agent token
// ---------------------------------------------------------------------------

/// The MCP world: a migrated db, a copied fixture store, and one live stdio
/// client (`pulse mcp --local --agent-name Test-Agent`).
struct McpWorld {
    _tmp_db: TempDir,
    _tmp_store: TempDir,
    client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    store_dir: PathBuf,
}

async fn mcp_world() -> McpWorld {
    let tmp_db = TempDir::new().expect("tempdir");
    let (db_path, _db) = support::mcp::migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().expect("tempdir");
    let store_dir = fixture_store(&tmp_store);
    let client = support::mcp::spawn_client(&db_path, &store_dir).await;
    McpWorld {
        _tmp_db: tmp_db,
        _tmp_store: tmp_store,
        client,
        store_dir,
    }
}

/// The same rule explains identically from an inline document and from a stored
/// version — the two rule sources share one validation path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_dsl_and_a_stored_version_give_the_same_answer() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let bar = bars[300];
    let mark = world.mark(Timeframe::M15, bar, MarkSide::Long).await;

    let inline = world
        .explain(
            MarkSelection::Ids(vec![mark.id.clone()]),
            PATTERN_RULE,
            None,
        )
        .await
        .expect("the inline rule explains");

    // The same rule, submitted through the app's own submit path and then
    // explained by id. The DSL is seeded directly on the store (the submit path
    // is `submit_strategy_version`'s contract).
    let stored = store_rule(&world.db, PATTERN_RULE).await;
    let world_marks = SqliteExampleMarkRepo::with_deps(world.db.pool().clone(), SystemClock);
    let candles = store(&world.store_dir);
    let strategies = pulse::SqliteStrategyRepo::new(world.db.pool().clone());
    let exchange = BinanceAdapter;
    let by_version = explain_marks(
        &world_marks,
        &candles,
        &strategies,
        &exchange,
        None,
        ExplainRequest {
            marks: MarkSelection::Ids(vec![mark.id.clone()]),
            rule: RuleRequest::Version(stored),
        },
    )
    .await
    .expect("the stored rule explains");

    assert_eq!(
        serde_json::to_value(&inline).expect("the inline report serializes"),
        serde_json::to_value(&by_version).expect("the version report serializes"),
        "an inline rule and the same rule stored must give one answer"
    );
}

/// Persist one rule as a strategy version, returning its id — the store-side
/// fixture for the `version_id` rule source.
async fn store_rule(db: &Db, rule: &str) -> pulse::VersionId {
    let repo = pulse::SqliteStrategyRepo::new(db.pool().clone());
    let strategy = repo
        .create_strategy("w2 explain fixture", None, &[])
        .await
        .expect("the strategy row");
    let version = repo
        .create_version(pulse::NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: rule.to_owned(),
            created_by: pulse::CreatedBy::Human,
            creating_llm_call_ids: Vec::new(),
        })
        .await
        .expect("the version row");
    version.id
}

/// An unknown version id is a typed refusal naming `version_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_version_id_is_refused() {
    let world = world().await;
    let bars = m15_bars(&world.store_dir);
    let mark = world.mark(Timeframe::M15, bars[300], MarkSide::Long).await;
    let marks = SqliteExampleMarkRepo::with_deps(world.db.pool().clone(), SystemClock);
    let candles = store(&world.store_dir);
    let strategies = pulse::SqliteStrategyRepo::new(world.db.pool().clone());
    let exchange = BinanceAdapter;
    let error = explain_marks(
        &marks,
        &candles,
        &strategies,
        &exchange,
        None,
        ExplainRequest {
            marks: MarkSelection::Ids(vec![mark.id]),
            rule: RuleRequest::Version(pulse::VersionId::new("no-such-version")),
        },
    )
    .await
    .expect_err("an unknown version id is refused");
    assert!(
        matches!(error, ExplainError::VersionNotFound(_)),
        "the version refusal is typed: {error:?}"
    );
}

/// The tool works over an agent token on `/mcp` (stdio here, the other mount):
/// a mark written through `write_mark` is explained through `explain_marks`,
/// and the response carries the spec's field names in the surface's own casing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tool_explains_over_the_agent_surface() {
    let world = mcp_world().await;
    let bars = m15_bars(&world.store_dir);
    let bar = bars[300];

    let written = support::mcp::call(
        &world.client,
        "write_mark",
        serde_json::json!({
            "pair": "BTCUSDT",
            "timeframe": "15m",
            "bar_time": rfc3339(bar),
            "side": "long",
            "setup": "explain-tool",
            "note": "the tool's case",
        }),
    )
    .await;
    let mark_id = written["mark_id"]
        .as_str()
        .expect("the written mark's id")
        .to_owned();

    let explained = support::mcp::call(
        &world.client,
        "explain_marks",
        serde_json::json!({
            "mark_ids": [mark_id],
            "rule": rule_json(PATTERN_RULE),
        }),
    )
    .await;

    // The spec's field names, snake_case, matching `list_marks`/`write_mark`.
    assert_eq!(explained["marks_explained"], 1, "one mark: {explained}");
    assert_eq!(
        explained["hidden_by_holdout"],
        Value::Null,
        "no freeze is open on this world"
    );
    let mark = &explained["marks"][0];
    assert_eq!(mark["mark"]["mark_id"].as_str(), Some(mark_id.as_str()));
    assert_eq!(mark["mark"]["timeframe"], "15m");
    assert_eq!(mark["mark"]["writer"]["kind"], "agent");
    assert_eq!(mark["mark"]["writer"]["self_reported"], true);
    assert!(
        mark["evaluation_bar"].as_str().is_some(),
        "the evaluation bar crosses the MCP surface as an RFC 3339 string: {mark}"
    );
    assert!(
        mark["conjuncts"].as_array().is_some_and(|c| c.len() == 4),
        "the four conjuncts are reported: {mark}"
    );
    assert_eq!(mark["conjuncts"][0]["state"], "held");
    assert!(mark["all_held"].as_bool().is_some());
    assert!(
        mark["patterns"].as_array().is_some_and(|p| p.len() == 17),
        "the 17 patterns are reported: {mark}"
    );
    assert!(
        explained["held_by_every_mark"]
            .as_array()
            .is_some_and(|held| held.len() == 4),
        "every conjunct held at the one mark: {explained}"
    );

    // The surface refuses a malformed request by argument name.
    let refused = support::mcp::call_err(
        &world.client,
        "explain_marks",
        serde_json::json!({ "mark_ids": [mark_id], "setup": "x", "rule": rule_json(NEVER_RULE) }),
    )
    .await;
    assert_eq!(
        refused["field"], "mark_ids",
        "the shape refusal names its argument"
    );
}
