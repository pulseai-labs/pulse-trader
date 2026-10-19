//! r5.s5.w1 — AC-1 (demo line d109): the power review core.
//!
//! Before a certify attempt spends one of the open freeze's hypotheses, a power review reports how many
//! trades the candidate makes in the freeze's holdout window — the count and nothing else about those
//! trades — and the chance ADR-0028's C1 test would pass a true `+0.25R` edge at that count, against the
//! bar fixed at plan time (PDR-0040, ADR-0032). It spends no hypothesis and writes one immutable
//! `power_review` row; a refused review writes nothing.
//!
//! This suite drives the use case (`review_power`) over the shared synthetic world
//! (`support::certification`) that `tests/certification_holdout.rs` uses, so the two suites cannot test
//! different worlds:
//!
//! - the maths: `normal_cdf` at its published values, `pass_chance` against the hand check and ADR-0028's
//!   seeded power table, `c1_z` against `holdout_test`'s own quantiles, `sample_sigma`, the verdict order;
//! - the count-only read: one row, the serialized key set is exactly the record's field list, and the CLI's
//!   print-out carries no outcome token;
//! - the same window as certify (G2): a review and a certification report the same holdout `n`;
//! - free and recorded (G3): `count_for_freeze` unchanged, one row added, and each refusal adds none;
//! - immutability: `UPDATE` and `DELETE` are refused by `0022`'s triggers.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::process::Command;

use pulse::{
    ASSUMED_EDGE_R, BacktestAppError, BinanceAdapter, CertificationRepository, CertifyRefusal,
    FakeClock, MIN_HOLDOUT_TRADES, OpenFreezeRequest, POWER_BAR, PowerBlock, PowerReviewError,
    PowerReviewOutcome, PowerReviewRecord, PowerReviewRepository, PowerReviewRequest,
    SqliteBacktestRunRepo, SqliteCertificationFreezeRepo, SqliteCertificationRepo,
    SqlitePowerReviewRepo, SqliteStrategyRepo, WalkForwardRunRepository, c1_z, holdout_test,
    normal_cdf, pass_chance, review_power, sample_sigma, verdict,
};
use rust_decimal::Decimal;
use support::certification::{
    BARS, FREEZE_OPENED_MS, H, SEED, World, certifications, certify, m15_candles, runs,
    seed_search_run, seeded_draft, world, world_with_clocks,
};

// ---------------------------------------------------------------------------
// The maths — the analytic pass chance is the decision; the seeded table is its test
// ---------------------------------------------------------------------------

/// One decimal from an integer literal, for the series below.
fn dec(n: i64) -> Decimal {
    Decimal::from(n)
}

/// Φ at the published values (Abramowitz & Stegun 26.2.17, `|ε| < 7.5e-8`).
#[test]
fn normal_cdf_matches_published_values() {
    for (x, expected) in [
        (0.0_f64, 0.5_f64),
        (1.0, 0.841_345),
        (-1.0, 0.158_655),
        (1.96, 0.975_002),
        (-1.96, 0.024_998),
        (3.0, 0.998_650),
        (-3.0, 0.001_350),
    ] {
        let got = normal_cdf(x);
        assert!(
            (got - expected).abs() < 1e-6,
            "Φ({x}) = {got}, published {expected}"
        );
    }
}

/// ADR-0028's seeded power table, as `tests/wf_v2_measurement.rs`'s `PINNED_POWER` carries it:
/// `(H, holdout trades, passes of 1000)`, μ 0.25, σ 1.2.
const PINNED_POWER: [(u8, usize, f64); 10] = [
    (12, 100, 0.312),
    (12, 150, 0.455),
    (12, 160, 0.486),
    (12, 200, 0.628),
    (12, 250, 0.734),
    (6, 100, 0.395),
    (6, 150, 0.557),
    (6, 160, 0.592),
    (6, 200, 0.716),
    (6, 250, 0.817),
];

/// The hand check (n = 20, H = 2, σ = 1.2 → 0.1519) and ADR-0028's seeded power table, whose cells the
/// instruction says the analytic formula must sit within 0.03 of.
#[test]
fn pass_chance_reproduces_the_hand_check_and_the_power_table() {
    let hand = pass_chance(20, c1_z(2), ASSUMED_EDGE_R, 1.2);
    assert!(
        (hand - 0.1519).abs() <= 0.0005,
        "n=20, H=2, σ=1.2 must give 0.1519 ± 0.0005, got {hand}"
    );

    for (h, n, seeded) in PINNED_POWER {
        let analytic = pass_chance(n, c1_z(h), ASSUMED_EDGE_R, 1.2);
        assert!(
            (analytic - seeded).abs() <= 0.03,
            "H={h}, n={n}: analytic {analytic} vs the seeded {seeded} — outside 0.03"
        );
    }

    // Rising in n, falling in H — the two monotonicities the formula must have.
    let f = |n: usize, h: u8| pass_chance(n, c1_z(h), ASSUMED_EDGE_R, 1.2);
    assert!(
        f(100, 2) < f(150, 2) && f(150, 2) < f(200, 2),
        "rises with n"
    );
    assert!(f(200, 12) < f(200, 2), "falls with H");
}

