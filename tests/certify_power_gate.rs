//! r5.s5.w2 — AC-1 (demo line d110): the certify gate.
//!
//! `certify_version` spends one of the open freeze's hypotheses only when the latest power review for the
//! version, pair and open freeze allows an attempt **and** was taken on the data versions certify would pin
//! now (SPINE G5, PDR-0040, ADR-0032). Otherwise it refuses with a typed refusal whose text names no number,
//! writes no certification record and spends nothing. The freeze checks keep their precedence: no open
//! freeze, a pre-freeze lineage root and a spent budget are refused before the gate ever reads a review.
//!
//! Every refusal here asserts three things: the exact [`pulse::PowerGateBlock`] that fired, that the
//! freeze's budget count is unchanged, and that the rendered message carries no ASCII digit — G5's "its
//! refusal names no number", checked mechanically rather than by inspection.
//!
//! A synthetic review is built from a real one ([`draft_from`]) and inserted with a clock past 2100, so it
//! is strictly the latest row for its `(version, pair, freeze)`: the gate reads it, not the real review
//! underneath.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    CertificationRepository, CertifyRefusal, FakeClock, Pair, PowerGateBlock, PowerReviewDraft,
    PowerReviewRecord, PowerReviewRepository, SqlitePowerReviewRepo,
};
use serde_json::json;
use support::certification::{
    BARS, H, SEED, certifications, certify, certify_with_pair, inverted_dsl, power_review,
    seed_search_run, seeded_draft, world, world_with, write_pair_series,
};
use support::mcp::{call_err, spawn_client};

/// Later than any real `created_at`: a synthetic row inserted with this clock is the latest review for its
/// `(version, pair, freeze)`.
const LATER_MS: i64 = 4_102_444_800_000;

/// The stored record as a draft — every draft field copied, so a test changes only the fields it names.
fn draft_from(record: &PowerReviewRecord) -> PowerReviewDraft {
    PowerReviewDraft {
        version_id: record.version_id.clone(),
        freeze_id: record.freeze_id.clone(),
        pair: record.pair.clone(),
        h: record.h,
        z: record.z,
        holdout_start_ms: record.holdout_start_ms,
        holdout_end_ms: record.holdout_end_ms,
        holdout_n: record.holdout_n,
        holdout_inputs: record.holdout_inputs.clone(),
        search_walk_forward_run_id: record.search_walk_forward_run_id.clone(),
        search_pass: record.search_pass,
        search_n: record.search_n,
        sigma_r: record.sigma_r.clone(),
        assumed_edge_r: record.assumed_edge_r.clone(),
        bar: record.bar.clone(),
        min_holdout_trades: record.min_holdout_trades,
        pass_chance: record.pass_chance,
        allowed: record.allowed,
        reason: record.reason.clone(),
        engine_fingerprint: record.engine_fingerprint.clone(),
        called_by: record.called_by.clone(),
    }
}

/// The repo that writes the synthetic latest row — a clock past every real review's `created_at`.
fn later_reviews(world: &support::certification::World) -> SqlitePowerReviewRepo<FakeClock> {
    SqlitePowerReviewRepo::with_deps(world.db.pool().clone(), FakeClock::at(LATER_MS))
}

/// G5, the empty case: no review for this version and pair under the open freeze — no attempt, no number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_without_a_power_review_spends_nothing_and_names_no_number() {
    let world = world().await;
    let error = certify(&world, Some(&world.freeze))
        .await
        .expect_err("no qualifying review, no attempt");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::NoQualifyingPowerReview {
                block: PowerGateBlock::NoReview
            })
        ),
        "the gate refuses naming the missing review: {error}"
    );
    assert!(
        !error.to_string().chars().any(|c| c.is_ascii_digit()),
        "the refusal names no number (G5): {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refused call spends no hypothesis"
    );
}

/// G5: the latest review for this version and pair does not allow an attempt (here: the search span fails).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_refuses_when_the_latest_review_does_not_allow_it() {
    let world = world_with(inverted_dsl()).await;
    let review = power_review(&world, Some(&world.freeze))
        .await
        .expect("the review runs");
    assert!(
        !review.record.allowed,
        "the anti-edge version's review does not allow an attempt: {review:?}"
    );

    let error = certify(&world, Some(&world.freeze))
        .await
        .expect_err("a non-allowing review blocks the attempt");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::NoQualifyingPowerReview {
                block: PowerGateBlock::NotAllowed
            })
        ),
        "the gate refuses naming the review: {error}"
    );
    assert!(
        !error.to_string().chars().any(|c| c.is_ascii_digit()),
        "the refusal names no number (G5): {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refused call spends no hypothesis"
    );
}

