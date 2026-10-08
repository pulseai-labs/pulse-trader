//! r4.s1.w5 — AC-3: the promotion gate reads the certification record.
//!
//! A `wf-v2` verdict is a SEARCH-span verdict, and the campaign tunes its
//! candidates towards it (grill Q2/C1). So a `wf-v2` pass certifies nothing by
//! itself: the version promotes as `Certified` only through a
//! `certification.certified = true` record, and the run that certifies it is
//! the record's `search_walk_forward_run_id`. The `wf-v1` path — today's rule,
//! kept for the fixture and older lineages — and the override path are
//! unchanged.
//!
//! These tests drive the REAL use case (`promote`) over the shared synthetic
//! world plus one pure-gate case for the rule itself, so the policy is pinned
//! both where it is decided (the gate) and where it is resolved (the use case).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BacktestRunRepository, CertificationRepository, Graduation, NonEmptyLabel,
    SqlitePaperSessionRepo, StrategyRepository, SystemClock, VerdictRule, VersionId,
    decide_promotion, promote,
};
use support::certification::{
    Probe, certifications, probe_draft, runs, seed_search_run, walk_forward_under, world,
};

/// Promote `version_id` through the real promotion use case over the world's
/// own pool and store. No override: the gate must decide on the record alone.
async fn promote_version(
    world: &support::certification::World,
    version_id: &VersionId,
) -> Result<pulse::PaperSession, pulse::PaperPromotionError> {
    let paper = SqlitePaperSessionRepo::with_clock(
        world.db.pool().clone(),
        SystemClock,
        world.store.clone(),
    );
    promote(
        &pulse::SqliteStrategyRepo::new(world.db.pool().clone()),
        &runs(world),
        &runs(world),
        &paper,
        &SystemClock,
        &pulse::SqliteCertificationRepo::new(world.db.pool().clone()),
        version_id,
        None,
        NonEmptyLabel::try_new("operator-token").unwrap(),
    )
    .await
}

/// A certified record promotes the version, naming the record's own search run
/// as the certifying run — the end-to-end half of spec A5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_reads_certification_promotes_through_the_record() {
    let world = world().await;
    let outcome = support::certification::certify(&world, Some(&world.freeze))
        .await
        .expect("the planted edge certifies");
    assert!(outcome.record.certified, "the record is the certification");

    // The version's latest walk-forward run IS the record's search run (the step
    // ran it), so this promotion exercises the wf-v2 path with a record present.
    let session = promote_version(&world, &world.version)
        .await
        .expect("a certified record promotes");
    let Graduation::Certified {
        walk_forward_run_id,
        data_versions,
    } = &session.graduation
    else {
        panic!(
            "expected a certified graduation, got {:?}",
            session.graduation
        );
    };
    assert_eq!(
        *walk_forward_run_id, outcome.record.search_walk_forward_run_id,
        "the session names the record's search run"
    );
    assert_eq!(session.strategy_version_id, world.version);
    assert_eq!(session.pair, world.pair);
    assert!(
        data_versions
            .iter()
            .any(|selection| selection.data_version == world.m15_version),
        "the certified data versions come from that run's folds: {data_versions:?}"
    );
}

/// A passing `wf-v2` search run with NO certified record refuses: the campaign's
/// own tuning target is not a certification (spec A5).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_reads_certification_refuses_a_wf_v2_pass_without_a_record() {
    let world = world().await;
    let search = walk_forward_under(&world, VerdictRule::WfV2).await;
    assert!(
        search.run.verdict.pass,
        "the planted edge passes the search span — the refusal below is about the \
         missing RECORD, not a failing run"
    );
    assert!(
        certifications(&world)
            .list_for_version(&world.version)
            .await
            .unwrap()
            .is_empty(),
        "no certification record exists"
    );

    let error = promote_version(&world, &world.version)
        .await
        .expect_err("a wf-v2 pass alone does not promote");
    assert!(
        matches!(&error, pulse::PaperPromotionError::Refused(refused) if *refused == pulse::PromotionRefused::Uncertified),
        "the refusal is the typed `Uncertified`: {error}"
    );
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_session")
        .fetch_one(world.db.pool())
        .await
        .unwrap();
    assert_eq!(sessions, 0, "a refusal writes no session");
}

/// The `wf-v1` path is unchanged: a passing `wf-v1` run certifies with no
/// record anywhere — the fixture's own lineage shape, kept for older versions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_reads_certification_keeps_the_wf_v1_path() {
    let world = world().await;
    let search = walk_forward_under(&world, VerdictRule::WfV1).await;
    assert!(
        search.run.verdict.pass,
        "the fixture shape passes under wf-v1"
    );
    assert!(
        certifications(&world)
            .list_for_version(&world.version)
            .await
            .unwrap()
            .is_empty(),
        "no certification record exists — wf-v1 does not need one"
    );

    let session = promote_version(&world, &world.version)
        .await
        .expect("a wf-v1 pass still certifies");
    let Graduation::Certified {
        walk_forward_run_id,
        ..
    } = &session.graduation
    else {
        panic!(
            "expected a certified graduation, got {:?}",
            session.graduation
        );
    };
    assert_eq!(*walk_forward_run_id, search.run.id);
}

