//! r1.s3.w3 — the desktop backtest command over freshly persisted runs.
//!
//! This is the spine's `d11` ledger line and the backend half of Backtest Lab. It
//! drives the REAL command core over the committed BTCUSDT M15/H4 Parquet fixture,
//! a migrated temp database, a real persisted strategy version, the real engine and
//! the real SQLite repositories — no permanent double anywhere on the product path.
//!
//! What it proves, and why each one is here rather than assumed:
//!
//! 1. **Two cold runs agree.** `DesktopState` is dropped and reopened between calls,
//!    so the second run shares no in-process cache with the first. Every response
//!    field is compared after normalizing ONLY `run_id` and `created_at`. Comparing
//!    `result_content_hash` alone would pass while provenance, equity, regimes or
//!    histograms silently diverged.
//! 2. **HTF provenance is asserted directly.** The golden strategy has no HTF
//!    condition, so M15-only and M15+H4 produce byte-identical results — measured.
//!    A matching hash therefore proves nothing about the H4 snapshot. The recorded
//!    `htf` identity is checked on the row and reloaded through `load_version` after
//!    both HEAD pointers move.
//! 3. **A post-save read-back failure names the run.** Every stage is injected and
//!    must return `SavedButReadBackFailed` with the exact persisted id, and the
//!    mapped `BusError` must carry it as a FIELD — W4 cannot parse prose.
//! 4. **`save_run` does not read back internally.** It commits and returns the
//!    minted id; the caller owns the read. Guarded by a source scan with comments
//!    blanked and positive controls, so it cannot pass vacuously.
//! 5. **Blocking work runs off the async runtime.** A probe records the thread the
//!    candle read happens on and compares it to the calling worker.
//! 6. **No order capability exists in the application ring.**
//! 7. **The histogram is exactly as pinned** — boundaries, underflow, overflow.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use pulse::{
    BacktestAppError, BacktestConfig, BacktestRunId, BacktestRunRepository, BacktestRunRequest,
    Candle, CandleSeriesRepository, CandleStore, CreatedBy, DataError, DataVersion, Db,
    DesktopState, HISTOGRAM_BIN_COUNT, NewVersion, Pair, PersistedRun, ReadBackFailure,
    ReadBackStage, SqliteBacktestRunRepo, SqliteStrategyRepo, StoredCandleSeries, StrategyId,
    StrategyRepository, SummaryStats, Timeframe, Trade, VersionId, histogram_bin_width,
    project_histogram, run_backtest_version_core, run_version_backtest,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

mod source_scan;
use source_scan::{blank_comments, read_source};

/// The committed candle fixture every arm runs over.
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The golden strategy the histogram contract was measured against.
const GOLDEN_STRATEGY: &str = "tests/fixtures/strategies/rsi-oversold-long.json";

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// A temp environment: a db path and a WRITABLE copy of the candle fixture, so a
/// test may advance HEAD without touching the committed store.
struct Env {
    _tmp: TempDir,
    db_path: PathBuf,
    store_root: PathBuf,
}

fn env() -> Env {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let store_root = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_root);
    Env {
        _tmp: tmp,
        db_path,
        store_root,
    }
}

impl Env {
    fn store(&self) -> CandleStore {
        CandleStore::with_base_dir(self.store_root.clone())
    }

    /// A cold `DesktopState` over this environment — a fresh pool and a fresh
    /// store handle every time, which is what "reopen between runs" means.
    async fn cold_state(&self) -> DesktopState {
        DesktopState::open_with_store(&self.db_path, self.store())
            .await
            .expect("open desktop state over the temp db + fixture store")
    }

    async fn db(&self) -> Db {
        Db::with_path(&self.db_path).await.expect("open db")
    }
}

/// Seed a strategy plus one real, compilable version from the golden DSL.
async fn seed_version(env: &Env) -> VersionId {
    let state = env.cold_state().await;
    let repo = state.strategy_repo();
    let strat = repo
        .create_strategy("Backtest Lab demo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let dsl = std::fs::read_to_string(manifest(GOLDEN_STRATEGY)).expect("read golden strategy");
    repo.create_version(NewVersion {
        strategy_id: StrategyId::new(strat.id.as_str().to_owned()),
        parent_version_id: None,
        dsl_json: dsl,
        created_by: CreatedBy::Human,
        creating_llm_call_ids: vec![],
    })
    .await
    .expect("create version")
    .id
}

fn request(version_id: &VersionId) -> BacktestRunRequest {
    BacktestRunRequest {
        version_id: version_id.as_str().to_owned(),
    }
}

// ---------------------------------------------------------------------------
// 1. the two-cold-run oracle (ledger line d11)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_runs_agree_on_every_field_except_identity_and_time() {
    let env = env();
    let version_id = seed_version(&env).await;

    // Two invocations with the state dropped in between — no shared cache, no
    // shared pool, no shared store handle.
    let first = {
        let state = env.cold_state().await;
        run_backtest_version_core(&state, request(&version_id))
            .await
            .expect("first cold run succeeds")
    };
    let second = {
        let state = env.cold_state().await;
        run_backtest_version_core(&state, request(&version_id))
            .await
            .expect("second cold run succeeds")
    };

    assert_ne!(
        first.run_id, second.run_id,
        "every invocation mints a fresh run id — there is no cached path"
    );

    // Normalize ONLY identity and time; everything else must match exactly.
    let mut a = first.clone();
    let mut b = second.clone();
    for dto in [&mut a, &mut b] {
        dto.run_id = String::new();
        dto.created_at = String::new();
    }
    assert_eq!(
        a, b,
        "two cold runs over the same version and the same snapshots differ only in \
         run id and created_at"
    );

    // The run is real, not an empty shell.
    assert!(
        first.trade_count > 0 && !first.trades.is_empty(),
        "the fixture run produced trades: {first:?}"
    );
    assert_eq!(first.trades.len(), first.trade_count as usize);

    // The saved projections agree directly too, not just the DTO built from them.
    let db = env.db().await;
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let run_a = runs
        .get_run(&BacktestRunId::new(first.run_id.clone()))
        .await
        .expect("read run a")
        .expect("run a exists");
    let run_b = runs
        .get_run(&BacktestRunId::new(second.run_id.clone()))
        .await
        .expect("read run b")
        .expect("run b exists");
    assert_eq!(run_a.result_content_hash, run_b.result_content_hash);
    assert_eq!(run_a.inputs, run_b.inputs, "identical persisted inputs");
    assert_eq!(
        runs.get_trades(&run_a.id).await.expect("trades a"),
        runs.get_trades(&run_b.id).await.expect("trades b"),
        "identical persisted trade logs"
    );
}