/// `n < 2` or a non-positive σ cannot pass, and the chance is exactly zero there.
#[test]
fn pass_chance_is_zero_below_two_trades_or_without_dispersion() {
    let z = c1_z(2);
    let zero = |n: usize, sigma: f64| pass_chance(n, z, ASSUMED_EDGE_R, sigma);
    for (n, sigma) in [(0_usize, 1.2_f64), (1, 1.2), (50, 0.0), (50, -1.0)] {
        let got = zero(n, sigma);
        assert!(got.abs() < 1e-12, "n={n}, σ={sigma} must be 0.0, got {got}");
    }
    assert!(zero(50, 1.2) > 0.0);
}

/// `c1_z` IS `holdout_test`'s own `z` — the review's quantile cannot drift from C1's.
#[test]
fn c1_z_is_holdout_tests_own_quantile() {
    for h in [1_u8, 2, 6, 12] {
        let own = holdout_test(&[Decimal::ZERO, Decimal::TWO], h).z;
        assert!(
            (c1_z(h) - own).abs() < 1e-12,
            "H={h}: c1_z {} must be holdout_test's own z {own}",
            c1_z(h)
        );
    }
    // The quantiles `holdout.rs`'s own tests pin (r4.s1.w3).
    for (h, pinned) in [
        (1_u8, 1.644_853_626_951_471_5_f64),
        (6, 2.393_979_799_818_510_4),
        (12, 2.638_257_273_476_751),
    ] {
        assert!(
            (c1_z(h) - pinned).abs() < 1e-6,
            "H={h}: {} vs the pinned {pinned}",
            c1_z(h)
        );
    }
    assert!((c1_z(2) - 1.959_963_984_540_053_6).abs() < 1e-8);
}

/// The search-span σ is Bessel's (N−1) sample σ over the pooled out-of-sample R's.
#[test]
fn sample_sigma_is_bessels_over_the_search_span() {
    assert_eq!(sample_sigma(&[]), None);
    assert_eq!(sample_sigma(&[dec(3)]), None);
    let sigma = sample_sigma(&[dec(1), dec(2), dec(3)]).expect("three trades have a σ");
    assert!(
        (sigma - 1.0).abs() < 1e-12,
        "Bessel σ of [1,2,3] is 1.0, got {sigma}"
    );
    let flat = sample_sigma(&[dec(2), dec(2)]).expect("two trades have a σ");
    assert!(
        flat.abs() < 1e-12,
        "a flat series has no dispersion, got {flat}"
    );
}

/// The verdict's fixed order: the search span first, then the floor, then the bar.
#[test]
fn verdict_refuses_in_priority_order() {
    let blocked = |search_pass, n, chance| verdict(search_pass, n, chance);
    assert_eq!(
        blocked(false, 0, 0.0),
        pulse::PowerVerdict {
            allowed: false,
            reason: Some(PowerBlock::SearchSpanFails)
        }
    );
    assert_eq!(
        blocked(true, MIN_HOLDOUT_TRADES - 1, 0.99),
        pulse::PowerVerdict {
            allowed: false,
            reason: Some(PowerBlock::TooFewHoldoutTrades)
        }
    );
    assert_eq!(
        blocked(true, MIN_HOLDOUT_TRADES, POWER_BAR - 0.01),
        pulse::PowerVerdict {
            allowed: false,
            reason: Some(PowerBlock::BelowBar)
        }
    );
    assert_eq!(
        blocked(true, MIN_HOLDOUT_TRADES, POWER_BAR),
        pulse::PowerVerdict {
            allowed: true,
            reason: None
        },
        "exactly at the bar is allowed (the comparison is a strict below)"
    );
}

// ---------------------------------------------------------------------------
// The use case's world
// ---------------------------------------------------------------------------

