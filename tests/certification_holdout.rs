//! r4.s1.w5 — AC-1 (demo lines d64/d66): the certification step.
//!
//! One call = one hypothesis, and the record it writes is immutable whatever
//! the outcome. This suite drives the step itself (`certify_version`) over the
//! shared synthetic world (`support::certification`): a temporary database,
//! synthetic snapshots, one version and an open H = 12 freeze.
//!
//! - a planted-edge version certifies (search passes under `wf-v2`, the C1
//!   holdout test passes), and its record names its pair and its data versions;
//! - a version with no edge is recorded `certified = false` and still counts;
//! - the 13th call is refused by name and writes nothing;
//! - with no open freeze it refuses;
//! - a version whose lineage root predates the freeze refuses;
//! - the app reads the full record, and the route that serves it is `app`-scope;
//! - the same on a second pair (SOLUSDT, demo line d66);
//! - and the record tolerates no UPDATE and no DELETE.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BacktestRunRepository, CertificationRepository, CertifyRefusal, FakeClock, OpenFreezeRequest,
    Pair, SqliteBacktestRunRepo, SqliteCertificationFreezeRepo, SqliteCertificationRepo,
    SqliteStrategyRepo, StrategyRepository, VerdictRule, WalkForwardRunRepository,
    fixture_strategy_dsl,
};
use support::certification::{
    BARS, FREEZE_OPENED_MS, H, Probe, SEED, World, certifications, certify, certify_with_pair,
    inverted_dsl, probe_draft, runs, seed_search_run, seeded_draft, walk_forward_run_count,
    walk_forward_under, world, world_with, world_with_clocks, write_pair_series,
};

// ---------------------------------------------------------------------------
// The refusals and the happy path
// ---------------------------------------------------------------------------

/// (d) With no open freeze the step refuses by name and writes nothing: no
/// record, no run, no hypothesis spent (C4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certification_refuses_without_an_open_freeze() {
    let world = world().await;
    let error = certify(&world, None).await.expect_err("no freeze, no step");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::NoOpenFreeze)
        ),
        "the refusal must be the typed one, naming the missing freeze: {error}"
    );
    assert!(
        error
            .to_string()
            .contains("no certification freeze is open"),
        "the message names the reason: {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refusal writes nothing"
    );
    assert!(
        runs(&world)
            .list_runs_for_version(&world.version)
            .await
            .unwrap()
            .is_empty(),
        "a refusal runs nothing"
    );
}

/// (a) A planted-edge version certifies: the `wf-v2` search span passes, the C1
/// holdout test passes, and the record names its pair and its data versions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certification_of_a_planted_edge_names_its_pair_and_data_versions() {
    let world = world().await;
    let outcome = certify(&world, Some(&world.freeze))
        .await
        .expect("the planted edge certifies");
    let record = &outcome.record;

    assert!(
        record.search_pass,
        "the wf-v2 search span passes on the planted edge (folds {} / pooled {} at n={})",
        record.holdout_n, record.holdout_lower_bound, record.holdout_n
    );
    assert!(
        record.holdout_passes,
        "the C1 holdout test passes: n={}, mean={}, bound={} (z={})",
        record.holdout_n, record.holdout_mean_r, record.holdout_lower_bound, record.holdout_z
    );
    assert!(
        record.certified,
        "certified = search_pass AND holdout_passes"
    );
    assert_eq!(record.rule, "wf-v2");
    assert_eq!(
        record.hypothesis_index, 1,
        "the first hypothesis under the freeze"
    );
    assert_eq!(outcome.hypotheses_used, 1, "one call is one hypothesis");
    assert_eq!(outcome.hypotheses_left, u32::from(H) - 1);

    assert_record_names_provenance(&world, record);
    // The numbers, on the record's own terms — the evidence AC-1's report
    // section quotes and a re-run reproduces.
    eprintln!(
        "w5 AC-1 evidence: search_pass={} holdout_passes={} holdout_n={} mean_r={} z={} bound={}",
        record.search_pass,
        record.holdout_passes,
        record.holdout_n,
        record.holdout_mean_r,
        record.holdout_z,
        record.holdout_lower_bound
    );

    // The search span is a persisted wf-v2 walk-forward that ENDS at the
    // holdout start — the freeze's clamp, not a caller-supplied window.
    let search = runs(&world)
        .get_walk_forward_run(&record.search_walk_forward_run_id)
        .await
        .unwrap()
        .expect("the search run is persisted");
    assert_eq!(search.rule, pulse::VerdictRule::WfV2);
    assert_eq!(
        search.span.to_ms, world.holdout_start_ms,
        "the clamp put the search span's end on the holdout start"
    );
    assert_eq!(search.folds.len(), 6, "the default K");

    // The holdout run is NEVER persisted: no run of this version starts at or
    // after the holdout start (Q4 — no run read may surface holdout trades).
    let run_repo = runs(&world);
    for summary in run_repo
        .list_runs_for_version(&world.version)
        .await
        .unwrap()
    {
        let persisted = run_repo
            .get_run(&summary.id)
            .await
            .unwrap()
            .expect("the summary names a readable run");
        if let Some(inputs) = persisted.inputs
            && let Some(window) = inputs.window
        {
            assert!(
                window.from_ms < world.holdout_start_ms,
                "run `{}` covers the holdout — it must never be persisted",
                summary.id.as_str()
            );
        }
    }

    // The store agrees with the answer.
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        1
    );
    let stored = certifications(&world)
        .list_for_version(&world.version)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0], *record, "the answer is the stored row");
}

