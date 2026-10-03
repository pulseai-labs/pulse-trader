//! The live paper runtime (r3.s4.w3, ADR-0027): promote a session, and it
//! trades — REST-polled closed bars, stepped through its `EngineSession`,
//! appended to its log one atomic batch per bar, caught up on boot, and
//! shadow-checked against its own recorded bars.
//!
//! **What runs where.** [`PaperRuntime`] polls each running session's own
//! timeframes, steps the session's engine, and writes through the
//! [`PaperSessionRepository`] port; `pulse serve` owns the timer and the
//! shutdown (step 8 of the spec). The runtime is generic over the repository,
//! the closed-bar source, the candle-snapshot repository, the clock and the
//! [`SessionEnv`] seam that compiles a session's strategy version and resolves
//! its symbol filters — this ring names no exchange adapter and no store, and
//! its ONE adapters import is the deterministic engine (`crate::adapters::backtest`,
//! ADR-0015).
//!
//! **No order capability.** The application ring must not name an
//! order-placing surface (`tests/tauri_backtest.rs`'s ring guard). Every event
//! this file writes is built from the domain's own values or from
//! [`crate::domain::paper::runtime`]'s derivation; the signal/fill vocabulary
//! lives in that domain module and nowhere else.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::adapters::backtest::{
    BacktestConfig, EngineSession, SessionTimeframes, first_fully_warm_bar_ms, run_backtest,
};
use crate::application::paper_control::{PaperCommand, ShadowCheckReply, StopAllReply, StopReply};
use crate::domain::backtest::{BacktestError, OpenPositionMark};
use crate::domain::paper::event::{PaperEvent, StopActor};
use crate::domain::paper::runtime::{
    EpochStart, ShadowResult, StepView, boundaries, compare, daily_shadow_due, events_for_step,
    first_open_bar_ms,
};
use crate::domain::paper::session::{NonEmptyLabel, PaperSession, PaperSessionId};
use crate::domain::paper::state::{PaperSessionState, PaperSessionStatus, ReplayError};
use crate::domain::strategy::VersionId;
use crate::domain::{
    Candle, CandleSeries, CandleSeriesRepository, Clock, ClosedBarSource, CompiledStrategy,
    DataError, DataVersion, EngineFingerprint, ExchangeError, Pair, PaperSessionRepository,
    SeriesEnd, StrategyRepository, SymbolFilters, Timeframe, compile, validate,
};

/// How long the serve loop waits before re-scanning for new sessions when
/// nothing is running.
const IDLE_SCAN_MS: i64 = 60_000;

/// The probe's depth cap, in primary bars: a strategy that never warms must not
/// page the whole exchange history.
const PROBE_MAX_DEPTH: i64 = 4_096;

/// How many catch-up passes a boot may run before it gives up (each pass
/// consumes at least one bar or stops).
const CATCH_UP_PASSES: usize = 64;

/// The snapshot version tag the warm-up probe's throwaway series carries; the
/// engine never reads it.
const PROBE_VERSION: &str = "paper-probe";

// ---------------------------------------------------------------------------
// The seams
// ---------------------------------------------------------------------------

/// Where the runtime writes its one-line diagnostics. The server routes them
/// into the same sink as its startup lines; tests capture them.
pub trait RuntimeLog: Send + Sync {
    /// Write one complete line.
    fn write(&self, line: String);
}

/// How the runtime gets a session's trading environment: the compiled strategy
/// and the symbol filters its engine is constructed with.
///
/// `Send + Sync` because the runtime's futures cross threads (the serve task):
/// the environment is shared behind `&self` for the whole process.
pub trait SessionEnv: Send + Sync {
    /// Compile the session's strategy version.
    ///
    /// # Errors
    ///
    /// [`PaperRuntimeError::Data`] when the version cannot be read,
    /// [`PaperRuntimeError::UnknownVersion`] when it does not exist, and
    /// [`PaperRuntimeError::Compile`] when it does not validate or compile.
    fn compile(
        &self,
        session: &PaperSession,
    ) -> impl Future<Output = Result<CompiledStrategy, PaperRuntimeError>> + Send;

    /// The exchange filters for a pair.
    ///
    /// # Errors
    ///
    /// [`PaperRuntimeError::Exchange`] when the pair is unknown to the adapter.
    fn filters(&self, pair: &Pair) -> Result<SymbolFilters, PaperRuntimeError>;
}

/// The production [`SessionEnv`]: the strategy repository and the exchange
/// adapter, both domain ports.
pub struct LiveEnv<S, X> {
    strategies: S,
    exchange: X,
}

impl<S, X> LiveEnv<S, X> {
    /// Compose over a strategy repository and an exchange adapter.
    #[must_use]
    pub fn new(strategies: S, exchange: X) -> Self {
        Self {
            strategies,
            exchange,
        }
    }
}

