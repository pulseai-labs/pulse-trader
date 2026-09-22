//! r2.s3.w5 — AC-1 (d26): the walk-forward tools over a real MCP stdio session.
//!
//! `run_walk_forward` walks a persisted version over `rolling-oos/v1` folds —
//! each fold an ordinary persisted windowed run with full-history lead-in
//! (L8) — and answers one `WalkForwardRunDetail`; `get_walk_forward_run` reads
//! the same shape back byte-identically. Fold runs stay visible through the
//! unchanged `list_runs`/`get_run` surfaces carrying their `walk_forward`
//! membership, and a windowed `run_backtest` still records its
//! `inputs.lead_in_from` (r2.s3.w2 — asserted on the wire so a regression
//! cannot hide behind the kind it shares code with).
//!
//! The fixture is the write-suite's: the seeded version tree PLUS one real run
//! on the parent (`seed_real_run`), so `run_walk_forward` on the child resolves
//! its snapshot pins the way `run_backtest` does — off the parent's recorded
//! inputs. A K=2 walk-forward over the one-month fixture runs in seconds and
//! yields `pass = false` honestly; the shape, the membership, the determinism
//! and the refusals are what d26 proves.
//!
//! Negative paths call `client.call_tool` directly through [`call_err`] — the
//! `call` helper asserts success and cannot see a tool error.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{CandleSeriesRepository, CandleStore, Pair, Timeframe};
use rmcp::model::CallToolRequestParams;
use serde_json::{Value, json};
use support::mcp::{
    FIXTURE_STORE, Fixture, arguments, call, call_err, copy_tree, manifest, migrated_db,
    seed_real_run, seed_versions, spawn_client,
};
use tempfile::TempDir;

/// The oracle's first fully-warm bar, pinned by `tests/walk_forward.rs` — the
/// seeded `MINIMAL_DSL` entry is RSI(14) on M15, the same warm gate the golden
/// strategy carries, so the same measured bar bounds `from`.
const ORACLE_FIRST_WARM_MS: i64 = 1_735_702_200_000;

