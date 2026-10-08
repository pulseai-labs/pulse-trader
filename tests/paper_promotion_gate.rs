//! r3.s4.w2 — AC-1: the promotion gate (`paper_promotion_gate`, ledger d57).
//!
//! A version is promoted to a live paper session either by a CERTIFIED
//! path (its latest walk-forward run passed on THIS build; the session names
//! the run and the fold runs' data versions) or by an OVERRIDE (a human's
//! reasoned manual promotion, which is never a live basis and never
//! OOS-comparable). Everything runs on a real migrated database through the
//! real use case (`promote`) — the refusals below are the gate's typed
//! errors, and "no row" is asserted against the actual table.
//!
//! The seven cases:
//!
//!   i. **Certified.** A certified version on this build promotes as
//!      `certified`, with the run id and the fold runs' data versions.
//!  ii. **Uncertified, no override.** Refused as `Uncertified`; no row.
//! iii. **Empty override.** An empty or whitespace-only reason is refused by
//!      the type AND by the table `CHECK`.
//!  iv. **Override.** A non-empty reason promotes as `override`, with its
//!      reason and `at`.
//!   v. **E2.** A passing walk-forward with a foreign `engine_fingerprint`
//!      refuses as `CertifiedUnderOtherEngine`, naming both fingerprints —
//!      also with an override supplied; no row.
//!  vi. **Fixture.** Fixture data gives `fixture = true` and
//!      `oos_comparable() = false`; non-fixture certified data gives
//!      `fixture = false` and `oos_comparable() = true`; an override session
//!      is never OOS-comparable.
//! vii. **`promoted_by`.** Every promotion records the label it was given;
//!      an empty label is refused.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)] // the case bodies assert whole scenarios

mod support;

use pulse::{
    BacktestConfig, BacktestInputs, BacktestRequest, BacktestResult, BacktestRunId,
    BacktestRunRepository, BinanceAdapter, CandleSeries, CandleStore, CandleWindow, CreatedBy,
    DataError, DataVersion, Db, EngineFingerprint, FakeClock, FoldScheme, FoldVerdict,
    FundingConfig, Graduation, LatestReadableRun, NewVersion, NonEmptyLabel, NonEmptyReason,
    PaperPromotionError, PaperSessionRepository, PersistedRun, PromotionRefused, RunSummary,
    RunVerdict, SnapshotPins, SnapshotSelection, SqliteBacktestRunRepo, SqlitePaperSessionRepo,
    SqliteStrategyRepo, StrategyRepository, SummaryStats, Timeframe, Trade, VerdictRule,
    WalkForwardFoldDraft, WalkForwardRequest, WalkForwardRunDraft, WalkForwardRunRepository,
    decide_promotion, fixture_h4_candles, fixture_h4_candles_from, fixture_m15_candles,
    fixture_pair, fixture_strategy_dsl, promote, run_version_backtest, run_walk_forward,
};
use rust_decimal::Decimal;
use support::mcp::migrated_db;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// World: a real migrated db, and a version whose certification we control.
// ---------------------------------------------------------------------------

struct World {
    _tmp: TempDir,
    db: Db,
    strategies: SqliteStrategyRepo<pulse::SystemClock>,
    runs: SqliteBacktestRunRepo<pulse::SystemClock>,
    paper: SqlitePaperSessionRepo<FakeClock>,
    store: CandleStore,
}

/// The test clock: a fixed instant, so an override's `at` is assertable.
const NOW_MS: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00Z

async fn world() -> World {
    let tmp = TempDir::new().unwrap();
    let (_path, db) = migrated_db(&tmp).await;
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let store = CandleStore::with_base_dir(tmp.path().join("candles"));
    let paper =
        SqlitePaperSessionRepo::with_clock(db.pool().clone(), FakeClock::at(NOW_MS), store.clone());
    World {
        _tmp: tmp,
        db,
        strategies,
        runs,
        paper,
        store,
    }
}

/// Write ONE series as a content-addressed snapshot (never HEAD).
fn write_series_snapshots(
    world: &World,
    pair: &pulse::Pair,
    timeframe: Timeframe,
    version: pulse::DataVersion,
    candles: Vec<pulse::Candle>,
) {
    world
        .store
        .write_snapshot(&CandleSeries {
            pair: pair.clone(),
            timeframe,
            version,
            candles,
        })
        .unwrap();
}

/// The fixture series, written as snapshots (never HEAD), with their versions.
fn write_fixture_snapshots(world: &World) -> (pulse::DataVersion, pulse::DataVersion) {
    let pair = fixture_pair();
    let m15 = fixture_m15_candles();
    let h4 = fixture_h4_candles();
    let m15_version = CandleStore::content_version(&pair, Timeframe::M15, &m15);
    let h4_version = CandleStore::content_version(&pair, Timeframe::H4, &h4);
    write_series_snapshots(world, &pair, Timeframe::M15, m15_version.clone(), m15);
    write_series_snapshots(world, &pair, Timeframe::H4, h4_version.clone(), h4);
    (m15_version, h4_version)
}