impl<S, X> SessionEnv for LiveEnv<S, X>
where
    S: StrategyRepository + Send + Sync,
    X: crate::domain::ExchangeAdapter + Send + Sync,
{
    fn compile(
        &self,
        session: &PaperSession,
    ) -> impl Future<Output = Result<CompiledStrategy, PaperRuntimeError>> + Send {
        let version_id: VersionId = session.strategy_version_id.clone();
        async move {
            let version = self
                .strategies
                .get_version(&version_id)
                .await
                .map_err(PaperRuntimeError::Data)?
                .ok_or_else(|| PaperRuntimeError::UnknownVersion(version_id.clone()))?;
            let validated =
                validate(&version.dsl).map_err(|e| PaperRuntimeError::Compile(e.to_string()))?;
            compile(&validated).map_err(|e| PaperRuntimeError::Compile(e.to_string()))
        }
    }

    fn filters(&self, pair: &Pair) -> Result<SymbolFilters, PaperRuntimeError> {
        self.exchange
            .symbol_filters(pair)
            .map_err(PaperRuntimeError::Exchange)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything the runtime can refuse with. One session's failure is never
/// another's, and never the server's.
#[derive(Debug)]
pub enum PaperRuntimeError {
    /// A store read or write failed.
    Data(DataError),
    /// A session's log does not replay.
    Replay(ReplayError),
    /// The engine refused an input the runtime handed it outside the bar path.
    Engine(BacktestError),
    /// The session's strategy version failed validation or compilation.
    Compile(String),
    /// The exchange adapter does not know the pair.
    Exchange(ExchangeError),
    /// The session's strategy version is gone.
    UnknownVersion(VersionId),
    /// No such paper-session row.
    UnknownSession(PaperSessionId),
    /// The session is not (or no longer) run by this runtime.
    NotRunning(PaperSessionId),
}

impl core::fmt::Display for PaperRuntimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Data(error) => write!(f, "paper runtime store failure: {error}"),
            Self::Replay(error) => write!(f, "paper runtime replay refused: {error}"),
            Self::Engine(error) => write!(f, "paper runtime engine refused: {error}"),
            Self::Compile(error) => write!(f, "paper runtime strategy refused: {error}"),
            Self::Exchange(error) => write!(f, "paper runtime exchange refused: {error}"),
            Self::UnknownVersion(id) => {
                write!(f, "paper runtime: no such strategy version {}", id.as_str())
            }
            Self::UnknownSession(id) => {
                write!(f, "paper runtime: no such paper session {}", id.as_str())
            }
            Self::NotRunning(id) => write!(f, "paper runtime: session {id} is not running"),
        }
    }
}

impl PaperRuntimeError {
    /// The stable snake-case code a control reply names this refusal by
    /// (`stop-all`'s `failures[].code`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Data(_) => "data",
            Self::Replay(_) => "replay",
            Self::Engine(_) => "engine",
            Self::Compile(_) => "compile",
            Self::Exchange(_) => "exchange",
            Self::UnknownVersion(_) => "unknown_version",
            Self::UnknownSession(_) => "unknown_session",
            Self::NotRunning(_) => "not_running",
        }
    }
}

impl std::error::Error for PaperRuntimeError {}

impl From<DataError> for PaperRuntimeError {
    fn from(error: DataError) -> Self {
        Self::Data(error)
    }
}

impl From<ReplayError> for PaperRuntimeError {
    fn from(error: ReplayError) -> Self {
        Self::Replay(error)
    }
}

impl From<BacktestError> for PaperRuntimeError {
    fn from(error: BacktestError) -> Self {
        Self::Engine(error)
    }
}

/// One session's failure in a pass that kept going (the log carries the same
/// account).
#[derive(Debug)]
pub struct SessionFailure {
    /// The session that failed.
    pub session_id: PaperSessionId,
    /// What failed.
    pub error: PaperRuntimeError,
}

// ---------------------------------------------------------------------------
// The runtime
// ---------------------------------------------------------------------------

/// The live runtime: one `EngineSession` per running session, one fetch per
/// `(pair, timeframe)` per wake, one atomic append per consumed bar.
pub struct PaperRuntime<R, B, S, C, E> {
    repo: R,
    source: B,
    series: S,
    clock: C,
    env: E,
    grace_ms: i64,
    log: Arc<dyn RuntimeLog>,
    sessions: BTreeMap<PaperSessionId, RunningSession>,
}

/// One attached session's live state.
struct RunningSession {
    session: PaperSession,
    compiled: CompiledStrategy,
    config: BacktestConfig,
    filters: SymbolFilters,
    engine: EngineSession,
    /// Every recorded bar per timeframe, keyed by `open_time` — the rebuild's
    /// material, the disagreement check's reference and the "what is new"
    /// boundary.
    recorded: BTreeMap<Timeframe, BTreeMap<i64, Candle>>,
    /// The `open_time` the session's recording starts from (its first live
    /// bar), used as the fetch floor for a timeframe that has no rows yet.
    since_floor_ms: i64,
    /// Which slice of the log the live epoch owns (E3).
    epoch: EpochStart,
    /// When the last shadow check ran (the daily cadence's anchor).
    last_shadow_ms: Option<i64>,
    /// The refusals and data disagreements already written, by bar `open_time`
    /// — at most one `data_event` per distinct bar and refusal.
    reported: BTreeMap<i64, String>,
    /// A failed append rebuilds from the log before the next attempt.
    needs_rebuild: bool,
}

/// What one bar's processing produced.
enum StepOutcome {
    /// The bar stepped; its batch is ready to append.
    Consumed {
        bars: Vec<(Timeframe, Candle, bool)>,
        events: Vec<PaperEvent>,
    },
    /// The step refused; the session holds at this bar.
    Refused { summary: String, first_time: bool },
    /// No new bar to consume.
    Idle,
}

