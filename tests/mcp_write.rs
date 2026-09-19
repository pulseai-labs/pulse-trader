//! AC-1 / AC-8 (r2.s1.w3): the two write tools over a real MCP stdio session.
//!
//! `submit_strategy_version` persists an agent-authored DSL variant as
//! `created_by: external_agent` with its `agent_submission` row; `run_backtest`
//! resolves its defaults through the shared resolver (the parent's latest run
//! pins the exact `data_version`s) and slices both series to an optional
//! `[from, to)` window. Every refusal shape, the row-count invariant on failed
//! submits, and the byte-identical hash on a repeated windowed run are asserted
//! against the seeded fixture the `tests/support/mcp.rs` harness builds.
//!
//! Negative paths call `client.call_tool` directly through [`call_err`] — the
//! `call` helper asserts success and cannot see a tool error.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BacktestRunRepository, CandleSeriesRepository, CandleStore, Pair, SqliteBacktestRunRepo,
    SqliteStrategyRepo, StrategyRepository, Timeframe, VersionId,
};
use serde_json::{Value, json};
use support::mcp::{
    FIXTURE_STORE, Fixture, MINIMAL_DSL, call, call_err, copy_tree, manifest, migrated_db,
    seed_real_run, seed_versions, spawn_client,
};
use tempfile::TempDir;

/// The write-suite fixture: the version tree PLUS a real run on the PARENT —
/// the child's `run_backtest` resolves its snapshot pins off that row, and the
/// pins only load because a genuine backtest recorded the fixture's real
/// `data_version`s.
async fn write_fixture() -> (Fixture, rmcp::service::RunningService<rmcp::RoleClient, ()>) {
    let tmp_db = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp_db).await;
    let (parent, child) = seed_versions(&db).await;

    let tmp_store = TempDir::new().unwrap();
    let store_dir = tmp_store.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_dir);

    let parent_run = seed_real_run(&db, &store_dir, &parent).await;

    let client = spawn_client(&db_path, &store_dir).await;
    (
        Fixture {
            _tmp_db: tmp_db,
            _tmp_store: tmp_store,
            db,
            db_path,
            store_dir,
            seed: (parent, child, parent_run),
        },
        client,
    )
}

/// A valid DSL variant off [`MINIMAL_DSL`] — renamed, otherwise identical.
fn variant_dsl(name: &str) -> Value {
    let mut doc: Value = serde_json::from_str(MINIMAL_DSL).expect("MINIMAL_DSL parses");
    doc["name"] = json!(name);
    doc
}

/// A document that LOADS (valid JSON shape, current schema) but fails semantic
/// validation twice over: an empty `name` and an empty `exits` list.
fn invalid_dsl() -> Value {
    let mut doc = variant_dsl("");
    doc["exits"] = json!([]);
    doc
}

/// `SELECT COUNT(*)` on one table through the test-side pool — the row-count
/// invariant's measuring stick (the server holds its own connection; WAL
/// readers coexist).
async fn row_count(fixture: &Fixture, table: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {table}"))
        .fetch_one(fixture.db.pool())
        .await
        .expect("row count query")
}

/// The fixture's copied store, for reading real candle bounds off it.
fn fixture_store(fixture: &Fixture) -> CandleStore {
    CandleStore::with_base_dir(fixture.store_dir.clone())
}