/// The record's provenance, asserted per timeframe: the pair, the search
/// side's M15 + H4 snapshots, the holdout side's, and a real holdout window.
fn assert_record_names_provenance(world: &World, record: &pulse::CertificationRecord) {
    assert_eq!(record.pair, world.pair);
    assert_eq!(
        record.search_inputs.primary.data_version, world.m15_version,
        "the search side names the snapshot it ran on"
    );
    assert_eq!(
        record
            .search_inputs
            .htf
            .as_ref()
            .map(|selection| selection.data_version.clone()),
        Some(world.h4_version.clone()),
        "the search side names its HTF snapshot"
    );
    assert_eq!(
        record.holdout_inputs.primary.data_version, world.m15_version,
        "the holdout side names the snapshot it ran on"
    );
    assert_eq!(record.holdout_start_ms, world.holdout_start_ms);
    assert!(
        record.holdout_end_ms > record.holdout_start_ms,
        "the holdout window is a real window"
    );
    assert!(
        record.holdout_n >= 160,
        "the holdout must carry the trade count the C1 test's power needs, got {}",
        record.holdout_n
    );
    assert_eq!(
        record.called_by, "w5-test-agent",
        "the record names the caller"
    );
}

/// (b) A version with no edge is recorded `certified = false` and still counts:
/// a failed attempt is a hypothesis spent (Q2).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certification_records_a_non_certifying_version_and_counts_it() {
    let world = world_with(inverted_dsl()).await;
    let outcome = certify(&world, Some(&world.freeze))
        .await
        .expect("the step records whatever the outcome");
    assert!(
        !outcome.record.holdout_passes,
        "the anti-edge version cannot pass the C1 test: n={}, mean={}, bound={}",
        outcome.record.holdout_n, outcome.record.holdout_mean_r, outcome.record.holdout_lower_bound
    );
    assert!(!outcome.record.certified);
    assert_eq!(
        outcome.hypotheses_used, 1,
        "an uncertified attempt still counts"
    );
    assert_eq!(outcome.hypotheses_left, u32::from(H) - 1);
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        1,
        "the failed hypothesis is recorded"
    );
}

// ---------------------------------------------------------------------------
// The app's own read, and a second pair
// ---------------------------------------------------------------------------

/// (f) The app reads the record in FULL — the holdout's numbers included,
/// which is exactly what the agent surface never sees — through the core read
/// and through the route it is mounted on (an `agent` token is refused there).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_app_reads_the_full_record_and_the_route_is_app_scope() {
    let world = world().await;
    let outcome = certify(&world, Some(&world.freeze))
        .await
        .expect("the planted edge certifies");
    assert_the_app_reads_the_record(&world, &outcome.record).await;
    assert_the_route_is_app_scope().await;
}

