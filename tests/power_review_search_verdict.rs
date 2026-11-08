//! r6.s5.w2 (PDR-0047 decision 3, ADR-0032) — a power review shows the wf-v2 verdict of the search span it
//! ran: the rule, the fold count, the folds holding of the folds required, and the pooled trade count, mean R,
//! lower bound and pass.
//!
//! The verdict is read from the review's stored search walk-forward at read time, so the record and the
//! migrations are unchanged. It is search-span data, so G1 stands: the holdout still shows its count and
//! nothing else.
//!
//! The server owns its own temp database (`support::server::spawn_server`), so the rows are seeded into THAT
//! pool in the shape `tests/power_review_app_read.rs` seeds them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    CertificationInputs, DataVersion, EngineFingerprint, FakeClock, FreezeRecord,
    OpenFreezeRequest, Pair, PowerReviewDraft, PowerReviewRepository, SearchSpanVerdict,
    SnapshotSelection, SqliteBacktestRunRepo, SqliteCertificationFreezeRepo, SqlitePowerReviewRepo,
    Timeframe, VersionId, WalkForwardRunId, WalkForwardRunRepository,
};
use support::certification::{FREEZE_OPENED_MS, H};

/// The holdout window the seeded freeze opens at (the certification-holdout helper's instant).
const HOLDOUT_START_MS: i64 = 1_750_000_000_000;

/// The three seeded reviews' write instants (the same as `tests/power_review_app_read.rs`).
const ROW_A_MS: i64 = 1_800_000_000_000;
const ROW_B_MS: i64 = 1_800_000_060_000;
const ROW_C_MS: i64 = 1_800_000_000_000;

/// A spawned server with one version, one persisted walk-forward run and one open freeze.
struct Seeded {
    server: support::server::TestServer,
    version: VersionId,
    run: WalkForwardRunId,
    freeze: FreezeRecord,
}

async fn seeded() -> Seeded {
    let server = support::server::spawn_server(support::server::ServerOptions::default()).await;
    let server_state = server.desktop().await;
    let version = support::server::seed_version(&server_state).await;
    let run = SqliteBacktestRunRepo::new(server.db.pool().clone())
        .save_walk_forward_run(&version, &support::mcp::seeded_walk_forward_draft(true))
        .await
        .expect("the seeded run persists");
    let freeze = SqliteCertificationFreezeRepo::with_deps(
        server.db.pool().clone(),
        FakeClock::at(FREEZE_OPENED_MS),
    )
    .open(&OpenFreezeRequest {
        holdout_start_ms: HOLDOUT_START_MS,
        h: H,
        alpha: "0.05".to_owned(),
        holdout_test: "one-sided lower confidence bound at 1 - 0.05/H (C1)".to_owned(),
    })
    .await
    .expect("the server's freeze opens");
    Seeded {
        server,
        version,
        run,
        freeze,
    }
}

/// The pinned inputs a review's holdout read names.
fn holdout_inputs() -> CertificationInputs {
    let selection = |timeframe: Timeframe, version: &str| SnapshotSelection {
        timeframe,
        data_version: DataVersion::new(version.to_owned()),
    };
    CertificationInputs {
        primary: selection(Timeframe::M15, "m15-fixture-version"),
        htf: Some(selection(Timeframe::H4, "h4-fixture-version")),
        d1: None,
    }
}

/// One review draft in the CLI's own shape, varying only the fields a row is about.
fn draft(
    seeded: &Seeded,
    pair: &str,
    holdout_n: usize,
    pass_chance: f64,
    allowed: bool,
    reason: Option<&str>,
) -> PowerReviewDraft {
    PowerReviewDraft {
        version_id: seeded.version.clone(),
        freeze_id: seeded.freeze.id.clone(),
        pair: Pair::new(pair),
        h: seeded.freeze.h,
        z: pulse::c1_z(seeded.freeze.h),
        holdout_start_ms: seeded.freeze.holdout_start_ms,
        holdout_end_ms: seeded.freeze.holdout_start_ms + 1,
        holdout_n,
        holdout_inputs: holdout_inputs(),
        search_walk_forward_run_id: seeded.run.clone(),
        search_pass: true,
        search_n: 100,
        sigma_r: "1.000000".to_owned(),
        assumed_edge_r: "0.25".to_owned(),
        bar: "0.50".to_owned(),
        min_holdout_trades: 20,
        pass_chance,
        allowed,
        reason: reason.map(str::to_owned),
        engine_fingerprint: EngineFingerprint::current().as_str().to_owned(),
        called_by: "w2-test-operator".to_owned(),
    }
}

/// Insert the three rows (A, B, C) at their own clocks.
async fn seed_reviews(seeded: &Seeded) {
    let rows = [
        (
            ROW_A_MS,
            draft(seeded, "BTCUSDT", 31, 0.4213, false, Some("below_bar")),
        ),
        (ROW_B_MS, draft(seeded, "BTCUSDT", 40, 0.5521, true, None)),
        (
            ROW_C_MS,
            draft(
                seeded,
                "SOLUSDT",
                12,
                0.1,
                false,
                Some("too_few_holdout_trades"),
            ),
        ),
    ];
    for (at, row) in rows {
        SqlitePowerReviewRepo::with_deps(seeded.server.db.pool().clone(), FakeClock::at(at))
            .insert(&row)
            .await
            .expect("the seeded review persists");
    }
}

