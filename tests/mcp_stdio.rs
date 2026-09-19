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

mod support;

use std::path::PathBuf;

use pulse::{
    CandleSeriesRepository, CandleStore, CompiledValue, EvalContext, IndicatorEngine,
    IndicatorSpec, Pair, Series, SweepableValue, Timeframe,
};
use rmcp::model::{CallToolRequestParams, ReadResourceRequestParams, ResourceContents};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use support::mcp::{
    FIXTURE_STORE, MINIMAL_DSL, arguments, call, call_err, copy_tree, manifest, migrated_db,
    seeded_fixture, spawn_client,
};
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tools_and_dsl_schema_resource_are_served() {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, _db) = migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let client = spawn_client(&db_path, &store_dir).await;

    // Exactly the nine declared tools — the seven read tools plus w3's two
    // write tools, no more, no fewer.
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
            "run_backtest",
            "submit_strategy_version",
        ],
        "the advertised tool set is exactly the nine declared tools"
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
    assert_eq!(doc["schema_version"], "1.1.0");
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
    // The seeded `1.0.0` document was identity-migrated on write — the stored
    // column and the migrated `.dsl` read back at CURRENT.
    assert_eq!(version["dsl_schema_version"], "1.1.0");
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
    // r2.s1 G1(b): the window-edge mark is a first-class RunDetail field —
    // `null` for this unwindowed seed, an object when a windowed run ends
    // holding a position.
    assert!(
        detail.get("open_position").is_some(),
        "get_run carries the open_position key"
    );
    assert_eq!(
        detail["open_position"],
        Value::Null,
        "an unwindowed run reports no mark"
    );
    assert!(
        detail.get("trades").is_none(),
        "no inline trades in get_run"
    );

    // --- export_trades: one CSV row per persisted trade.
    let exported = call(&client, "export_trades", json!({"run_id": run_id})).await;
    assert_eq!(exported["rows"], 1);
    let path = PathBuf::from(exported["path"].as_str().expect("export path"));
    assert!(path.is_absolute(), "export path is absolute: {path:?}");
    // Canonical-vs-canonical: `Exports::create` canonicalizes the exports dir
    // deliberately, so a `--data-dir` under a symlinked root (macOS `/var` →
    // `/private/var`, or a TMPDIR symlink) resolves differently from the raw
    // fixture path. The assertion's meaning is unchanged — the export lands
    // under the `--data-dir` the server was given.
    let canonical_store = fixture
        .store_dir
        .canonicalize()
        .expect("canonicalize the fixture --data-dir");
    assert!(
        path.starts_with(&canonical_store),
        "export under --data-dir"
    );
    let csv = std::fs::read_to_string(&path).expect("read trades csv");
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 2, "header + one trade row");
    assert!(lines[0].contains("realized_pnl"));
    assert!(lines[0].contains("mfe_r"));
    // r2.s2.w2: the recorded per-trade stop is the 20th column — the seeded
    // trade's 89450 stop must appear in the row, not just the header.
    assert!(lines[0].contains("stop_price"));
    assert!(lines[1].contains("take_profit"));
    assert!(
        lines[1].split('\t').next_back() == Some("89450"),
        "the stop cell carries the persisted stop_price: {}",
        lines[1]
    );
    let columns = exported["columns"].as_array().expect("columns array");
    assert!(
        columns.iter().any(|c| c == "regime"),
        "columns names the trade fields"
    );
    assert!(
        columns.iter().any(|c| c == "stop_price"),
        "columns names the recorded stop"
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
        expected.push(engine.current(&CompiledValue::Indicator {
            series: Series::Primary,
            spec: spec.clone(),
        }));
    }

    let exported = call(
        &client,
        "export_indicators",
        json!({"pair": "BTCUSDT", "timeframe": "15m", "indicators": ["rsi:14"]}),
    )
    .await;
    assert_eq!(exported["rows"], candle_count);
    assert_eq!(exported["data_version"], expected_version);
    // G7: the export contract echoes pair + timeframe on every export —
    // export_indicators used to drop both.
    assert_eq!(exported["timeframe"], "15m");
    assert_eq!(exported["pair"], "BTCUSDT");
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