/// One power review over the world, exactly as the CLI arm makes it.
async fn review(
    world: &World,
    freeze: Option<&pulse::FreezeRecord>,
) -> Result<PowerReviewOutcome, PowerReviewError> {
    review_power(
        &SqliteStrategyRepo::new(world.db.pool().clone()),
        &world.store,
        &BinanceAdapter::new(),
        &SqliteBacktestRunRepo::new(world.db.pool().clone()),
        &SqliteCertificationRepo::new(world.db.pool().clone()),
        &SqlitePowerReviewRepo::new(world.db.pool().clone()),
        freeze,
        &PowerReviewRequest {
            version_id: world.version.clone(),
            pair: None,
            called_by: "w1-test-operator".to_owned(),
        },
    )
    .await
}

/// How many `power_review` rows the world holds — the "each refusal adds none" probe.
async fn power_review_rows(world: &World) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM power_review")
        .fetch_one(world.db.pool())
        .await
        .unwrap()
}

/// The record's serialized key set, sorted — G1's field-list check.
fn record_keys(record: &PowerReviewRecord) -> Vec<String> {
    let value = serde_json::to_value(record).expect("the record serializes");
    let mut keys: Vec<String> = value
        .as_object()
        .expect("a struct serializes to an object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// The exact fields the record may carry (spec Approach 2 / C-8) — nothing about the holdout's outcomes.
const EXPECTED_KEYS: [&str; 23] = [
    "allowed",
    "assumed_edge_r",
    "bar",
    "called_by",
    "created_at",
    "engine_fingerprint",
    "freeze_id",
    "h",
    "holdout_end_ms",
    "holdout_inputs",
    "holdout_n",
    "holdout_start_ms",
    "id",
    "min_holdout_trades",
    "pair",
    "pass_chance",
    "reason",
    "search_n",
    "search_pass",
    "search_walk_forward_run_id",
    "sigma_r",
    "version_id",
    "z",
];

/// The outcome tokens the CLI's stdout may never carry, matched as WORDS: `holdout window:` is a required
/// key and contains `win` as a substring, so a substring test is unsatisfiable by construction.
fn outcome_tokens(text: &str) -> Vec<&'static str> {
    const BANNED: [&str; 5] = ["mean", "bound", "pnl", "win", "realized"];
    let words: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .collect();
    BANNED
        .into_iter()
        .filter(|banned| words.iter().any(|word| word == banned))
        .collect()
}

// ---------------------------------------------------------------------------
// One review: the count-only read, the record, its provenance
// ---------------------------------------------------------------------------

/// A review of the planted-edge world reports the count and the bar's arithmetic, writes exactly one
/// immutable row, spends no hypothesis, and carries no holdout outcome (G1/G3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_reports_the_count_only_and_writes_one_record() {
    let world = world().await;
    let certs = certifications(&world);
    let spent_before = certs.count_for_freeze(&world.freeze.id).await.unwrap();

    let outcome = review(&world, Some(&world.freeze))
        .await
        .expect("the review runs over the planted-edge world");
    let record = &outcome.record;

    // The record's own terms.
    assert_eq!(record.version_id, world.version);
    assert_eq!(record.freeze_id, world.freeze.id);
    assert_eq!(record.pair, world.pair);
    assert_eq!(record.h, H);
    assert!(
        (record.z - c1_z(H)).abs() < 1e-12,
        "z is C1's own quantile at the freeze's H: {} vs {}",
        record.z,
        c1_z(H)
    );
    assert_eq!(record.holdout_start_ms, world.holdout_start_ms);
    assert!(
        record.holdout_end_ms > record.holdout_start_ms,
        "the holdout window is real: {} .. {}",
        record.holdout_start_ms,
        record.holdout_end_ms
    );
    assert!(
        record.holdout_n > 0,
        "the planted edge trades in its holdout"
    );
    assert_eq!(record.assumed_edge_r, "0.25");
    assert_eq!(record.bar, "0.50");
    assert_eq!(record.min_holdout_trades, MIN_HOLDOUT_TRADES);
    assert!(record.search_n > 0, "the search span trades");
    assert!(
        record.sigma_r.parse::<f64>().is_ok(),
        "σ is stored as a decimal string: {}",
        record.sigma_r
    );
    assert!(
        record.sigma_r.parse::<f64>().unwrap() > 0.0,
        "the planted edge's search span has dispersion: {}",
        record.sigma_r
    );
    assert_eq!(record.called_by, "w1-test-operator");
    assert_eq!(
        record.allowed,
        record.reason.is_none(),
        "allowed ⇔ no reason"
    );

    // The record's provenance: the holdout's pinned versions are the world's own, and the fingerprint is
    // the search run's (the same expression `certification_draft` uses).
    assert_eq!(
        record.holdout_inputs.primary.data_version, world.m15_version,
        "the holdout ran on the world's M15 snapshot"
    );
    let search = runs(&world)
        .get_walk_forward_run(&record.search_walk_forward_run_id)
        .await
        .unwrap()
        .expect("the search run is persisted");
    assert_eq!(
        record.engine_fingerprint, search.engine_fingerprint,
        "the engine fingerprint is the search run's own"
    );
    assert_eq!(record.search_pass, search.verdict.pass);

    // G1: the key set is exactly the field list.
    let keys = record_keys(record);
    let expected: Vec<String> = EXPECTED_KEYS.iter().map(|k| (*k).to_owned()).collect();
    assert_eq!(
        keys, expected,
        "the serialized record carries exactly its fields"
    );

    // G3: one row, and the hypothesis budget is untouched.
    assert_eq!(
        power_review_rows(&world).await,
        1,
        "one review writes one row"
    );
    assert_eq!(
        certs.count_for_freeze(&world.freeze.id).await.unwrap(),
        spent_before,
        "a review spends no hypothesis"
    );
    let stored = SqlitePowerReviewRepo::new(world.db.pool().clone())
        .latest_for(&world.version, &world.pair, &world.freeze.id)
        .await
        .unwrap()
        .expect("the row reads back");
    assert_eq!(stored, *record, "the answer is the stored row");

    eprintln!(
        "w1 evidence: n={} search_n={} sigma={} z={} pass_chance={} allowed={} reason={:?}",
        record.holdout_n,
        record.search_n,
        record.sigma_r,
        record.z,
        record.pass_chance,
        record.allowed,
        record.reason
    );
}