/// The core read, over the very database the step just wrote: every field the
/// record carries crosses intact (C5), nothing is invented, and the holdout's
/// window and numbers are the app's to see.
async fn assert_the_app_reads_the_record(world: &World, record: &pulse::CertificationRecord) {
    let state = pulse::DesktopState::open_with_store(&world.db_path, world.store.clone())
        .await
        .expect("the app's own state opens");
    let dto = pulse::certification_records_core(
        &state,
        pulse::CertificationRecordsRequest {
            version_id: world.version.as_str().to_owned(),
        },
    )
    .await
    .expect("the app read succeeds");
    assert_eq!(dto.version_id, world.version.as_str());
    assert_eq!(dto.records.len(), 1, "one hypothesis, one record");
    let row = &dto.records[0];
    assert_eq!(row.id, record.id);
    assert!(row.certified && row.search_pass && row.holdout_passes);
    assert_eq!(row.pair, "BTCUSDT");
    assert_eq!(row.rule, "wf-v2");
    assert_eq!(row.hypothesis_index, 1);
    assert_eq!(row.called_by, "w5-test-agent");
    assert_eq!(
        row.holdout_n,
        u32::try_from(record.holdout_n).unwrap(),
        "the app sees the holdout's trade count (C5)"
    );
    assert_eq!(row.holdout_mean_r, record.holdout_mean_r.to_string());
    assert_eq!(
        row.holdout_z.to_bits(),
        record.holdout_z.to_bits(),
        "the C1 quantile crosses bit-exactly"
    );
    assert_eq!(
        row.holdout_lower_bound.to_bits(),
        record.holdout_lower_bound.to_bits(),
        "the C1 bound crosses bit-exactly"
    );
    assert!(
        row.holdout_start.starts_with("20") && row.holdout_end.starts_with("20"),
        "the holdout window crosses as RFC 3339 text: {} .. {}",
        row.holdout_start,
        row.holdout_end
    );
    assert_eq!(
        row.search_data_versions
            .iter()
            .map(|selection| selection.timeframe.as_str())
            .collect::<Vec<_>>(),
        vec!["15m", "4h"],
        "the search side names its snapshots per timeframe (the DTOs' Binance spelling)"
    );
    assert_eq!(
        row.search_data_versions[0].data_version,
        world.m15_version.as_str()
    );
    assert_eq!(
        row.holdout_data_versions[0].data_version,
        world.m15_version.as_str()
    );
}

/// The route that serves the same DTO: mounted `app`-scope, so an `agent`
/// token is a 403 `scope_refused` with an audit row. The server owns its own
/// temp DB, so the record is seeded into THAT database (the shape the step
/// writes, not a second engine run).
async fn assert_the_route_is_app_scope() {
    let server = support::server::spawn_server(support::server::ServerOptions::default()).await;
    let server_state = server.desktop().await;
    let golden = support::server::seed_version(&server_state).await;
    let run_id = SqliteBacktestRunRepo::new(server.db.pool().clone())
        .save_walk_forward_run(&golden, &support::mcp::seeded_walk_forward_draft(true))
        .await
        .expect("the seeded run persists");
    let holdout_start_ms = 1_750_000_000_000;
    let freeze = SqliteCertificationFreezeRepo::with_deps(
        server.db.pool().clone(),
        FakeClock::at(FREEZE_OPENED_MS),
    )
    .open(&OpenFreezeRequest {
        holdout_start_ms,
        h: H,
        alpha: "0.05".to_owned(),
        holdout_test: "one-sided lower confidence bound at 1 - 0.05/H (C1)".to_owned(),
    })
    .await
    .expect("the server's freeze opens");
    SqliteCertificationRepo::with_deps(server.db.pool().clone(), FakeClock::at(FREEZE_OPENED_MS))
        .insert(&probe_draft(
            &Probe {
                version: &golden,
                pair: &Pair::new("BTCUSDT"),
                freeze_id: &freeze.id,
                holdout_start_ms,
                primary_version: "m15-fixture-version",
                htf_version: "h4-fixture-version",
            },
            &run_id,
        ))
        .await
        .expect("the record persists on the server's own pool");

    let client = reqwest::Client::new();
    let read = client
        .post(format!("{}/api/v1/certification-records", server.base))
        .bearer_auth(&server.app_token)
        .json(&serde_json::json!({ "versionId": golden.as_str() }))
        .send()
        .await
        .expect("POST the app route");
    assert_eq!(read.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = read.json().await.expect("the route's JSON");
    assert_eq!(body["versionId"], golden.as_str());
    assert_eq!(body["records"][0]["certified"], true);
    assert_eq!(
        body["records"][0]["holdoutN"], 200,
        "the app's route carries the holdout's numbers: {body}"
    );
    assert_eq!(body["records"][0]["holdoutMeanR"], "0.4");
    assert_eq!(body["records"][0]["calledBy"], "w5-test-agent");

    let refused = client
        .post(format!("{}/api/v1/certification-records", server.base))
        .bearer_auth(&server.agent_token)
        .json(&serde_json::json!({ "versionId": golden.as_str() }))
        .send()
        .await
        .expect("POST the app route with an agent token");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an agent token cannot read the app's full record"
    );
    let body: serde_json::Value = refused.json().await.expect("the refusal's JSON");
    assert_eq!(body["code"], "scope_refused");
    let refusals: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM token_audit WHERE event = 'refused' AND reason = 'scope'",
    )
    .fetch_one(server.db.pool())
    .await
    .unwrap();
    assert_eq!(refusals, 1, "the refusal is audited");
}

