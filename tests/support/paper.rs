//! Shared harness for the r3.s4.w3 live-runtime suites (`paper_shadow_identity`,
//! `paper_catch_up`, `paper_runtime_poll`, `paper_epochs`, `paper_runtime_stop`).
//!
//! A real migrated database, a real `CandleStore`, real strategy/backtest
//! repositories, the real `PaperRuntime` over a **scripted** `ClosedBarSource`
//! and a hand-advanced clock. Nothing here is a production code path: the
//! scripted source and the clock are the two injected seams the spec names.
//!
//! This module compiles into every suite that declares `mod support;`, so
//! helpers only one suite uses read as dead code elsewhere — a named allowance,
//! the `support/server.rs` precedent.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines,
    dead_code
)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pulse::{
    BinanceAdapter, Candle, CandleStore, Clock, ClosedBarSource, CreatedBy, DataError, Db, LiveEnv,
    NewVersion, NonEmptyLabel, NonEmptyReason, OverrideRequest, Pair, PaperControl, PaperRuntime,
    PaperSession, PaperSessionId, PaperSessionRepository, SettlePolicy, SqliteBacktestRunRepo,
    SqliteCertificationRepo, SqlitePaperSessionRepo, SqliteStrategyRepo, StrategyDsl,
    StrategyRepository, SystemClock, Timeframe, VersionId, promote,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

use crate::support::mcp::migrated_db;

// ---------------------------------------------------------------------------
// The clock: one handle the repository and the runtime share
// ---------------------------------------------------------------------------

/// Lock a mutex, recovering from poisoning (a panicking test must not take the
/// harness down with it — the `CaptureLog` precedent).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A clock the test advances by hand. Cloning shares the handle, so the
/// repository that stamps `created_at` and the runtime that applies the
/// closed-candle cutoff always read the same instant.
#[derive(Clone)]
pub struct SteppedClock {
    now_ms: Arc<AtomicI64>,
}

impl SteppedClock {
    /// A clock pinned to `now_ms`.
    pub fn at(now_ms: i64) -> Self {
        Self {
            now_ms: Arc::new(AtomicI64::new(now_ms)),
        }
    }

    /// Jump to an absolute instant.
    pub fn set(&self, now_ms: i64) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }

    /// The current instant.
    pub fn now(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }

    /// Advance by `by_ms`.
    pub fn advance(&self, by_ms: i64) {
        self.now_ms.fetch_add(by_ms, Ordering::SeqCst);
    }
}

impl Clock for SteppedClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// The scripted closed-bar source
// ---------------------------------------------------------------------------

/// A `ClosedBarSource` over a fixed per-timeframe script. By default it serves
/// exactly what the production REST source does — bars strictly newer than
/// `since_ms`, and only those already closed at the clock's instant — but the
/// cutoff can be switched off so a suite can prove the runtime applies its own.
#[derive(Clone)]
pub struct ScriptedBars {
    pair: String,
    bars: Arc<Mutex<BTreeMap<Timeframe, Vec<Candle>>>>,
    calls: Arc<Mutex<Vec<(Timeframe, i64)>>>,
    clock: SteppedClock,
    cutoff: bool,
}