/// G2: the count a review reports is the count a certification on the same version, pair and pinned data
/// would test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_review_and_a_certify_call_report_the_same_holdout_count() {
    let world = world().await;
    let review = review(&world, Some(&world.freeze))
        .await
        .expect("the review runs");
    let cert = certify(&world, Some(&world.freeze))
        .await
        .expect("the certification runs");

    assert_eq!(
        review.record.holdout_n, cert.record.holdout_n,
        "the same window and the same pinned data give the same count"
    );
    assert_eq!(review.record.holdout_start_ms, cert.record.holdout_start_ms);
    assert_eq!(review.record.holdout_end_ms, cert.record.holdout_end_ms);
    assert_eq!(
        review.record.holdout_inputs, cert.record.holdout_inputs,
        "both read the same pinned holdout inputs"
    );
}

// ---------------------------------------------------------------------------
// Refusals and immutability
// ---------------------------------------------------------------------------

/// Every refusal writes nothing: no open freeze, a pre-freeze lineage, a spent budget, and an empty holdout
/// window (G3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_review_writes_nothing() {
    // (a) No open freeze.
    let no_freeze = world().await;
    let error = review(&no_freeze, None)
        .await
        .expect_err("no freeze, no review");
    assert!(
        matches!(
            error,
            PowerReviewError::Refused(CertifyRefusal::NoOpenFreeze)
        ),
        "the typed refusal names the missing freeze: {error}"
    );
    assert_eq!(
        power_review_rows(&no_freeze).await,
        0,
        "a refusal writes no row"
    );

    // (b) A lineage rooted before the freeze opened.
    let pre = world_with_clocks(
        pulse::fixture_strategy_dsl(),
        FREEZE_OPENED_MS - 86_400_000,
        FREEZE_OPENED_MS,
    )
    .await;
    let error = review(&pre, Some(&pre.freeze))
        .await
        .expect_err("a pre-freeze root is refused");
    assert!(
        matches!(
            error,
            PowerReviewError::Refused(CertifyRefusal::PreFreezeLineage { .. })
        ),
        "the typed refusal names the lineage: {error}"
    );
    assert_eq!(power_review_rows(&pre).await, 0, "a refusal writes no row");

    // (c) The freeze's budget is spent.
    let spent = world().await;
    let run_id = seed_search_run(&spent).await;
    for _ in 1..=u32::from(H) {
        certifications(&spent)
            .insert(&seeded_draft(&spent, &run_id))
            .await
            .expect("the seeded hypothesis persists");
    }
    let error = review(&spent, Some(&spent.freeze))
        .await
        .expect_err("a spent budget is refused");
    assert!(
        matches!(
            error,
            PowerReviewError::Refused(CertifyRefusal::HypothesisBudgetSpent { h: H })
        ),
        "the typed refusal names the budget: {error}"
    );
    assert_eq!(
        power_review_rows(&spent).await,
        0,
        "a refusal writes no row"
    );

    // (d) The holdout window is empty: the snapshot ends before the freeze's holdout start.
    let (empty, freeze) = world_with_an_empty_holdout().await;
    let error = review(&empty, Some(&freeze))
        .await
        .expect_err("an empty holdout window cannot be reviewed");
    assert!(
        matches!(
            error,
            PowerReviewError::Backtest(BacktestAppError::WindowEmpty { .. })
        ),
        "the window-empty case keeps run_holdout's own error: {error}"
    );
    assert_eq!(power_review_rows(&empty).await, 0, "an error writes no row");
}