/// (g) The same step on SOLUSDT: the pair override resolves that pair's own
/// `HEAD` snapshots, and the record names SOLUSDT and ITS data versions
/// (demo line d66).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certification_on_a_second_pair_names_that_pair_and_its_data_version() {
    let world = world().await;
    let sol = Pair::new("SOLUSDT");
    let (sol_m15_version, _sol_h4_version) =
        write_pair_series(&world.store, &sol, SEED ^ 0x5010, BARS);

    let outcome = certify_with_pair(&world, Some(&world.freeze), Some(sol.clone()))
        .await
        .expect("the SOLUSDT hypothesis runs");

    assert_eq!(outcome.record.pair, sol, "the record names SOLUSDT");
    assert_eq!(
        outcome.record.holdout_inputs.primary.data_version, sol_m15_version,
        "the holdout ran on SOLUSDT's own snapshot, not BTCUSDT's"
    );
    assert_eq!(
        outcome.record.search_inputs.primary.data_version, sol_m15_version,
        "and so did the search span"
    );
    assert_ne!(
        outcome.record.holdout_inputs.primary.data_version, world.m15_version,
        "the two pairs' snapshots are different content"
    );
    assert!(
        outcome.record.certified,
        "the same edge certifies on SOLUSDT"
    );
}

/// (c) The 13th call under an H = 12 freeze is refused by name and writes
/// nothing: the budget is counted from the records, and a spent budget spends
/// no more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certification_refuses_the_thirteenth_call_and_writes_nothing() {
    let world = world().await;
    let run_id = seed_search_run(&world).await;
    for _ in 1..=u32::from(H) {
        let draft = seeded_draft(&world, &run_id);
        certifications(&world)
            .insert(&draft)
            .await
            .expect("the seeded hypothesis persists");
    }
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        u32::from(H),
        "the freeze's budget is exactly spent"
    );
    let walk_forwards_before = walk_forward_run_count(&world).await;

    let error = certify(&world, Some(&world.freeze))
        .await
        .expect_err("the 13th hypothesis is refused");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::HypothesisBudgetSpent { h: H })
        ),
        "the refusal names the budget: {error}"
    );
    assert!(
        error.to_string().contains("12"),
        "the message names H: {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        u32::from(H),
        "a refused call writes no record"
    );
    assert_eq!(
        walk_forward_run_count(&world).await,
        walk_forwards_before,
        "a refused call runs nothing"
    );
}