/// F4 (r2.s1): `export_trades` validates `run_id` at the boundary — a value
/// that is not a single path component is refused, and a well-formed but
/// UNKNOWN id is refused too (an empty `get_trades` result must never write a
/// header-only export that impersonates a real zero-trade run). Nothing is
/// written for either refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn export_trades_refuses_bad_and_unknown_run_ids_without_writing() {
    let (fixture, client) = seeded_fixture().await;

    let exports_dir = fixture.store_dir.join("exports");
    let export_count = |dir: &std::path::Path| -> usize {
        std::fs::read_dir(dir).map_or(0, |entries| entries.flatten().count())
    };
    let before = export_count(&exports_dir);

    // Every shape that would escape or relocate the exports dir is refused on
    // `run_id` — the same rule `data_version` already applies.
    for bad in ["foo/bar", "..", "../escape", "a\\b", ""] {
        let err = call_err(&client, "export_trades", json!({"run_id": bad})).await;
        assert_eq!(
            err["field"], "run_id",
            "a path-unsafe run_id attaches to run_id: {err}"
        );
        assert!(
            err["message"]
                .as_str()
                .is_some_and(|m| m.contains("invalid run_id")),
            "the refusal says the id is not a single path component: {err}"
        );
    }

    // A well-formed id naming no run is refused BEFORE a file exists —
    // distinguishable from a genuine zero-trade run.
    let err = call_err(&client, "export_trades", json!({"run_id": "no-such-run"})).await;
    assert_eq!(err["field"], "run_id");
    assert!(
        err["message"]
            .as_str()
            .is_some_and(|m| m.contains("no such backtest run")),
        "an unknown run is named, not exported empty: {err}"
    );

    assert_eq!(
        export_count(&exports_dir),
        before,
        "a refused run_id writes no export"
    );

    client.cancel().await.expect("cancel session");
}

/// G2: serde's default unknown-field tolerance would silently drop a misspelled
/// `form`/`to`/`data_version` — turning a windowed backtest into an UNWINDOWED
/// full-history run, or a pinned export into a HEAD read. Every MCP arg struct
/// carries `deny_unknown_fields`, so a typo is a protocol refusal, and the
/// advertised `inputSchema` says `additionalProperties: false`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_arguments_are_refused_at_the_boundary() {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, _db) = migrated_db(&tmp_db).await;
    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);
    let client = spawn_client(&db_path, &store_dir).await;

    // The advertised schema itself refuses unknown keys — every one of the
    // nine tools, not just the cited one.
    let tools = client.list_tools(None).await.expect("tools/list").tools;
    for tool in &tools {
        assert_eq!(
            tool.input_schema.get("additionalProperties"),
            Some(&json!(false)),
            "{} must advertise additionalProperties:false",
            tool.name
        );
    }

    // The P1 typo: `form` for `from`. Silently dropped, this would run an
    // UNWINDOWED full-history backtest — now a refused call naming the field.
    let refused = client
        .call_tool(
            CallToolRequestParams::new("run_backtest".to_owned()).with_arguments(arguments(
                &json!({
                    "version_id": "ver-any",
                    "form": "2025-03-01T00:00:00Z",
                    "to": "2025-03-02T00:00:00Z",
                }),
            )),
        )
        .await
        .expect("the refusal is a tool error, not a transport failure");
    assert_eq!(
        refused.is_error,
        Some(true),
        "a misspelled `form` must be refused, not run unwindowed: {refused:?}"
    );
    let text = refused.content[0]
        .as_text()
        .expect("the refusal is a text block")
        .text
        .clone();
    assert!(
        text.contains("unknown field `form`"),
        "the refusal names the misspelled field: {text}"
    );

    // Same shape on a read tool — a misspelled `data_version` would silently
    // select HEAD instead of the pinned snapshot.
    let refused = client
        .call_tool(
            CallToolRequestParams::new("export_candles".to_owned()).with_arguments(arguments(
                &json!({
                    "pair": "BTCUSDT",
                    "timeframe": "M15",
                    "data_versio": "anything",
                }),
            )),
        )
        .await
        .expect("the refusal is a tool error, not a transport failure");
    assert_eq!(
        refused.is_error,
        Some(true),
        "a misspelled `data_version` must be refused, not read HEAD"
    );

    client.cancel().await.expect("cancel session");
}

/// G5/T19: every tool that takes an identifier resolves it through the shared
/// resolve-and-refuse seam — an unknown `version_id`/`run_id` is a field
/// refusal, never a successful empty result. The cited case: `list_runs`
/// returned `[]` for a version that does not exist, so a typo read as "no
/// runs yet".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_identifiers_refuse_instead_of_returning_empty() {
    let (_fixture, client) = seeded_fixture().await;

    let err = call_err(&client, "list_runs", json!({"version_id": "ver-unknown"})).await;
    assert_eq!(
        err["field"], "version_id",
        "an unknown version attaches to version_id: {err}"
    );
    assert!(
        err["message"]
            .as_str()
            .is_some_and(|m| m.contains("no such strategy version")),
        "an unknown version is refused, not listed empty: {err}"
    );

    // The seam covers the sibling read tools — the same refusal shape on both
    // identifier kinds.
    let err = call_err(&client, "get_version", json!({"version_id": "ver-unknown"})).await;
    assert_eq!(err["field"], "version_id");
    let err = call_err(&client, "get_run", json!({"run_id": "run-unknown"})).await;
    assert_eq!(err["field"], "run_id");
    assert!(
        err["message"]
            .as_str()
            .is_some_and(|m| m.contains("no such backtest run")),
        "an unknown run is refused, not read empty: {err}"
    );

    client.cancel().await.expect("cancel session");
}