impl<R, B, S, C, E> PaperRuntime<R, B, S, C, E>
where
    R: PaperSessionRepository,
    B: ClosedBarSource,
    S: CandleSeriesRepository,
    C: Clock,
    E: SessionEnv,
{
    /// Compose the runtime. `grace_ms` is the polling grace past a bar's close
    /// (the spec's 5 s default, set in `ServeConfig`).
    pub fn new(
        repo: R,
        source: B,
        series: S,
        clock: C,
        env: E,
        grace_ms: i64,
        log: Arc<dyn RuntimeLog>,
    ) -> Self {
        Self {
            repo,
            source,
            series,
            clock,
            env,
            grace_ms,
            log,
            sessions: BTreeMap::new(),
        }
    }

    /// The next instant any attached session's timeframes need a poll — or, with
    /// nothing running, the idle re-scan instant (a session promoted later must
    /// be picked up without a restart).
    #[must_use]
    pub fn next_wake_ms(&self) -> Option<i64> {
        let now = self.clock.now_ms();
        let mut next: Option<i64> = None;
        for run in self.sessions.values() {
            for timeframe in run.session.timeframes() {
                let probe = boundaries(timeframe.duration_ms(), now, self.grace_ms);
                next = Some(next.map_or(probe.next_poll_ms, |cur| cur.min(probe.next_poll_ms)));
            }
        }
        Some(next.unwrap_or(now + IDLE_SCAN_MS))
    }

    /// The server's boot: attach every running session (replay, epoch, rebuild,
    /// catch-up) and shadow-check each once it has caught up. Per-session
    /// failures are returned and logged; the listener is never touched.
    pub async fn boot(&mut self) -> Vec<SessionFailure> {
        let mut failures = self.attach_all().await;
        failures.extend(self.poll_pass().await.0);
        failures.extend(self.daily_checks().await);
        for failure in &failures {
            self.log.write(format!(
                "paper runtime: boot: session {}: {}",
                failure.session_id, failure.error
            ));
        }
        failures
    }

    /// One wake: pick up newly promoted sessions, run one polling pass, then
    /// any due daily shadow check.
    pub async fn wake(&mut self) -> Vec<SessionFailure> {
        let mut failures = self.attach_all().await;
        failures.extend(self.poll_pass().await.0);
        failures.extend(self.daily_checks().await);
        failures
    }

    /// Run the shadow check for one session now (the on-demand entry w4's route
    /// calls), appending the `shadow_checked` event and returning the verdict.
    ///
    /// # Errors
    ///
    /// [`PaperRuntimeError::NotRunning`] for a session this runtime does not
    /// run, and the store/engine refusals a materialisation or a shadow run can
    /// raise.
    #[allow(clippy::too_many_lines)] // one linear sequence: materialise, load, run, compare, append
    pub async fn shadow_check(
        &mut self,
        session_id: &PaperSessionId,
    ) -> Result<ShadowResult, PaperRuntimeError> {
        let run = self
            .sessions
            .get(session_id)
            .ok_or_else(|| PaperRuntimeError::NotRunning(session_id.clone()))?;
        let session = run.session.clone();
        let compiled = run.compiled.clone();
        let config = run.config;
        let filters = run.filters.clone();
        let epoch = run.epoch;
        let bar_count: u64 = run
            .recorded
            .values()
            .map(|bars| u64::try_from(bars.len()).unwrap_or(u64::MAX))
            .sum();
        let live_open: Option<OpenPositionMark> = run
            .recorded
            .get(&session.primary_timeframe)
            .and_then(|bars| bars.values().next_back())
            .and_then(|last| run.engine.open_position_mark(last));
        let live_trades = run.engine.closed_trades().to_vec();

        // The session's own rows, materialised to content-addressed snapshots
        // and read back BY VERSION (never HEAD); the shadow runs over exactly
        // those.
        let selections = self.repo.materialise(session_id).await?;
        let pairs = session.pair.clone();
        let mut series: BTreeMap<Timeframe, CandleSeries> = BTreeMap::new();
        for selection in &selections {
            let stored =
                self.series
                    .load_version(&pairs, selection.timeframe, &selection.data_version)?;
            series.insert(selection.timeframe, stored.series);
        }
        let count_from_ms = self.repo.count_from_ms(session_id).await?;
        // Nothing recorded yet: there is no shadow to run, and the live side
        // has nothing either — an honest empty comparison.
        let Some(primary) = series.get(&session.primary_timeframe) else {
            let verdict = compare(
                &live_trades,
                live_open.as_ref(),
                &[],
                None,
                EpochStart::Empty,
            );
            let event = PaperEvent::ShadowChecked {
                seq: 0,
                at: self.now_text(),
                data_versions: Vec::new(),
                bar_count: 0,
                result: serde_json::to_value(&verdict).unwrap_or(serde_json::Value::Null),
            };
            self.repo.append_bar(session_id, &[], &[event]).await?;
            if let Some(run) = self.sessions.get_mut(session_id) {
                run.last_shadow_ms = Some(self.clock.now_ms());
            }
            return Ok(verdict);
        };
        let htf = session
            .htf_timeframe
            .and_then(|timeframe| series.get(&timeframe));
        let d1 = session
            .d1_timeframe()
            .and_then(|timeframe| series.get(&timeframe));
        let result = run_backtest(
            &compiled,
            primary,
            htf,
            d1,
            &config,
            &filters,
            SeriesEnd::WindowEdge,
            count_from_ms,
        )
        .map_err(PaperRuntimeError::Engine)?;

        let verdict = compare(
            &live_trades,
            live_open.as_ref(),
            &result.trades,
            result.open_position.as_ref(),
            epoch,
        );
        let data_versions = selections
            .iter()
            .map(
                |selection| crate::domain::paper::session::CertifiedDataVersion {
                    timeframe: selection.timeframe,
                    data_version: selection.data_version.clone(),
                },
            )
            .collect();
        let event = PaperEvent::ShadowChecked {
            seq: 0,
            at: self.now_text(),
            data_versions,
            bar_count,
            result: serde_json::to_value(&verdict).unwrap_or(serde_json::Value::Null),
        };
        self.repo.append_bar(session_id, &[], &[event]).await?;
        if let Some(run) = self.sessions.get_mut(session_id) {
            run.last_shadow_ms = Some(self.clock.now_ms());
        }
        if let ShadowResult::Drift {
            first_divergence, ..
        } = &verdict
        {
            self.log.write(format!(
                "paper runtime: session {session_id} shadow drift: {first_divergence}"
            ));
        }
        Ok(verdict)
    }

    /// Stop one session: attempt a final shadow check (the schema refuses
    /// anything after `stop`), append the `stop` event, then forget it.
    ///
    /// The kill switch is fail-safe. A failed final shadow check is logged but
    /// never vetoes the stop — the check is evidence, not a gate. And the
    /// session leaves `self.sessions` even when the `stop` append itself is
    /// refused: the in-memory halt wins over the log write, so a store
    /// failure cannot keep it trading. The append's error still comes back
    /// (`stop_all` lists it under `failures`); the log then holds no `stop`,
    /// so a later boot re-attaches the session.
    ///
    /// # Errors
    ///
    /// [`PaperRuntimeError::NotRunning`] for a session this runtime does not
    /// run; the `stop` append's refusal otherwise (a failed check is logged,
    /// never returned).
    pub async fn stop(
        &mut self,
        session_id: &PaperSessionId,
        actor: StopActor,
    ) -> Result<(), PaperRuntimeError> {
        if !self.sessions.contains_key(session_id) {
            return Err(PaperRuntimeError::NotRunning(session_id.clone()));
        }
        // The shadow check runs BEFORE `stop` — the trigger makes the log
        // read-only afterwards — but its failure is not a veto: a kill switch
        // a failed check can block is not a kill switch.
        if let Err(error) = self.shadow_check(session_id).await {
            self.log.write(format!(
                "paper runtime: stop: session {session_id}: final shadow check failed: {error}"
            ));
        }
        let event = PaperEvent::Stop {
            seq: 0,
            at: self.now_text(),
            actor,
        };
        // The halt precedes the append's outcome: the session stops trading
        // now even if the log write is refused.
        self.sessions.remove(session_id);
        self.repo.append_bar(session_id, &[], &[event]).await?;
        Ok(())
    }

    /// Stop every running session with `stop_all`. A failure on one session
    /// never leaves the others running: every stop is attempted, and the
    /// failures come back (and are logged).
    pub async fn stop_all(&mut self, issuer: NonEmptyLabel) -> Vec<SessionFailure> {
        let ids: Vec<PaperSessionId> = self.sessions.keys().cloned().collect();
        let mut failures = Vec::new();
        for id in ids {
            let actor = StopActor::StopAll {
                issuer: issuer.clone(),
            };
            if let Err(error) = self.stop(&id, actor).await {
                self.log.write(format!(
                    "paper runtime: stop_all: session {id} failed: {error}"
                ));
                failures.push(SessionFailure {
                    session_id: id,
                    error,
                });
            }
        }
        failures
    }

    // -- the control handle's entry points (r3.s4.w4, spec §1) --------------

    /// The ids of the sessions this runtime currently runs.
    #[must_use]
    pub fn attached_ids(&self) -> Vec<PaperSessionId> {
        self.sessions.keys().cloned().collect()
    }

    /// Attach one persisted session now (the promote path's `Attach`): the
    /// per-session half of [`Self::attach_all`], reachable for one id. An
    /// already-attached session is a no-op.
    ///
    /// # Errors
    ///
    /// [`PaperRuntimeError::UnknownSession`] for an absent row; the replay,
    /// compile, store and engine refusals [`Self::attach`] raises otherwise.
    pub async fn attach_one(&mut self, id: &PaperSessionId) -> Result<(), PaperRuntimeError> {
        if self.sessions.contains_key(id) {
            return Ok(());
        }
        let session = self
            .repo
            .get_session(id)
            .await?
            .ok_or_else(|| PaperRuntimeError::UnknownSession(id.clone()))?;
        self.attach(session).await
    }

    /// Stop a session whether or not it is attached (spec §1): an attached one
    /// goes through the ordinary final-shadow-check path; an unattached one is
    /// stopped through the repository directly — a final `Stop` append with no
    /// shadow check. A session can always be stopped.
    ///
    /// # Errors
    ///
    /// The replay/store refusals the direct path can raise; an attached
    /// session's errors come from [`Self::stop`].
    pub async fn stop_or_unattached(
        &mut self,
        id: &PaperSessionId,
        actor: StopActor,
    ) -> Result<StopReply, PaperRuntimeError> {
        if self.sessions.contains_key(id) {
            self.stop(id, actor).await?;
            return Ok(StopReply::Stopped);
        }
        let Some(session) = self.repo.get_session(id).await? else {
            return Ok(StopReply::Unknown);
        };
        let log = self.repo.events(id).await?;
        let state = PaperSessionState::replay(&session, &log)?;
        if state.status == PaperSessionStatus::Stopped {
            return Ok(StopReply::AlreadyStopped);
        }
        let event = PaperEvent::Stop {
            seq: 0,
            at: self.now_text(),
            actor,
        };
        self.repo.append_bar(id, &[], &[event]).await?;
        Ok(StopReply::StoppedWithoutShadow)
    }

    /// Run one session's shadow check on demand, or answer why not: an
    /// unattached session has no live state to check, a stopped one has
    /// nothing to check, and an absent one does not exist.
    ///
    /// # Errors
    ///
    /// The store/replay/materialisation/engine refusals the check can raise.
    pub async fn shadow_check_outcome(
        &mut self,
        id: &PaperSessionId,
    ) -> Result<ShadowCheckReply, PaperRuntimeError> {
        if self.sessions.contains_key(id) {
            return self.shadow_check(id).await.map(ShadowCheckReply::Checked);
        }
        let Some(session) = self.repo.get_session(id).await? else {
            return Ok(ShadowCheckReply::Unknown);
        };
        let log = self.repo.events(id).await?;
        let state = PaperSessionState::replay(&session, &log)?;
        Ok(if state.status == PaperSessionStatus::Stopped {
            ShadowCheckReply::Stopped
        } else {
            ShadowCheckReply::NotAttached
        })
    }

    /// Apply one control command and answer through its reply channel. The
    /// production loop and the test host both call exactly this, so the
    /// command semantics have one home.
    pub async fn handle_command(&mut self, command: PaperCommand) {
        match command {
            PaperCommand::Attach { session_id, reply } => {
                let result = self.attach_one(&session_id).await;
                if let Err(error) = &result {
                    self.log.write(format!(
                        "paper runtime: attach: session {session_id} could not start: {error}"
                    ));
                }
                let _ = reply.send(result);
            }
            PaperCommand::Stop {
                session_id,
                actor,
                reply,
            } => {
                let result = self.stop_or_unattached(&session_id, actor).await;
                let _ = reply.send(result);
            }
            PaperCommand::StopAll { issuer, reply } => {
                let ids = self.attached_ids();
                let failures = self.stop_all(issuer).await;
                let stopped = ids
                    .into_iter()
                    .filter(|id| !failures.iter().any(|failure| failure.session_id == *id))
                    .collect();
                let _ = reply.send(StopAllReply { stopped, failures });
            }
            PaperCommand::ShadowCheck { session_id, reply } => {
                let result = self.shadow_check_outcome(&session_id).await;
                let _ = reply.send(result);
            }
        }
    }

    // -- internals ----------------------------------------------------------

    /// Attach every persisted session that runs and is not attached yet.
    async fn attach_all(&mut self) -> Vec<SessionFailure> {
        let mut failures = Vec::new();
        let sessions = match self.repo.list_sessions().await {
            Ok(sessions) => sessions,
            Err(error) => {
                failures.push(SessionFailure {
                    session_id: PaperSessionId::new(String::new()),
                    error: PaperRuntimeError::Data(error),
                });
                return failures;
            }
        };
        for session in sessions {
            if self.sessions.contains_key(&session.id) {
                continue;
            }
            let id = session.id.clone();
            if let Err(error) = self.attach(session).await {
                self.log.write(format!(
                    "paper runtime: session {id} could not start: {error}"
                ));
                failures.push(SessionFailure {
                    session_id: id,
                    error,
                });
            }
        }
        failures
    }

    /// Replay a session, open a new epoch if the build changed, rebuild the
    /// engine from its recorded bars (first start included), catch up through
    /// the ordinary polling path, then shadow-check it.
    async fn attach(&mut self, session: PaperSession) -> Result<(), PaperRuntimeError> {
        let id = session.id.clone();
        let log = self.repo.events(&id).await?;
        let state = PaperSessionState::replay(&session, &log)?;
        if state.status == PaperSessionStatus::Stopped {
            return Ok(());
        }
        // E3: a build change opens exactly one new epoch, before the rebuild's
        // bars are consumed, so the epoch boundary sits at the live bar.
        let mut epoch = epoch_start(&log);
        if state.epochs.last() != Some(&EngineFingerprint::current()) {
            let old = state
                .epochs
                .last()
                .cloned()
                .unwrap_or_else(EngineFingerprint::current);
            let event = PaperEvent::EngineUpgraded {
                seq: 0,
                at: self.now_text(),
                old,
                new: EngineFingerprint::current(),
            };
            self.repo.append_bar(&id, &[], &[event]).await?;
            epoch = EpochStart::Empty;
        }

        let compiled = self.env.compile(&session).await?;
        let filters = self.env.filters(&session.pair)?;
        let config = config_of(&session);
        let loaded = self
            .load_and_build(&session, &compiled, config, &filters)
            .await?;
        self.sessions.insert(
            id.clone(),
            RunningSession {
                session,
                compiled,
                config,
                filters,
                engine: loaded.engine,
                recorded: loaded.recorded,
                since_floor_ms: loaded.since_floor_ms,
                epoch,
                last_shadow_ms: None,
                reported: BTreeMap::new(),
                needs_rebuild: false,
            },
        );

        // Catch up through the SAME polling pass a wake runs — there is no
        // special catch-up code path: repeat the pass until it consumes
        // nothing (or a pass fails).
        for _ in 0..CATCH_UP_PASSES {
            let (failures, consumed) = self.poll_pass().await;
            if consumed == 0 || !failures.is_empty() {
                break;
            }
        }
        // A session whose first start only landed lead-in has nothing to check
        // yet; the check still runs (an empty comparison) so the boot's shape is
        // uniform.
        self.shadow_check(&id).await?;
        Ok(())
    }

    /// Rebuild one session's engine from the log's recorded bars (the
    /// append-failure recovery, and the boot path).
    async fn rebuild(&mut self, id: &PaperSessionId) -> Result<(), PaperRuntimeError> {
        let (session, compiled, config, filters) = {
            let run = self
                .sessions
                .get(id)
                .ok_or_else(|| PaperRuntimeError::NotRunning(id.clone()))?;
            (
                run.session.clone(),
                run.compiled.clone(),
                run.config,
                run.filters.clone(),
            )
        };
        let loaded = self
            .load_and_build(&session, &compiled, config, &filters)
            .await?;
        let run = self
            .sessions
            .get_mut(id)
            .ok_or_else(|| PaperRuntimeError::NotRunning(id.clone()))?;
        run.engine = loaded.engine;
        run.recorded = loaded.recorded;
        run.since_floor_ms = loaded.since_floor_ms;
        // `reported` deliberately survives the rebuild: "at most one
        // `data_event` per distinct bar and refusal" is a property of the
        // session's life, not of one engine instance.
        run.needs_rebuild = false;
        Ok(())
    }

    /// Read the session's recorded bars, first-start it when there are none,
    /// and fold them into a fresh engine (the `run_backtest` order: drain each
    /// higher series by `close_time <= primary.close_time`).
    async fn load_and_build(
        &self,
        session: &PaperSession,
        compiled: &CompiledStrategy,
        config: BacktestConfig,
        filters: &SymbolFilters,
    ) -> Result<LoadedSession, PaperRuntimeError> {
        let primary = session.primary_timeframe;
        if self.repo.bars(&session.id, primary).await?.is_empty() {
            // First start: probe the warm-up window and append it as lead-in.
            let (lead_in, first_live) = self.probe_lead_in(session, compiled).await?;
            let mut batch: Vec<(Timeframe, Candle, bool)> = Vec::new();
            for timeframe in session.timeframes() {
                if let Some(candles) = lead_in.get(&timeframe) {
                    for candle in candles {
                        batch.push((timeframe, candle.clone(), true));
                    }
                }
            }
            if !batch.is_empty() {
                self.repo.append_bar(&session.id, &batch, &[]).await?;
            }
            let _ = first_live;
        }

        let mut bars_by_timeframe: BTreeMap<Timeframe, Vec<Candle>> = BTreeMap::new();
        for timeframe in session.timeframes() {
            bars_by_timeframe.insert(timeframe, self.repo.bars(&session.id, timeframe).await?);
        }
        let count_from_ms = self.repo.count_from_ms(&session.id).await?;
        let mut engine = EngineSession::new(
            compiled,
            &session.pair,
            session_timeframes(session),
            config,
            filters.clone(),
            count_from_ms,
        )
        .map_err(PaperRuntimeError::Engine)?;

        let primary_bars = bars_by_timeframe.get(&primary).cloned().unwrap_or_default();
        let htf_bars: Vec<Candle> = session
            .htf_timeframe
            .and_then(|timeframe| bars_by_timeframe.get(&timeframe))
            .cloned()
            .unwrap_or_default();
        let d1_bars: Vec<Candle> = session
            .d1_timeframe()
            .and_then(|timeframe| bars_by_timeframe.get(&timeframe))
            .cloned()
            .unwrap_or_default();
        let (mut htf_cursor, mut d1_cursor) = (0_usize, 0_usize);
        for bar in &primary_bars {
            let mut closed_htf: Vec<Candle> = Vec::new();
            while let Some(candle) = htf_bars.get(htf_cursor)
                && candle.close_time <= bar.close_time
            {
                closed_htf.push(candle.clone());
                htf_cursor += 1;
            }
            let mut closed_d1: Vec<Candle> = Vec::new();
            while let Some(candle) = d1_bars.get(d1_cursor)
                && candle.close_time <= bar.close_time
            {
                closed_d1.push(candle.clone());
                d1_cursor += 1;
            }
            engine
                .step(bar, &closed_htf, &closed_d1)
                .map_err(PaperRuntimeError::Engine)?;
        }

        let since_floor_ms = count_from_ms.unwrap_or_else(|| {
            primary_bars.last().map_or_else(
                || first_open_bar_ms(primary.duration_ms(), self.clock.now_ms()),
                |bar| bar.open_time + primary.duration_ms(),
            )
        });
        let recorded = bars_by_timeframe
            .into_iter()
            .map(|(timeframe, candles)| {
                (
                    timeframe,
                    candles
                        .into_iter()
                        .map(|candle| (candle.open_time, candle))
                        .collect::<BTreeMap<i64, Candle>>(),
                )
            })
            .collect();
        Ok(LoadedSession {
            engine,
            recorded,
            since_floor_ms,
        })
    }

    /// The warm-up probe (spec step 2): the lead-in window for a session whose
    /// first bar is `L` = the first bar of its primary cadence not closed at
    /// `now`. The window starts two primary bars back and doubles until
    /// [`first_fully_warm_bar_ms`] finds a fully warm bar inside it — the probe
    /// series IS the windows the live engine will step, so a warm bar in it
    /// proves the engine arrives warm at `L` (warmth is monotone), and no
    /// constant is invented. A source that cannot supply more history ends the
    /// loop honestly: the engine then warms up live.
    async fn probe_lead_in(
        &self,
        session: &PaperSession,
        compiled: &CompiledStrategy,
    ) -> Result<(BTreeMap<Timeframe, Vec<Candle>>, i64), PaperRuntimeError> {
        let now = self.clock.now_ms();
        let primary = session.primary_timeframe;
        let step = primary.duration_ms();
        let first_live = first_open_bar_ms(step, now);
        let mut depth = 2_i64;
        let mut previous_len: Option<usize> = None;
        loop {
            let since = first_live - depth * step - 1;
            let mut probe: BTreeMap<Timeframe, Vec<Candle>> = BTreeMap::new();
            for timeframe in session.timeframes() {
                let bars = self
                    .source
                    .closed_since(&session.pair, timeframe, since)
                    .await?;
                let bars: Vec<Candle> = bars
                    .into_iter()
                    .filter(|bar| bar.open_time < first_live && bar.close_time < now)
                    .collect();
                probe.insert(timeframe, bars);
            }
            let primary_series = probe_series(&session.pair, primary, &probe);
            let htf_series = session
                .htf_timeframe
                .map(|timeframe| probe_series(&session.pair, timeframe, &probe));
            let d1_series = session
                .d1_timeframe()
                .map(|timeframe| probe_series(&session.pair, timeframe, &probe));
            let len = primary_series.candles.len();
            let warm = first_fully_warm_bar_ms(
                compiled,
                &primary_series,
                htf_series.as_ref(),
                d1_series.as_ref(),
            )
            .is_some();
            let grew = previous_len.is_none_or(|previous| len > previous);
            if warm || !grew || len == 0 || depth >= PROBE_MAX_DEPTH {
                return Ok((probe, first_live));
            }
            previous_len = Some(len);
            depth *= 2;
        }
    }

    /// One polling pass: one fetch per `(pair, timeframe)` from the oldest
    /// boundary any session needs, then each session consumes its new bars.
    /// Returns the pass's failures and how many bars it consumed (the catch-up
    /// loop's progress signal).
    async fn poll_pass(&mut self) -> (Vec<SessionFailure>, usize) {
        let mut failures = Vec::new();
        let rebuild_ids: Vec<PaperSessionId> = self
            .sessions
            .iter()
            .filter(|(_, run)| run.needs_rebuild)
            .map(|(id, _)| id.clone())
            .collect();
        for id in rebuild_ids {
            if let Err(error) = self.rebuild(&id).await {
                self.log.write(format!(
                    "paper runtime: session {id} rebuild failed: {error}"
                ));
                failures.push(SessionFailure {
                    session_id: id,
                    error,
                });
            }
        }

        // The fetch set: every `(pair, timeframe)` any session needs, from the
        // OLDEST boundary — one fetch serves them all.
        let mut needs: BTreeMap<(Pair, Timeframe), i64> = BTreeMap::new();
        for run in self.sessions.values() {
            for timeframe in run.session.timeframes() {
                let since = run.since_for(timeframe);
                needs
                    .entry((run.session.pair.clone(), timeframe))
                    .and_modify(|current| *current = (*current).min(since))
                    .or_insert(since);
            }
        }
        let now = self.clock.now_ms();
        let mut fetched: BTreeMap<(Pair, Timeframe), Vec<Candle>> = BTreeMap::new();
        let mut fetch_errors: BTreeMap<(Pair, Timeframe), DataError> = BTreeMap::new();
        for ((pair, timeframe), since) in needs {
            match self.source.closed_since(&pair, timeframe, since).await {
                Ok(bars) => {
                    fetched.insert(
                        (pair, timeframe),
                        bars.into_iter()
                            .filter(|bar| bar.close_time < now)
                            .collect(),
                    );
                }
                Err(error) => {
                    fetch_errors.insert((pair, timeframe), error);
                }
            }
        }

        let mut consumed_total = 0_usize;
        let ids: Vec<PaperSessionId> = self.sessions.keys().cloned().collect();
        for id in ids {
            let blocked: Option<PaperRuntimeError> = {
                let Some(run) = self.sessions.get(&id) else {
                    continue;
                };
                let pair = run.session.pair.clone();
                run.session.timeframes().into_iter().find_map(|timeframe| {
                    fetch_errors
                        .get(&(pair.clone(), timeframe))
                        .map(|error| PaperRuntimeError::Data(DataError::Io(error.to_string())))
                })
            };
            if let Some(error) = blocked {
                failures.push(SessionFailure {
                    session_id: id.clone(),
                    error,
                });
                continue;
            }
            let mut consumed = true;
            while consumed {
                match self.consume_one_bar(&id, &fetched).await {
                    Ok(false) => consumed = false,
                    Ok(true) => consumed_total += 1,
                    Err(error) => {
                        self.log
                            .write(format!("paper runtime: session {id} poll failed: {error}"));
                        failures.push(SessionFailure {
                            session_id: id.clone(),
                            error,
                        });
                        consumed = false;
                    }
                }
            }
        }
        (failures, consumed_total)
    }

    /// Consume the next new primary bar of one session, with its newly closed
    /// higher bars, as ONE append. Returns whether a bar was consumed.
    async fn consume_one_bar(
        &mut self,
        id: &PaperSessionId,
        fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>,
    ) -> Result<bool, PaperRuntimeError> {
        let at = self.now_text();
        let outcome = {
            let run = self
                .sessions
                .get_mut(id)
                .ok_or_else(|| PaperRuntimeError::NotRunning(id.clone()))?;
            let disagreements = scan_disagreements(run, fetched);
            if let Some((_bar_open_time, summary, first_time)) = disagreements.first() {
                // Report the first and hold this session for the pass; the
                // recorded bar is never replaced. At most one `data_event` per
                // distinct bar and refusal.
                StepOutcome::Refused {
                    summary: summary.clone(),
                    first_time: *first_time,
                }
            } else {
                prepare_step(run, fetched, &at)
            }
        };

        match outcome {
            StepOutcome::Idle => Ok(false),
            StepOutcome::Consumed { bars, events } => {
                let primary_candle = bars[0].1.clone();
                let appended = self.repo.append_bar(id, &bars, &events).await;
                match appended {
                    Ok(_) => {
                        let run = self
                            .sessions
                            .get_mut(id)
                            .ok_or_else(|| PaperRuntimeError::NotRunning(id.clone()))?;
                        for (timeframe, candle, _) in &bars {
                            run.recorded
                                .entry(*timeframe)
                                .or_default()
                                .insert(candle.open_time, candle.clone());
                        }
                        if run.epoch == EpochStart::Empty {
                            run.epoch = EpochStart::Bar(primary_candle.open_time);
                        }
                        Ok(true)
                    }
                    Err(error) => {
                        // The batch rolled back whole; rebuild from the log
                        // before the next attempt.
                        if let Some(run) = self.sessions.get_mut(id) {
                            run.needs_rebuild = true;
                        }
                        self.log.write(format!(
                            "paper runtime: session {id} append failed: {error}"
                        ));
                        Err(PaperRuntimeError::Data(error))
                    }
                }
            }
            StepOutcome::Refused {
                summary,
                first_time,
            } => {
                if first_time {
                    let event = PaperEvent::DataEvent {
                        seq: 0,
                        at,
                        summary: summary.clone(),
                    };
                    self.repo.append_bar(id, &[], &[event]).await?;
                    self.log
                        .write(format!("paper runtime: session {id}: {summary}"));
                }
                Ok(false)
            }
        }
    }

    /// Any daily shadow check that has come due since the last one.
    async fn daily_checks(&mut self) -> Vec<SessionFailure> {
        let now = self.clock.now_ms();
        let due: Vec<PaperSessionId> = self
            .sessions
            .iter()
            .filter(|(_, run)| daily_shadow_due(now, run.last_shadow_ms.unwrap_or(now)))
            .map(|(id, _)| id.clone())
            .collect();
        let mut failures = Vec::new();
        for id in due {
            if let Err(error) = self.shadow_check(&id).await {
                self.log.write(format!(
                    "paper runtime: session {id} shadow check failed: {error}"
                ));
                failures.push(SessionFailure {
                    session_id: id,
                    error,
                });
            }
        }
        failures
    }

    /// The injected clock's instant, as the RFC3339 text every event row
    /// carries.
    fn now_text(&self) -> String {
        clock_text(&self.clock)
    }
}