/// G5: a review taken below the `n >= 20` floor does not qualify, even when every other field says it
/// would — the gate trusts `allowed`, and `allowed` is what the floor decided.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_refuses_a_review_with_too_few_holdout_trades() {
    let world = world().await;
    let review = power_review(&world, Some(&world.freeze))
        .await
        .expect("the review runs");
    let mut draft = draft_from(&review.record);
    draft.holdout_n = 19;
    draft.allowed = false;
    draft.reason = Some("too_few_holdout_trades".to_owned());
    later_reviews(&world)
        .insert(&draft)
        .await
        .expect("the synthetic review persists");

    let error = certify(&world, Some(&world.freeze))
        .await
        .expect_err("a review below the trade floor blocks the attempt");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::NoQualifyingPowerReview {
                block: PowerGateBlock::NotAllowed
            })
        ),
        "the gate refuses naming the review: {error}"
    );
    assert!(
        !error.to_string().chars().any(|c| c.is_ascii_digit()),
        "the refusal names no number (G5): {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refused call spends no hypothesis"
    );
}

/// G5: a review below the pass-chance bar does not qualify.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_refuses_a_review_below_the_bar() {
    let world = world().await;
    let review = power_review(&world, Some(&world.freeze))
        .await
        .expect("the review runs");
    let mut draft = draft_from(&review.record);
    draft.pass_chance = 0.42;
    draft.allowed = false;
    draft.reason = Some("below_bar".to_owned());
    later_reviews(&world)
        .insert(&draft)
        .await
        .expect("the synthetic review persists");

    let error = certify(&world, Some(&world.freeze))
        .await
        .expect_err("a review below the bar blocks the attempt");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::NoQualifyingPowerReview {
                block: PowerGateBlock::NotAllowed
            })
        ),
        "the gate refuses naming the review: {error}"
    );
    assert!(
        !error.to_string().chars().any(|c| c.is_ascii_digit()),
        "the refusal names no number (G5): {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refused call spends no hypothesis"
    );
}

/// G5: a review that allows the attempt but was taken on other data versions does not qualify — the
/// snapshot moved between the read and the attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_refuses_a_review_taken_on_other_data_versions() {
    let world = world().await;
    let review = power_review(&world, Some(&world.freeze))
        .await
        .expect("the review runs");
    assert!(
        review.record.holdout_inputs.htf.is_some(),
        "STOP AND REPORT: the fixture review's `holdout_inputs.htf` is `None`, so the synthetic \
         `htf = None` draft would not differ from the real row"
    );
    let mut draft = draft_from(&review.record);
    draft.holdout_inputs.htf = None;
    assert!(
        draft.allowed,
        "the synthetic review still allows the attempt — only its snapshot differs"
    );
    later_reviews(&world)
        .insert(&draft)
        .await
        .expect("the synthetic review persists");

    let error = certify(&world, Some(&world.freeze))
        .await
        .expect_err("a review on other data versions blocks the attempt");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::NoQualifyingPowerReview {
                block: PowerGateBlock::SnapshotMoved
            })
        ),
        "the gate refuses naming the moved snapshot: {error}"
    );
    assert!(
        !error.to_string().chars().any(|c| c.is_ascii_digit()),
        "the refusal names no number (G5): {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refused call spends no hypothesis"
    );
}

/// G5: the review is scoped to the pair. A review on the inherited pair qualifies nothing for a call that
/// overrides the pair to SOLUSDT.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_on_another_pair_does_not_qualify() {
    let world = world().await;
    let sol = Pair::new("SOLUSDT");
    write_pair_series(&world.store, &sol, SEED ^ 0x5010, BARS);
    // A review on the DEFAULT pair only.
    power_review(&world, Some(&world.freeze))
        .await
        .expect("the default pair's review runs");

    let error = certify_with_pair(&world, Some(&world.freeze), Some(sol.clone()))
        .await
        .expect_err("a review on another pair qualifies nothing");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::NoQualifyingPowerReview {
                block: PowerGateBlock::NoReview
            })
        ),
        "the gate refuses naming the missing review for SOLUSDT: {error}"
    );
    assert!(
        !error.to_string().chars().any(|c| c.is_ascii_digit()),
        "the refusal names no number (G5): {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refused call spends no hypothesis"
    );
}

