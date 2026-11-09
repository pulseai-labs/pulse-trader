//! r7.s1.w1 — the fixture's history (ADR-0027 amendment, PDR-0051 decision 2).
//!
//! The `pulse fixture seed` command is retired. The deterministic generator
//! stays: it still yields a real wf-v1 pass on this build (the promotion-gate
//! tests certify over it) and its data versions still equal the two the seeder
//! stamped. These proofs moved here from `tests/certify_fixture.rs` cases (i)
//! and (ii). An existing fixture session reading fixture / OOS n/a is
//! `tests/paper_promotion_gate.rs` case (vi).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)] // the case bodies assert whole scenarios

mod support;

use std::process::Command;

use pulse::{
    BacktestConfig, BinanceAdapter, CandleSeries, CandleStore, CreatedBy, FoldScheme, NewVersion,
    SnapshotPins, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyRepository, Timeframe,
    VerdictRule, WalkForwardRequest, fixture_h4_candles, fixture_m15_candles, fixture_pair,
    fixture_strategy_dsl, run_walk_forward,
};
use support::mcp::migrated_db;
use tempfile::TempDir;

/// Write both fixture series as snapshots (never HEAD) and mint the fixture strategy
/// and its version.
async fn stamp_snapshots_and_strategy(
    store: &CandleStore,
    strategies: &SqliteStrategyRepo<pulse::SystemClock>,
) -> (pulse::VersionId, pulse::DataVersion, pulse::DataVersion) {
    let pair = fixture_pair();
    let m15 = fixture_m15_candles();
    let h4 = fixture_h4_candles();
    let m15_version = CandleStore::content_version(&pair, Timeframe::M15, &m15);
    let h4_version = CandleStore::content_version(&pair, Timeframe::H4, &h4);
    store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe: Timeframe::M15,
            version: m15_version.clone(),
            candles: m15,
        })
        .unwrap();
    store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe: Timeframe::H4,
            version: h4_version.clone(),
            candles: h4,
        })
        .unwrap();
    let strategy = strategies
        .create_strategy(
            pulse::FIXTURE_STRATEGY_NAME,
            None,
            &[pulse::FIXTURE_STRATEGY_TAG.to_owned()],
        )
        .await
        .unwrap();
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&fixture_strategy_dsl()).unwrap(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap();
    (version.id, m15_version, h4_version)
}

/// The pinned fixture walk-forward request (K default, snapshots pinned —
/// never HEAD).
fn fixture_walk_forward_request(
    version_id: pulse::VersionId,
    m15_version: pulse::DataVersion,
    h4_version: pulse::DataVersion,
) -> WalkForwardRequest {
    WalkForwardRequest {
        version_id,
        pair: fixture_pair(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: BacktestConfig::default(),
        snapshots: Some(SnapshotPins {
            primary: m15_version,
            htf: Some(h4_version),
            d1: None,
        }),
        from_ms: None,
        to_ms: None,
        k: None,
        rule: None,
    }
}

/// The fixture's own walk-forward is a real wf-v1 pass — six
/// folds, every fold `n >= 20` and holding, the run passing on the CURRENT
/// build's engine fingerprint. The series ride `SnapshotPins`, so the proof
/// never reads HEAD and never fabricates a verdict.
#[tokio::test]
async fn the_generator_walk_forward_is_a_real_wf_v1_pass_on_this_build() {
    let tmp = TempDir::new().unwrap();
    let (_path, db) = migrated_db(&tmp).await;
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let store = CandleStore::with_base_dir(tmp.path().join("candles"));

    let (version_id, m15_version, h4_version) =
        stamp_snapshots_and_strategy(&store, &strategies).await;
    let outcome = run_walk_forward(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &fixture_walk_forward_request(version_id, m15_version, h4_version),
        None,
    )
    .await
    .expect("the fixture walk-forward runs on this build");

    let run = &outcome.run;
    assert_eq!(
        run.rule,
        VerdictRule::WfV1,
        "the fixture is judged by wf-v1, unchanged"
    );
    assert_eq!(
        run.scheme,
        FoldScheme::RollingOos { k: 6 },
        "K stays wf-v1's 6"
    );
    assert_eq!(run.folds.len(), 6);
    assert!(
        run.verdict.pass,
        "the fixture walk-forward must genuinely pass wf-v1; folds: {:?}",
        run.folds
            .iter()
            .map(|f| (f.verdict.n, f.verdict.mean_r, f.verdict.lower_bound))
            .collect::<Vec<_>>()
    );
    for fold in &run.folds {
        assert!(
            fold.verdict.holds,
            "every fold must hold wf-v1 (n >= 20, lower bound > 0); got {:?}",
            fold.verdict
        );
        assert!(
            fold.verdict.n >= 20,
            "every fold must trade at least N_MIN=20 times; got {}",
            fold.verdict.n
        );
    }

    // Evidence for the work-item report (seed, length, strategy, per-fold stats).
    let fold_evidence = run
        .folds
        .iter()
        .map(|f| {
            format!(
                "fold {}: n={} mean_r={:.4} lower_bound={:.6} holds={}",
                f.index, f.verdict.n, f.verdict.mean_r, f.verdict.lower_bound, f.verdict.holds
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    println!(
        "FIXTURE PROOF: pair={} seed=0x{:016X} m15_bars={} h4_bars={} strategy=\"{}\" \
         pass={} pooled_lower_bound={:.6} folds[{}]",
        fixture_pair(),
        pulse::FIXTURE_SEED,
        fixture_m15_candles().len(),
        fixture_h4_candles().len(),
        pulse::FIXTURE_STRATEGY_NAME,
        run.verdict.pass,
        run.verdict.pooled.lower_bound,
        fold_evidence,
    );
}

/// The pinned M15 data version (spec §5: a stable contract, not a value that
/// drifts with the generator's internals).
const PINNED_M15_DATA_VERSION: &str = "0a2c929a27848083";

/// The pinned H4 data version.
const PINNED_H4_DATA_VERSION: &str = "76f15836cc357256";

/// The generator's data versions equal the pinned constants — the two a pre-r7
/// database's `fixture_snapshot` rows name.
#[tokio::test]
async fn the_generator_data_versions_are_the_pinned_constants() {
    let pair = fixture_pair();
    let m15_version = CandleStore::content_version(&pair, Timeframe::M15, &fixture_m15_candles());
    let h4_version = CandleStore::content_version(&pair, Timeframe::H4, &fixture_h4_candles());
    assert_eq!(
        m15_version.as_str(),
        PINNED_M15_DATA_VERSION,
        "M15 data version"
    );
    assert_eq!(
        h4_version.as_str(),
        PINNED_H4_DATA_VERSION,
        "H4 data version"
    );
}

/// `pulse fixture …` is clap's unknown-subcommand refusal, and `pulse --help`
/// no longer lists a `fixture` command.
#[test]
fn the_fixture_seed_command_is_gone() {
    for args in [["fixture", "seed"], ["fixture", "--help"]] {
        let out = Command::new(env!("CARGO_BIN_EXE_pulse"))
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "`pulse {}` must be refused with exit 2; stderr: {stderr}",
            args.join(" ")
        );
        assert!(
            stderr.contains("unrecognized subcommand 'fixture'"),
            "`pulse {}` must be clap's unknown-subcommand refusal; stderr: {stderr}",
            args.join(" ")
        );
    }

    let help = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success(), "`pulse --help` must succeed");
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(
        !stdout
            .lines()
            .any(|l| l.trim_start().starts_with("fixture")),
        "`pulse --help` must not list a fixture command; stdout: {stdout}"
    );
}
