//! r1.s1.w3 AC-1 — the Strategy Library's read (ledger line `d2`'s own command).
//!
//! `d2` claims: "the Strategy Library lists every strategy and version persisted
//! in `pulse.db`, with per-version stats where a run exists". This test asserts
//! that claim against the transport-free core (`library_overview_core`) over a
//! migrated tempfile `pulse.db` seeded through the REAL repositories — never a
//! fixture row, never the real Application Support dir.
//!
//! Seeding mirrors `tests/strategy_persistence.rs` (strategy + versions via
//! `SqliteStrategyRepo`) and `tests/runs_cli.rs` (a persisted run via
//! `SqliteBacktestRunRepo::save_run` over a trade-free `BacktestResult` — the
//! content hash is derived from the run totals, not the summary columns, so a
//! hand-built `SummaryStats` round-trips and the KPIs the screen renders are the
//! ones this test pinned).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    AgentHypothesis, AgentName, BacktestInputs, BacktestResult, BacktestRunRepository, CreatedBy,
    DataVersion, DesktopState, EngineFingerprint, EquityCurve, FakeClock, FundingConfig,
    NewAgentSubmission, NewVersion, Pair, RegimeBreakdown, SkippedEntryCounts, SnapshotSelection,
    SqliteBacktestRunRepo, StrategyRepository, SummaryStats, Timeframe, VersionId,
    WalkForwardRunRepository, library_overview_core,
};
use support::mcp::seeded_walk_forward_draft;

/// The input provenance a fresh `save_run` now requires (r1.s3.w2, #110). These
/// tests are about coach/library behaviour, not provenance, so the tuple is a
/// plain complete single-timeframe one; `tests/backtest_provenance.rs` owns the
/// provenance shapes themselves.
fn seed_inputs() -> BacktestInputs {
    BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new("v-primary"),
        },
        htf: None,
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: FundingConfig::SnapshotRates,
        window: None,
        lead_in_from_ms: None,
    }
}
use rust_decimal::Decimal;
use tempfile::TempDir;

/// A valid canonical `1.0.0` RSI-oversold DSL (same shape as `runs_cli.rs`'s
/// `MINIMAL_DSL` — `create_version` validates, so the document must be real).
const RSI_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#;