/// (e) A version whose lineage ROOT was created before the freeze opened is
/// refused by name, naming the root and the freeze — nothing written (C4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certification_refuses_a_pre_freeze_lineage_root() {
    // The version exists a day BEFORE the freeze opens (`world_with_clocks`
    // reverses the two instants): exactly the lineage C4 bars — a strategy the
    // campaign could refine towards its own knowledge of the holdout.
    let world = world_with_clocks(
        fixture_strategy_dsl(),
        FREEZE_OPENED_MS - 86_400_000,
        FREEZE_OPENED_MS,
    )
    .await;
    let error = certify(&world, Some(&world.freeze))
        .await
        .expect_err("a pre-freeze root is refused");
    match &error {
        pulse::CertifyError::Refused(CertifyRefusal::PreFreezeLineage {
            root_version_id,
            root_created_at_ms,
            freeze_opened_at_ms,
            ..
        }) => {
            assert_eq!(
                *root_version_id, world.version,
                "the root of a single-version lineage is the version itself"
            );
            assert!(
                *root_created_at_ms < *freeze_opened_at_ms,
                "the refusal carries both instants"
            );
        }
        other => panic!("expected the pre-freeze lineage refusal, got {other}"),
    }
    let message = error.to_string();
    assert!(
        message.contains(world.version.as_str()) && message.contains("before the freeze opened"),
        "the message names the root and the freeze: {message}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refusal writes nothing"
    );
    assert!(
        runs(&world)
            .list_runs_for_version(&world.version)
            .await
            .unwrap()
            .is_empty(),
        "a refusal runs nothing"
    );
}

/// (j) A certification record tolerates no UPDATE and no DELETE: the `0020`
/// triggers refuse both, whatever the statement's shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certification_records_are_immutable() {
    let world = world().await;
    let outcome = certify(&world, Some(&world.freeze))
        .await
        .expect("the planted edge certifies");
    let id = outcome.record.id.clone();

    let update = sqlx::query("UPDATE certification SET certified = 0 WHERE id = ?1")
        .bind(&id)
        .execute(world.db.pool())
        .await
        .expect_err("an edit is refused");
    assert!(
        update.to_string().contains("immutable"),
        "the trigger says why: {update}"
    );
    let delete = sqlx::query("DELETE FROM certification WHERE id = ?1")
        .bind(&id)
        .execute(world.db.pool())
        .await
        .expect_err("a delete is refused");
    assert!(
        delete.to_string().contains("never deleted"),
        "the trigger says why: {delete}"
    );

    let stored = certifications(&world)
        .list_for_version(&world.version)
        .await
        .unwrap();
    assert_eq!(stored.len(), 1, "the record survives both refusals");
    assert!(stored[0].certified, "and it is unchanged");
}

/// The `certified` flag is DERIVED from two facts (spec A5): a `wf-v1` latest
/// run that passed, OR a certification record with `certified = true`. A
/// `wf-v2` search-span pass on its own is neither — the campaign tunes its
/// candidates towards that verdict, so it must not read as a certification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certified_flag_does_not_follow_a_wf_v2_search_pass() {
    let world = world().await;
    let search = walk_forward_under(&world, VerdictRule::WfV2).await;
    assert!(
        search.run.verdict.pass,
        "the planted edge passes the search span"
    );

    let strategies = SqliteStrategyRepo::new(world.db.pool().clone());
    let version = strategies
        .get_version(&world.version)
        .await
        .unwrap()
        .expect("the version exists");
    assert_eq!(
        version.latest_walk_forward_run_id.as_ref(),
        Some(&search.run.id),
        "the pointer names the passing wf-v2 run"
    );
    assert!(
        !version.certified,
        "a passing wf-v2 search run alone certifies nothing"
    );

    // The same version reads `true` once the record exists — the flag follows
    // the record, and the record names the run the step just made.
    let outcome = certify(&world, Some(&world.freeze))
        .await
        .expect("the planted edge certifies");
    assert!(outcome.record.certified);
    let version = strategies
        .get_version(&world.version)
        .await
        .unwrap()
        .expect("the version exists");
    assert!(
        version.certified,
        "a certified record sets the flag (record `{}`)",
        outcome.record.id
    );
}

/// The flag is not set by an UNCERTIFIED record either: a version whose
/// hypothesis was recorded `certified = false` stays uncertified.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certified_flag_stays_unset_for_a_failed_record() {
    let world = world_with(inverted_dsl()).await;
    let outcome = certify(&world, Some(&world.freeze))
        .await
        .expect("the step records whatever the outcome");
    assert!(!outcome.record.certified);
    let version = SqliteStrategyRepo::new(world.db.pool().clone())
        .get_version(&world.version)
        .await
        .unwrap()
        .expect("the version exists");
    assert!(
        !version.certified,
        "an uncertified record does not certify the version"
    );
    assert!(
        version.latest_walk_forward_run_id.is_some(),
        "the pointer is set — the flag is about the RECORD, not the pointer"
    );
}