impl ScriptedBars {
    /// An empty script for `pair`, cutting off at `clock`.
    pub fn new(pair: &str, clock: &SteppedClock) -> Self {
        Self {
            pair: pair.to_owned(),
            bars: Arc::new(Mutex::new(BTreeMap::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
            clock: clock.clone(),
            cutoff: true,
        }
    }

    /// Turn the source-side closed-candle cutoff off (the runtime must then
    /// enforce it itself).
    pub fn without_cutoff(mut self) -> Self {
        self.cutoff = false;
        self
    }

    /// Replace a timeframe's script (bars must be sorted ascending).
    pub fn script(&self, timeframe: Timeframe, bars: Vec<Candle>) {
        lock(&self.bars).insert(timeframe, bars);
    }

    /// Every `(timeframe, since_ms)` the runtime has asked for, in order.
    pub fn calls(&self) -> Vec<(Timeframe, i64)> {
        lock(&self.calls).clone()
    }

    /// How many fetches this timeframe saw.
    pub fn call_count(&self, timeframe: Timeframe) -> usize {
        lock(&self.calls)
            .iter()
            .filter(|(tf, _)| *tf == timeframe)
            .count()
    }
}

impl ClosedBarSource for ScriptedBars {
    fn closed_since(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        since_ms: i64,
    ) -> impl Future<Output = Result<Vec<Candle>, DataError>> + Send {
        assert_eq!(pair.as_str(), self.pair, "the runtimes' pair is BTCUSDT");
        lock(&self.calls).push((timeframe, since_ms));
        let cutoff = self.cutoff;
        let now = self.clock.now();
        let out: Vec<Candle> = lock(&self.bars)
            .get(&timeframe)
            .map(|bars| {
                bars.iter()
                    .filter(|bar| bar.open_time > since_ms)
                    .filter(|bar| !cutoff || bar.close_time < now)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        async move { Ok(out) }
    }
}

// ---------------------------------------------------------------------------
// A capture log
// ---------------------------------------------------------------------------

/// A `RuntimeLog` that keeps every line, for assertions.
#[derive(Default)]
pub struct VecLog(Mutex<Vec<String>>);

impl VecLog {
    /// The captured lines, oldest first.
    pub fn lines(&self) -> Vec<String> {
        lock(&self.0).clone()
    }

    /// Whether any line contains `needle`.
    pub fn contains(&self, needle: &str) -> bool {
        self.lines().iter().any(|line| line.contains(needle))
    }
}

impl pulse::RuntimeLog for VecLog {
    fn write(&self, line: String) {
        lock(&self.0).push(line);
    }
}

// ---------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------

/// The concrete runtime the suites drive: the real repository, store and
/// strategy repository, the real Binance metadata adapter, the scripted source
/// and the hand-advanced clock.
pub type TestRuntime = PaperRuntime<
    SqlitePaperSessionRepo<SteppedClock>,
    ScriptedBars,
    CandleStore,
    SteppedClock,
    LiveEnv<SqliteStrategyRepo<SystemClock>, BinanceAdapter>,
>;

/// Everything a runtime suite needs: a migrated DB, a candle store, the
/// repositories, the scripted source and the shared clock/log handles.
pub struct PaperWorld {
    pub tmp: TempDir,
    pub candles_tmp: TempDir,
    pub db: Db,
    pub store: CandleStore,
    pub clock: SteppedClock,
    pub source: ScriptedBars,
    pub log: Arc<VecLog>,
    pub grace_ms: i64,
    /// The runtime's settle gate (#306): the production policy unless a suite
    /// opts out with `None` (every closed bar final on its first read).
    pub settle: Option<SettlePolicy>,
}

/// The clock every suite starts at unless it says otherwise: a Monday, one day
/// into the fixture series' second month, mid-15-minute-bar.
pub const DEFAULT_NOW_MS: i64 = 1_738_368_900_000; // 2025-02-01T00:15:00Z

impl PaperWorld {
    /// Build the world over a fresh temp DB and candle store.
    pub async fn new() -> Self {
        let tmp = TempDir::new().unwrap();
        let (_path, db) = migrated_db(&tmp).await;
        let candles_tmp = TempDir::new().unwrap();
        let clock = SteppedClock::at(DEFAULT_NOW_MS);
        Self {
            store: CandleStore::with_base_dir(candles_tmp.path().to_path_buf()),
            source: ScriptedBars::new("BTCUSDT", &clock),
            log: Arc::new(VecLog::default()),
            clock,
            grace_ms: 0,
            settle: Some(SettlePolicy::DEFAULT),
            db,
            tmp,
            candles_tmp,
        }
    }

    /// The world without the #306 settle gate, for suites that wake exactly
    /// at a bar's close over a source that never revises a bar (the gate has
    /// its own suite, `tests/paper_bar_settle.rs`).
    pub async fn ungated() -> Self {
        Self {
            settle: None,
            ..Self::new().await
        }
    }

    /// A fresh certification-record store over the world's pool (r4.s1.w5) —
    /// the promotion gate's read.
    pub fn certifications(&self) -> SqliteCertificationRepo<SystemClock> {
        SqliteCertificationRepo::new(self.db.pool().clone())
    }

    /// A fresh paper-session repository over the world's pool/store/clock.
    pub fn paper(&self) -> SqlitePaperSessionRepo<SteppedClock> {
        SqlitePaperSessionRepo::with_clock(
            self.db.pool().clone(),
            self.clock.clone(),
            self.store.clone(),
        )
    }

    /// A fresh strategy repository over the world's pool.
    pub fn strategies(&self) -> SqliteStrategyRepo<SystemClock> {
        SqliteStrategyRepo::new(self.db.pool().clone())
    }

    /// A fresh backtest-run repository over the world's pool.
    pub fn runs(&self) -> SqliteBacktestRunRepo<SystemClock> {
        SqliteBacktestRunRepo::new(self.db.pool().clone())
    }

    /// The candle store's base dir (where a HEAD pointer would live).
    pub fn store_base(&self) -> PathBuf {
        self.candles_tmp.path().to_path_buf()
    }

    /// A runtime over this world, with the standard seams.
    pub fn runtime(&self) -> TestRuntime {
        let runtime = PaperRuntime::new(
            self.paper(),
            self.source.clone(),
            self.store.clone(),
            self.clock.clone(),
            LiveEnv::new(self.strategies(), BinanceAdapter::new()),
            self.grace_ms,
            self.log.clone(),
        );
        match self.settle {
            Some(policy) => runtime.with_settle(policy),
            None => runtime.without_settle_gate(),
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

/// Mint a strategy version carrying `dsl` — the w2 `create_strategy` +
/// `create_version` pair.
pub async fn create_version(world: &PaperWorld, name: &str, dsl: &StrategyDsl) -> VersionId {
    let strategies = world.strategies();
    let strategy = strategies
        .create_strategy(name, None, &[])
        .await
        .expect("create the strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(dsl).expect("serialize the DSL"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create the version");
    version.id
}

/// Promote one override session (the w2 `promote` use case), returning the row.
pub async fn promote_session(
    world: &PaperWorld,
    version_id: &VersionId,
    primary: Timeframe,
    htf: Option<Timeframe>,
    uses_d1: bool,
) -> PaperSession {
    let request = OverrideRequest {
        reason: NonEmptyReason::try_new("w3 runtime test").unwrap(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: primary,
        htf_timeframe: htf,
        uses_d1,
    };
    promote(
        &world.strategies(),
        &world.runs(),
        &world.runs(),
        &world.paper(),
        &world.clock.clone(),
        &world.certifications(),
        version_id,
        Some(&request),
        NonEmptyLabel::try_new("operator-token").unwrap(),
    )
    .await
    .expect("the override promotion succeeds")
}

/// A timerange of the fixture M15 series: `count` bars from `start_open_ms`.
pub fn fixture_m15_slice(start_open_ms: i64, count: usize) -> Vec<Candle> {
    pulse::fixture_m15_candles()
        .into_iter()
        .filter(|bar| bar.open_time >= start_open_ms)
        .take(count)
        .collect()
}

/// The H4 fixture bars whose `open_time` falls in `[start_open_ms, end_ms)`.
pub fn fixture_h4_between(start_open_ms: i64, end_ms: i64) -> Vec<Candle> {
    pulse::fixture_h4_candles()
        .into_iter()
        .filter(|bar| bar.open_time >= start_open_ms && bar.open_time < end_ms)
        .collect()
}

/// One M15-cadence bar on the fixture grid, funding stamped every 8h.
pub fn m15_bar(open_time: i64, open: i64, close: i64) -> Candle {
    Candle {
        open_time,
        close_time: open_time + 899_999,
        open: Decimal::from(open),
        high: Decimal::from(open.max(close) + 20),
        low: Decimal::from(open.min(close) - 20),
        close: Decimal::from(close),
        volume: Decimal::from(100),
        funding_rate: (open_time % 28_800_000 == 0).then(|| Decimal::from_str("0.00001").unwrap()),
    }
}

/// One bar of any cadence.
pub fn bar_of(timeframe: Timeframe, open_time: i64, open: i64, close: i64) -> Candle {
    let step = timeframe.duration_ms();
    Candle {
        open_time,
        close_time: open_time + step - 1,
        open: Decimal::from(open),
        high: Decimal::from(open.max(close) + 20),
        low: Decimal::from(open.min(close) - 20),
        close: Decimal::from(close),
        volume: Decimal::from(100),
        funding_rate: None,
    }
}

/// The session's recorded bars for one timeframe, straight from the table.
pub async fn recorded_bars(world: &PaperWorld, id: &PaperSessionId, tf: Timeframe) -> Vec<Candle> {
    world.paper().bars(id, tf).await.expect("bars read back")
}

/// The session's log, decoded.
pub async fn log_events(world: &PaperWorld, id: &PaperSessionId) -> Vec<pulse::PaperEvent> {
    world.paper().events(id).await.expect("the log reads back")
}

// ---------------------------------------------------------------------------
// The runtime host (r3.s4.w4): the live runtime behind the API suites
// ---------------------------------------------------------------------------

/// The server-seam host: the real [`PaperRuntime`] over the same doubles the
/// world builds, driven through the SAME command path the production loop
/// uses — with a test-controlled wake trigger instead of the wall-clock timer,
/// so a test advances [`Self::clock`] and calls [`Self::tick`] to run one
/// deterministic wake.
pub struct PaperHost {
    /// The control handle the server stores (its commands reach the runtime).
    pub control: PaperControl,
    /// The hand-advanced clock the runtime and the repositories share.
    pub clock: SteppedClock,
    /// The scripted closed-bar source.
    pub source: ScriptedBars,
    /// Every runtime log line.
    pub log: Arc<VecLog>,
    db: Db,
    store: CandleStore,
    tick: tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>,
    stop: tokio::sync::watch::Sender<bool>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl PaperHost {
    /// Build the host over the server's own pool and store base, and start its
    /// runtime thread (boot, then serve commands and ticks until dropped).
    pub fn spawn(db: &Db, store_base: &std::path::Path) -> Arc<Self> {
        let clock = SteppedClock::at(DEFAULT_NOW_MS);
        let source = ScriptedBars::new("BTCUSDT", &clock);
        let log = Arc::new(VecLog::default());
        let store = CandleStore::with_base_dir(store_base.to_path_buf());
        let (control, control_rx) = PaperControl::channel(30_000);
        let (tick_tx, tick_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let pool = db.pool().clone();
        let host = Arc::new(Self {
            control: control.clone(),
            clock: clock.clone(),
            source: source.clone(),
            log: log.clone(),
            db: db.clone(),
            store: store.clone(),
            tick: tick_tx,
            stop: stop_tx,
            join: Mutex::new(None),
        });
        let handle = std::thread::Builder::new()
            .name("paper-test-runtime".to_owned())
            .spawn(move || {
                let Ok(thread_rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build()
                else {
                    return;
                };
                // The runtime is BUILT on this thread (its engine sessions are
                // deliberately `!Send`, the w3 shape) over the shared doubles.
                let repo =
                    SqlitePaperSessionRepo::with_clock(pool.clone(), clock.clone(), store.clone());
                let runtime = PaperRuntime::new(
                    repo,
                    source,
                    store,
                    clock,
                    LiveEnv::new(SqliteStrategyRepo::new(pool.clone()), BinanceAdapter::new()),
                    0,
                    log,
                )
                // The host's suites wake exactly at a bar's close over a
                // source that never revises a bar: no settle gate (#306's
                // own suite drives the gate).
                .without_settle_gate();
                // The PRODUCTION loop, driven by a tick instead of the
                // wall-clock timer: commands and wakes interleave exactly as
                // they do in `pulse serve`.
                thread_rt.block_on(pulse::run_paper_runtime(
                    runtime,
                    stop_rx,
                    control_rx,
                    pulse::WakeTrigger::Tick(tick_rx),
                ));
            })
            .expect("spawn the paper runtime host");
        *lock(&host.join) = Some(handle);
        host
    }

    /// A fresh paper-session repository over the host's pool/store/clock.
    pub fn paper(&self) -> SqlitePaperSessionRepo<SteppedClock> {
        SqlitePaperSessionRepo::with_clock(
            self.db.pool().clone(),
            self.clock.clone(),
            self.store.clone(),
        )
    }

    /// A fresh certification-record store over the host's pool (r4.s1.w5) —
    /// the app-side promotion's record read.
    pub fn certifications(&self) -> SqliteCertificationRepo<SystemClock> {
        SqliteCertificationRepo::new(self.db.pool().clone())
    }

    /// A fresh strategy repository over the host's pool.
    pub fn strategies(&self) -> SqliteStrategyRepo<SystemClock> {
        SqliteStrategyRepo::new(self.db.pool().clone())
    }

    /// A fresh backtest-run repository over the host's pool.
    pub fn runs(&self) -> SqliteBacktestRunRepo<SystemClock> {
        SqliteBacktestRunRepo::new(self.db.pool().clone())
    }

    /// The candle store the host's repository materialises through.
    pub fn store(&self) -> CandleStore {
        self.store.clone()
    }

    /// Run one wake now and wait for it to finish (the deterministic timer
    /// stand-in: the test advances [`Self::clock`] first).
    pub async fn tick(&self) {
        let (reply, done) = tokio::sync::oneshot::channel();
        self.tick
            .send(reply)
            .expect("the runtime host is still alive");
        done.await.expect("the wake completes");
    }
}

impl Drop for PaperHost {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        if let Some(handle) = lock(&self.join).take() {
            let _ = handle.join();
        }
    }
}