/// The walk-forward fixture: the version tree PLUS a real run on the PARENT —
/// the child's `run_walk_forward` resolves its snapshot pins off that row, the
/// same inherit-first path `run_backtest` uses (the `mcp_write` fixture shape).
async fn wf_fixture() -> (Fixture, rmcp::service::RunningService<rmcp::RoleClient, ()>) {
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

/// RFC 3339 (seconds) rendering of an epoch-ms bound — the wire's timestamp
/// shape for spans and windows.
fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("a real candle ms")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// RFC 3339 milliseconds — the shape `inputs.lead_in_from` serializes as.
fn rfc3339_millis(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("a real candle ms")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// RFC 3339 → epoch ms, for comparing the wire's windows with the run rows'.
fn parse_rfc3339_ms(raw: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(raw)
        .expect("an RFC 3339 bound")
        .timestamp_millis()
}

/// One `run_walk_forward` call on the seeded child — `k: 2`, no bounds.
async fn run_k2(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    version_id: &str,
) -> Value {
    call(
        client,
        "run_walk_forward",
        json!({ "version_id": version_id, "k": 2 }),
    )
    .await
}

/// Assert the one `WalkForwardRunDetail` shape — every key both tools answer
/// with — so a missing key fails once here rather than per assertion.
fn assert_detail_shape(detail: &Value, k: usize) {
    for key in [
        "walk_forward_run_id",
        "version_id",
        "scheme",
        "k",
        "rule",
        "span",
        "engine_fingerprint",
        "verdict",
        "folds",
    ] {
        assert!(
            detail.get(key).is_some(),
            "detail carries `{key}`: {detail}"
        );
    }
    let span = &detail["span"];
    for key in ["from", "to", "from_defaulted"] {
        assert!(span.get(key).is_some(), "span carries `{key}`: {span}");
    }
    let verdict = &detail["verdict"];
    for key in ["pass", "folds_holding", "folds_required", "pooled"] {
        assert!(
            verdict.get(key).is_some(),
            "verdict carries `{key}`: {verdict}"
        );
    }
    for key in ["n", "mean_r", "lower_bound", "holds"] {
        assert!(
            verdict["pooled"].get(key).is_some(),
            "pooled verdict carries `{key}`: {verdict}"
        );
    }
    let folds = detail["folds"].as_array().expect("folds is an array");
    assert_eq!(folds.len(), k, "one row per fold: {folds:?}");
    for fold in folds {
        for key in ["index", "window", "backtest_run_id", "verdict", "summary"] {
            assert!(fold.get(key).is_some(), "a fold carries `{key}`: {fold}");
        }
        for key in ["from", "to"] {
            assert!(
                fold["window"].get(key).is_some(),
                "a fold window carries `{key}`: {fold}"
            );
        }
        for key in ["n", "mean_r", "lower_bound", "holds"] {
            assert!(
                fold["verdict"].get(key).is_some(),
                "a fold verdict carries `{key}`: {fold}"
            );
        }
    }
}

/// The fixture store's first M15 `open_time` — what every lead-in records.
fn fixture_first_open(fixture: &Fixture) -> i64 {
    CandleStore::with_base_dir(fixture.store_dir.clone())
        .load_head(&Pair::new("BTCUSDT"), Timeframe::M15)
        .expect("load_head")
        .expect("15m HEAD exists")
        .series
        .candles
        .first()
        .expect("non-empty series")
        .open_time
}

/// (i) the `k: 2` run's structured detail, and (ii) `get_walk_forward_run`
/// reading the identical shape back byte-identically.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_walk_forward_returns_the_detail_and_get_reads_it_back_identically() {
    let (fixture, client) = wf_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    let detail = run_k2(&client, child.as_str()).await;
    assert_detail_shape(&detail, 2);
    assert_eq!(detail["version_id"], child.as_str());
    assert_eq!(detail["scheme"], "rolling-oos/v1");
    assert_eq!(detail["k"], 2);
    assert_eq!(detail["rule"], "wf-v1");
    assert_eq!(
        detail["span"]["from_defaulted"], true,
        "no `from` was given — the span opened at the first fully-warm bar"
    );

    // Two contiguous windows whose union is exactly the counted span.
    let folds = detail["folds"].as_array().expect("folds");
    assert_eq!(folds[0]["index"], 0);
    assert_eq!(folds[1]["index"], 1);
    assert_eq!(
        folds[0]["window"]["from"], detail["span"]["from"],
        "the first fold opens the counted span"
    );
    assert_eq!(
        folds[1]["window"]["from"], folds[0]["window"]["to"],
        "the folds are contiguous — no gap, no overlap"
    );
    assert_eq!(
        folds[1]["window"]["to"], detail["span"]["to"],
        "the last fold closes the counted span"
    );

    // Each fold's `backtest_run_id` names a real persisted run — and its own
    // summary row agrees with the id the fold names (L8, a7).
    for fold in folds {
        let run_id = fold["backtest_run_id"].as_str().expect("run id");
        assert_eq!(
            fold["summary"]["id"], run_id,
            "the fold's summary is its own run's catalog row: {fold}"
        );
        let got = call(&client, "get_run", json!({ "run_id": run_id })).await;
        assert!(
            got.get("summary").is_some() && got.get("result_content_hash").is_some(),
            "the fold id resolves to a real run detail: {got}"
        );
    }

    // The same run read back: one shape, byte-identical JSON.
    let fetched = call(
        &client,
        "get_walk_forward_run",
        json!({ "walk_forward_run_id": detail["walk_forward_run_id"] }),
    )
    .await;
    assert_eq!(
        serde_json::to_string(&fetched).expect("fetched serializes"),
        serde_json::to_string(&detail).expect("detail serializes"),
        "get_walk_forward_run reads back the run_walk_forward detail verbatim"
    );

    client.cancel().await.expect("cancel session");
}

/// (iii) the fold runs are ordinary runs: `list_runs` lists them and `get_run`
/// on each carries the `walk_forward` membership plus the recorded window with
/// the snapshot-start `lead_in_from`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fold_runs_are_ordinary_runs_carrying_their_membership() {
    let (fixture, client) = wf_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    let detail = run_k2(&client, child.as_str()).await;
    let wf_id = detail["walk_forward_run_id"].as_str().expect("wf id");
    let folds = detail["folds"].as_array().expect("folds");
    let fold_run_ids: Vec<&str> = folds
        .iter()
        .map(|fold| fold["backtest_run_id"].as_str().expect("fold run id"))
        .collect();

    // The unchanged catalog surface lists the fold runs with their membership.
    let listed = call(
        &client,
        "list_runs",
        json!({ "version_id": child.as_str() }),
    )
    .await;
    let runs = listed["runs"].as_array().expect("runs array");
    for (index, run_id) in fold_run_ids.iter().enumerate() {
        let row = runs
            .iter()
            .find(|row| row["run_id"] == *run_id)
            .unwrap_or_else(|| panic!("fold run {run_id} is listed for the version"));
        assert_eq!(
            row["walk_forward"]["run_id"], wf_id,
            "the list row names its walk-forward parent: {row}"
        );
        assert_eq!(
            row["walk_forward"]["fold_index"], index as u64,
            "the list row names its fold index: {row}"
        );
    }

    // The unchanged detail surface carries the same membership, the recorded
    // counted window, and the lead-in start — the snapshot's first candle
    // (r2.s3.w2: the fold warmed on full history before its `from`).
    let first_open = fixture_first_open(&fixture);
    for (index, fold) in folds.iter().enumerate() {
        let run_id = fold["backtest_run_id"].as_str().expect("fold run id");
        let got = call(&client, "get_run", json!({ "run_id": run_id })).await;
        assert_eq!(
            got["walk_forward"]["run_id"], wf_id,
            "get_run names the walk-forward parent: {got}"
        );
        assert_eq!(got["walk_forward"]["fold_index"], index as u64);
        let inputs = &got["inputs"];
        assert_eq!(
            inputs["window"]["from_ms"],
            parse_rfc3339_ms(fold["window"]["from"].as_str().expect("fold from")),
            "the persisted window is the fold's counted window"
        );
        assert_eq!(
            inputs["window"]["to_ms"],
            parse_rfc3339_ms(fold["window"]["to"].as_str().expect("fold to"))
        );
        assert_eq!(
            inputs["lead_in_from"],
            rfc3339_millis(first_open).as_str(),
            "L8: the fold counted only its window but warmed on the full snapshot"
        );
    }

    client.cancel().await.expect("cancel session");
}

