//! r5.s5.w3 — the Library's power-review read (demo line d111's data).
//!
//! Before a certify attempt spends a hypothesis, the app shows the latest power review per pair under the
//! open freeze: the holdout trade count, the pass chance, the bar and whether an attempt is allowed — and
//! nothing about how those holdout trades went (SPINE G1). The read is one app-scope route,
//! `POST /api/v1/power-reviews`, served by `power_reviews_core`; an `agent` token is a 403 `scope_refused`
//! with one audit row (SPINE G6), and no power read exists over MCP.
//!
//! The server owns its own temp database (`support::server::spawn_server`), so the rows are seeded into
//! THAT pool in the shape the CLI's `pulse certify power` writes — three reviews across two pairs, two of
//! them for BTCUSDT so "the latest per pair" is a real choice rather than the only row.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    CertificationInputs, DataVersion, EngineFingerprint, FakeClock, FreezeRecord,
    OpenFreezeRequest, Pair, PowerReviewDraft, PowerReviewRepository, SnapshotSelection,
    SqliteBacktestRunRepo, SqliteCertificationFreezeRepo, SqlitePowerReviewRepo, Timeframe,
    VersionId, WalkForwardRunId, WalkForwardRunRepository,
};
use support::certification::{FREEZE_OPENED_MS, H};

/// The holdout window the seeded freeze opens at (the certification-holdout helper's instant).
const HOLDOUT_START_MS: i64 = 1_750_000_000_000;

/// The three seeded reviews' write instants. B is strictly newer than A and C ties A, so under the old
/// `created_at DESC, id DESC` order the list led with B — the instants are what make the insertion order
/// this test's subject rather than an accident of the seed sequence.
const ROW_A_MS: i64 = 1_800_000_000_000;
const ROW_B_MS: i64 = 1_800_000_060_000;
const ROW_C_MS: i64 = 1_800_000_000_000;

/// A spawned server with one version, one persisted walk-forward run and one open freeze — the exact
/// shape `tests/certification_holdout.rs::assert_the_route_is_app_scope` seeds, so the two suites cannot
/// disagree about what an app read is mounted over.
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

/// The pinned inputs a review's holdout read names — built the way the certification suite builds its
/// seeded draft's, so the two rows describe the same fixture snapshots.
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
        called_by: "w3-test-operator".to_owned(),
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

// ---------------------------------------------------------------------------
// The read
// ---------------------------------------------------------------------------

/// The app answers the LATEST review per pair under the open freeze, newest first. "Newest" is insertion
/// order: the rows are seeded A (BTCUSDT), B (BTCUSDT), then C (SOLUSDT), so B is BTCUSDT's latest and C,
/// written last of all, leads the list. A is the same pair as B and was inserted before it, so it never
/// appears.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_reads_the_latest_review_per_pair_under_the_open_freeze() {
    let seeded = seeded().await;
    seed_reviews(&seeded).await;

    let (status, body) = post(&seeded, &seeded.server.app_token, request(&seeded)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");

    let mut keys: Vec<&str> = body
        .as_object()
        .expect("the answer is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["openFreezeId", "reviews", "versionId"],
        "the answer's top-level key set is exactly the DTO's"
    );
    assert_eq!(body["versionId"], seeded.version.as_str());
    assert_eq!(body["openFreezeId"], seeded.freeze.id);

    let reviews = body["reviews"].as_array().expect("reviews is an array");
    assert_eq!(reviews.len(), 2, "one row per pair: {body}");
    assert_eq!(reviews[0]["pair"], "SOLUSDT");
    assert_eq!(reviews[0]["holdoutN"], 12);
    assert_eq!(reviews[0]["reason"], "too_few_holdout_trades");
    assert_eq!(reviews[1]["pair"], "BTCUSDT");
    assert_eq!(reviews[1]["holdoutN"], 40);
    assert_eq!(reviews[1]["passChance"], 0.5521);
    assert_eq!(reviews[1]["allowed"], true);
    assert_eq!(reviews[1]["reason"], serde_json::Value::Null);
}

/// The panel's read resolves the latest review per pair by **insertion order**, not by `created_at`: a
/// same-millisecond tie and a backward clock step both show the row inserted last. `id` is a random v4 UUID
/// and `created_at` comes from the injected clock, so `created_at DESC, id DESC` could show the older row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_panel_resolves_a_tie_and_a_backward_clock_by_insertion_order() {
    const TIE_MS: i64 = 1_900_000_000_000;
    const LATER_MS: i64 = 1_850_000_000_000;
    const EARLIER_MS: i64 = 1_800_000_000_000;
    let seeded = seeded().await;
    let pool = seeded.server.db.pool().clone();

    // BTCUSDT: two rows at ONE millisecond — the last inserted must win.
    let tied = SqlitePowerReviewRepo::with_deps(pool.clone(), FakeClock::at(TIE_MS));
    tied.insert(&draft(
        &seeded,
        "BTCUSDT",
        31,
        0.4213,
        false,
        Some("below_bar"),
    ))
    .await
    .expect("the tied review persists");
    tied.insert(&draft(&seeded, "BTCUSDT", 40, 0.5521, true, None))
        .await
        .expect("the tied review persists");

    // SOLUSDT: the later insert carries the EARLIER `created_at`, and must still win.
    SqlitePowerReviewRepo::with_deps(pool.clone(), FakeClock::at(LATER_MS))
        .insert(&draft(&seeded, "SOLUSDT", 50, 0.70, true, None))
        .await
        .expect("the later review persists");
    SqlitePowerReviewRepo::with_deps(pool.clone(), FakeClock::at(EARLIER_MS))
        .insert(&draft(
            &seeded,
            "SOLUSDT",
            12,
            0.10,
            false,
            Some("too_few_holdout_trades"),
        ))
        .await
        .expect("the earlier-clocked review persists");

    let (status, body) = post(&seeded, &seeded.server.app_token, request(&seeded)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    let reviews = body["reviews"].as_array().expect("reviews is an array");
    assert_eq!(reviews.len(), 2, "one row per pair: {body}");
    let by_pair = |pair: &str| {
        reviews
            .iter()
            .find(|review| review["pair"] == pair)
            .unwrap_or_else(|| panic!("{pair} is shown: {body}"))
    };
    assert_eq!(
        by_pair("BTCUSDT")["holdoutN"],
        40,
        "a same-millisecond tie shows the row inserted last: {body}"
    );
    assert_eq!(
        by_pair("SOLUSDT")["holdoutN"],
        12,
        "a backward clock step still shows the row inserted last: {body}"
    );
}

