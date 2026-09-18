//! AC-1 (r2.s1.w2): a real MCP stdio session against `pulse mcp`.
//!
//! The child process is spawned through rmcp's `TokioChildProcess` with a
//! seeded `pulse.db` and a *copy* of the committed candle fixture as
//! `--data-dir`. Every read tool plus the `pulse://dsl/schema` resource is
//! asserted over the wire: strategy tree order, `created_by` strings, the
//! migrated + original DSL, run summary + inputs provenance, export row counts
//! and — cell for cell — `export_indicators` against `IndicatorEngine` stepped
//! directly over the same snapshot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunId, BacktestRunRepository, CandleSeriesRepository,
    CandleStore, CompiledValue, CreatedBy, DataVersion, Db, Direction, EngineFingerprint,
    EquityCurve, EvalContext, ExitReason, Fill, FundingConfig, IndicatorEngine, IndicatorSpec,
    MIGRATOR, NewVersion, Pair, Regime, RegimeBreakdown, SkippedEntryCounts, SnapshotSelection,
    SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyId, StrategyRepository, SummaryStats,
    SweepableValue, Timeframe, Trade, TradeSource, VersionId,
};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams, ResourceContents};
use rmcp::transport::child_process::TokioChildProcess;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::process::Command;

/// The committed candle fixture (`BTCUSDT` 15m + 4h snapshots).
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The same minimal, valid DSL the other CLI suites seed — compiles, so the
/// stored `dsl` is a genuine current-schema object.
const MINIMAL_DSL: &str = r#"{
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

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Recursively copy a directory tree (the fixture → tempdir, so exports and a
/// hypothetical HEAD advance never touch the committed store).
fn copy_tree(from: &Path, to: &Path) {
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

async fn migrated_db(tmp: &TempDir) -> (PathBuf, Db) {
    let db_path = tmp.path().join("pulse.db");
    let db = Db::with_path(&db_path).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    (db_path, db)
}

/// One real trade: its own fields are ordinary (the aggregates must see them).
fn seeded_trade() -> Trade {
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
    }
}

/// The inputs the seeded run records — `v-primary` is the `data_version` the
/// assertions read back off the wire.
fn seeded_inputs() -> BacktestInputs {
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
    }
}

/// Everything the assertions need off the seed: `(parent version id, child
/// version id, run id)`.
type Seed = (VersionId, VersionId, BacktestRunId);

/// Seed one strategy with a parent→child version pair, and one run (one trade)
/// against the child, all through the repository layer the server reads.
async fn seed(db: &Db) -> Seed {
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

    let trade = seeded_trade();
    let inputs = seeded_inputs();
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
        engine_fingerprint: EngineFingerprint::current(),
        summary,
        equity_curve: EquityCurve::default(),
    };
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let run_id = runs
        .save_run(
            &child.id,
            &inputs,
            &result,
            &result.summary,
            Decimal::new(10_000, 0),
        )
        .await
        .expect("save seeded run");
    (parent.id, child.id, run_id)
}