/// POST the route with a token and return `(status, body)`.
async fn post(
    seeded: &Seeded,
    token: &str,
    body: serde_json::Value,
) -> (reqwest::StatusCode, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("{}/api/v1/power-reviews", seeded.server.base))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .expect("POST the power-review route");
    let status = response.status();
    let body = response.json().await.expect("the route's JSON");
    (status, body)
}

/// The request body every test sends.
fn request(seeded: &Seeded) -> serde_json::Value {
    serde_json::json!({ "versionId": seeded.version.as_str() })
}

/// The seeded walk-forward run, read back from the store.
async fn read_back(seeded: &Seeded) -> pulse::WalkForwardRun {
    SqliteBacktestRunRepo::new(seeded.server.db.pool().clone())
        .get_walk_forward_run(&seeded.run)
        .await
        .expect("the run reads")
        .expect("the seeded run is stored")
}

// ---------------------------------------------------------------------------
// The domain mapping
// ---------------------------------------------------------------------------

/// `SearchSpanVerdict::from_run` maps every field from the stored run: its rule's name, its fold count and
/// its own `RunVerdict`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn from_run_maps_the_stored_verdict() {
    let seeded = seeded().await;
    let run = read_back(&seeded).await;

    let verdict = SearchSpanVerdict::from_run(&run);

    assert_eq!(verdict.rule, run.rule.name());
    assert_eq!(usize::from(verdict.k), run.folds.len());
    assert_eq!(verdict.folds_holding, run.verdict.folds_holding);
    assert_eq!(verdict.folds_required, run.verdict.folds_required);
    assert_eq!(verdict.pooled_n, run.verdict.pooled.n);
    assert_eq!(
        verdict.pooled_mean_r,
        run.verdict.pooled.mean_r.normalize().to_string()
    );
    assert!(
        (verdict.pooled_lower_bound - run.verdict.pooled.lower_bound).abs() < f64::EPSILON,
        "the lower bound is the run's own pooled bound"
    );
    assert_eq!(verdict.pass, run.verdict.pass);
}

// ---------------------------------------------------------------------------
// The application read
// ---------------------------------------------------------------------------

/// A record whose search walk-forward id names no stored run is refused with `SearchRunMissing`, not read as
/// an empty verdict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_span_verdict_refuses_a_missing_run() {
    let seeded = seeded().await;
    seed_reviews(&seeded).await;
    let pool = seeded.server.db.pool().clone();
    let mut record = SqlitePowerReviewRepo::new(pool.clone())
        .list_for_version(&seeded.version)
        .await
        .expect("the reviews list")
        .into_iter()
        .next()
        .expect("a seeded review is stored");
    let missing = WalkForwardRunId::new("no-such-walk-forward-run");
    record.search_walk_forward_run_id = missing.clone();

    let error = pulse::search_span_verdict(&SqliteBacktestRunRepo::new(pool), &record)
        .await
        .expect_err("a record naming no stored run is refused");

    match error {
        pulse::PowerReviewError::SearchRunMissing(id) => assert_eq!(id, missing),
        other => panic!("expected SearchRunMissing, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The app read
// ---------------------------------------------------------------------------

/// Every review the app read returns carries the search verdict of the run it names — the same
/// `SearchSpanVerdictDto` the domain mapping gives for that run read back. Every seeded review names the same
/// run, so every review carries the same `search` value.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_read_carries_the_search_verdict() {
    let seeded = seeded().await;
    seed_reviews(&seeded).await;
    let run = read_back(&seeded).await;
    let expected = serde_json::to_value(pulse::SearchSpanVerdictDto::from(
        &SearchSpanVerdict::from_run(&run),
    ))
    .expect("the DTO serializes");

    let (status, body) = post(&seeded, &seeded.server.app_token, request(&seeded)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");

    let reviews = body["reviews"].as_array().expect("reviews is an array");
    assert_eq!(reviews.len(), 2, "one row per pair: {body}");
    for (index, review) in reviews.iter().enumerate() {
        assert_eq!(
            review["search"], expected,
            "review {index} carries the run's search verdict: {body}"
        );
    }
}

/// G1: the `search` object holds search-span fields only — the key set is exactly the verdict's, so no
/// holdout value rides beside it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_search_verdict_carries_no_holdout_field() {
    let seeded = seeded().await;
    seed_reviews(&seeded).await;

    let (status, body) = post(&seeded, &seeded.server.app_token, request(&seeded)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");

    let mut keys: Vec<&str> = body["reviews"][0]["search"]
        .as_object()
        .expect("the search verdict is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "foldsHolding",
            "foldsRequired",
            "k",
            "pass",
            "pooledLowerBound",
            "pooledMeanR",
            "pooledN",
            "rule",
        ],
        "the search verdict carries search-span fields only (G1): {body}"
    );
}