/// (iv) determinism on the wire: a second `run_walk_forward` with the same
/// arguments mints a new run id but returns identical fold
/// `result_content_hash` values and an identical verdict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_run_walk_forward_is_deterministic() {
    let (fixture, client) = wf_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    let first = run_k2(&client, child.as_str()).await;
    let second = run_k2(&client, child.as_str()).await;

    assert_ne!(
        first["walk_forward_run_id"], second["walk_forward_run_id"],
        "every invocation mints a fresh run id — there is no cached path"
    );
    assert_eq!(
        first["verdict"], second["verdict"],
        "the same arguments judge the same verdict"
    );
    assert_eq!(
        first["span"], second["span"],
        "the defaulted span is deterministic"
    );
    let hashes = |detail: &Value| {
        detail["folds"]
            .as_array()
            .expect("folds")
            .iter()
            .map(|fold| fold["summary"]["result_content_hash"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        hashes(&first),
        hashes(&second),
        "identical inputs produce identical fold result hashes"
    );

    client.cancel().await.expect("cancel session");
}

/// (v.a) the `run_walk_forward` request-field refusals: `k` out of range, an
/// explicit `from` before the first fully-warm bar, a malformed `to`, and an
/// unknown `version_id` — each names its own field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_walk_forward_refusals_name_their_field() {
    let (fixture, client) = wf_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    // `k` outside 2..=12 names the `k` field, both directions.
    for k in [13_u8, 1_u8] {
        let err = call_err(
            &client,
            "run_walk_forward",
            json!({ "version_id": child.as_str(), "k": k }),
        )
        .await;
        assert_eq!(err["field"], "k", "k={k} refuses on `k`: {err}");
    }

    // An explicit `from` one primary bar before the first fully-warm bar
    // refuses on `from`, naming the earliest allowed bound as RFC 3339.
    let too_early = ORACLE_FIRST_WARM_MS - Timeframe::M15.duration_ms();
    let err = call_err(
        &client,
        "run_walk_forward",
        json!({ "version_id": child.as_str(), "from": rfc3339(too_early) }),
    )
    .await;
    assert_eq!(
        err["field"], "from",
        "a pre-warm `from` refuses on `from`: {err}"
    );
    assert!(
        err["message"]
            .as_str()
            .is_some_and(|m| m.contains(&rfc3339(ORACLE_FIRST_WARM_MS))),
        "the refusal names the earliest allowed `from` as RFC 3339: {err}"
    );

    // A malformed `to` names `to` (the bounds are independent — `to` alone is
    // legal, so only its own parse can fail).
    let err = call_err(
        &client,
        "run_walk_forward",
        json!({ "version_id": child.as_str(), "to": "not-a-timestamp" }),
    )
    .await;
    assert_eq!(
        err["field"], "to",
        "a malformed `to` refuses on `to`: {err}"
    );

    // An unknown `version_id` names `version_id` — the shared resolve seam.
    let err = call_err(
        &client,
        "run_walk_forward",
        json!({ "version_id": "ver-unknown", "k": 2 }),
    )
    .await;
    assert_eq!(
        err["field"], "version_id",
        "an unknown version names itself: {err}"
    );

    client.cancel().await.expect("cancel session");
}