/// G1: one review crosses the count, the chance, the bar and the decision — never an outcome. The key set
/// is the mechanical half: `z`, the window, the inputs, `search_pass`/`search_n`, σ, the assumed edge and
/// the fingerprint are all absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_carries_no_holdout_outcome() {
    let seeded = seeded().await;
    seed_reviews(&seeded).await;

    let (status, body) = post(&seeded, &seeded.server.app_token, request(&seeded)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");

    let mut keys: Vec<&str> = body["reviews"][0]
        .as_object()
        .expect("one review is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "allowed",
            "bar",
            "calledBy",
            "createdAt",
            "freezeId",
            "h",
            "holdoutN",
            "id",
            "minHoldoutTrades",
            "pair",
            "passChance",
            "reason",
            "versionId",
        ],
        "a review carries no holdout outcome (G1): {body}"
    );
}

/// Nothing from another freeze: after the freeze closes the route answers no open freeze and no reviews;
/// a new freeze answers its own id and still no reviews (the old rows belong to the closed freeze).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_shows_nothing_from_another_freeze() {
    let seeded = seeded().await;
    seed_reviews(&seeded).await;

    SqliteCertificationFreezeRepo::with_deps(
        seeded.server.db.pool().clone(),
        FakeClock::at(FREEZE_OPENED_MS),
    )
    .close()
    .await
    .expect("the seeded freeze closes");

    let (status, body) = post(&seeded, &seeded.server.app_token, request(&seeded)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    assert_eq!(body["openFreezeId"], serde_json::Value::Null);
    assert_eq!(body["reviews"], serde_json::json!([]));

    let reopened = SqliteCertificationFreezeRepo::with_deps(
        seeded.server.db.pool().clone(),
        FakeClock::at(FREEZE_OPENED_MS + 1),
    )
    .open(&OpenFreezeRequest {
        holdout_start_ms: 1_800_000_000_000,
        h: H,
        alpha: "0.05".to_owned(),
        holdout_test: "one-sided lower confidence bound at 1 - 0.05/H (C1)".to_owned(),
    })
    .await
    .expect("a new freeze opens after the first closes");

    let (status, body) = post(&seeded, &seeded.server.app_token, request(&seeded)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    assert_eq!(body["openFreezeId"], reopened.id);
    assert_eq!(
        body["reviews"],
        serde_json::json!([]),
        "the closed freeze's rows do not cross into the new one"
    );
}

// ---------------------------------------------------------------------------
// The scope (G6)
// ---------------------------------------------------------------------------

/// An `agent` token is a 403 `scope_refused`, audited once — the route is app scope, and no power read
/// exists over MCP.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_token_is_refused_the_power_review_route() {
    let seeded = seeded().await;
    seed_reviews(&seeded).await;

    let (status, body) = post(&seeded, &seeded.server.agent_token, request(&seeded)).await;
    assert_eq!(
        status,
        reqwest::StatusCode::FORBIDDEN,
        "an agent token cannot read the power review: {body:?}"
    );
    assert_eq!(body["code"], "scope_refused");

    let refusals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM token_audit WHERE event = 'refused' AND reason = 'scope'",
    )
    .fetch_one(seeded.server.db.pool())
    .await
    .unwrap();
    assert_eq!(refusals, 1, "the refusal is audited exactly once");
}

// ---------------------------------------------------------------------------
// Route and core agree
// ---------------------------------------------------------------------------

/// The route is a transport for the core, not a second read: its JSON equals the core's own DTO on the
/// same database.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_answers_the_bus_core_dto() {
    let seeded = seeded().await;
    seed_reviews(&seeded).await;

    let core = pulse::power_reviews_core(
        &seeded.server.desktop().await,
        pulse::PowerReviewsRequest {
            version_id: seeded.version.as_str().to_owned(),
        },
    )
    .await
    .expect("the core answers on the same state");
    let expected = serde_json::to_value(&core).expect("the core DTO serializes");

    let (status, body) = post(&seeded, &seeded.server.app_token, request(&seeded)).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{body:?}");
    assert_eq!(
        body, expected,
        "the route's body is the core's own DTO, not a second projection"
    );
}