/// RFC 3339 rendering of an epoch-ms candle bound — the wire's timestamp shape.
fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("a real candle ms")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_child_persists_external_agent_version_and_submission() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, _run) = &fixture.seed;
    let child_id = child.as_str().to_owned();

    let result = call(
        &client,
        "submit_strategy_version",
        json!({
            "parent_version_id": child_id,
            "dsl": variant_dsl("Agent Child"),
            "hypothesis": "  lower RSI entry threshold catches more dips  ",
        }),
    )
    .await;

    let version_id = result["version_id"]
        .as_str()
        .expect("version_id reported")
        .to_owned();
    assert_ne!(version_id, child_id, "a NEW version id was minted");
    assert_eq!(result["parent_version_id"], child_id);
    assert_eq!(result["created_by"], "external_agent");
    assert_eq!(
        result["agent_name"], "test-agent",
        "the flag identity lowercases into the submission"
    );
    assert!(
        result["submission_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "submission_id reported"
    );
    assert!(
        result["version_hash"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "version_hash reported"
    );
    assert!(
        result["strategy_id"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    );

    // The library read path agrees: get_version shows the attribution.
    let version = call(&client, "get_version", json!({"version_id": version_id})).await;
    assert_eq!(version["created_by"], "external_agent");
    assert_eq!(version["parent_version_id"], child_id);
    assert_eq!(version["dsl"]["name"], "Agent Child");

    // And the agent_submission row exists beside it — the repo read the UI
    // projection also uses.
    let repo = SqliteStrategyRepo::new(fixture.db.pool().clone());
    let submission = repo
        .get_agent_submission(&VersionId::new(&version_id))
        .await
        .expect("submission read")
        .expect("an agent_submission row exists for the new version");
    assert_eq!(submission.agent_name.as_str(), "test-agent");
    assert_eq!(
        submission.hypothesis.as_str(),
        "lower RSI entry threshold catches more dips",
        "the hypothesis persisted trimmed"
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_invalid_dsl_collects_every_validation_error_and_writes_nothing() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    let versions_before = row_count(&fixture, "strategy_version").await;
    let submissions_before = row_count(&fixture, "agent_submission").await;

    let err = call_err(
        &client,
        "submit_strategy_version",
        json!({
            "parent_version_id": child.as_str(),
            "dsl": invalid_dsl(),
            "hypothesis": "an invalid document",
        }),
    )
    .await;

    assert_eq!(err["field"], "dsl");
    let errors = err["errors"].as_array().expect("errors[] is an array");
    assert!(
        errors.len() >= 2,
        "EVERY FieldError is collected, not just the first: {err}"
    );
    assert!(
        errors
            .iter()
            .all(|e| e["field"].is_string() && e["code"].is_string() && e["message"].is_string()),
        "each error carries field/code/message: {errors:?}"
    );
    assert!(
        errors.iter().any(|e| e["field"] == "name"),
        "the empty name is one of the reported errors: {errors:?}"
    );

    // The database is untouched by a failed validation.
    assert_eq!(
        row_count(&fixture, "strategy_version").await,
        versions_before,
        "no strategy_version row was written"
    );
    assert_eq!(
        row_count(&fixture, "agent_submission").await,
        submissions_before,
        "no agent_submission row was written"
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_root_creates_strategy_and_parentless_version() {
    let (fixture, client) = write_fixture().await;

    let result = call(
        &client,
        "submit_strategy_version",
        json!({
            "strategy_name": "Agent Root",
            "dsl": variant_dsl("Agent Root v1"),
            "hypothesis": "fresh tree, first hypothesis",
        }),
    )
    .await;

    assert_eq!(result["created_by"], "external_agent");
    assert_eq!(result["agent_name"], "test-agent");
    assert!(
        result.get("parent_version_id").is_none() || result["parent_version_id"].is_null(),
        "a root version has no parent: {result}"
    );
    let strategy_id = result["strategy_id"]
        .as_str()
        .expect("strategy_id reported")
        .to_owned();

    // The strategy row exists with exactly that name.
    let repo = SqliteStrategyRepo::new(fixture.db.pool().clone());
    let strategies = repo.list_strategies(true).await.expect("list strategies");
    let created = strategies
        .iter()
        .find(|s| s.id.as_str() == strategy_id)
        .expect("the new strategy row exists");
    assert_eq!(created.name, "Agent Root");

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_duplicate_root_name_is_refused_and_creates_nothing() {
    let (fixture, client) = write_fixture().await;

    let strategies_before = row_count(&fixture, "strategy").await;
    let versions_before = row_count(&fixture, "strategy_version").await;

    // The seeded "MCP demo" name is taken — a second root with it must refuse.
    let err = call_err(
        &client,
        "submit_strategy_version",
        json!({
            "strategy_name": "MCP demo",
            "dsl": variant_dsl("Dup"),
            "hypothesis": "duplicate name should refuse",
        }),
    )
    .await;

    assert_eq!(err["field"], "strategy_name");
    assert!(
        err["message"]
            .as_str()
            .is_some_and(|m| m.contains("already exists")),
        "the refusal names the duplicate: {err}"
    );
    assert_eq!(row_count(&fixture, "strategy").await, strategies_before);
    assert_eq!(
        row_count(&fixture, "strategy_version").await,
        versions_before
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_blank_root_name_is_refused_and_creates_nothing() {
    let (fixture, client) = write_fixture().await;
    let strategies_before = row_count(&fixture, "strategy").await;

    // Empty, spaces, and tabs all trim to nothing — each must refuse rather
    // than land an unnamed strategy row in the Library.
    for name in ["", "   ", " \t "] {
        let err = call_err(
            &client,
            "submit_strategy_version",
            json!({
                "strategy_name": name,
                "dsl": variant_dsl("Blank"),
                "hypothesis": "a blank root name should refuse",
            }),
        )
        .await;
        assert_eq!(err["field"], "strategy_name", "name {name:?}: {err}");
        assert!(
            err["message"].as_str().is_some_and(|m| m.contains("blank")),
            "the refusal names the blank name: {err}"
        );
    }
    assert_eq!(
        row_count(&fixture, "strategy").await,
        strategies_before,
        "no strategy row was created"
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_requires_exactly_one_target() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    // Both targets → refused, naming both arguments on `parent_version_id`.
    let both = call_err(
        &client,
        "submit_strategy_version",
        json!({
            "parent_version_id": child.as_str(),
            "strategy_name": "Both",
            "dsl": variant_dsl("Both"),
            "hypothesis": "two targets refuse",
        }),
    )
    .await;
    assert_eq!(both["field"], "parent_version_id");
    assert!(
        both["message"]
            .as_str()
            .is_some_and(|m| m.contains("strategy_name")),
        "the message names both arguments: {both}"
    );

    // Neither → refused the same way.
    let neither = call_err(
        &client,
        "submit_strategy_version",
        json!({
            "dsl": variant_dsl("Neither"),
            "hypothesis": "no target refuses",
        }),
    )
    .await;
    assert_eq!(neither["field"], "parent_version_id");
    assert!(
        neither["message"]
            .as_str()
            .is_some_and(|m| m.contains("strategy_name")),
        "the message names both arguments: {neither}"
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn submit_rejects_bad_hypothesis_and_unknown_parent() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    let bad_hypothesis = call_err(
        &client,
        "submit_strategy_version",
        json!({
            "parent_version_id": child.as_str(),
            "dsl": variant_dsl("H"),
            "hypothesis": "",
        }),
    )
    .await;
    assert_eq!(bad_hypothesis["field"], "hypothesis");

    let unknown_parent = call_err(
        &client,
        "submit_strategy_version",
        json!({
            "parent_version_id": "no-such-version",
            "dsl": variant_dsl("P"),
            "hypothesis": "missing parent refuses",
        }),
    )
    .await;
    assert_eq!(unknown_parent["field"], "parent_version_id");

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_backtest_without_window_pins_the_parent_runs_snapshots() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, parent_run) = &fixture.seed;

    // What the parent run recorded — the expected pins.
    let runs_repo = SqliteBacktestRunRepo::new(fixture.db.pool().clone());
    let parent_inputs = runs_repo
        .get_run(parent_run)
        .await
        .expect("parent run reads")
        .expect("parent run exists")
        .inputs
        .expect("a fresh run carries inputs");

    let result = call(
        &client,
        "run_backtest",
        json!({"version_id": child.as_str()}),
    )
    .await;

    assert_eq!(result["version_id"], child.as_str());
    assert!(result["run_id"].as_str().is_some_and(|s| !s.is_empty()));
    let inputs = &result["run"]["inputs"];
    assert_eq!(
        inputs["primary"]["data_version"],
        parent_inputs.primary.data_version.as_str(),
        "the child pins the parent run's exact primary data_version"
    );
    assert_eq!(
        inputs["htf"]["data_version"],
        parent_inputs
            .htf
            .as_ref()
            .expect("parent run recorded an htf")
            .data_version
            .as_str(),
        "the child pins the parent run's exact htf data_version"
    );
    assert_eq!(inputs["pair"], "BTCUSDT");
    assert_eq!(inputs["primary"]["timeframe"], "15m");
    assert_eq!(inputs["htf"]["timeframe"], "4h");
    assert!(
        inputs["window"].is_null(),
        "no window requested, none recorded: {inputs}"
    );

    client.cancel().await.expect("cancel session");
}

/// One windowed `run_backtest` over the fixture: `[candle[k].open_time,
/// candles.last().open_time)` — candle-aligned bounds (amendment: bounds align
/// to candle boundaries) that trim the head of the series and the last candle.
async fn windowed_call(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    fixture: &Fixture,
    version_id: &str,
    head_trim: usize,
) -> (Value, i64, i64) {
    let store = fixture_store(fixture);
    let series = store
        .load_head(&Pair::new("BTCUSDT"), Timeframe::M15)
        .expect("load_head")
        .expect("15m HEAD exists")
        .series;
    let from_ms = series.candles[head_trim].open_time;
    let to_ms = series.candles.last().expect("non-empty series").open_time;
    let result = call(
        client,
        "run_backtest",
        json!({
            "version_id": version_id,
            "from": rfc3339(from_ms),
            "to": rfc3339(to_ms),
        }),
    )
    .await;
    (result, from_ms, to_ms)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windowed_run_records_the_window_and_trades_stay_inside() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    let (result, from_ms, to_ms) = windowed_call(&client, &fixture, child.as_str(), 200).await;

    let inputs = &result["run"]["inputs"];
    assert_eq!(
        inputs["window"]["from_ms"], from_ms,
        "the recorded window is the requested one"
    );
    assert_eq!(inputs["window"]["to_ms"], to_ms);

    // Every persisted trade's fills lie inside [from_ms, to_ms) — the engine
    // only ever saw the sliced series.
    let run_id = pulse::BacktestRunId::new(result["run_id"].as_str().expect("run_id").to_owned());
    let runs_repo = SqliteBacktestRunRepo::new(fixture.db.pool().clone());
    let trades = runs_repo.get_trades(&run_id).await.expect("read trades");
    assert!(!trades.is_empty(), "the windowed run produced trades");
    for trade in &trades {
        assert!(
            trade.entry_fill_time >= from_ms,
            "entry fill {} is inside the window",
            trade.entry_fill_time
        );
        assert!(
            trade.exit_fill_time < to_ms,
            "exit fill {} is inside the window",
            trade.exit_fill_time
        );
    }

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windowed_run_repeats_byte_identical() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    let (first, ..) = windowed_call(&client, &fixture, child.as_str(), 400).await;
    let (second, ..) = windowed_call(&client, &fixture, child.as_str(), 400).await;

    let first_id =
        pulse::BacktestRunId::new(first["run_id"].as_str().expect("first run_id").to_owned());
    let second_id =
        pulse::BacktestRunId::new(second["run_id"].as_str().expect("second run_id").to_owned());
    assert_ne!(first_id, second_id, "two distinct rows were written");

    let runs_repo = SqliteBacktestRunRepo::new(fixture.db.pool().clone());
    let run_a = runs_repo
        .get_run(&first_id)
        .await
        .expect("read a")
        .expect("a");
    let run_b = runs_repo
        .get_run(&second_id)
        .await
        .expect("read b")
        .expect("b");
    assert_eq!(
        run_a.result_content_hash, run_b.result_content_hash,
        "identical windowed inputs produce a byte-identical result hash"
    );
    assert_eq!(
        run_a.inputs, run_b.inputs,
        "identical recorded inputs, window included"
    );
    assert_eq!(
        runs_repo.get_trades(&first_id).await.expect("trades a"),
        runs_repo.get_trades(&second_id).await.expect("trades b"),
        "identical persisted trade logs"
    );

    client.cancel().await.expect("cancel session");
}

/// G1 ruling (b): a windowed run that ends holding a position reports it on
/// `get_run` — direction, entry fill and size as opened, marked at the last
/// in-window candle's close. Never a trade, never in the closed-trade stats.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_windowed_run_ending_mid_hold_reports_its_open_position() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, parent_run) = &fixture.seed;

    let runs_repo = SqliteBacktestRunRepo::new(fixture.db.pool().clone());
    let store = fixture_store(&fixture);
    let series = store
        .load_head(&Pair::new("BTCUSDT"), Timeframe::M15)
        .expect("load_head")
        .expect("15m HEAD exists")
        .series;

    // The parent's full-snapshot run is the baseline — bisect its last
    // multi-bar hold: `to` falls at the open_time of the bar the exit FILLED
    // on, so the position is still open when the window ends.
    let baseline = runs_repo
        .get_trades(parent_run)
        .await
        .expect("parent trades");
    let held = baseline
        .iter()
        .rev()
        .find(|t| {
            t.entry_fill_time < t.exit_signal_time && t.exit_reason != pulse::ExitReason::EndOfData
        })
        .expect("the golden strategy holds a multi-bar position that exits on a bar");
    let to_ms = series
        .candles
        .iter()
        .map(|c| c.open_time)
        .find(|t| *t >= held.exit_fill_time)
        .expect("the exit-fill bar exists in the snapshot");
    let from_ms = series.candles[0].open_time;

    let result = call(
        &client,
        "run_backtest",
        json!({
            "version_id": child.as_str(),
            "from": rfc3339(from_ms),
            "to": rfc3339(to_ms),
        }),
    )
    .await;
    let run_id = result["run_id"].as_str().expect("run_id").to_owned();

    // The mark reaches the wire on `get_run`.
    let detail = call(&client, "get_run", json!({ "run_id": run_id })).await;
    let mark = &detail["open_position"];
    assert!(mark.is_object(), "open_position carries the mark: {detail}");
    assert_eq!(
        mark["direction"],
        serde_json::to_value(held.direction).unwrap()
    );
    let entry_price: rust_decimal::Decimal = mark["entry_price"]
        .as_str()
        .expect("entry_price")
        .parse()
        .unwrap();
    assert_eq!(entry_price, held.entry_price, "the entry fill as opened");
    let qty: rust_decimal::Decimal = mark["qty"].as_str().expect("qty").parse().unwrap();
    assert_eq!(qty, held.qty);
    assert_eq!(mark["entry_signal_time"], held.entry_signal_time);
    assert_eq!(mark["entry_fill_time"], held.entry_fill_time);
    let last_in_window = series
        .candles
        .iter()
        .rfind(|c| c.open_time < to_ms)
        .expect("non-empty window");
    assert_eq!(mark["mark_time"], last_in_window.close_time);
    let mark_price: rust_decimal::Decimal = mark["mark_price"]
        .as_str()
        .expect("mark_price")
        .parse()
        .unwrap();
    assert_eq!(mark_price, last_in_window.close);

    // And it is not a trade: no fabricated EndOfData row, and the closed-trade
    // count is the trade log's length — the mark excluded visibly.
    let trades = runs_repo
        .get_trades(&pulse::BacktestRunId::new(run_id))
        .await
        .expect("trades");
    assert!(
        trades
            .iter()
            .all(|t| t.exit_reason != pulse::ExitReason::EndOfData),
        "no fabricated close at the window edge"
    );
    assert_eq!(
        detail["summary"]["trade_count"].as_u64().unwrap(),
        trades.len() as u64
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windowed_run_empty_slice_is_refused_and_writes_nothing() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    let store = fixture_store(&fixture);
    let series = store
        .load_head(&Pair::new("BTCUSDT"), Timeframe::M15)
        .expect("load_head")
        .expect("15m HEAD exists")
        .series;
    let step = Timeframe::M15.duration_ms();
    let last_open = series.candles.last().expect("non-empty series").open_time;
    // A window entirely after the series: no candle's open_time falls inside.
    let from_ms = last_open + step;
    let to_ms = last_open + 2 * step;

    let runs_before = row_count(&fixture, "backtest_run").await;
    let err = call_err(
        &client,
        "run_backtest",
        json!({
            "version_id": child.as_str(),
            "from": rfc3339(from_ms),
            "to": rfc3339(to_ms),
        }),
    )
    .await;

    assert_eq!(err["field"], "window");
    assert!(
        err["message"].as_str().is_some_and(|m| m.contains("empty")),
        "the refusal explains the window is empty: {err}"
    );
    assert_eq!(
        row_count(&fixture, "backtest_run").await,
        runs_before,
        "an empty window writes no run"
    );

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn window_bounds_are_validated() {
    let (fixture, client) = write_fixture().await;
    let (_parent, child, _run) = &fixture.seed;
    let version_id = child.as_str();

    // One bound only → `window`.
    let only_from = call_err(
        &client,
        "run_backtest",
        json!({"version_id": version_id, "from": "2025-01-01T00:00:00Z"}),
    )
    .await;
    assert_eq!(only_from["field"], "window");

    let only_to = call_err(
        &client,
        "run_backtest",
        json!({"version_id": version_id, "to": "2025-01-02T00:00:00Z"}),
    )
    .await;
    assert_eq!(only_to["field"], "window");

    // from >= to → `window`.
    let inverted = call_err(
        &client,
        "run_backtest",
        json!({
            "version_id": version_id,
            "from": "2025-01-02T00:00:00Z",
            "to": "2025-01-01T00:00:00Z",
        }),
    )
    .await;
    assert_eq!(inverted["field"], "window");

    // An unparseable bound names itself, not `window`.
    let unparsable = call_err(
        &client,
        "run_backtest",
        json!({
            "version_id": version_id,
            "from": "not-a-timestamp",
            "to": "2025-01-02T00:00:00Z",
        }),
    )
    .await;
    assert_eq!(unparsable["field"], "from");

    client.cancel().await.expect("cancel session");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_backtest_unknown_version_is_refused() {
    let (_fixture, client) = write_fixture().await;

    let err = call_err(
        &client,
        "run_backtest",
        json!({"version_id": "no-such-version"}),
    )
    .await;
    assert_eq!(err["field"], "version_id");

    client.cancel().await.expect("cancel session");
}
