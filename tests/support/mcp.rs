//! The `pulse mcp` integration harness (extracted from `tests/mcp_stdio.rs`,
//! r2.s1.w3).
//!
//! The child process is spawned through rmcp's `TokioChildProcess` with a
//! seeded `pulse.db` and a *copy* of the committed candle fixture as
//! `--data-dir`. Every helper is shared by `mcp_stdio.rs` (read tools) and
//! `mcp_write.rs` (submit + windowed backtest).

#![allow(clippy::unwrap_used, clippy::expect_used)]
// Shared harness: every `tests/*.rs` consumer compiles this module separately,
// so a helper only `mcp_stdio.rs` uses reads as dead code inside `mcp_write.rs`
// (and vice versa). Named-lint allow, scoped to this module — the alternative
// is a copy of the harness per suite, which is exactly what the extraction
// exists to prevent.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use pulse::{
    BacktestConfig, BacktestInputs, BacktestRequest, BacktestResult, BacktestRunId,
    BacktestRunRepository, BinanceAdapter, CandleStore, CandleWindow, CreatedBy, DataVersion, Db,
    Direction, EngineFingerprint, EquityCurve, ExitReason, Fill, FoldScheme, FoldVerdict,
    FundingConfig, MIGRATOR, NewVersion, Pair, Regime, RegimeBreakdown, RunVerdict,
    SkippedEntryCounts, SnapshotSelection, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyId,
    StrategyRepository, SummaryStats, Timeframe, Trade, TradeSource, VerdictRule, VersionId,
    WalkForwardFoldDraft, WalkForwardRunDraft, fold_windows, run_version_backtest,
};
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::child_process::TokioChildProcess;
use rust_decimal::Decimal;
use serde_json::Value;
use tempfile::TempDir;
use tokio::process::Command;

/// The committed candle fixture (`BTCUSDT` 15m + 4h snapshots).
pub const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The same minimal, valid DSL the other CLI suites seed — compiles, so the
/// stored `dsl` is a genuine current-schema object.
pub const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (mcp)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#;

pub fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Recursively copy a directory tree (the fixture → tempdir, so exports and a
/// hypothetical HEAD advance never touch the committed store).
pub fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

pub async fn migrated_db(tmp: &TempDir) -> (PathBuf, Db) {
    let db_path = tmp.path().join("pulse.db");
    let db = Db::with_path(&db_path).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    (db_path, db)
}

/// One real trade: its own fields are ordinary (the aggregates must see them).
pub fn seeded_trade() -> Trade {
    let entry = Decimal::new(90_000, 0);
    let exit = Decimal::new(91_100, 0);
    Trade {
        direction: Direction::Long,
        qty: Decimal::new(1, 0),
        entry_price: entry,
        exit_price: exit,
        entry_signal_time: 1_699_999_100_000,
        entry_fill_time: 1_700_000_000_000,
        exit_signal_time: 1_700_000_000_000,
        exit_fill_time: 1_700_000_900_000,
        fills: vec![
            Fill {
                price: entry,
                qty: Decimal::new(1, 0),
                time_ms: 1_700_000_000_000,
                fee: Decimal::new(1, 2),
            },
            Fill {
                price: exit,
                qty: Decimal::new(1, 0),
                time_ms: 1_700_000_900_000,
                fee: Decimal::new(1, 2),
            },
        ],
        fees_total: Decimal::new(2, 2),
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        realized_pnl: Decimal::new(1100, 0),
        realized_r: Decimal::new(2, 0),
        mfe_r: Decimal::new(25, 1),
        mae_r: Decimal::new(-5, 1),
        exit_reason: ExitReason::TakeProfit,
        source: TradeSource::Backtest,
        regime: Regime::TrendingUp,
        stop_price: Some(Decimal::new(89_450, 0)),
    }
}

/// The inputs the seeded run records — `v-primary` is the `data_version` the
/// assertions read back off the wire.
pub fn seeded_inputs() -> BacktestInputs {
    BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new("v-primary"),
        },
        htf: Some(SnapshotSelection {
            timeframe: Timeframe::H4,
            data_version: DataVersion::new("v-htf"),
        }),
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: FundingConfig::SnapshotRates,
        window: None,
        lead_in_from_ms: None,
    }
}

/// Everything the assertions need off the seed: `(parent version id, child
/// version id, run id)`.
pub type Seed = (VersionId, VersionId, BacktestRunId);