/// The standard real-backtest request: the fixture pair, M15 primary + H4
/// htf, default config, the snapshots pinned (never HEAD), whole window.
fn backtest_request(
    version_id: &pulse::VersionId,
    m15_version: pulse::DataVersion,
    h4_version: pulse::DataVersion,
) -> BacktestRequest {
    BacktestRequest {
        version_id: version_id.clone(),
        pair: fixture_pair(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: BacktestConfig::default(),
        snapshots: Some(SnapshotPins {
            primary: m15_version,
            htf: Some(h4_version),
            d1: None,
        }),
        window: None,
    }
}

/// The standard walk-forward request over the same pinned series (K default).
fn walk_forward_request(
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

/// A version whose LATEST walk-forward run is whatever this build produces
/// over the fixture series (a real pass — the fixture proof, `certify_fixture`
/// case (i), guarantees wf-v1 holds for it).
async fn certified_version(world: &World, name: &str) -> pulse::VersionId {
    certified_version_over(world, name, fixture_m15_candles()).await
}

/// The same, over a caller-chosen M15 series — the shifted-slice variant
/// certifies over content that is NOT the fixture stamp (case vi).
async fn certified_version_over(
    world: &World,
    name: &str,
    m15: Vec<pulse::Candle>,
) -> pulse::VersionId {
    let pair = fixture_pair();
    let m15_version = CandleStore::content_version(&pair, Timeframe::M15, &m15);
    let h4 = fixture_h4_candles_from(&m15);
    let h4_version = CandleStore::content_version(&pair, Timeframe::H4, &h4);
    write_series_snapshots(world, &pair, Timeframe::M15, m15_version.clone(), m15);
    write_series_snapshots(world, &pair, Timeframe::H4, h4_version.clone(), h4);
    let version_id = create_fixture_version(world, name).await;
    let outcome = run_walk_forward(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &walk_forward_request(version_id.clone(), m15_version, h4_version),
        None,
    )
    .await
    .unwrap();
    assert!(
        outcome.run.verdict.pass,
        "the fixture walk-forward must pass on this build"
    );
    version_id
}

/// Mint the fixture strategy + version under `name` (the shared document).
async fn create_fixture_version(world: &World, name: &str) -> pulse::VersionId {
    let strategy = world
        .strategies
        .create_strategy(name, None, &[])
        .await
        .unwrap();
    world
        .strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&fixture_strategy_dsl()).unwrap(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap()
        .id
}

/// A version with NO walk-forward run at all.
async fn uncertified_version(world: &World, name: &str) -> pulse::VersionId {
    let strategy = world
        .strategies
        .create_strategy(name, None, &[])
        .await
        .unwrap();
    world
        .strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&fixture_strategy_dsl()).unwrap(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .unwrap()
        .id
}

/// The shared persisted-walk-forward scenario pieces: a REAL engine run over
/// a fixture-pinned version, and that run's real trades split across two
/// contiguous fold windows by exit fill time. Every persisted-draft helper
/// below recombines these pieces into its own shape — the foreign-fingerprint
/// pre-upgrade row, the D1-pinned certification, the sub-N_MIN failing run.
struct RunScenario {
    version_id: pulse::VersionId,
    outcome: pulse::BacktestOutcome,
    first: i64,
    span: CandleWindow,
    windows: [CandleWindow; 2],
    fold_trades: [Vec<Trade>; 2],
}

impl RunScenario {
    /// The scenario over `version_id`: write the fixture snapshots and run
    /// the REAL engine over the pinned series (this build).
    async fn new(world: &World, version_id: pulse::VersionId) -> Self {
        let (m15_version, h4_version) = write_fixture_snapshots(world);
        let outcome = run_version_backtest(
            &world.strategies,
            &world.store,
            &BinanceAdapter::new(),
            &world.runs,
            &backtest_request(&version_id, m15_version, h4_version),
            None,
        )
        .await
        .unwrap();
        let candles = &outcome.primary.candles;
        let first = candles.first().unwrap().open_time;
        let last_close = candles.last().unwrap().open_time + 900_000;
        let mid = candles[candles.len() / 2].open_time;
        let fold_trades = [
            outcome
                .trades
                .iter()
                .filter(|t| t.exit_fill_time < mid)
                .cloned()
                .collect(),
            outcome
                .trades
                .iter()
                .filter(|t| t.exit_fill_time >= mid)
                .cloned()
                .collect(),
        ];
        Self {
            version_id,
            outcome,
            first,
            span: CandleWindow {
                from_ms: first,
                to_ms: last_close,
            },
            windows: [
                CandleWindow {
                    from_ms: first,
                    to_ms: mid,
                },
                CandleWindow {
                    from_ms: mid,
                    to_ms: last_close,
                },
            ],
            fold_trades,
        }
    }

    /// The two fold drafts + the verdict the REAL wf-v1 assessment computes,
    /// shaped by the caller: which trades each fold keeps and what typed-input
    /// preparation (the D1 pin) every fold's inputs receive. Each fold's
    /// result is the run's real persisted result pieces with the fold's own
    /// trades and the caller's fingerprint; the equity curve is never
    /// persisted — the same rebuild the read-back performs.
    fn fold_drafts(
        &self,
        fingerprint: &str,
        keep_trades: impl Fn(&[Trade]) -> Vec<Trade>,
        prepare_inputs: impl Fn(&mut BacktestInputs),
    ) -> (Vec<WalkForwardFoldDraft>, RunVerdict) {
        let equity_curve = self.outcome.equity_curve();
        let mut fold_verdicts = Vec::new();
        let mut pooled_rs: Vec<Decimal> = Vec::new();
        let mut folds = Vec::new();
        for (index, (window, raw_trades)) in self.windows.iter().zip(&self.fold_trades).enumerate()
        {
            let trades = keep_trades(raw_trades);
            let rs: Vec<Decimal> = trades.iter().map(|t| t.realized_r).collect();
            let fold_verdict = FoldVerdict::from_rs(&rs);
            pooled_rs.extend_from_slice(&rs);
            fold_verdicts.push(fold_verdict.clone());
            let mut inputs = self.outcome.inputs.clone();
            inputs.window = Some(window.clone());
            inputs.lead_in_from_ms = Some(self.first);
            prepare_inputs(&mut inputs);
            folds.push(WalkForwardFoldDraft {
                index: u8::try_from(index).unwrap(),
                window: window.clone(),
                verdict: fold_verdict,
                inputs,
                result: BacktestResult {
                    trades,
                    equity_curve: equity_curve.clone(),
                    net_pnl: self.outcome.run.net_pnl,
                    fees_total: self.outcome.run.fees_total,
                    funding_total: self.outcome.run.funding_total,
                    slippage_total: self.outcome.run.slippage_total,
                    regime_breakdown: self.outcome.run.regime_breakdown,
                    skipped_entries: self.outcome.run.skipped_entries,
                    open_position: self.outcome.run.open_position.clone(),
                    engine_fingerprint: EngineFingerprint::from_stored(fingerprint.to_owned()),
                    summary: self.outcome.run.summary.clone(),
                },
                summary: self.outcome.run.summary.clone(),
                starting_equity: self.outcome.run.starting_equity,
            });
        }
        let verdict = RunVerdict::assess(&fold_verdicts, &pooled_rs);
        (folds, verdict)
    }
}

/// Persist the draft through the REAL one-transaction walk-forward save.
async fn save_walk_forward(
    world: &World,
    version_id: &pulse::VersionId,
    fingerprint: &str,
    span: CandleWindow,
    folds: Vec<WalkForwardFoldDraft>,
    verdict: RunVerdict,
) {
    world
        .runs
        .save_walk_forward_run(
            version_id,
            &WalkForwardRunDraft {
                scheme: FoldScheme::rolling_oos(2).unwrap(),
                rule: VerdictRule::WfV1,
                span,
                from_defaulted: false,
                engine_fingerprint: fingerprint.to_owned(),
                verdict,
                folds,
            },
        )
        .await
        .unwrap();
}

/// The pre-upgrade state, honestly persisted: a walk-forward run whose
/// verdict PASSED but whose fingerprint is a foreign build's — exactly what
/// an older passing build would have written. `walk_forward_run` is
/// immutable, so this state can ONLY be created by the build that made it;
/// that is the point of the case: a promotion reading this row reads a
/// certification made by another engine.
async fn foreign_fingerprint_version(world: &World, name: &str) -> pulse::VersionId {
    const FOREIGN: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    let version_id = create_fixture_version(world, name).await;
    let scenario = RunScenario::new(world, version_id.clone()).await;
    let (folds, verdict) = scenario.fold_drafts(FOREIGN, <[pulse::Trade]>::to_vec, |_| ());
    assert!(
        verdict.pass,
        "the fabricated pre-upgrade run must pass (real fixture trades)"
    );
    save_walk_forward(
        world,
        &scenario.version_id,
        FOREIGN,
        scenario.span,
        folds,
        verdict,
    )
    .await;
    scenario.version_id
}

/// Promote with the given override, asserting the outcome.
async fn try_promote(
    world: &World,
    version_id: &pulse::VersionId,
    override_request: Option<pulse::OverrideRequest>,
    label: &str,
) -> Result<pulse::PaperSession, PaperPromotionError> {
    let clock = FakeClock::at(NOW_MS);
    promote(
        &world.strategies,
        &world.runs,
        &world.runs,
        &world.paper,
        &clock,
        version_id,
        override_request.as_ref(),
        NonEmptyLabel::try_new(label).unwrap(),
    )
    .await
}

async fn session_count(world: &World) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM paper_session")
        .fetch_one(world.db.pool())
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------

/// (i) A certified version promotes as `certified`, naming the run and the
/// fold runs' data versions, with the A1 session settings.
#[tokio::test]
async fn i_certified_promotion_names_run_and_data_versions() {
    let world = world().await;
    let version_id = certified_version(&world, "certified-promotion").await;
    let session = try_promote(&world, &version_id, None, "operator-token")
        .await
        .expect("a certified version promotes");

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
    // The named run is the version's certifying run.
    let run = world
        .runs
        .get_walk_forward_run(walk_forward_run_id)
        .await
        .unwrap()
        .expect("the named run exists");
    assert_eq!(&run.strategy_version_id, &version_id);
    assert!(run.verdict.pass);
    // The data versions are the fold inputs' distinct (timeframe, version)
    // pairs — the fixture runs M15 + H4.
    assert_eq!(data_versions.len(), 2);
    assert!(data_versions.iter().any(|v| v.timeframe == Timeframe::M15));
    assert!(data_versions.iter().any(|v| v.timeframe == Timeframe::H4));
    // A10: the session's timeframes come from the same inputs.
    assert_eq!(session.pair, fixture_pair());
    assert_eq!(session.primary_timeframe, Timeframe::M15);
    assert_eq!(session.htf_timeframe, Some(Timeframe::H4));
    assert!(!session.uses_d1);
    // A1 settings on every session.
    assert_eq!(session.starting_equity, rust_decimal::Decimal::from(10_000));
    assert_eq!(session.taker_fee_bps, rust_decimal::Decimal::from(4));
    assert_eq!(session.slippage_bps, rust_decimal::Decimal::from(1));
    assert_eq!(session.min_trades, 20);
    // The fingerprint at start is THIS build's.
    assert_eq!(
        session.engine_fingerprint,
        pulse::EngineFingerprint::current()
    );
    // The row round-trips through the repository.
    let read_back = world.paper.get_session(&session.id).await.unwrap().unwrap();
    assert_eq!(read_back, session);
}

/// (ii) An uncertified version refuses as `Uncertified`; no row is written.
#[tokio::test]
async fn ii_uncertified_refusal_writes_no_row() {
    let world = world().await;
    let version_id = uncertified_version(&world, "uncertified").await;
    let error = try_promote(&world, &version_id, None, "operator-token")
        .await
        .expect_err("an uncertified version refuses");
    assert_eq!(
        error,
        PaperPromotionError::Refused(PromotionRefused::Uncertified)
    );
    assert_eq!(session_count(&world).await, 0, "no row on refusal");
}

/// (iii) An empty or whitespace-only override reason is refused by the type
/// AND by the table CHECK (a raw insert carrying one aborts).
#[tokio::test]
async fn iii_empty_override_reason_refused_by_type_and_check() {
    let world = world().await;
    // The type refuses both.
    assert!(NonEmptyReason::try_new("").is_err());
    assert!(NonEmptyReason::try_new("   \t\n").is_err());

    // The table CHECK refuses what a badly-typed caller might still send:
    // an `override` row whose reason is whitespace-only aborts.
    let version_id = uncertified_version(&world, "empty-override").await;
    let err = sqlx::query(
        "INSERT INTO paper_session (id, seq, strategy_version_id, created_at, pair, \
         primary_timeframe, htf_timeframe, uses_d1, starting_equity, taker_fee_bps, \
         slippage_bps, engine_fingerprint, graduation, walk_forward_run_id, \
         override_reason, override_at, certified_data_versions, fixture, min_trades, \
         promoted_by) \
         VALUES ('s', 1, ?1, '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, 0, \
         '10000', '4', '1', 'fp', 'override', NULL, '   ', \
         '2026-01-01T00:00:00.000Z', '[]', 0, 20, 'operator-token')",
    )
    .bind(version_id.as_str())
    .execute(world.db.pool())
    .await
    .expect_err("the CHECK refuses a whitespace-only reason");
    let message = format!("{err}");
    assert!(
        message.contains("override"),
        "the refusal names the CHECK: {message}"
    );
    assert_eq!(session_count(&world).await, 0);
}

/// (iv) A non-empty reason promotes as `override`, with the reason and the
/// injected clock's `at`.
#[tokio::test]
async fn iv_override_promotion_records_reason_and_at() {
    let world = world().await;
    let version_id = uncertified_version(&world, "override-promotion").await;
    let request = pulse::OverrideRequest {
        reason: NonEmptyReason::try_new("operator decision: shadow-list the strategy").unwrap(),
        pair: fixture_pair(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        uses_d1: false,
    };
    let session = try_promote(&world, &version_id, Some(request), "operator-token")
        .await
        .expect("an override with a reason promotes");
    let Graduation::Override { reason, at } = &session.graduation else {
        panic!(
            "expected an override graduation, got {:?}",
            session.graduation
        );
    };
    assert_eq!(
        reason.as_str(),
        "operator decision: shadow-list the strategy"
    );
    assert_eq!(
        at, "2026-01-01T00:00:00.000Z",
        "`at` is the injected clock's instant"
    );
    // The request's session shape is recorded (spec §4.2).
    assert_eq!(session.primary_timeframe, Timeframe::M15);
    assert_eq!(session.htf_timeframe, Some(Timeframe::H4));
    // The row round-trips.
    let read_back = world.paper.get_session(&session.id).await.unwrap().unwrap();
    assert_eq!(read_back, session);
}

/// Round 1 (iQ): an override's session shape is held to the backtest's
/// request-shape rules — a D1 primary, a D1 HTF and an HTF not higher than the
/// primary are each refused as `InvalidShape`, and no row is written.
#[tokio::test]
async fn iv_b_override_with_an_impossible_shape_is_refused() {
    let world = world().await;
    let version_id = uncertified_version(&world, "override-shape").await;
    let shapes = [
        (Timeframe::D1, None),
        (Timeframe::M15, Some(Timeframe::D1)),
        (Timeframe::H4, Some(Timeframe::H4)),
        (Timeframe::H4, Some(Timeframe::M15)),
    ];
    for (primary, htf) in shapes {
        let request = pulse::OverrideRequest {
            reason: NonEmptyReason::try_new("operator decision").unwrap(),
            pair: fixture_pair(),
            primary_timeframe: primary,
            htf_timeframe: htf,
            uses_d1: false,
        };
        let error = try_promote(&world, &version_id, Some(request), "operator-token")
            .await
            .expect_err("an impossible shape refuses");
        assert!(
            matches!(error, PaperPromotionError::InvalidShape(_)),
            "{primary:?}/{htf:?}: {error:?}"
        );
    }
    assert_eq!(session_count(&world).await, 0, "no row is written");
}

/// (v) E2: a passing walk-forward under a foreign fingerprint refuses as
/// `CertifiedUnderOtherEngine`, naming both fingerprints — with or without an
/// override — and writes no row.
#[tokio::test]
async fn v_foreign_fingerprint_refuses_even_with_override() {
    let world = world().await;
    let version_id = foreign_fingerprint_version(&world, "foreign-fp").await;

    let error = try_promote(&world, &version_id, None, "operator-token")
        .await
        .expect_err("a foreign-fingerprint certification refuses");
    let PaperPromotionError::Refused(PromotionRefused::CertifiedUnderOtherEngine {
        certified_under,
        current,
    }) = &error
    else {
        panic!("expected CertifiedUnderOtherEngine, got {error:?}");
    };
    assert_eq!(
        certified_under.as_str(),
        "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
    );
    assert_eq!(*current, pulse::EngineFingerprint::current());
    assert!(
        error
            .to_string()
            .contains("re-run walk-forward to re-certify"),
        "the message tells the operator what to do: {error}"
    );

    // The override cannot bypass it (E2).
    let request = pulse::OverrideRequest {
        reason: NonEmptyReason::try_new("the old engine was fine, trust me").unwrap(),
        pair: fixture_pair(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        uses_d1: false,
    };
    let error = try_promote(&world, &version_id, Some(request), "operator-token")
        .await
        .expect_err("the override cannot bypass a foreign fingerprint");
    assert!(matches!(
        error,
        PaperPromotionError::Refused(PromotionRefused::CertifiedUnderOtherEngine { .. })
    ));
    assert_eq!(session_count(&world).await, 0, "no row on either refusal");
}

/// (vi) Fixture data: `fixture = true`, `oos_comparable() = false`.
/// Non-fixture certified data: `fixture = false`, `oos_comparable() = true`.
/// An override session is never OOS-comparable.
#[tokio::test]
async fn vi_fixture_flag_and_oos_comparability() {
    let world = world().await;
    // Fixture-certified: every certified data version has a fixture_snapshot
    // row (the seed writes them; here the test writes the two stamps
    // directly — the seed's own idempotency is AC-4's (iii)).
    let version_id = certified_version(&world, "fixture-certified").await;
    let (m15_version, h4_version) = write_fixture_snapshots(&world);
    world
        .paper
        .insert_fixture_snapshot(&fixture_pair(), Timeframe::M15, &m15_version)
        .await
        .unwrap();
    world
        .paper
        .insert_fixture_snapshot(&fixture_pair(), Timeframe::H4, &h4_version)
        .await
        .unwrap();
    let session = try_promote(&world, &version_id, None, "operator-token")
        .await
        .expect("the fixture-certified version promotes");
    assert!(session.fixture, "all versions are fixture_snapshot rows");
    assert!(
        !session.oos_comparable(),
        "a fixture session has no OOS comparison"
    );

    // Non-fixture certified: certified over a SHIFTED slice of the same
    // generator (different content, different data versions), and no
    // fixture_snapshot rows exist for those versions.
    let mut shifted = fixture_m15_candles();
    shifted.drain(0..16);
    let version_id = certified_version_over(&world, "non-fixture-certified", shifted).await;
    let session = try_promote(&world, &version_id, None, "operator-token")
        .await
        .expect("the non-fixture certified version promotes");
    assert!(!session.fixture);
    assert!(
        session.oos_comparable(),
        "a real-data certified session is OOS-comparable"
    );

    // An override session: never OOS-comparable, regardless of fixture.
    let version_id = uncertified_version(&world, "override-oos").await;
    let request = pulse::OverrideRequest {
        reason: NonEmptyReason::try_new("manual shadow list").unwrap(),
        pair: fixture_pair(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        uses_d1: false,
    };
    let session = try_promote(&world, &version_id, Some(request), "operator-token")
        .await
        .expect("the override promotes");
    assert!(!session.fixture, "an override names no certified versions");
    assert!(!session.oos_comparable());
}

/// (vii) Every promotion records the label it was given; an empty label is
/// refused — by the type, and by the table CHECK on a raw insert.
#[tokio::test]
async fn vii_promoted_by_is_recorded_and_refuses_empty() {
    let world = world().await;
    // The type refuses both empty and whitespace-only labels.
    assert!(NonEmptyLabel::try_new("").is_err());
    assert!(NonEmptyLabel::try_new("  \u{00a0}").is_err());

    // A promotion records exactly the label it was given.
    let version_id = certified_version(&world, "promoted-by").await;
    let session = try_promote(&world, &version_id, None, "desk-9-token")
        .await
        .expect("the promotion succeeds");
    assert_eq!(session.promoted_by.as_str(), "desk-9-token");

    // The table CHECK refuses a blank label even on a raw insert.
    let err = sqlx::query(
        "INSERT INTO paper_session (id, seq, strategy_version_id, created_at, pair, \
         primary_timeframe, htf_timeframe, uses_d1, starting_equity, taker_fee_bps, \
         slippage_bps, engine_fingerprint, graduation, walk_forward_run_id, \
         override_reason, override_at, certified_data_versions, fixture, min_trades, \
         promoted_by) \
         VALUES ('s2', 2, ?1, '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, 0, \
         '10000', '4', '1', 'fp', 'override', NULL, 'a reason', \
         '2026-01-01T00:00:00.000Z', '[]', 0, 20, '   ')",
    )
    .bind(version_id.as_str())
    .execute(world.db.pool())
    .await
    .expect_err("the CHECK refuses a whitespace-only promoted_by");
    let message = format!("{err}");
    assert!(
        message.contains("promoted_by"),
        "the refusal names the CHECK: {message}"
    );
}

/// Spec §3b's session catalog read: `list_sessions` is the typed full read —
/// an empty catalog reads as no sessions, every promotion appears with its
/// complete row (identical to `get_session`), the order is the insertion
/// sequence (`seq` ascending — the tables' own mint), the SAME version may
/// appear once per promotion, and a STOPPED session's row stays readable.
#[tokio::test]
async fn viii_list_sessions_catalog_is_ordered_and_faithful() {
    let world = world().await;
    assert!(
        world.paper.list_sessions().await.unwrap().is_empty(),
        "an empty catalog reads as no sessions"
    );

    // Four promotions: the SAME fixture-certified version TWICE (several
    // sessions per version are allowed), an override, and a non-fixture
    // certified one.
    let certified = certified_version(&world, "catalog-certified").await;
    let first = try_promote(&world, &certified, None, "operator-token")
        .await
        .expect("the first promotion of the version succeeds");
    let second = try_promote(&world, &certified, None, "desk-9-token")
        .await
        .expect("a second promotion of the SAME version succeeds");
    let override_version = uncertified_version(&world, "catalog-override").await;
    let request = pulse::OverrideRequest {
        reason: NonEmptyReason::try_new("catalog variety").unwrap(),
        pair: fixture_pair(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        uses_d1: false,
    };
    let third = try_promote(&world, &override_version, Some(request), "operator-token")
        .await
        .expect("the override promotes");
    let mut shifted = fixture_m15_candles();
    shifted.drain(0..16);
    let shifted_version = certified_version_over(&world, "catalog-shifted", shifted).await;
    let fourth = try_promote(&world, &shifted_version, None, "operator-token")
        .await
        .expect("the non-fixture certified promotes");

    // The full read: every row, in insertion order, faithful to get_session.
    let catalog = world.paper.list_sessions().await.unwrap();
    assert_eq!(catalog.len(), 4, "every promotion is cataloged");
    assert_eq!(
        catalog.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        [
            first.id.clone(),
            second.id.clone(),
            third.id.clone(),
            fourth.id.clone()
        ],
        "the catalog reads in insertion order"
    );
    let seqs: Vec<i64> = catalog.iter().map(|s| s.seq).collect();
    let strictly_increasing = seqs.windows(2).all(|pair| pair[0] < pair[1]);
    assert!(strictly_increasing, "seq orders the catalog: {seqs:?}");
    for session in &catalog {
        assert_eq!(
            Some(session),
            world.paper.get_session(&session.id).await.unwrap().as_ref(),
            "the catalog row is the whole row"
        );
    }
    // The SAME version twice: distinct sessions, one per promotion.
    assert_eq!(first.strategy_version_id, second.strategy_version_id);
    assert_ne!(first.id, second.id);

    // Stop the first session; its row remains readable in the catalog.
    world
        .paper
        .append_bar(
            &first.id,
            &[],
            &[pulse::PaperEvent::Stop {
                seq: 0,
                at: "2026-01-01T00:00:00.000Z".to_owned(),
                actor: pulse::StopActor::Token {
                    label: NonEmptyLabel::try_new("operator-token").unwrap(),
                },
            }],
        )
        .await
        .expect("the stop appends");
    let after = world.paper.list_sessions().await.unwrap();
    assert_eq!(after.len(), 4, "a stopped session is still cataloged");
    assert_eq!(
        after.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        catalog.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        "the order is stable across reads"
    );
}

// ---------------------------------------------------------------------------
// Fail-closed certification (spec §4.1): an unreadable certification is not
// one — and a non-passing latest run is uncertified even when an earlier
// passing run exists.
// ---------------------------------------------------------------------------

/// Controlled fault injection at the [`BacktestRunRepository`] boundary (the
/// coordinator-sanctioned construction): everything delegates to the REAL
/// repository except that ONE run id decodes with `inputs = None` — exactly
/// the shape a pre-`0006` row leaves after `decode_inputs`. The state cannot
/// be written directly: `backtest_run_inputs_complete` refuses a
/// NULL-provenance row at INSERT and the update/delete triggers refuse
/// surgery, so the port boundary is the only honest place it can enter the
/// use case from.
struct UnreadableFoldInputs<'a> {
    inner: &'a SqliteBacktestRunRepo<pulse::SystemClock>,
    /// The fold run whose provenance is unreadable.
    unreadable: BacktestRunId,
}

impl BacktestRunRepository for UnreadableFoldInputs<'_> {
    async fn save_run(
        &self,
        strategy_version_id: &pulse::VersionId,
        inputs: &BacktestInputs,
        result: &BacktestResult,
        summary: &SummaryStats,
        starting_equity: Decimal,
    ) -> Result<BacktestRunId, DataError> {
        self.inner
            .save_run(
                strategy_version_id,
                inputs,
                result,
                summary,
                starting_equity,
            )
            .await
    }

    async fn get_run(&self, id: &BacktestRunId) -> Result<Option<PersistedRun>, DataError> {
        let run = self.inner.get_run(id).await?;
        Ok(run.map(|mut persisted| {
            if persisted.id == self.unreadable {
                persisted.inputs = None;
            }
            persisted
        }))
    }

    async fn latest_run_for_version(
        &self,
        strategy_version_id: &pulse::VersionId,
    ) -> Result<Option<PersistedRun>, DataError> {
        self.inner.latest_run_for_version(strategy_version_id).await
    }

    async fn list_runs_for_version(
        &self,
        strategy_version_id: &pulse::VersionId,
    ) -> Result<Vec<RunSummary>, DataError> {
        self.inner.list_runs_for_version(strategy_version_id).await
    }

    async fn latest_readable_run_for_version(
        &self,
        strategy_version_id: &pulse::VersionId,
    ) -> Result<LatestReadableRun, DataError> {
        self.inner
            .latest_readable_run_for_version(strategy_version_id)
            .await
    }

    async fn get_trades(&self, id: &BacktestRunId) -> Result<Vec<Trade>, DataError> {
        self.inner.get_trades(id).await
    }
}

/// The certifying run's id for a version promoted by [`certified_version`].
async fn latest_run_of(world: &World, version_id: &pulse::VersionId) -> pulse::WalkForwardRun {
    let version = world
        .strategies
        .get_version(version_id)
        .await
        .unwrap()
        .expect("the version exists");
    world
        .runs
        .get_walk_forward_run(version.latest_walk_forward_run_id.as_ref().unwrap())
        .await
        .unwrap()
        .expect("the certifying run exists")
}

/// Typed fold inputs, ready for the pure gate — the pair/timeframe/cost
/// shape every fixture run here carries, parameterised by the primary
/// snapshot's identity so two of these can disagree.
fn typed_inputs(primary_version: &str) -> BacktestInputs {
    BacktestInputs {
        pair: fixture_pair(),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: DataVersion::new(primary_version),
        },
        htf: Some(SnapshotSelection {
            timeframe: Timeframe::H4,
            data_version: DataVersion::new("76f15836cc357256"),
        }),
        d1: None,
        taker_fee_bps: Decimal::from(4),
        slippage_bps: Decimal::from(1),
        funding: FundingConfig::SnapshotRates,
        symbol_filters: None,
        window: None,
        lead_in_from_ms: None,
    }
}

/// A fold run whose recorded inputs are unreadable refuses the promotion as
/// `CertificationUnreadable` — a certification whose data provenance cannot
/// be named is not one — and writes no session row.
#[tokio::test]
async fn ix_unreadable_fold_inputs_refuse_certification() {
    let world = world().await;
    let version_id = certified_version(&world, "unreadable").await;
    let run = latest_run_of(&world, &version_id).await;
    let fold_run_id = run.folds[0].backtest_run_id.clone();

    // The REAL use case over the REAL database; the only fault is the one
    // field an older binary's row cannot answer.
    let unreadable = UnreadableFoldInputs {
        inner: &world.runs,
        unreadable: fold_run_id,
    };
    let error = promote(
        &world.strategies,
        &world.runs,
        &unreadable,
        &world.paper,
        &FakeClock::at(NOW_MS),
        &version_id,
        None,
        NonEmptyLabel::try_new("operator-token").unwrap(),
    )
    .await
    .expect_err("an unreadable certification refuses");
    assert_eq!(
        error,
        PaperPromotionError::Refused(PromotionRefused::CertificationUnreadable)
    );
    assert_eq!(session_count(&world).await, 0, "no row on the refusal");
    assert!(
        error.to_string().contains("unreadable"),
        "the message says the inputs cannot be read: {error}"
    );
}

/// The pure gate refuses fold inputs that DISAGREE across folds (the
/// across-fold arm of `CertificationUnreadable`). The persisted save path
/// refuses that shape on its own (the fold-consistency gate), so the arm's
/// contract is pinned here on the gate's own signature — with real typed
/// inputs and the real certifying run.
#[tokio::test]
async fn x_disagreeing_fold_inputs_refuse_at_the_gate() {
    let world = world().await;
    let version_id = certified_version(&world, "disagreeing").await;
    let version = world
        .strategies
        .get_version(&version_id)
        .await
        .unwrap()
        .expect("the version exists");
    let run = latest_run_of(&world, &version_id).await;

    // Every fold unreadable is the missing-provenance arm …
    let error = decide_promotion(
        &version,
        Some(&run),
        &[None, None],
        &EngineFingerprint::current(),
        None,
        NonEmptyLabel::try_new("operator-token").unwrap(),
    )
    .expect_err("no fold inputs name no provenance");
    assert_eq!(error, PromotionRefused::CertificationUnreadable);

    // … and two folds whose primary snapshots disagree cannot name ONE
    // certification either.
    let fold_inputs = [
        typed_inputs("0a2c929a27848083"),
        typed_inputs("bbbbbbbbbbbbbbbb"),
    ];
    let borrowed: Vec<Option<&BacktestInputs>> = fold_inputs.iter().map(Some).collect();
    let error = decide_promotion(
        &version,
        Some(&run),
        &borrowed,
        &EngineFingerprint::current(),
        None,
        NonEmptyLabel::try_new("operator-token").unwrap(),
    )
    .expect_err("disagreeing fold inputs refuse");
    assert_eq!(error, PromotionRefused::CertificationUnreadable);

    // The same inputs, consistent, are readable — the control arm.
    let fold_inputs = [
        typed_inputs("0a2c929a27848083"),
        typed_inputs("0a2c929a27848083"),
    ];
    let borrowed: Vec<Option<&BacktestInputs>> = fold_inputs.iter().map(Some).collect();
    let draft = decide_promotion(
        &version,
        Some(&run),
        &borrowed,
        &EngineFingerprint::current(),
        None,
        NonEmptyLabel::try_new("operator-token").unwrap(),
    )
    .expect("consistent fold inputs certify");
    let Graduation::Certified { data_versions, .. } = draft.graduation else {
        panic!("expected a certified draft");
    };
    assert_eq!(data_versions.len(), 2, "M15 + H4, deduped across folds");
}

/// A version whose latest walk-forward carried a D1 pin certifies with the
/// D1 data version in its certified list — distinct, in first-occurrence
/// fold order, with `uses_d1` and the A10 timeframes. Returns the D1 version
/// its typed inputs pin, so the caller can prove it is the one actually
/// derived from the snapshot written here. The persisted shape is the
/// honest-fixture precedent (the foreign-fingerprint case): a REAL engine
/// run's trades and result pieces, typed inputs the REAL save path wrote
/// (its cross-fold consistency and path-safety gates included). A
/// d1-consuming ENGINE run is not reachable for the fixture strategy — the
/// walk-forward use case records d1 only for a strategy with
/// `series: "d1"` operands.
async fn d1_certified_version(world: &World, name: &str) -> (pulse::VersionId, DataVersion) {
    // The D1 snapshot the inputs name, written through the store; the typed
    // inputs carry ITS content-derived version (path-safe hex).
    let pair = fixture_pair();
    let d1_candles = d1_series();
    let derived = CandleStore::content_version(&pair, Timeframe::D1, &d1_candles);
    write_series_snapshots(world, &pair, Timeframe::D1, derived.clone(), d1_candles);
    let version_id = create_fixture_version(world, name).await;
    let scenario = RunScenario::new(world, version_id.clone()).await;
    let (folds, verdict) = scenario.fold_drafts(
        EngineFingerprint::current().as_str(),
        <[pulse::Trade]>::to_vec,
        |inputs| {
            inputs.d1 = Some(SnapshotSelection {
                timeframe: Timeframe::D1,
                data_version: derived.clone(),
            });
        },
    );
    assert!(verdict.pass, "the fabricated run must pass (real trades)");
    save_walk_forward(
        world,
        &version_id,
        EngineFingerprint::current().as_str(),
        scenario.span,
        folds,
        verdict,
    )
    .await;
    (version_id, derived)
}

/// Three daily candles on the fixture's day grid — the D1 snapshot the
/// d1-certified scenario writes and its typed inputs name.
fn d1_series() -> Vec<pulse::Candle> {
    (0..3)
        .map(|day| pulse::Candle {
            open_time: 1_735_689_600_000 + day * 86_400_000,
            close_time: 1_735_689_600_000 + day * 86_400_000 + 86_399_999,
            open: Decimal::from(60_000),
            high: Decimal::from(60_100),
            low: Decimal::from(59_900),
            close: Decimal::from(60_050),
            volume: Decimal::from(100),
            funding_rate: None,
        })
        .collect()
}

/// The D1-certified collection: data versions distinct in fold order, the
/// D1 version included and IT IS the content-derived version of the D1
/// snapshot the fixture actually wrote (readback proves it), `uses_d1` on,
/// A10 timeframes from the same inputs.
#[tokio::test]
async fn xi_d1_certified_versions_are_collected_deduped_in_fold_order() {
    let world = world().await;
    let (version_id, d1_version) = d1_certified_version(&world, "d1-certified").await;
    let session = try_promote(&world, &version_id, None, "operator-token")
        .await
        .expect("the d1-pinned certification promotes");
    assert!(session.uses_d1, "the certified inputs consumed D1");
    assert_eq!(session.htf_timeframe, Some(Timeframe::H4));
    assert_eq!(session.primary_timeframe, Timeframe::M15);
    let Graduation::Certified { data_versions, .. } = &session.graduation else {
        panic!(
            "expected a certified graduation, got {:?}",
            session.graduation
        );
    };
    let timeframes: Vec<Timeframe> = data_versions.iter().map(|v| v.timeframe).collect();
    assert_eq!(
        timeframes,
        vec![Timeframe::M15, Timeframe::H4, Timeframe::D1],
        "distinct, in first-occurrence fold order"
    );
    // The certified D1 provenance names the snapshot the fixture WROTE — the
    // content-derived version, not an unrelated tag.
    let d1_selection = data_versions
        .iter()
        .find(|v| v.timeframe == Timeframe::D1)
        .expect("D1 is among the certified versions");
    assert_eq!(
        d1_selection.data_version, d1_version,
        "the certified D1 version is the derived version of the written snapshot"
    );
    // The pinned read: the D1 snapshot loads by ITS version.
    let series = world
        .store
        .read_snapshot(&fixture_pair(), Timeframe::D1, &d1_version)
        .expect("the written D1 snapshot reads back by its version");
    assert_eq!(series.candles.len(), 3, "the three daily candles");
}

/// A version whose LATEST walk-forward does NOT pass is uncertified — an
/// earlier PASSING run must not rescue it. The failing run is an honest
/// persisted fixture: real engine trades, so few per fold that the REAL
/// wf-v1 assessment fails them (n < 20 — no threshold touched).
async fn failing_latest_run_version(world: &World, name: &str) -> pulse::VersionId {
    let (m15_version, h4_version) = write_fixture_snapshots(world);
    let version_id = create_fixture_version(world, name).await;

    // First, a REAL passing certification (the pointer names it).
    let passing = run_walk_forward(
        &world.strategies,
        &world.store,
        &BinanceAdapter::new(),
        &world.runs,
        &walk_forward_request(version_id.clone(), m15_version.clone(), h4_version.clone()),
        None,
    )
    .await
    .unwrap();
    assert!(passing.run.verdict.pass);

    // Then a real run's trades, trimmed per fold below N_MIN — the REAL
    // verdict computation fails it honestly.
    let scenario = RunScenario::new(world, version_id.clone()).await;
    let (folds, verdict) = scenario.fold_drafts(
        EngineFingerprint::current().as_str(),
        |trades| trades.iter().take(3).cloned().collect(),
        |_| (),
    );
    assert!(
        folds.iter().all(|fold| fold.result.trades.len() < 20),
        "each fold holds fewer trades than N_MIN, so wf-v1 fails it"
    );
    assert!(
        !verdict.pass,
        "the real wf-v1 assessment fails a sub-N_MIN run"
    );
    save_walk_forward(
        world,
        &version_id,
        EngineFingerprint::current().as_str(),
        scenario.span,
        folds,
        verdict,
    )
    .await;
    version_id
}

/// The latest run failing means UNCERTIFIED, even though a passing run for
/// the same version exists from before: the gate reads the LATEST run's
/// verdict, nothing else. Without an override it refuses and writes no row;
/// with one it promotes as an override.
#[tokio::test]
async fn xii_failing_latest_run_is_not_rescued_by_an_earlier_pass() {
    let world = world().await;
    let version_id = failing_latest_run_version(&world, "failing-latest").await;

    // The pointer names the FAILING run; the earlier passing run is still in
    // the table — and did not rescue the promotion.
    let version = world
        .strategies
        .get_version(&version_id)
        .await
        .unwrap()
        .expect("the version exists");
    let latest = world
        .runs
        .get_walk_forward_run(version.latest_walk_forward_run_id.as_ref().unwrap())
        .await
        .unwrap()
        .expect("the latest run exists");
    assert!(!latest.verdict.pass, "the latest run fails");
    let runs_for_version: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run WHERE strategy_version_id = ?1")
            .bind(version_id.as_str())
            .fetch_one(world.db.pool())
            .await
            .unwrap();
    assert_eq!(runs_for_version, 2, "the passing run is still persisted");

    let error = try_promote(&world, &version_id, None, "operator-token")
        .await
        .expect_err("a non-passing latest run is uncertified");
    assert_eq!(
        error,
        PaperPromotionError::Refused(PromotionRefused::Uncertified)
    );
    assert_eq!(session_count(&world).await, 0, "no row on the refusal");

    // The override path still applies to a merely-uncertified version.
    let request = pulse::OverrideRequest {
        reason: NonEmptyReason::try_new("the earlier folds were sound").unwrap(),
        pair: fixture_pair(),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        uses_d1: false,
    };
    let session = try_promote(&world, &version_id, Some(request), "operator-token")
        .await
        .expect("the override promotes the uncertified version");
    assert!(matches!(session.graduation, Graduation::Override { .. }));
}