/// (v.b) the read tool's refusal and the argument boundary: an unknown
/// `walk_forward_run_id` names its own field, and a misspelled argument is a
/// `deny_unknown_fields` protocol refusal — never a silent read or run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_walk_forward_run_and_unknown_args_refuse() {
    let (fixture, client) = wf_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    // An unknown walk-forward run id names `walk_forward_run_id` — refused,
    // never an empty read.
    let err = call_err(
        &client,
        "get_walk_forward_run",
        json!({ "walk_forward_run_id": "wf-unknown" }),
    )
    .await;
    assert_eq!(
        err["field"], "walk_forward_run_id",
        "an unknown id names itself: {err}"
    );

    // An unknown argument is refused at the boundary (`deny_unknown_fields`)
    // — the same protocol refusal `run_backtest`'s `form` typo draws.
    let refused = client
        .call_tool(
            CallToolRequestParams::new("run_walk_forward".to_owned()).with_arguments(arguments(
                &json!({ "version_id": child.as_str(), "bogus": 1 }),
            )),
        )
        .await
        .expect("the refusal is a tool error, not a transport failure");
    assert_eq!(
        refused.is_error,
        Some(true),
        "an unknown argument must be refused: {refused:?}"
    );
    let text = refused.content[0]
        .as_text()
        .expect("the refusal is a text block")
        .text
        .clone();
    assert!(
        text.contains("unknown field `bogus`"),
        "the refusal names the unknown field: {text}"
    );

    client.cancel().await.expect("cancel session");
}

/// (vi) the unchanged `run_backtest` surface: a windowed call still returns
/// the pinned `{run_id, version_id, run}` shape and records the window plus
/// its full-history `lead_in_from` (r2.s3.w2) — asserted here so the invariant
/// the fold runs inherit cannot silently regress behind the shared code path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windowed_run_backtest_still_records_its_full_history_lead_in() {
    let (fixture, client) = wf_fixture().await;
    let (_parent, child, _run) = &fixture.seed;

    // The windowed_call shape from mcp_write: [candle[200].open_time, last
    // candle's open_time) — candle-aligned bounds trimming the head.
    let first_open = fixture_first_open(&fixture);
    let series = CandleStore::with_base_dir(fixture.store_dir.clone())
        .load_head(&Pair::new("BTCUSDT"), Timeframe::M15)
        .expect("load_head")
        .expect("15m HEAD exists")
        .series;
    let from_ms = series.candles[200].open_time;
    let to_ms = series.candles.last().expect("non-empty series").open_time;

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

    // The unchanged envelope: `{run_id, version_id, run}` with the run detail
    // inside — the shape `run_tools_return_the_seeded_run` pins.
    for key in ["run_id", "version_id", "run"] {
        assert!(
            result.get(key).is_some(),
            "run_backtest carries `{key}`: {result}"
        );
    }
    assert_eq!(result["version_id"], child.as_str());
    let inputs = &result["run"]["inputs"];
    assert_eq!(inputs["window"]["from_ms"], from_ms);
    assert_eq!(inputs["window"]["to_ms"], to_ms);
    assert_eq!(
        inputs["lead_in_from"],
        rfc3339_millis(first_open).as_str(),
        "a windowed run warms on the full snapshot — the recorded lead-in is its first candle"
    );

    client.cancel().await.expect("cancel session");
}