/// The order of refusals is load-bearing: no open freeze and a spent budget are refused inside
/// `prepare_attempt`, before the gate reads any review.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_freeze_checks_keep_precedence_over_the_gate() {
    let world = world().await;
    // (a) No freeze at all, and no review either: the freeze refusal wins.
    let error = certify(&world, None)
        .await
        .expect_err("no freeze, no attempt");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::NoOpenFreeze)
        ),
        "the missing freeze is refused first: {error}"
    );

    // (b) The budget is spent, and still no review: the budget refusal wins.
    let run_id = seed_search_run(&world).await;
    for _ in 1..=u32::from(H) {
        certifications(&world)
            .insert(&seeded_draft(&world, &run_id))
            .await
            .expect("the seeded hypothesis persists");
    }
    let error = certify(&world, Some(&world.freeze))
        .await
        .expect_err("the budget is spent");
    assert!(
        matches!(
            error,
            pulse::CertifyError::Refused(CertifyRefusal::HypothesisBudgetSpent { h: H })
        ),
        "the spent budget is refused before the gate: {error}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        u32::from(H),
        "a refused call writes no record"
    );
}

/// G5's positive half: a qualifying review lets the one attempt through, and the attempt spends exactly one
/// hypothesis on the same holdout the review counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certify_spends_one_hypothesis_after_a_qualifying_review() {
    let world = world().await;
    let review = power_review(&world, Some(&world.freeze))
        .await
        .expect("the review runs");
    assert!(
        review.record.allowed,
        "the fixture's review allows an attempt: {review:?}"
    );

    let outcome = certify(&world, Some(&world.freeze))
        .await
        .expect("the qualifying review admits the attempt");
    assert_eq!(outcome.hypotheses_used, 1, "one call is one hypothesis");
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        1,
        "the attempt is recorded"
    );
    assert_eq!(
        outcome.record.holdout_inputs, review.record.holdout_inputs,
        "the attempt tested the review's own pinned data versions"
    );
    // The two counts are both `usize` (the record types in `src/domain/certification.rs:68` and
    // `src/domain/power_review.rs:172`), so this comparison is exact and needs no conversion.
    assert_eq!(
        outcome.record.holdout_n, review.record.holdout_n,
        "the attempt tested the count the review reported"
    );
}

/// The fixture holdout is pinned (w1 verifier's O1): the count a review reports is the fenced window's,
/// on the fixture world, at its fixed instants.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fixture_holdout_is_pinned() {
    let world = world().await;
    let review = power_review(&world, Some(&world.freeze))
        .await
        .expect("the review runs");
    assert_eq!(
        review.record.holdout_start_ms, 1_760_439_600_000,
        "the fixture holdout starts at its pinned instant"
    );
    assert_eq!(
        review.record.holdout_end_ms, 1_780_689_599_999,
        "and ends at its pinned instant"
    );
    assert_eq!(
        review.record.holdout_n, 1920,
        "and holds the pinned trade count"
    );
}

/// G5 through the MCP edge: the refusal is field-pathed to `power_review`, says what is missing, and names
/// no number.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_mcp_refusal_names_the_power_review_field_and_no_number() {
    let world = world().await;
    let client = spawn_client(&world.db_path, &world.data_dir()).await;
    let error = call_err(
        &client,
        "certify_version",
        json!({ "version_id": world.version.as_str() }),
    )
    .await;
    assert_eq!(
        error["field"], "power_review",
        "the refusal is field-pathed to the gate: {error}"
    );
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("no qualifying power review"),
        "the refusal names the reason: {message}"
    );
    assert!(
        !message.chars().any(|c| c.is_ascii_digit()),
        "the refusal names no number (G5): {message}"
    );
    assert_eq!(
        certifications(&world)
            .count_for_freeze(&world.freeze.id)
            .await
            .unwrap(),
        0,
        "a refused call spends no hypothesis"
    );
}