// ---------------------------------------------------------------------------
// 2. HTF provenance — asserted directly, because results cannot prove it
// ---------------------------------------------------------------------------

fn distinct_candles(tf: Timeframe, count: i64) -> Vec<Candle> {
    let step = tf.duration_ms();
    (0..count)
        .map(|i| {
            let price = Decimal::new(90_000 + i, 0);
            Candle {
                open_time: i * step,
                close_time: i * step + step - 1,
                open: price,
                high: price,
                low: price,
                close: price,
                volume: Decimal::ONE,
                funding_rate: None,
            }
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn htf_provenance_is_recorded_and_reloads_after_both_heads_move() {
    let env = env();
    let version_id = seed_version(&env).await;
    let state = env.cold_state().await;
    let dto = run_backtest_version_core(&state, request(&version_id))
        .await
        .expect("run succeeds");
    drop(state);

    // The r1 request is M15+H4, so BOTH identities must be on the row. The golden
    // has no HTF condition — M15-only and M15+H4 are byte-identical — so nothing
    // about the result would have caught a dropped H4 snapshot.
    assert_eq!(dto.primary_timeframe, "15m");
    assert_eq!(
        dto.htf_timeframe.as_deref(),
        Some("4h"),
        "the fixed r1 request records its HTF timeframe"
    );
    let htf_version = dto
        .htf_data_version
        .clone()
        .expect("the fixed r1 request records its HTF data_version");
    assert!(!dto.primary_data_version.is_empty());

    let db = env.db().await;
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let inputs = runs
        .get_run(&BacktestRunId::new(dto.run_id.clone()))
        .await
        .expect("read run")
        .expect("run exists")
        .inputs
        .expect("a fresh run carries inputs");
    let recorded_htf = inputs.htf.expect("inputs.htf is present on the saved row");
    assert_eq!(recorded_htf.timeframe, Timeframe::H4);
    assert_eq!(recorded_htf.data_version.as_str(), htf_version);

    // Advance BOTH HEADs, then reload each recorded identity exactly.
    let store = env.store();
    for tf in [Timeframe::M15, Timeframe::H4] {
        store
            .commit(&inputs.pair, tf, distinct_candles(tf, 40))
            .expect("commit a new snapshot");
        let head = store
            .load_head(&inputs.pair, tf)
            .expect("load_head")
            .expect("HEAD present")
            .series
            .version;
        let recorded = if tf == Timeframe::M15 {
            &inputs.primary.data_version
        } else {
            &recorded_htf.data_version
        };
        assert_ne!(&head, recorded, "HEAD moved off the recorded snapshot");
        let reloaded = store
            .load_version(&inputs.pair, tf, recorded)
            .expect("the recorded snapshot still resolves after HEAD advanced")
            .series;
        assert!(!reloaded.candles.is_empty());
        assert_eq!(&reloaded.version, recorded);
    }
}

// ---------------------------------------------------------------------------
// 3. post-save read-back failures carry the persisted run id
// ---------------------------------------------------------------------------

/// Where an injected double should fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailAt {
    None,
    /// A pre-save read: the strategy version lookup.
    VersionRead,
    /// A pre-save read: the primary HEAD snapshot load.
    PrimaryHeadRead,
    /// A pre-save read: the prior-run lookup for the FR-7 comparison.
    PriorRunRead,
    GetRun,
    GetRunMissing,
    GetRunLegacyInputs,
    GetTrades,
    PrimaryReload,
    HtfReload,
    Save,
}

/// A run repository that delegates to the real SQLite one and fails exactly one
/// post-save read. It is a focused double behind a product-owned port, injected
/// only to reach failure paths the real adapter will not produce on demand.
struct InjectingRunRepo {
    inner: SqliteBacktestRunRepo<pulse::SystemClock>,
    fail: FailAt,
}

impl BacktestRunRepository for InjectingRunRepo {
    fn save_run(
        &self,
        strategy_version_id: &VersionId,
        inputs: &pulse::BacktestInputs,
        result: &pulse::BacktestResult,
        summary: &SummaryStats,
        starting_equity: Decimal,
    ) -> impl std::future::Future<Output = Result<BacktestRunId, DataError>> + Send {
        let fail = self.fail;
        let inner = &self.inner;
        async move {
            if fail == FailAt::Save {
                return Err(DataError::Db("injected pre-save failure".to_owned()));
            }
            inner
                .save_run(
                    strategy_version_id,
                    inputs,
                    result,
                    summary,
                    starting_equity,
                )
                .await
        }
    }

    fn get_run(
        &self,
        id: &BacktestRunId,
    ) -> impl std::future::Future<Output = Result<Option<PersistedRun>, DataError>> + Send {
        let fail = self.fail;
        let inner = &self.inner;
        let id = id.clone();
        async move {
            match fail {
                FailAt::GetRun => Err(DataError::Db("injected get_run failure".to_owned())),
                FailAt::GetRunMissing => Ok(None),
                FailAt::GetRunLegacyInputs => {
                    let mut run = inner.get_run(&id).await?;
                    if let Some(run) = run.as_mut() {
                        run.inputs = None;
                    }
                    Ok(run)
                }
                _ => inner.get_run(&id).await,
            }
        }
    }

    fn latest_run_for_version(
        &self,
        strategy_version_id: &VersionId,
    ) -> impl std::future::Future<Output = Result<Option<PersistedRun>, DataError>> + Send {
        let fail = self.fail;
        let inner = &self.inner;
        let id = strategy_version_id.clone();
        async move {
            if fail == FailAt::PriorRunRead {
                return Err(DataError::Db("injected prior-run read failure".to_owned()));
            }
            inner.latest_run_for_version(&id).await
        }
    }

    fn list_runs_for_version(
        &self,
        strategy_version_id: &VersionId,
    ) -> impl std::future::Future<Output = Result<Vec<pulse::RunSummary>, DataError>> + Send {
        self.inner.list_runs_for_version(strategy_version_id)
    }

    fn get_trades(
        &self,
        id: &BacktestRunId,
    ) -> impl std::future::Future<Output = Result<Vec<Trade>, DataError>> + Send {
        let fail = self.fail;
        let inner = &self.inner;
        let id = id.clone();
        async move {
            if fail == FailAt::GetTrades {
                return Err(DataError::Db("injected get_trades failure".to_owned()));
            }
            inner.get_trades(&id).await
        }
    }
}

/// A candle repository that delegates to the real store and fails the exact-version
/// reload for one timeframe.
#[derive(Clone)]
struct InjectingCandleRepo {
    inner: CandleStore,
    fail: FailAt,
    /// Set when the double should also record the thread each read happened on.
    probe: Option<std::sync::Arc<Mutex<Vec<std::thread::ThreadId>>>>,
}

impl CandleSeriesRepository for InjectingCandleRepo {
    fn load_head(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
    ) -> Result<Option<StoredCandleSeries>, DataError> {
        if let Some(probe) = self.probe.as_ref() {
            probe.lock().unwrap().push(std::thread::current().id());
        }
        if self.fail == FailAt::PrimaryHeadRead && timeframe == Timeframe::M15 {
            return Err(DataError::Db(
                "injected primary HEAD read failure".to_owned(),
            ));
        }
        self.inner.load_head(pair, timeframe)
    }

    fn load_version(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        version: &DataVersion,
    ) -> Result<StoredCandleSeries, DataError> {
        let fails = matches!(
            (self.fail, timeframe),
            (FailAt::PrimaryReload, Timeframe::M15) | (FailAt::HtfReload, Timeframe::H4)
        );
        if fails {
            return Err(DataError::Db(format!(
                "injected {} reload failure",
                timeframe.binance_interval()
            )));
        }
        self.inner.load_version(pair, timeframe, version)
    }

    fn commit(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        candles: Vec<Candle>,
    ) -> Result<StoredCandleSeries, DataError> {
        self.inner.commit(pair, timeframe, candles)
    }
}

fn r1_request(version_id: &VersionId) -> pulse::BacktestRequest {
    pulse::BacktestRequest {
        version_id: version_id.clone(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: BacktestConfig::default(),
    }
}

/// A strategy repository that delegates to the real one and can fail the version
/// read — a PRE-save failure, which must not claim anything was persisted.
struct InjectingStrategyRepo {
    inner: SqliteStrategyRepo<pulse::SystemClock>,
    fail: FailAt,
}

impl StrategyRepository for InjectingStrategyRepo {
    fn get_version(
        &self,
        id: &VersionId,
    ) -> impl std::future::Future<Output = Result<Option<pulse::StrategyVersion>, DataError>> + Send
    {
        let fail = self.fail;
        let inner = &self.inner;
        let id = id.clone();
        async move {
            if fail == FailAt::VersionRead {
                return Err(DataError::Db("injected version read failure".to_owned()));
            }
            inner.get_version(&id).await
        }
    }

    fn create_strategy(
        &self,
        name: &str,
        author: Option<&str>,
        tags: &[String],
    ) -> impl std::future::Future<Output = Result<pulse::Strategy, DataError>> + Send {
        self.inner.create_strategy(name, author, tags)
    }

    fn get_strategy(
        &self,
        id: &StrategyId,
    ) -> impl std::future::Future<Output = Result<Option<pulse::Strategy>, DataError>> + Send {
        self.inner.get_strategy(id)
    }

    fn list_strategies(
        &self,
        include_archived: bool,
    ) -> impl std::future::Future<Output = Result<Vec<pulse::Strategy>, DataError>> + Send {
        self.inner.list_strategies(include_archived)
    }

    fn rename_strategy(
        &self,
        id: &StrategyId,
        name: &str,
    ) -> impl std::future::Future<Output = Result<pulse::Strategy, DataError>> + Send {
        self.inner.rename_strategy(id, name)
    }

    fn set_tags(
        &self,
        id: &StrategyId,
        tags: &[String],
    ) -> impl std::future::Future<Output = Result<pulse::Strategy, DataError>> + Send {
        self.inner.set_tags(id, tags)
    }

    fn set_pinned_version(
        &self,
        id: &StrategyId,
        version_id: Option<&VersionId>,
    ) -> impl std::future::Future<Output = Result<pulse::Strategy, DataError>> + Send {
        self.inner.set_pinned_version(id, version_id)
    }

    fn archive_strategy(
        &self,
        id: &StrategyId,
        archived: bool,
    ) -> impl std::future::Future<Output = Result<pulse::Strategy, DataError>> + Send {
        self.inner.archive_strategy(id, archived)
    }

    fn create_version(
        &self,
        new_version: NewVersion,
    ) -> impl std::future::Future<Output = Result<pulse::StrategyVersion, DataError>> + Send {
        self.inner.create_version(new_version)
    }

    fn list_versions(
        &self,
        strategy_id: &StrategyId,
    ) -> impl std::future::Future<Output = Result<Vec<pulse::StrategyVersion>, DataError>> + Send
    {
        self.inner.list_versions(strategy_id)
    }

    fn version_tree(
        &self,
        strategy_id: &StrategyId,
    ) -> impl std::future::Future<Output = Result<Vec<pulse::StrategyVersion>, DataError>> + Send
    {
        self.inner.version_tree(strategy_id)
    }

    fn create_agent_version(
        &self,
        request: NewVersion,
        submission: pulse::NewAgentSubmission,
    ) -> impl std::future::Future<
        Output = Result<(pulse::StrategyVersion, pulse::AgentSubmission), DataError>,
    > + Send {
        self.inner.create_agent_version(request, submission)
    }

    fn get_agent_submission(
        &self,
        version_id: &VersionId,
    ) -> impl std::future::Future<Output = Result<Option<pulse::AgentSubmission>, DataError>> + Send
    {
        self.inner.get_agent_submission(version_id)
    }
}

/// Drive the use case with one injected failure and return the error.
async fn run_with_failure(env: &Env, version_id: &VersionId, fail: FailAt) -> BacktestAppError {
    let db = env.db().await;
    let strategies = InjectingStrategyRepo {
        inner: SqliteStrategyRepo::new(db.pool().clone()),
        fail,
    };
    let candles = InjectingCandleRepo {
        inner: env.store(),
        fail,
        probe: None,
    };
    let runs = InjectingRunRepo {
        inner: SqliteBacktestRunRepo::new(db.pool().clone()),
        fail,
    };
    run_version_backtest(
        &strategies,
        &candles,
        &pulse::BinanceAdapter::new(),
        &runs,
        &r1_request(version_id),
    )
    .await
    .expect_err("the injected failure must surface")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_post_save_failure_stage_names_the_persisted_run() {
    let env = env();
    let version_id = seed_version(&env).await;

    let cases = [
        (FailAt::GetRun, ReadBackStage::Run),
        (FailAt::GetRunMissing, ReadBackStage::Run),
        (FailAt::GetRunLegacyInputs, ReadBackStage::Run),
        (FailAt::GetTrades, ReadBackStage::Trades),
        (FailAt::PrimaryReload, ReadBackStage::PrimarySnapshot),
        (FailAt::HtfReload, ReadBackStage::HtfSnapshot),
    ];
    for (fail, expected_stage) in cases {
        let err = run_with_failure(&env, &version_id, fail).await;
        let BacktestAppError::SavedButReadBackFailed {
            run_id,
            stage,
            failure,
        } = err.clone()
        else {
            panic!("{fail:?} must map to SavedButReadBackFailed, got {err:?}");
        };
        let (run_id, stage, failure) = (&run_id, &stage, &failure);
        assert_eq!(stage, &expected_stage, "{fail:?} names its stage");
        assert!(
            !run_id.as_str().is_empty(),
            "{fail:?} carries the persisted run id"
        );
        // The specific failure kinds are distinguished, not collapsed.
        match (fail, failure) {
            (FailAt::GetRunMissing, ReadBackFailure::Missing)
            | (FailAt::GetRunLegacyInputs, ReadBackFailure::FreshInputsMissing)
            | (_, ReadBackFailure::Data(_)) => {}
            other => panic!("unexpected failure kind for {fail:?}: {other:?}"),
        }
        // The prose says the run WAS saved, so a reader is not left guessing.
        let message = err.to_string();
        assert!(
            message.contains(run_id.as_str()),
            "the message names the run id: {message}"
        );
        assert!(
            message.contains("saved"),
            "the message states the run was saved: {message}"
        );
        // And the row really is there — this is not a message that lies.
        let db = env.db().await;
        let runs = SqliteBacktestRunRepo::new(db.pool().clone());
        assert!(
            runs.get_run(run_id)
                .await
                .expect("read the saved run")
                .is_some(),
            "{fail:?}: the run the error names actually exists"
        );

        // The mapped BusError carries the id as a FIELD, not as prose W4 must parse.
        let bus = pulse::BusError::from(err);
        assert_eq!(
            bus.run_id.as_deref(),
            Some(run_id.as_str()),
            "BusError.run_id carries the persisted id"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_save_failure_has_no_run_id() {
    let env = env();
    let version_id = seed_version(&env).await;
    let err = run_with_failure(&env, &version_id, FailAt::Save).await;
    assert!(
        matches!(err, BacktestAppError::Persist(_)),
        "a save that never committed is Persist, never SavedButReadBackFailed: {err:?}"
    );
    let bus = pulse::BusError::from(err);
    assert_eq!(
        bus.run_id, None,
        "no row exists, so there is no id to report"
    );

    // A missing version is likewise pre-save and id-free.
    let missing = {
        let db = env.db().await;
        let strategies = SqliteStrategyRepo::new(db.pool().clone());
        let candles = InjectingCandleRepo {
            inner: env.store(),
            fail: FailAt::None,
            probe: None,
        };
        let runs = InjectingRunRepo {
            inner: SqliteBacktestRunRepo::new(db.pool().clone()),
            fail: FailAt::None,
        };
        let mut req = r1_request(&version_id);
        req.version_id = VersionId::new("no-such-version");
        run_version_backtest(
            &strategies,
            &candles,
            &pulse::BinanceAdapter::new(),
            &runs,
            &req,
        )
        .await
        .expect_err("an unknown version id is an error")
    };
    assert!(matches!(missing, BacktestAppError::VersionNotFound(_)));
    assert_eq!(pulse::BusError::from(missing).run_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_save_read_failure_names_the_read_not_the_save() {
    let env = env();
    let version_id = seed_version(&env).await;

    // Each of these fails a READ, with nothing written. Saying "persist backtest
    // run" there names an operation that never happened, and a reader chasing a
    // persistence bug would start in the wrong place.
    let cases = [
        (FailAt::VersionRead, "the strategy version"),
        (FailAt::PrimaryHeadRead, "the primary candle snapshot"),
        (FailAt::PriorRunRead, "the prior run"),
    ];
    for (fail, expected_stage) in cases {
        let err = run_with_failure(&env, &version_id, fail).await;
        assert!(
            matches!(err, BacktestAppError::PreSaveRead { .. }),
            "{fail:?} is a pre-save read, not a persist: {err:?}"
        );
        let message = err.to_string();
        assert!(
            message.contains(expected_stage),
            "{fail:?} names what it was reading: {message}"
        );
        assert!(
            !message.contains("persist backtest run"),
            "{fail:?} must not claim a save was attempted: {message}"
        );
        assert!(
            err.persisted_run_id().is_none(),
            "{fail:?} wrote nothing, so it names no run"
        );

        let bus = pulse::BusError::from(err);
        assert_eq!(bus.code, pulse::BusErrorCode::Data);
        assert_eq!(bus.run_id, None, "{fail:?} carries no run id");

        // And truly nothing was written.
        let db = env.db().await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM backtest_run")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(count, 0, "{fail:?} left no row behind");
    }
}

// ---------------------------------------------------------------------------
// 3b. the wire projection refuses rather than fabricating
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stored_value_that_will_not_fit_the_wire_refuses_and_names_the_run() {
    let env = env();
    let version_id = seed_version(&env).await;
    let state = env.cold_state().await;
    let dto = run_backtest_version_core(&state, request(&version_id))
        .await
        .expect("the happy path still projects");
    drop(state);

    // Rebuild the same outcome, then corrupt exactly one stored count beyond the
    // wire's u32. Clamping here would render a plausible false number for a row the
    // truth-source contract says must refuse.
    let db = env.db().await;
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let run = runs
        .get_run(&BacktestRunId::new(dto.run_id.clone()))
        .await
        .expect("read")
        .expect("exists");
    let trades = runs.get_trades(&run.id).await.expect("trades");
    let store = env.store();
    let inputs = run.inputs.clone().expect("fresh run carries inputs");
    let primary = store
        .load_version(
            &inputs.pair,
            inputs.primary.timeframe,
            &inputs.primary.data_version,
        )
        .expect("reload primary")
        .series;

    let mut broken = run.clone();
    broken.summary.trade_count = usize::MAX;
    let outcome = pulse::BacktestOutcome {
        run: broken,
        inputs: inputs.clone(),
        trades: trades.clone(),
        primary: primary.clone(),
        htf: None,
        fingerprint_warning: None,
        mfe: project_histogram(std::iter::empty()),
        mae: project_histogram(std::iter::empty()),
    };
    let err = pulse::backtest_run_dto(&outcome)
        .expect_err("a count that does not fit the wire must refuse, never clamp");
    let BacktestAppError::SavedButReadBackFailed {
        run_id,
        stage,
        failure,
    } = &err
    else {
        panic!("a projection failure is a post-save failure: {err:?}");
    };
    assert_eq!(stage, &ReadBackStage::Projection);
    assert_eq!(run_id.as_str(), dto.run_id, "it names the run that exists");
    assert!(
        matches!(failure, ReadBackFailure::Projection(reason) if reason.contains("trade_count")),
        "the refusal names the offending field: {failure:?}"
    );
    assert_eq!(
        pulse::BusError::from(err).run_id.as_deref(),
        Some(dto.run_id.as_str()),
        "the saved id still reaches the frontend as a field"
    );

    // The intact outcome still projects — the guard is not simply always-failing.
    let good = pulse::BacktestOutcome {
        run,
        inputs,
        trades,
        primary,
        htf: None,
        fingerprint_warning: None,
        mfe: project_histogram(std::iter::empty()),
        mae: project_histogram(std::iter::empty()),
    };
    assert!(
        pulse::backtest_run_dto(&good).is_ok(),
        "positive control: an uncorrupted outcome projects"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enum_tokens_match_the_stored_column_text_exactly() {
    let env = env();
    let version_id = seed_version(&env).await;
    let state = env.cold_state().await;
    let dto = run_backtest_version_core(&state, request(&version_id))
        .await
        .expect("run succeeds");

    // The labels are exhaustive matches, not a serde round-trip with an "unknown"
    // fallback — so they must still equal what the database actually stores.
    let db = env.db().await;
    let stored: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT direction, exit_reason, source, regime FROM trade \
         WHERE backtest_run_id = ?1 ORDER BY seq",
    )
    .bind(&dto.run_id)
    .fetch_all(db.pool())
    .await
    .expect("read the persisted trade tokens");
    assert_eq!(stored.len(), dto.trades.len());
    for (row, wire) in stored.iter().zip(dto.trades.iter()) {
        assert_eq!(row.0, wire.direction);
        assert_eq!(row.1, wire.exit_reason);
        assert_eq!(row.2, wire.source);
        assert_eq!(row.3, wire.regime);
    }
    assert_eq!(dto.funding, "snapshot_rates");
    assert!(
        dto.trades.iter().all(|t| t.exit_reason != "unknown"),
        "no token is ever fabricated"
    );
}

// ---------------------------------------------------------------------------
// 4. `save_run` commits and returns; it does not read back internally
// ---------------------------------------------------------------------------

/// The `save_run` body, comments blanked — from its signature to the next `async fn`.
/// The scanner itself is the shared `source_scan` module, self-tested there.
fn save_run_body() -> String {
    let code = blank_comments(&read_source("src/adapters/db/backtest_run_repo.rs"));
    let start = code
        .find("async fn save_run(")
        .expect("save_run is defined in the adapter");
    let rest = &code[start + 1..];
    let end = rest.find("async fn ").unwrap_or(rest.len());
    rest[..end].to_owned()
}

#[test]
fn save_run_commits_and_returns_the_id_without_reading_back() {
    let body = save_run_body();
    // Positive controls FIRST: if these ever stop matching, the scan below is
    // reading the wrong text and its negative assertion means nothing.
    assert!(
        body.contains("tx.commit()"),
        "positive control: save_run still commits its transaction"
    );
    assert!(
        body.contains("Ok(BacktestRunId::new(run_id))"),
        "positive control: save_run still returns the minted id"
    );
    // r1.s4.w4: the two INSERT mappings moved OUT of `save_run` into the
    // crate-private `insert_run_row` / `insert_trade_rows`, which the coach's
    // accept transaction now reuses rather than copying. The positive control has
    // to follow them, and it is checked in two halves so it still fails loudly if
    // either the call or the mapping disappears: `save_run` must still drive both
    // writes, and the file must still contain both statements.
    let file = blank_comments(&read_source("src/adapters/db/backtest_run_repo.rs"));
    assert!(
        body.contains("insert_run_row("),
        "positive control: save_run still writes the run"
    );
    assert!(
        body.contains("insert_trade_rows("),
        "positive control: save_run still writes the trades"
    );
    assert!(
        file.contains("INSERT INTO backtest_run"),
        "positive control: the run insert mapping is still in this adapter"
    );
    assert!(
        file.contains("INSERT INTO trade"),
        "positive control: the trade insert mapping is still in this adapter"
    );
    // The actual rule.
    assert!(
        !body.contains("self.get_run("),
        "save_run must not read back internally: the read runs outside the \
         transaction on another connection and discards the minted id on failure, \
         which makes the saved-run guarantee unstatable. W3's use case owns it."
    );
}

// ---------------------------------------------------------------------------
// 5. blocking work runs off the async runtime
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn candle_and_engine_work_runs_off_the_calling_worker_thread() {
    let env = env();
    let version_id = seed_version(&env).await;
    let caller = std::thread::current().id();
    let probe = std::sync::Arc::new(Mutex::new(Vec::new()));

    let db = env.db().await;
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let candles = InjectingCandleRepo {
        inner: env.store(),
        fail: FailAt::None,
        probe: Some(probe.clone()),
    };
    let runs = InjectingRunRepo {
        inner: SqliteBacktestRunRepo::new(db.pool().clone()),
        fail: FailAt::None,
    };
    run_version_backtest(
        &strategies,
        &candles,
        &pulse::BinanceAdapter::new(),
        &runs,
        &r1_request(&version_id),
    )
    .await
    .expect("run succeeds");

    let threads = probe.lock().unwrap().clone();
    let head_reads: Vec<_> = threads.iter().copied().take(2).collect();
    assert_eq!(
        head_reads.len(),
        2,
        "both HEAD snapshots were loaded: {threads:?}"
    );
    for tid in head_reads {
        assert_ne!(
            tid, caller,
            "synchronous Parquet reads must not run on the calling Tokio worker"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. the application ring has no order capability
// ---------------------------------------------------------------------------

/// EVERY Rust source file in the application ring, by glob (r1.s4.w2, #150).
///
/// It used to be a hard-coded pair, which meant the guarantee below held for the
/// two files someone remembered and for no others — and the ring grew a third file
/// (`coach.rs`) and then a fourth (`coach_decision.rs`) without either of them ever
/// being scanned. A glob is what makes "the application ring" the subject of the
/// assertion instead of "two files in it".
fn application_ring_sources() -> Vec<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/application");
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|entry| entry.expect("a readable dir entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .map(|path| {
            format!(
                "src/application/{}",
                path.file_name().expect("a file name").to_string_lossy()
            )
        })
        .collect();
    files.sort();
    assert!(
        files.len() >= 4,
        "the application ring has at least mod/backtest/coach/coach_decision; the glob \
         found {files:?} — a scan that reads nothing passes vacuously"
    );
    files
}

#[test]
fn the_application_ring_exposes_no_order_or_broker_capability() {
    for relative in application_ring_sources() {
        let code = blank_comments(&read_source(&relative));
        for banned in [
            "broker",
            "Broker",
            "OrderRepository",
            "place_order",
            "Order",
        ] {
            assert!(
                !code.contains(banned),
                "{relative} names `{banned}` in code — the use case's dependency set is \
                 the backtest-only kill-switch proof, so an order-capable surface may \
                 not be reachable from it"
            );
        }
        // Positive control: the four permitted ports ARE named, so the scan is
        // reading a real dependency set and not an empty file.
        if relative.ends_with("backtest.rs") {
            for required in [
                "StrategyRepository",
                "CandleSeriesRepository",
                "ExchangeAdapter",
                "BacktestRunRepository",
            ] {
                assert!(
                    code.contains(required),
                    "{relative} must name the permitted port `{required}`"
                );
            }
        }
    }
}

/// ADR-0015 keeps EXACTLY ONE deliberate adapters import in the application ring:
/// `crate::adapters::backtest`, the deterministic engine, which owns no I/O.
///
/// r1.s4.w2 / `pulseai-labs/pulse-trader#150`: there used to be a second —
/// `crate::adapters::llm::redacting_logging::Redactor` in `coach.rs` — and the pair
/// of them turned "one deliberate exception" into "however many have accumulated".
/// The redactor's pure text logic now lives in the domain ring, and this is the
/// assertion that keeps the exception count at one.
#[test]
fn the_application_ring_imports_exactly_one_adapter_namespace() {
    let mut engine_import_seen = false;
    for relative in application_ring_sources() {
        let code = blank_comments(&read_source(&relative));
        for line in code.lines() {
            let Some(at) = line.find("crate::adapters::") else {
                continue;
            };
            let named = &line[at..];
            assert!(
                named.starts_with("crate::adapters::backtest"),
                "{relative} names `{}` — ADR-0015 keeps exactly ONE deliberate adapters \
                 import in the application ring (`crate::adapters::backtest`, the \
                 deterministic engine), and this is a second one",
                named.split_whitespace().next().unwrap_or(named)
            );
            engine_import_seen = true;
        }
    }
    // Positive control: the ONE permitted import really is there, so a scan that
    // matched nothing cannot pass by reading an empty ring.
    assert!(
        engine_import_seen,
        "no file in the application ring imports `crate::adapters::backtest` — either \
         the engine import moved or this scan is reading the wrong tree"
    );
}

// ---------------------------------------------------------------------------
// 7. the pinned histogram projection
// ---------------------------------------------------------------------------

fn d(s: &str) -> Decimal {
    s.parse().unwrap()
}

#[test]
fn the_histogram_contract_is_exactly_as_pinned() {
    assert_eq!(histogram_bin_width(), d("0.25"));
    assert_eq!(HISTOGRAM_BIN_COUNT, 12);

    let empty = project_histogram(std::iter::empty());
    assert_eq!(empty.bins.len(), 12);
    assert_eq!(empty.bin_width, histogram_bin_width());
    assert_eq!(empty.underflow, 0);
    assert_eq!(empty.overflow, 0);
    // 12 bins of 0.25 cover exactly [0, 3).
    assert_eq!(empty.bins[0].lower, d("0"));
    assert_eq!(empty.bins[0].upper, d("0.25"));
    assert_eq!(empty.bins[11].lower, d("2.75"));
    assert_eq!(empty.bins[11].upper, d("3"));
}

#[test]
fn histogram_boundaries_are_lower_inclusive_and_upper_exclusive() {
    // Exact boundary values, not near-misses: 0.25 belongs to [0.25,0.50), never
    // to [0.00,0.25).
    let h = project_histogram([d("0"), d("0.25"), d("0.5"), d("2.75")].into_iter());
    assert_eq!(h.bins[0].count, 1, "0 lands in [0.00,0.25)");
    assert_eq!(h.bins[1].count, 1, "0.25 lands in [0.25,0.50)");
    assert_eq!(h.bins[2].count, 1, "0.50 lands in [0.50,0.75)");
    assert_eq!(h.bins[11].count, 1, "2.75 lands in [2.75,3.00)");
    assert_eq!(h.underflow, 0);
    assert_eq!(h.overflow, 0);
}

#[test]
fn histogram_underflow_and_overflow_are_named_not_dropped() {
    let h = project_histogram(
        [
            d("-0.0000001"),
            d("-5"),
            d("3"),
            d("3.0000001"),
            d("99"),
            d("1"),
        ]
        .into_iter(),
    );
    assert_eq!(h.underflow, 2, "negative normalized values underflow");
    assert_eq!(
        h.overflow, 3,
        "3 is the exclusive upper bound, so 3 overflows"
    );
    assert_eq!(h.bins[4].count, 1, "1.0 lands in [1.00,1.25)");
    let binned: u32 = h.bins.iter().map(|b| b.count).sum();
    assert_eq!(
        binned + h.underflow + h.overflow,
        6,
        "no value is ever dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_measured_fixture_produces_the_pinned_bins() {
    let env = env();
    let version_id = seed_version(&env).await;
    let state = env.cold_state().await;
    let dto = run_backtest_version_core(&state, request(&version_id))
        .await
        .expect("run succeeds");

    // Measured 2026-09-01 through the shipped binary over the committed fixture:
    // MFE-R 0.026340…–2.116691…, MAE-R -1.036511…–-0.109302…, 6 trades.
    let expect = |h: &pulse::HistogramDto, nonzero: &[(usize, u32)], label: &str| {
        assert_eq!(h.bins.len(), 12, "{label} has 12 finite bins");
        assert_eq!(h.underflow, 0, "{label} underflow");
        assert_eq!(h.overflow, 0, "{label} overflow");
        for (idx, bin) in h.bins.iter().enumerate() {
            let want = nonzero
                .iter()
                .find(|(i, _)| *i == idx)
                .map_or(0, |(_, n)| *n);
            assert_eq!(
                bin.count, want,
                "{label} bin {idx} [{}, {})",
                bin.lower, bin.upper
            );
        }
        let total: u32 = h.bins.iter().map(|b| b.count).sum();
        assert_eq!(total, 6, "{label} counts every trade");
    };
    expect(&dto.mfe, &[(0, 2), (4, 1), (6, 1), (8, 2)], "MFE");
    expect(&dto.mae, &[(0, 1), (1, 1), (2, 1), (4, 3)], "MAE");

    // The exact per-trade values live beside the projection, unrounded.
    assert!(
        dto.trades.iter().any(|t| t.mfe_r.starts_with("2.116691")),
        "exact MFE values survive: {:?}",
        dto.trades.iter().map(|t| &t.mfe_r).collect::<Vec<_>>()
    );
    assert!(
        dto.trades.iter().any(|t| t.mae_r.starts_with("-1.036511")),
        "exact MAE values keep their sign: {:?}",
        dto.trades.iter().map(|t| &t.mae_r).collect::<Vec<_>>()
    );
}

// ---------------------------------------------------------------------------
// 8. the DTO is built from the read-back, and the command is registered
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_response_reports_the_reloaded_snapshot_date_range_and_saved_values() {
    let env = env();
    let version_id = seed_version(&env).await;
    let state = env.cold_state().await;
    let dto = run_backtest_version_core(&state, request(&version_id))
        .await
        .expect("run succeeds");

    // The date range comes from the RELOADED primary snapshot, not from HEAD and
    // not from the in-memory result.
    let store = env.store();
    let pair = Pair::new("BTCUSDT");
    let series = store
        .load_version(
            &pair,
            Timeframe::M15,
            &DataVersion::new(dto.primary_data_version.clone()),
        )
        .expect("the recorded primary snapshot reloads")
        .series;
    assert_eq!(
        dto.first_open_time_ms,
        series.candles.first().unwrap().open_time.to_string()
    );
    assert_eq!(
        dto.last_close_time_ms,
        series.candles.last().unwrap().close_time.to_string()
    );

    // Headline values equal the SAVED row, not the pre-save result.
    let db = env.db().await;
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let saved = runs
        .get_run(&BacktestRunId::new(dto.run_id.clone()))
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(dto.net_pnl, saved.net_pnl.normalize().to_string());
    assert_eq!(dto.result_content_hash, saved.result_content_hash);
    assert_eq!(dto.engine_fingerprint, saved.engine_fingerprint);
    assert_eq!(i64::from(dto.schema_version), saved.schema_version);
    assert_eq!(dto.strategy_version_id, version_id.as_str());
    assert_eq!(dto.starting_equity, "10000");
    assert_eq!(dto.taker_fee_bps, "4");
    assert_eq!(dto.slippage_bps, "1");
    assert_eq!(dto.funding, "snapshot_rates");

    // Equity is rebuilt from the persisted trades, and regimes come from the
    // persisted breakdown in fixed order.
    assert!(!dto.equity.is_empty(), "an equity curve was reconstructed");
    assert_eq!(dto.regimes.len(), 4);
    assert_eq!(dto.regimes[0].regime, "trending_up");
    assert_eq!(dto.regimes[1].regime, "trending_down");
    assert_eq!(dto.regimes[2].regime, "ranging");
    assert_eq!(dto.regimes[3].regime, "unknown");

    // Fills are deliberately absent from the wire shape.
    let json = serde_json::to_string(&dto).expect("serialize the DTO");
    assert!(
        !json.contains("\"fills\""),
        "the DTO omits inline fills — W4 renders no fill-level view"
    );
}

#[test]
fn the_command_is_registered_once_in_the_append_only_list() {
    assert!(
        pulse::BUS_COMMANDS.contains(&"run_backtest_version"),
        "the command joins the single append-only registry: {:?}",
        pulse::BUS_COMMANDS
    );
    assert_eq!(
        pulse::BUS_COMMANDS
            .iter()
            .filter(|c| **c == "run_backtest_version")
            .count(),
        1,
        "registered exactly once"
    );
}