/// The pure gate's rule, asserted where it is decided: a passing `wf-v2` run
/// certifies only when the record names THAT run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_reads_certification_refuses_a_record_naming_another_run() {
    let world = world().await;
    let search = walk_forward_under(&world, VerdictRule::WfV2).await;
    assert!(search.run.verdict.pass);
    let version = pulse::SqliteStrategyRepo::new(world.db.pool().clone())
        .get_version(&world.version)
        .await
        .unwrap()
        .expect("the version exists");
    let label = || NonEmptyLabel::try_new("operator-token").unwrap();

    // A certified record for the version that names a DIFFERENT walk-forward run
    // (the seeded one): the gate must not read it as this run's certification.
    let other_run = seed_search_run(&world).await;
    let record = certifications(&world)
        .insert(&probe_draft(
            &Probe {
                version: &world.version,
                pair: &world.pair,
                freeze_id: &world.freeze.id,
                holdout_start_ms: world.holdout_start_ms,
                primary_version: world.m15_version.as_str(),
                htf_version: world.h4_version.as_str(),
            },
            &other_run,
        ))
        .await
        .expect("the seeded record persists");
    assert!(record.certified);

    // The run's own fold provenance, read from the fold runs (the shape the
    // gate's `CertificationUnreadable` arm is about).
    let mut owned: Vec<Option<pulse::BacktestInputs>> = Vec::new();
    for fold in &search.run.folds {
        let persisted = runs(&world).get_run(&fold.backtest_run_id).await.unwrap();
        owned.push(persisted.and_then(|run| run.inputs));
    }
    let fold_inputs: Vec<Option<&pulse::BacktestInputs>> =
        owned.iter().map(|inputs| inputs.as_ref()).collect();

    let refused = decide_promotion(
        &version,
        Some(&search.run),
        Some(&record),
        &fold_inputs,
        &pulse::EngineFingerprint::current(),
        None,
        label(),
    )
    .expect_err("a record naming another run is not this run's certification");
    assert_eq!(refused, pulse::PromotionRefused::Uncertified);

    // The control arm: a record naming THIS run certifies it.
    let naming = certifications(&world)
        .insert(&probe_draft(
            &Probe {
                version: &world.version,
                pair: &world.pair,
                freeze_id: &world.freeze.id,
                holdout_start_ms: world.holdout_start_ms,
                primary_version: world.m15_version.as_str(),
                htf_version: world.h4_version.as_str(),
            },
            &search.run.id,
        ))
        .await
        .expect("the naming record persists");
    let draft = decide_promotion(
        &version,
        Some(&search.run),
        Some(&naming),
        &fold_inputs,
        &pulse::EngineFingerprint::current(),
        None,
        label(),
    )
    .expect("the naming record certifies the run");
    assert!(matches!(draft.graduation, Graduation::Certified { .. }));
    assert_eq!(
        draft.pair, world.pair,
        "the draft's pair comes from the certified inputs"
    );
}

/// The override path is untouched: a version with a passing `wf-v2` run and no
/// record still promotes through an operator override.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn promotion_reads_certification_keeps_the_override_path() {
    let world = world().await;
    let _search = walk_forward_under(&world, VerdictRule::WfV2).await;
    let paper = SqlitePaperSessionRepo::with_clock(
        world.db.pool().clone(),
        SystemClock,
        world.store.clone(),
    );
    let request = pulse::OverrideRequest {
        reason: pulse::NonEmptyReason::try_new("operator overrides the uncertified").unwrap(),
        pair: world.pair.clone(),
        primary_timeframe: pulse::Timeframe::M15,
        htf_timeframe: Some(pulse::Timeframe::H4),
        uses_d1: false,
    };
    let session = promote(
        &pulse::SqliteStrategyRepo::new(world.db.pool().clone()),
        &runs(&world),
        &runs(&world),
        &paper,
        &SystemClock,
        &pulse::SqliteCertificationRepo::new(world.db.pool().clone()),
        &world.version,
        Some(&request),
        NonEmptyLabel::try_new("operator-token").unwrap(),
    )
    .await
    .expect("the override promotes an uncertified version");
    assert!(
        matches!(session.graduation, Graduation::Override { .. }),
        "the override graduation is unchanged, got {:?}",
        session.graduation
    );
    assert_eq!(session.strategy_version_id, world.version);
}