/// A world whose snapshot ends BEFORE a re-opened freeze's holdout start: the search span still runs (its
/// defaulted end is the snapshot's last close), and the holdout window is empty by construction.
async fn world_with_an_empty_holdout() -> (World, pulse::FreezeRecord) {
    let world = world().await;
    let end_ms = m15_candles(SEED, BARS)
        .last()
        .expect("the series is non-empty")
        .close_time;
    let freezes = SqliteCertificationFreezeRepo::with_deps(
        world.db.pool().clone(),
        FakeClock::at(FREEZE_OPENED_MS),
    );
    freezes.close().await.expect("the world's freeze closes");
    let freeze = freezes
        .open(&OpenFreezeRequest {
            holdout_start_ms: end_ms + 1,
            h: H,
            alpha: "0.05".to_owned(),
            holdout_test: "one-sided lower confidence bound at 1 - 0.05/H (C1)".to_owned(),
        })
        .await
        .expect("the re-opened freeze starts its holdout past the snapshot's end");
    (world, freeze)
}

/// A review is an audit record: no edit, no delete (0022's triggers).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_power_review_record_is_immutable() {
    let world = world().await;
    review(&world, Some(&world.freeze))
        .await
        .expect("the review writes one row");

    let edit = sqlx::query("UPDATE power_review SET allowed = 0")
        .execute(world.db.pool())
        .await
        .expect_err("an edit is refused");
    assert!(edit.to_string().contains("immutable"), "{edit}");

    let delete = sqlx::query("DELETE FROM power_review")
        .execute(world.db.pool())
        .await
        .expect_err("a delete is refused");
    assert!(delete.to_string().contains("never deleted"), "{delete}");

    assert_eq!(power_review_rows(&world).await, 1, "the row survives both");
}

// ---------------------------------------------------------------------------
// The CLI arm (Approach 7)
// ---------------------------------------------------------------------------

/// `pulse certify power` prints every key of Approach 7 and carries no holdout outcome (G1); a refusal
/// exits non-zero and says `refused:`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_cli_arm_prints_every_key_and_no_outcome() {
    let world = world().await;
    let store = world.data_dir();
    let run = |extra: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_pulse"));
        cmd.arg("certify").arg("power");
        cmd.args(extra);
        cmd.output().expect("spawn pulse certify power")
    };

    let version = world.version.as_str().to_owned();
    let db = world.db_path.to_str().unwrap().to_owned();
    let store = store.to_str().unwrap().to_owned();
    let out = run(&["--version", &version, "--db", &db, "--store", &store]);
    assert!(
        out.status.success(),
        "exit {:?}; stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    for key in [
        "power review id:",
        "version:",
        "pair:",
        "freeze id:",
        "h:",
        "z:",
        "holdout window:",
        "holdout trades:",
        "search trades:",
        "search sigma (R):",
        "assumed edge (R):",
        "pass chance:",
        "bar:",
        "min holdout trades:",
        "search span passes:",
        "attempt allowed:",
        "holdout outcomes: not shown",
    ] {
        assert!(
            stdout.contains(key),
            "the print-out must carry `{key}`:\n{stdout}"
        );
    }
    assert!(
        stdout.contains("50.0%"),
        "the bar prints as a percentage:\n{stdout}"
    );
    let tokens = outcome_tokens(&stdout);
    assert!(
        tokens.is_empty(),
        "no outcome token reaches stdout, found {tokens:?}:\n{stdout}"
    );
    eprintln!("w1 CLI read-out:\n{stdout}");

    // A refused review: non-zero, and the refusal names itself on stderr.
    let refused = run(&[
        "--version",
        "no-such-version",
        "--db",
        &db,
        "--store",
        &store,
    ]);
    assert!(!refused.status.success(), "a refusal exits non-zero");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("refused:"),
        "the refusal is printed: {stderr}"
    );
    assert!(
        outcome_tokens(&stderr).is_empty(),
        "even the refusal carries no outcome token: {stderr}"
    );
}