/// One session's freshly built engine plus its recorded-bar map.
struct LoadedSession {
    engine: EngineSession,
    recorded: BTreeMap<Timeframe, BTreeMap<i64, Candle>>,
    since_floor_ms: i64,
}

impl RunningSession {
    /// The fetch boundary for one timeframe: one millisecond before its newest
    /// recorded bar (so the boundary bar itself is re-fetched and can be
    /// compared), or the session's recording floor while it has no rows.
    fn since_for(&self, timeframe: Timeframe) -> i64 {
        self.recorded
            .get(&timeframe)
            .and_then(|bars| bars.keys().next_back().copied())
            .map_or(self.since_floor_ms - 1, |last| last - 1)
    }

    /// Whether a fetched bar of `timeframe` is new to this session.
    fn is_new(&self, timeframe: Timeframe, bar: &Candle) -> bool {
        self.recorded
            .get(&timeframe)
            .is_none_or(|bars| bars.get(&bar.open_time).is_none())
    }

    /// The newly closed higher-timeframe bars for a primary bar: those that
    /// closed by its close and are not recorded yet, ascending.
    fn new_higher_bars(
        &self,
        timeframe: Option<Timeframe>,
        fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>,
        pair: &Pair,
        primary_close: i64,
    ) -> Vec<Candle> {
        let Some(timeframe) = timeframe else {
            return Vec::new();
        };
        fetched
            .get(&(pair.clone(), timeframe))
            .map(|bars| {
                bars.iter()
                    .filter(|bar| bar.close_time <= primary_close && self.is_new(timeframe, bar))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Every fetched bar that is already recorded with DIFFERENT values — the A6
/// data disagreement. Each entry is `(bar open_time, summary, first_time)`;
/// `first_time` is false when this exact `(bar, refusal)` was already written.
fn scan_disagreements(
    run: &mut RunningSession,
    fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>,
) -> Vec<(i64, String, bool)> {
    let pair = run.session.pair.clone();
    let mut disagreements: Vec<(i64, String, bool)> = Vec::new();
    for timeframe in run.session.timeframes() {
        let Some(bars) = fetched.get(&(pair.clone(), timeframe)) else {
            continue;
        };
        for bar in bars {
            if let Some(recorded) = run
                .recorded
                .get(&timeframe)
                .and_then(|bars| bars.get(&bar.open_time))
                && recorded != bar
            {
                let tag = format!("disagreement:{timeframe:?}");
                let first_time = run.reported.get(&bar.open_time) != Some(&tag);
                if first_time {
                    run.reported.insert(bar.open_time, tag);
                }
                disagreements.push((
                    bar.open_time,
                    format!(
                        "re-fetched {} bar {} differs from the recorded bar; \
                         the recorded bar is kept",
                        timeframe.binance_interval(),
                        bar.open_time
                    ),
                    first_time,
                ));
            }
        }
    }
    disagreements
}

/// Step one session's next new primary bar (with its newly closed higher bars)
/// and build the batch it should append. A refused step is returned as
/// [`StepOutcome::Refused`]; the engine's state is untouched by it.
fn prepare_step(
    run: &mut RunningSession,
    fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>,
    at: &str,
) -> StepOutcome {
    let pair = run.session.pair.clone();
    let primary = run.session.primary_timeframe;
    let next: Option<Candle> = fetched.get(&(pair.clone(), primary)).and_then(|bars| {
        bars.iter()
            .filter(|bar| run.is_new(primary, bar))
            .min_by_key(|bar| bar.open_time)
            .cloned()
    });
    let Some(bar) = next else {
        return StepOutcome::Idle;
    };
    let htf_new = run.new_higher_bars(run.session.htf_timeframe, fetched, &pair, bar.close_time);
    let d1_new = run.new_higher_bars(run.session.d1_timeframe(), fetched, &pair, bar.close_time);
    let before_len = run.engine.closed_trades().len();
    let before_mark = run
        .recorded
        .get(&primary)
        .and_then(|bars| bars.values().next_back())
        .and_then(|last| run.engine.open_position_mark(last));
    if let Err(error) = run.engine.step(&bar, &htf_new, &d1_new) {
        let tag = format!("{error}");
        let first_time = run.reported.get(&bar.open_time) != Some(&tag);
        if first_time {
            run.reported.insert(bar.open_time, tag);
        }
        return StepOutcome::Refused {
            summary: format!("step refused at bar {}: {error}", bar.open_time),
            first_time,
        };
    }
    let before = StepView {
        closed_trades: &run.engine.closed_trades()[..before_len],
        open_position: before_mark,
    };
    let after = StepView {
        closed_trades: run.engine.closed_trades(),
        open_position: run.engine.open_position_mark(&bar),
    };
    let mut bars: Vec<(Timeframe, Candle, bool)> = vec![(primary, bar.clone(), false)];
    if let Some(timeframe) = run.session.htf_timeframe {
        for candle in &htf_new {
            bars.push((timeframe, candle.clone(), false));
        }
    }
    if let Some(timeframe) = run.session.d1_timeframe() {
        for candle in &d1_new {
            bars.push((timeframe, candle.clone(), false));
        }
    }
    let refs: Vec<crate::domain::paper::event::BarRef> = bars
        .iter()
        .map(
            |(timeframe, candle, _)| crate::domain::paper::event::BarRef {
                timeframe: *timeframe,
                open_time: candle.open_time,
            },
        )
        .collect();
    let mut events = vec![PaperEvent::BarProcessed {
        seq: 0,
        at: at.to_owned(),
        bars: refs,
    }];
    events.extend(events_for_step(&before, &after, &bar, at));
    StepOutcome::Consumed { bars, events }
}

/// The `SessionTimeframes` an engine is constructed with.
fn session_timeframes(session: &PaperSession) -> SessionTimeframes {
    SessionTimeframes {
        primary: session.primary_timeframe,
        htf: session.htf_timeframe,
        d1: session.d1_timeframe(),
    }
}

/// The session's cost configuration (A1: equity, fee and slippage from the
/// row).
fn config_of(session: &PaperSession) -> BacktestConfig {
    BacktestConfig {
        starting_equity: session.starting_equity,
        taker_fee_bps: session.taker_fee_bps,
        slippage_bps: session.slippage_bps,
    }
}

/// A throwaway series over a probe window (the version tag is never read).
fn probe_series(
    pair: &Pair,
    timeframe: Timeframe,
    probe: &BTreeMap<Timeframe, Vec<Candle>>,
) -> CandleSeries {
    CandleSeries {
        pair: pair.clone(),
        timeframe,
        version: DataVersion::new(PROBE_VERSION),
        candles: probe.get(&timeframe).cloned().unwrap_or_default(),
    }
}

/// The live epoch a session's log starts (E3): no `engine_upgraded` means the
/// whole log; otherwise the first primary bar consumed after the last upgrade
/// (or none yet).
fn epoch_start(log: &[PaperEvent]) -> EpochStart {
    let mut epoch = EpochStart::WholeLog;
    for event in log {
        match event {
            PaperEvent::EngineUpgraded { .. } => epoch = EpochStart::Empty,
            PaperEvent::BarProcessed { bars, .. } => {
                if epoch == EpochStart::Empty
                    && let Some(bar) = bars.first()
                {
                    epoch = EpochStart::Bar(bar.open_time);
                }
            }
            _ => {}
        }
    }
    epoch
}

/// The injected clock's instant as RFC3339 UTC text (the repository's own
/// convention).
fn clock_text<C: Clock>(clock: &C) -> String {
    chrono::DateTime::from_timestamp_millis(clock.now_ms())
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}