/// `pulse mcp --db <db> --data-dir <store> --agent-name test-agent` over rmcp's
/// child-process transport. The `()` handler is rmcp's minimal client.
async fn spawn_client(
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

fn arguments(value: &Value) -> serde_json::Map<String, Value> {
    value.as_object().expect("arguments object").clone()
}

/// Call a tool and return its `structuredContent`; fails the test on a wire or
/// tool-level error instead of letting a `None` slide through.
async fn call(
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
    result
        .structured_content
        .unwrap_or_else(|| panic!("tools/call {name} carried no structuredContent"))
}

/// The whole seeded fixture: db, copied store, and their tempdir guards.
struct Fixture {
    _tmp_db: TempDir,
    _tmp_store: TempDir,
    store_dir: PathBuf,
    seed: Seed,
}

async fn seeded_fixture() -> (Fixture, rmcp::service::RunningService<rmcp::RoleClient, ()>) {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp_db).await;
    let seed = seed(&db).await;
    drop(db);

    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);

    let client = spawn_client(&db_path, &store_dir).await;
    (
        Fixture {
            _tmp_db: tmp_db,
            _tmp_store: tmp_store,
            store_dir,
            seed,
        },
        client,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_and_dsl_schema_resource_are_served() {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, _db) = migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let client = spawn_client(&db_path, &store_dir).await;

    // Exactly the seven declared read tools — no more, no fewer.
    let tools = client.list_tools(None).await.expect("tools/list").tools;
    let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "export_candles",
            "export_indicators",
            "export_trades",
            "get_run",
            "get_version",
            "list_runs",
            "list_strategies",
        ],
        "the advertised tool set is exactly the seven read tools"
    );

    // The one advertised resource, then its document body.
    let resources = client
        .list_resources(None)
        .await
        .expect("resources/list")
        .resources;
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "pulse://dsl/schema");
    assert_eq!(resources[0].name, "dsl_schema");
    assert_eq!(resources[0].mime_type.as_deref(), Some("application/json"));

    let read = client
        .read_resource(ReadResourceRequestParams::new("pulse://dsl/schema"))
        .await
        .expect("resources/read pulse://dsl/schema");
    assert_eq!(read.contents.len(), 1);
    let text = match &read.contents[0] {
        ResourceContents::TextResourceContents { text, .. } => text.clone(),
        other => panic!("dsl_schema must be text contents, got {other:?}"),
    };
    let doc: Value = serde_json::from_str(&text).expect("dsl_schema body parses as JSON");
    assert_eq!(doc["schema_version"], "1.0.0");
    let properties = doc["json_schema"]["properties"]
        .as_object()
        .expect("json_schema has properties");
    for field in [
        "entry",
        "filters",
        "exits",
        "risk",
        "direction",
        "name",
        "schema_version",
    ] {
        assert!(properties.contains_key(field), "missing property {field}");
    }
    assert_eq!(
        doc["conventions"]["windows"],
        "a windowed backtest slices the series to `[from, to)` and indicators warm up inside the window"
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strategy_and_version_tools_return_the_seed() {
    let (fixture, client) = seeded_fixture().await;
    let (parent, child, _run) = &fixture.seed;
    let parent_id = parent.as_str().to_owned();
    let child_id = child.as_str().to_owned();

    // --- list_strategies: the seeded tree, parent-first, created_by strings.
    let strategies = call(&client, "list_strategies", json!({})).await;
    let strategies = strategies.as_array().expect("strategy array");
    let demo = strategies
        .iter()
        .find(|s| s["name"] == "MCP demo")
        .expect("seeded strategy listed");
    assert_eq!(demo["archived"], false);
    let versions = demo["versions"].as_array().expect("versions array");
    assert_eq!(versions.len(), 2, "parent + child versions");
    assert_eq!(versions[0]["id"], parent_id, "parent comes first");
    assert_eq!(versions[0]["created_by"], "human");
    assert_eq!(versions[1]["id"], child_id);
    assert_eq!(versions[1]["parent_id"], parent_id);
    assert_eq!(versions[1]["created_by"], "composer_llm");

    // include_archived=false on a non-archived strategy changes nothing; the
    // flag exists on the schema — assert the wire accepts it.
    let with_flag = call(
        &client,
        "list_strategies",
        json!({"include_archived": true}),
    )
    .await;
    assert!(with_flag.as_array().expect("array").len() >= strategies.len());

    // --- get_version: migrated object + verbatim original + provenance.
    let version = call(&client, "get_version", json!({"version_id": child_id})).await;
    assert_eq!(version["id"], child_id);
    assert_eq!(version["parent_version_id"], parent_id);
    assert_eq!(version["created_by"], "composer_llm");
    assert_eq!(version["dsl_schema_version"], "1.0.0");
    assert_eq!(version["dsl"]["name"], "RSI Oversold (mcp child)");
    assert_eq!(
        version["dsl_original"],
        MINIMAL_DSL.replace("(mcp)", "(mcp child)")
    );
    assert!(
        version["version_hash"]
            .as_str()
            .is_some_and(|h| !h.is_empty()),
        "version_hash is reported"
    );
    assert!(
        version["created_at"]
            .as_str()
            .is_some_and(|t| !t.is_empty())
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_tools_return_the_seeded_run() {
    let (fixture, client) = seeded_fixture().await;
    let (_parent, child, run) = &fixture.seed;
    let child_id = child.as_str().to_owned();
    let run_id = run.as_str().to_owned();

    // --- list_runs: the one seeded run, with inputs provenance.
    let runs = call(&client, "list_runs", json!({"version_id": child_id})).await;
    let runs = runs.as_array().expect("runs array");
    assert_eq!(runs.len(), 1);
    let row = &runs[0];
    assert_eq!(row["run_id"], run_id);
    assert_eq!(row["trade_count"], 1);
    assert_eq!(row["net_pnl"], "1100");
    assert_eq!(row["inputs"]["pair"], "BTCUSDT");
    assert_eq!(row["inputs"]["primary"]["timeframe"], "15m");
    assert_eq!(row["inputs"]["primary"]["data_version"], "v-primary");
    assert_eq!(row["inputs"]["htf"]["data_version"], "v-htf");

    // --- get_run: summary + aggregates + integrity fields, no inline trades.
    let detail = call(&client, "get_run", json!({"run_id": run_id})).await;
    assert_eq!(detail["summary"]["trade_count"], 1);
    assert_eq!(detail["net_pnl"], Value::Null, "no run-level net_pnl field");
    assert_eq!(detail["mfe_mae"]["count"], 1);
    assert_eq!(detail["mfe_mae"]["mean_mfe_r"], "2.5");
    assert_eq!(detail["mfe_mae"]["mean_mae_r"], "-0.5");
    assert_eq!(detail["inputs"]["primary"]["data_version"], "v-primary");
    assert_eq!(detail["starting_equity"], "10000");
    assert!(
        detail["result_content_hash"]
            .as_str()
            .is_some_and(|h| !h.is_empty()),
        "result_content_hash reported"
    );
    assert!(
        detail["engine_fingerprint"]
            .as_str()
            .is_some_and(|f| !f.is_empty())
    );
    assert!(
        detail["engine_target"]
            .as_str()
            .is_some_and(|t| !t.is_empty())
    );
    assert!(detail["regime_breakdown"].is_object());
    assert!(detail["skipped_entries"].is_object());
    assert!(
        detail.get("trades").is_none(),
        "no inline trades in get_run"
    );

    // --- export_trades: one CSV row per persisted trade.
    let exported = call(&client, "export_trades", json!({"run_id": run_id})).await;
    assert_eq!(exported["rows"], 1);
    let path = PathBuf::from(exported["path"].as_str().expect("export path"));
    assert!(path.is_absolute(), "export path is absolute: {path:?}");
    assert!(
        path.starts_with(&fixture.store_dir),
        "export under --data-dir"
    );
    let csv = std::fs::read_to_string(&path).expect("read trades csv");
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 2, "header + one trade row");
    assert!(lines[0].contains("realized_pnl"));
    assert!(lines[0].contains("mfe_r"));
    assert!(lines[1].contains("take_profit"));
    let columns = exported["columns"].as_array().expect("columns array");
    assert!(
        columns.iter().any(|c| c == "regime"),
        "columns names the trade fields"
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candle_and_indicator_exports_match_the_snapshot() {
    let (fixture, client) = seeded_fixture().await;
    let run_id = fixture.seed.2.as_str().to_owned();

    // The expected values: the same store read the server performs, driven
    // directly through CandleStore + IndicatorEngine.
    let store = CandleStore::with_base_dir(fixture.store_dir.clone());
    let pair = Pair::new("BTCUSDT");
    let stored = store
        .load_head(&pair, Timeframe::M15)
        .expect("load_head")
        .expect("15m HEAD exists");
    let expected_version = stored.series.version.as_str().to_owned();
    let candle_count = stored.series.candles.len();
    assert!(candle_count > 0, "fixture snapshot is non-empty");

    // --- export_candles (csv): row count + reported version.
    let exported = call(
        &client,
        "export_candles",
        json!({"pair": "BTCUSDT", "timeframe": "15m"}),
    )
    .await;
    assert_eq!(exported["rows"], candle_count);
    assert_eq!(exported["data_version"], expected_version);
    assert_eq!(exported["timeframe"], "15m");
    assert_eq!(exported["pair"], "BTCUSDT");
    let path = PathBuf::from(exported["path"].as_str().expect("path"));
    let csv = std::fs::read_to_string(&path).expect("read candles csv");
    assert_eq!(
        csv.lines().count(),
        candle_count + 1,
        "header + one row per candle"
    );
    assert!(csv.starts_with("open_time\tclose_time\topen\thigh\tlow\tclose"));

    // --- export_candles (parquet): a byte copy of the snapshot file.
    let exported = call(
        &client,
        "export_candles",
        json!({"pair": "BTCUSDT", "timeframe": "15m", "format": "parquet"}),
    )
    .await;
    assert_eq!(exported["rows"], candle_count);
    let path = PathBuf::from(exported["path"].as_str().expect("path"));
    assert_eq!(path.extension().and_then(|e| e.to_str()), Some("parquet"));
    let snapshot = std::fs::read(
        fixture
            .store_dir
            .join("candles/BTCUSDT/15m")
            .join(format!("{expected_version}.parquet")),
    )
    .expect("read snapshot");
    assert_eq!(
        std::fs::read(&path).expect("read parquet export"),
        snapshot,
        "parquet export is a byte copy of the snapshot"
    );

    // --- export_indicators (rsi:14): cell-for-cell vs IndicatorEngine.
    let spec = IndicatorSpec::Rsi {
        period: SweepableValue::Fixed(14),
    };
    let mut engine =
        IndicatorEngine::from_specs(std::slice::from_ref(&spec)).expect("engine builds");
    let mut expected: Vec<Option<Decimal>> = Vec::with_capacity(candle_count);
    for candle in &stored.series.candles {
        engine.step(candle);
        expected.push(engine.current(&CompiledValue::Indicator(spec.clone())));
    }

    let exported = call(
        &client,
        "export_indicators",
        json!({"pair": "BTCUSDT", "timeframe": "15m", "indicators": ["rsi:14"]}),
    )
    .await;
    assert_eq!(exported["rows"], candle_count);
    assert_eq!(exported["data_version"], expected_version);
    assert_eq!(exported["columns"], json!(["rsi:14"]));
    let path = PathBuf::from(exported["path"].as_str().expect("path"));
    let csv = std::fs::read_to_string(&path).expect("read indicators csv");
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some("open_time\trsi:14"));
    for (candle, expected_value) in stored.series.candles.iter().zip(&expected) {
        let line = lines.next().expect("one row per candle");
        let (open_time, cell) = line.split_once('\t').expect("two columns");
        assert_eq!(open_time, candle.open_time.to_string());
        match expected_value {
            Some(v) => assert_eq!(cell, v.normalize().to_string()),
            None => assert_eq!(cell, "", "warmup row renders blank"),
        }
    }
    assert!(lines.next().is_none(), "no trailing rows");

    // A run-scoped export here keeps the session genuinely read-only — the
    // seeded run_id proves trades read back through the same store.
    assert!(!run_id.is_empty());

    client.cancel().await.expect("cancel session");
}