/// Seed one strategy with a parent→child version pair (NO run) — the version
/// tree every suite needs.
pub async fn seed_versions(db: &Db) -> (VersionId, VersionId) {
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("MCP demo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let parent = strategies
        .create_version(NewVersion {
            strategy_id: StrategyId::new(strategy.id.as_str().to_owned()),
            parent_version_id: None,
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create parent version");
    let child = strategies
        .create_version(NewVersion {
            strategy_id: StrategyId::new(strategy.id.as_str().to_owned()),
            parent_version_id: Some(parent.id.clone()),
            dsl_json: MINIMAL_DSL.replace("(mcp)", "(mcp child)"),
            created_by: CreatedBy::ComposerLlm,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create child version");
    (parent.id, child.id)
}

/// Seed one run carrying CALLER-SUPPLIED `inputs` — for states a real run can
/// no longer produce (e.g. round-1 fix F1's equal-timeframe `inputs.htf`, a
/// row only a pre-fix binary or an out-of-band writer could leave). The row's
/// trade and result are the ordinary seeded ones; only the provenance block
/// differs.
pub async fn seed_run_with_inputs(
    db: &Db,
    version_id: &VersionId,
    inputs: &BacktestInputs,
) -> BacktestRunId {
    let trade = seeded_trade();
    let summary = SummaryStats::from_trades(
        std::slice::from_ref(&trade),
        trade.realized_pnl,
        trade.fees_total,
        trade.funding_total,
        &EquityCurve::default(),
    );
    let result = BacktestResult {
        trades: vec![trade.clone()],
        net_pnl: trade.realized_pnl,
        fees_total: trade.fees_total,
        funding_total: trade.funding_total,
        slippage_total: trade.slippage_total,
        regime_breakdown: RegimeBreakdown::new(),
        skipped_entries: SkippedEntryCounts::new(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::current(),
        summary,
        equity_curve: EquityCurve::default(),
    };
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    runs.save_run(
        version_id,
        inputs,
        &result,
        &result.summary,
        Decimal::new(10_000, 0),
    )
    .await
    .expect("save seeded run")
}

/// Seed one strategy with a parent→child version pair, and one run (one trade)
/// against the child, all through the repository layer the server reads.
pub async fn seed(db: &Db) -> Seed {
    let (parent_id, child_id) = seed_versions(db).await;
    let run_id = seed_run_with_inputs(db, &child_id, &seeded_inputs()).await;
    (parent_id, child_id, run_id)
}

/// A synthetic `k=2` walk-forward draft whose recorded verdict is `pass`
/// (r2.s3.w4). The fixture cannot yield a passing verdict honestly, so the
/// certification seam — the pointer plus the recorded `pass` column — is what
/// a synthetic draft exercises: `save_walk_forward_run` persists the recorded
/// verdict, and the derived `certified` reads it back through the JOIN.
/// One trade whose `realized_r` is `r`, with the rest of its numbers CONSISTENT
/// with that R-multiple: a long entered at 90 000 with its stop 1 000 below
/// closes at `90 000 + 1 000·r`, so `(exit − entry) / stop_distance == r` exactly
/// and the trade does not contradict the verdict it feeds.
///
/// `idx` staggers the timestamps so a fold's trades are distinct bars.
pub fn seeded_trade_at_r(idx: i64, r: Decimal) -> Trade {
    let entry = Decimal::new(90_000, 0);
    let stop = Decimal::new(89_000, 0);
    let stop_distance = entry - stop;
    let exit = entry + r * stop_distance;
    let qty = Decimal::ONE;
    let open_ms = 1_700_000_000_000 + idx * 900_000;
    let close_ms = open_ms + 900_000;
    let fees = Decimal::new(2, 2);
    Trade {
        direction: Direction::Long,
        qty,
        entry_price: entry,
        exit_price: exit,
        entry_signal_time: open_ms,
        entry_fill_time: open_ms,
        exit_signal_time: close_ms,
        exit_fill_time: close_ms,
        fills: vec![
            Fill {
                price: entry,
                qty,
                time_ms: open_ms,
                fee: Decimal::new(1, 2),
            },
            Fill {
                price: exit,
                qty,
                time_ms: close_ms,
                fee: Decimal::new(1, 2),
            },
        ],
        fees_total: fees,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        realized_pnl: (exit - entry) * qty - fees,
        realized_r: r,
        mfe_r: if r > Decimal::ZERO { r } else { Decimal::ZERO },
        mae_r: if r < Decimal::ZERO { r } else { Decimal::ZERO },
        exit_reason: if r > Decimal::ZERO {
            ExitReason::TakeProfit
        } else {
            ExitReason::StopLoss
        },
        source: TradeSource::Backtest,
        regime: Regime::TrendingUp,
        stop_price: Some(stop),
    }
}

/// One fold's trades: `count` bars whose R-multiples alternate `lo`/`hi` around
/// their mean, so the `wf-v1` bound is computed over a real standard error rather
/// than a zero one. `fold_index` staggers the timestamps between folds.
pub fn seeded_fold_trades(count: usize, lo: Decimal, hi: Decimal, fold_index: usize) -> Vec<Trade> {
    (0..count)
        .map(|i| {
            let r = if i % 2 == 0 { lo } else { hi };
            seeded_trade_at_r(i64::try_from(fold_index * count + i).unwrap_or(i64::MAX), r)
        })
        .collect()
}

pub fn seeded_walk_forward_draft(pass: bool) -> WalkForwardRunDraft {
    seeded_walk_forward_draft_k(pass, 2)
}

/// The same draft at any fold count `k` — one coherent fold per scheme fold, so
/// an out-of-range `k` can be exercised with the folds a real caller would
/// supply. The scheme is built as the raw public variant, which is what lets a
/// test hand the gate one the domain would refuse to construct (R5).
pub fn seeded_walk_forward_draft_k(pass: bool, k: u8) -> WalkForwardRunDraft {
    let span = CandleWindow::new(1_735_702_200_000, 1_738_000_000_000).unwrap();
    // The R-multiples the folds carry: a genuinely HOLDING fold needs `n >= 20`
    // trades and a positive bound, which one trade cannot produce — so the
    // fixture says it with twenty, alternating around a positive mean (a real
    // standard error, not a zero one). Both folds take the same pattern, so the
    // pooled series holds with them.
    let (lo, hi) = if pass {
        (Decimal::new(5, 1), Decimal::new(15, 1))
    } else {
        (Decimal::new(-5, 1), Decimal::new(-15, 1))
    };
    let folds: Vec<WalkForwardFoldDraft> = fold_windows(&span, k)
        .iter()
        .enumerate()
        .map(|(i, window)| {
            let trades = seeded_fold_trades(20, lo, hi, i);
            let rs: Vec<Decimal> = trades.iter().map(|t| t.realized_r).collect();
            // The verdict is DERIVED from the trades, never asserted alongside
            // them (R1): the write gate recomputes exactly this from
            // `result.trades`, so a fixture that fabricated the verdict could not
            // persist at all.
            let verdict = FoldVerdict::from_rs(&rs);
            let net_pnl: Decimal = trades.iter().map(|t| t.realized_pnl).sum();
            let fees_total: Decimal = trades.iter().map(|t| t.fees_total).sum();
            let funding_total: Decimal = trades.iter().map(|t| t.funding_total).sum();
            let slippage_total: Decimal = trades.iter().map(|t| t.slippage_total).sum();
            let summary = SummaryStats::from_trades(
                &trades,
                net_pnl,
                fees_total,
                funding_total,
                &EquityCurve::default(),
            );
            let mut inputs = seeded_inputs();
            inputs.window = Some(window.clone());
            inputs.lead_in_from_ms = Some(window.from_ms);
            WalkForwardFoldDraft {
                index: u8::try_from(i).unwrap(),
                window: window.clone(),
                verdict,
                inputs,
                result: BacktestResult {
                    trades,
                    net_pnl,
                    fees_total,
                    funding_total,
                    slippage_total,
                    regime_breakdown: RegimeBreakdown::new(),
                    skipped_entries: SkippedEntryCounts::new(),
                    open_position: None,
                    engine_fingerprint: EngineFingerprint::current(),
                    summary: summary.clone(),
                    equity_curve: EquityCurve::default(),
                },
                summary,
                starting_equity: Decimal::new(10_000, 0),
            }
        })
        .collect();
    let pooled_rs: Vec<Decimal> = folds
        .iter()
        .flat_map(|f| f.result.trades.iter().map(|t| t.realized_r))
        .collect();
    let fold_verdicts: Vec<FoldVerdict> = folds.iter().map(|f| f.verdict.clone()).collect();
    WalkForwardRunDraft {
        scheme: FoldScheme::RollingOos { k },
        rule: VerdictRule::WfV1,
        span,
        from_defaulted: false,
        engine_fingerprint: EngineFingerprint::current().as_str().to_owned(),
        verdict: RunVerdict::assess(&fold_verdicts, &pooled_rs),
        folds,
    }
}

/// Run a REAL backtest on `version_id` over the copied fixture store, in-test,
/// through the same application use case the server calls (r2.s1.w3). The run
/// this persists records the fixture's REAL `data_version`s — the resolver's
/// snapshot pins are only meaningful if the versions they name exist. Returns
/// the persisted run id.
pub async fn seed_real_run(db: &Db, store_dir: &Path, version_id: &VersionId) -> BacktestRunId {
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let store = CandleStore::with_base_dir(store_dir.to_path_buf());
    run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: version_id.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect("the seeded real run completes over the fixture")
    .run
    .id
}

/// [`seed_real_run`] without the HTF snapshot: the persisted run records
/// `inputs.htf = None`, which is what leaves a child's resolved request without
/// an inherited HTF timeframe — the resolver's `Some(H4)` fallback trigger for
/// an htf-needing child (r2.s2 round-3 fix).
pub async fn seed_real_run_primary_only(
    db: &Db,
    store_dir: &Path,
    version_id: &VersionId,
) -> BacktestRunId {
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let store = CandleStore::with_base_dir(store_dir.to_path_buf());
    run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: version_id.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None,
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect("the seeded M15-only run completes over the fixture")
    .run
    .id
}

/// `pulse mcp --db <db> --data-dir <store> --agent-name Test-Agent` over rmcp's
/// child-process transport. The `()` handler is rmcp's minimal client.
pub async fn spawn_client(
    db_path: &Path,
    data_dir: &Path,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pulse"));
    command
        .arg("mcp")
        .arg("--db")
        .arg(db_path)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--agent-name")
        .arg("Test-Agent");
    let transport = TokioChildProcess::builder(command)
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn pulse mcp")
        .0;
    ().serve(transport).await.expect("initialize handshake")
}

pub fn arguments(value: &Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("arguments object").clone()
}

/// Call a tool and return its `structuredContent`; fails the test on a wire or
/// tool-level error instead of letting a `None` slide through. Negative paths
/// must call `client.call_tool` DIRECTLY (this helper asserts success).
///
/// The returned content is asserted a JSON OBJECT — the #183 invariant: the
/// spec types `structuredContent` as an object and Claude Code's validator
/// refuses a bare array. Every happy-path call in the suite funnels through
/// here, so the rule is enforced at the choke point, not per test.
pub async fn call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    args: Value,
) -> Value {
    let result = client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments(&args)))
        .await
        .unwrap_or_else(|e| panic!("tools/call {name} transport error: {e}"));
    assert!(
        result.is_error != Some(true),
        "tools/call {name} returned is_error: {:?}",
        result.content
    );
    let structured = result
        .structured_content
        .unwrap_or_else(|| panic!("tools/call {name} carried no structuredContent"));
    assert!(
        structured.is_object(),
        "tools/call {name} structuredContent must be a JSON object: {structured}"
    );
    structured
}

/// Call a tool EXPECTING a tool-level error and return its structured error
/// content — the negative-path counterpart of [`call`]. `structured_error`
/// always populates `structured_content`, so the wire shape is read back
/// exactly as the tool built it.
pub async fn call_err(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    args: Value,
) -> Value {
    let result = client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments(&args)))
        .await
        .unwrap_or_else(|e| panic!("tools/call {name} transport error: {e}"));
    assert!(
        result.is_error == Some(true),
        "tools/call {name} should have failed, got: {:?}",
        result.content
    );
    let structured = result
        .structured_content
        .unwrap_or_else(|| panic!("tools/call {name} error carried no structuredContent"));
    assert!(
        structured.is_object(),
        "tools/call {name} structured error content must be a JSON object: {structured}"
    );
    structured
}

/// The whole seeded fixture: db, copied store, and their tempdir guards.
pub struct Fixture {
    pub _tmp_db: TempDir,
    pub _tmp_store: TempDir,
    pub db: Db,
    pub db_path: PathBuf,
    pub store_dir: PathBuf,
    pub seed: Seed,
}

/// Seed the version tree + one synthetic run on the CHILD (the w2 shape).
pub async fn seeded_fixture() -> (Fixture, rmcp::service::RunningService<rmcp::RoleClient, ()>) {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp_db).await;
    let seed = seed(&db).await;

    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);

    let client = spawn_client(&db_path, &store_dir).await;
    (
        Fixture {
            _tmp_db: tmp_db,
            _tmp_store: tmp_store,
            db,
            db_path,
            store_dir,
            seed,
        },
        client,
    )
}