/// A trade-free `BacktestResult` whose totals all zero — `save_run` derives the
/// integrity hash from these, so any trade-free result persists cleanly.
fn trade_free_result() -> BacktestResult {
    BacktestResult {
        trades: vec![],
        net_pnl: Decimal::ZERO,
        fees_total: Decimal::ZERO,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        regime_breakdown: RegimeBreakdown::new(),
        skipped_entries: SkippedEntryCounts::new(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::current(),
        summary: SummaryStats::default(),
        equity_curve: EquityCurve::default(),
    }
}

/// The summary a persisted run carries on its row — the KPI source the overview
/// must surface. `expectancy`/`win_rate`/`trade_count` are the three the screen
/// renders; the rest are zeroed (they are not this test's subject).
fn kpi_summary(expectancy_milli: i64, win_rate_thousandths: i64, trades: usize) -> SummaryStats {
    SummaryStats {
        expectancy: Decimal::new(expectancy_milli, 3),
        win_rate: Decimal::new(win_rate_thousandths, 3),
        trade_count: trades,
        ..SummaryStats::default()
    }
}

/// The seeded shape two strategies give the overview: Alpha carries a three-node
/// version CHAIN (`va1 -> va2 -> va3`, so parent ordering is assertable) with a
/// persisted run against `va1` and `va2`; Beta carries one root version with no
/// run. Run-bearing set = `{va1, va2}` exactly (grill A1's backend half: stats
/// are present iff a run exists).
async fn seeded_state() -> (DesktopState, TempDir, Vec<VersionId>, String) {
    let tmp = TempDir::new().expect("tempdir");
    let state = DesktopState::open(&tmp.path().join("pulse.db"))
        .await
        .expect("open + migrate a tempfile pulse.db");
    let strategies = state.strategy_repo();

    let alpha = strategies
        .create_strategy("Alpha", Some("r1.s1.w3"), &["btc".to_owned()])
        .await
        .expect("create Alpha");

    let mut parent = None;
    let mut alpha_versions = Vec::new();
    for _ in 0..3 {
        let created = strategies
            .create_version(NewVersion {
                strategy_id: alpha.id.clone(),
                parent_version_id: parent.clone(),
                dsl_json: RSI_DSL.to_owned(),
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await
            .expect("create an Alpha version");
        parent = Some(created.id.clone());
        alpha_versions.push(created.id);
    }

    let beta = strategies
        .create_strategy("Beta", None, &[])
        .await
        .expect("create Beta");
    let beta_root = strategies
        .create_version(NewVersion {
            strategy_id: beta.id.clone(),
            parent_version_id: None,
            dsl_json: RSI_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create Beta's version");

    // A pinned version is real record state the card renders a marker from.
    strategies
        .set_pinned_version(&alpha.id, Some(&alpha_versions[1]))
        .await
        .expect("pin Alpha's second version");

    // Runs against va1 (expectancy +0.30R) and va2 (+0.42R) — so va2 carries a
    // delta vs its parent AND va3/Beta's version prove the no-run half.
    let runs = state.backtest_run_repo();
    let result = trade_free_result();
    runs.save_run(
        &alpha_versions[0],
        &seed_inputs(),
        &result,
        &kpi_summary(300, 462, 38),
        Decimal::new(10_000, 0),
    )
    .await
    .expect("save va1's run");
    runs.save_run(
        &alpha_versions[1],
        &seed_inputs(),
        &result,
        &kpi_summary(420, 483, 64),
        Decimal::new(10_000, 0),
    )
    .await
    .expect("save va2's run");

    (state, tmp, alpha_versions, beta_root.id.as_str().to_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overview_lists_every_strategy_and_version_with_stats_iff_a_run_exists() {
    let (state, _tmp, alpha_versions, beta_root) = seeded_state().await;

    let overview = library_overview_core(&state)
        .await
        .expect("the library read succeeds over the seeded db");

    // Every strategy is listed.
    assert_eq!(
        overview.strategies.len(),
        2,
        "both seeded strategies listed"
    );
    let alpha = overview
        .strategies
        .iter()
        .find(|s| s.name == "Alpha")
        .expect("Alpha is listed");
    let beta = overview
        .strategies
        .iter()
        .find(|s| s.name == "Beta")
        .expect("Beta is listed");

    // Every version is listed, and the tree order is parent-before-child.
    assert_eq!(alpha.versions.len(), 3, "all three Alpha versions listed");
    assert_eq!(beta.versions.len(), 1, "Beta's version listed");
    let pos = |id: &str| {
        alpha
            .versions
            .iter()
            .position(|v| v.id == id)
            .unwrap_or_else(|| panic!("version {id} missing from the overview"))
    };
    let (va1, va2, va3) = (&alpha_versions[0], &alpha_versions[1], &alpha_versions[2]);
    assert!(
        pos(va1.as_str()) < pos(va2.as_str()),
        "va1 precedes its child va2"
    );
    assert!(
        pos(va2.as_str()) < pos(va3.as_str()),
        "va2 precedes its child va3"
    );
    assert_eq!(beta.versions[0].id, beta_root);

    // Stats are present for exactly the run-bearing versions, carrying the
    // persisted run's own KPIs. `wire(id)` is the version's projection.
    let wire = |id: &str| &alpha.versions[pos(id)];
    let kpi = wire(va1.as_str())
        .stats
        .as_ref()
        .expect("va1 has a run -> stats");
    assert_eq!(kpi.expectancy, "+0.3R");
    assert_eq!(kpi.win_rate, "46.2%");
    assert_eq!(kpi.trades, 38);
    let kpi = wire(va2.as_str())
        .stats
        .as_ref()
        .expect("va2 has a run -> stats");
    assert_eq!(kpi.expectancy, "+0.42R");
    assert_eq!(kpi.win_rate, "48.3%");
    assert_eq!(kpi.trades, 64);
    assert!(
        wire(va3.as_str()).stats.is_none(),
        "no run on va3 -> no stats (A1)"
    );
    assert!(
        beta.versions[0].stats.is_none(),
        "no run on Beta's version -> no stats (A1)"
    );

    // The run-bearing child's expectancy delta vs its run-bearing parent.
    assert!(
        wire(va1.as_str()).delta_vs_parent.is_none(),
        "a root version has no parent delta"
    );
    assert_eq!(
        wire(va2.as_str()).delta_vs_parent.as_deref(),
        Some("+0.12R")
    );
    assert!(
        wire(va3.as_str()).delta_vs_parent.is_none(),
        "va3 has no run"
    );

    // Each version carries its DSL summary — the fields `StrategyDsl` actually
    // has, derived from the seeded document.
    for version in alpha.versions.iter().chain(beta.versions.iter()) {
        assert_eq!(version.dsl.name, "RSI Oversold");
        assert_eq!(version.dsl.direction, "long");
        assert_eq!(version.dsl.entry, vec!["rsi(14) < 30".to_owned()]);
        assert_eq!(
            version.dsl.exits,
            vec!["stop 5%".to_owned(), "take profit 2R".to_owned()]
        );
        assert_eq!(
            version.dsl.risk,
            vec!["risk 1% per trade".to_owned(), "max leverage 3x".to_owned()]
        );
        assert!(
            version.dsl.filters.is_empty(),
            "the seeded DSL has no filters — the summary carries that truth"
        );
    }

    // Recent runs: the run-bearing versions list their run, the others none.
    assert_eq!(wire(va1.as_str()).recent_runs.len(), 1);
    assert_eq!(wire(va2.as_str()).recent_runs.len(), 1);
    assert_eq!(wire(va2.as_str()).recent_runs[0].expectancy, "+0.42R");
    assert_eq!(wire(va2.as_str()).recent_runs[0].trades, 64);
    assert!(wire(va3.as_str()).recent_runs.is_empty());
    assert!(beta.versions[0].recent_runs.is_empty());

    // The pinned marker rides the strategy record.
    assert_eq!(alpha.pinned_version_id.as_deref(), Some(va2.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_database_lists_no_strategies() {
    let tmp = TempDir::new().expect("tempdir");
    let state = DesktopState::open(&tmp.path().join("pulse.db"))
        .await
        .expect("open a fresh db");

    let overview = library_overview_core(&state)
        .await
        .expect("the library read succeeds over an empty db");

    assert!(
        overview.strategies.is_empty(),
        "a fresh database reads as zero strategies — the screen's empty state (G4)"
    );
}

// ---------------------------------------------------------------------------
// r2.s1.w4 C1 — provenance and hypothesis on the wire
// ---------------------------------------------------------------------------

/// Every version carries its `created_by` label; an `external_agent` version
/// additionally carries the submission's normalized agent name and trimmed
/// hypothesis. A human version carries `human` and no submission fields, and an
/// agent version whose submission row is absent is NOT an error — the label
/// still reads `external_agent`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_and_hypothesis_reach_the_wire_per_version_kind() {
    let (state, _tmp, alpha_versions, _beta_root) = seeded_state().await;
    let strategies = state.strategy_repo();
    let alpha_id = strategies
        .list_strategies(true)
        .await
        .expect("list strategies")
        .into_iter()
        .find(|s| s.name == "Alpha")
        .expect("Alpha is seeded")
        .id;

    // An external-agent child of va3 with a submission row (the w1 audit seam).
    let (agent_version, _submission) = strategies
        .create_agent_version(
            NewVersion {
                strategy_id: alpha_id.clone(),
                parent_version_id: Some(alpha_versions[2].clone()),
                dsl_json: RSI_DSL.to_owned(),
                created_by: CreatedBy::ExternalAgent,
                creating_llm_call_ids: vec![],
            },
            NewAgentSubmission {
                agent_name: AgentName::parse("Claude-Code").expect("valid agent name"),
                hypothesis: AgentHypothesis::parse("  A wider stop cuts noise exits.  ")
                    .expect("valid hypothesis"),
            },
        )
        .await
        .expect("create the agent version");

    // An `external_agent` version WITHOUT a submission row. `create_version`
    // does not guard `created_by`, so this is exactly the migration-era /
    // hand-written shape the projection must tolerate rather than fail on.
    let bare_agent = strategies
        .create_version(NewVersion {
            strategy_id: alpha_id,
            parent_version_id: Some(agent_version.id.clone()),
            dsl_json: RSI_DSL.to_owned(),
            created_by: CreatedBy::ExternalAgent,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create a submission-less agent version");

    let overview = library_overview_core(&state)
        .await
        .expect("the library read succeeds over the seeded db");
    let alpha = overview
        .strategies
        .iter()
        .find(|s| s.name == "Alpha")
        .expect("Alpha is listed");
    let wire = |id: &VersionId| {
        alpha
            .versions
            .iter()
            .find(|v| v.id == id.as_str())
            .unwrap_or_else(|| panic!("version {} in the overview", id.as_str()))
    };

    let agent = wire(&agent_version.id);
    assert_eq!(agent.created_by, "external_agent");
    assert_eq!(
        agent.agent_name.as_deref(),
        Some("claude-code"),
        "the submission's normalized (lowercased) name reaches the wire"
    );
    assert_eq!(
        agent.hypothesis.as_deref(),
        Some("A wider stop cuts noise exits."),
        "the hypothesis crosses trimmed, exactly as the submission stored it"
    );

    let human = wire(&alpha_versions[0]);
    assert_eq!(human.created_by, "human");
    assert_eq!(human.agent_name, None);
    assert_eq!(human.hypothesis, None);

    let bare = wire(&bare_agent.id);
    assert_eq!(bare.created_by, "external_agent");
    assert_eq!(
        bare.agent_name, None,
        "a missing submission row is not an error"
    );
    assert_eq!(bare.hypothesis, None);
}

// ---------------------------------------------------------------------------
// r2.s3.w4 — a12: the Library wire carries certification
// ---------------------------------------------------------------------------

/// `LibraryVersion.certified`/`latest_walk_forward_run_id` are filled from the
/// version's joined latest walk-forward run: `true` + the run id when it
/// passed, `false` + `None` everywhere else — never invented.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn certification_fields_reach_the_wire() {
    let (state, _tmp, alpha_versions, _beta_root) = seeded_state().await;

    // Certify va1 with a synthetic passing walk-forward — the pointer +
    // recorded `pass` is the seam, and `save_walk_forward_run` is the only
    // product path that moves it.
    let wf_id = SqliteBacktestRunRepo::with_deps(
        state.db().pool().clone(),
        FakeClock::at(1_756_512_000_000),
    )
    .save_walk_forward_run(&alpha_versions[0], &seeded_walk_forward_draft(true))
    .await
    .expect("the certifying walk-forward run persists");

    let overview = library_overview_core(&state)
        .await
        .expect("the library read succeeds");
    let alpha = overview
        .strategies
        .iter()
        .find(|s| s.name == "Alpha")
        .expect("Alpha is listed");
    let beta = overview
        .strategies
        .iter()
        .find(|s| s.name == "Beta")
        .expect("Beta is listed");
    let wire = |id: &VersionId| {
        alpha
            .versions
            .iter()
            .find(|v| v.id == id.as_str())
            .unwrap_or_else(|| panic!("version {} in the overview", id.as_str()))
    };

    let certified = wire(&alpha_versions[0]);
    assert!(certified.certified, "va1's latest walk-forward passed");
    assert_eq!(
        certified.latest_walk_forward_run_id.as_deref(),
        Some(wf_id.as_str()),
        "the pointer names the certifying run"
    );

    for id in [&alpha_versions[1], &alpha_versions[2]] {
        let version = wire(id);
        assert!(!version.certified, "{}: no run — uncertified", id.as_str());
        assert_eq!(version.latest_walk_forward_run_id, None);
    }
    assert!(!beta.versions[0].certified);
    assert_eq!(beta.versions[0].latest_walk_forward_run_id, None);
}

// ---------------------------------------------------------------------------
// R2 — the latest run the screen may compare against is not a fold
// ---------------------------------------------------------------------------

/// R2: the Lab's parent comparison names the version's latest NON-fold run.
///
/// After a walk-forward completes, its folds head the run catalogue: they are
/// ordinary rows (L8 — so they stay listed) that share ONE `created_at`, and here
/// that instant is strictly later than the version's ordinary run. So
/// `recent_runs[0]` is an arbitrary UUID-selected sub-window fold, which is
/// exactly what the screen used to read as "the latest run" — while `latest_run`
/// answers the ordinary run, from the same `latest_run_for_version` read the KPIs
/// use (N1's discriminator).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_run_is_the_version_latest_non_fold_run() {
    let (state, _tmp, alpha_versions, _beta_root) = seeded_state().await;

    // A walk-forward strictly LATER than the SystemClock-dated ordinary run, so
    // its folds really do head the catalogue (that ordering is the bug's setup).
    let late = SqliteBacktestRunRepo::with_deps(
        state.db().pool().clone(),
        FakeClock::at(1_900_000_000_000),
    );
    let wf_id = late
        .save_walk_forward_run(&alpha_versions[1], &seeded_walk_forward_draft(true))
        .await
        .expect("the walk-forward run persists");
    let wf = late
        .get_walk_forward_run(&wf_id)
        .await
        .expect("the walk-forward reads back")
        .expect("the run exists");
    let fold_ids: Vec<&str> = wf
        .folds
        .iter()
        .map(|f| f.backtest_run_id.as_str())
        .collect();
    assert_eq!(fold_ids.len(), 2, "the fixture is a two-fold run");

    let overview = library_overview_core(&state)
        .await
        .expect("the library read succeeds");
    let va2 = overview
        .strategies
        .iter()
        .flat_map(|s| s.versions.iter())
        .find(|v| v.id == alpha_versions[1].as_str())
        .expect("va2 is listed");

    // The catalogue keeps every row, folds included — and a fold heads it.
    assert!(
        fold_ids.contains(&va2.recent_runs[0].id.as_str()),
        "a fold heads recent_runs: {}",
        va2.recent_runs[0].id
    );

    // The latest run is NOT that fold, and it is older than it.
    let latest = va2.latest_run.as_ref().expect("va2 has an ordinary run");
    assert!(
        !fold_ids.contains(&latest.id.as_str()),
        "the latest run is not a fold: {}",
        latest.id
    );
    assert!(
        latest.created_at < va2.recent_runs[0].created_at,
        "the fold is NEWER than the latest run — the selection this fixes"
    );

    // The KPIs answer from that same ordinary run (N1's half).
    assert_eq!(
        va2.stats.as_ref().map(|s| s.trades),
        Some(64),
        "va2's KPIs are its ordinary run's"
    );

    // And the certification pointer is the walk-forward, which is not the latest
    // RUN — the two are different questions about the same version.
    assert_eq!(
        va2.latest_walk_forward_run_id.as_deref(),
        Some(wf_id.as_str())
    );
}
