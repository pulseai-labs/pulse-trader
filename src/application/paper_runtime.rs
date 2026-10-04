//! The live paper runtime (r3.s4.w3, ADR-0027): promote a session, and it
//! trades — REST-polled closed bars, stepped through its `EngineSession`,
//! appended to its log one atomic batch per bar, caught up on boot, and
//! shadow-checked against its own recorded bars.
//!
//! **Only settled bars are recorded (#306).** The exchange can still update a
//! kline for some seconds after its close, so the first read past the close
//! is provisional. A bar is recorded and stepped only once it is final: read
//! at least [`SettlePolicy::settle_ms`] after its close and confirmed by an
//! identical read at least [`SettlePolicy::repoll_ms`] later; the runtime
//! re-polls on that spacing until then. Lead-in passes the same gate, and a
//! primary bar waits for every higher bar that closes with it — including one
//! that no read has returned yet while it lies inside the session's fetch
//! window (R2-4), so a read that omits the bar never steps the primary
//! without it. A disagreement
//! with a bar that was already recorded is a true revision and stays a
//! `data_event`.
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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use rust_decimal::Decimal;

use crate::adapters::backtest::{
    BacktestConfig, EngineSession, SessionTimeframes, first_fully_warm_bar_ms, run_backtest,
};
use crate::application::paper_control::{PaperCommand, ShadowCheckReply, StopAllReply, StopReply};
use crate::domain::backtest::{BacktestError, OpenPositionMark};
use crate::domain::paper::event::{PaperEvent, PaperSide, StopActor};
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

/// How long past its close a bar may stay unsettled before the runtime logs
/// it once (#306): the exchange normally converges within seconds.
const UNSETTLED_WARN_MS: i64 = 300_000;

/// The snapshot version tag the warm-up probe's throwaway series carries; the
/// engine never reads it.
const PROBE_VERSION: &str = "paper-probe";

/// When a fetched bar is final enough to record (#306).
///
/// A read counts only once it lies `settle_ms` or more past the bar's close,
/// and the bar is final once a read `repoll_ms` or more after the first
/// counting read still returns it identically. The
/// first poll of a bar is therefore `max(grace, settle_ms)` past its close and
/// the confirming poll `repoll_ms` after that: with the default policy and a
/// steady source a bar lands 40 s after its close (plus the serve loop's
/// jitter). Each read that differs from the one before restarts the
/// confirmation, one more `repoll_ms` each time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlePolicy {
    /// How long after its close a bar must be read for the read to count.
    pub settle_ms: i64,
    /// How long after the first counting read the confirming read comes, and
    /// how soon the runtime polls again while a bar awaits confirmation.
    pub repoll_ms: i64,
}

impl SettlePolicy {
    /// The production policy. Binance revised a 15m kline more than the 5 s
    /// grace after its close (2026-10-03); 30 s leaves the exchange six times
    /// that, and a 10 s re-poll confirms it at a fraction of the bar.
    pub const DEFAULT: Self = Self {
        settle_ms: 30_000,
        repoll_ms: 10_000,
    };
}

impl Default for SettlePolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

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
    /// The engine rebuilt from the recorded bars disagrees with the log's
    /// replayed trades: the session is held, never attached, until stopped.
    StateMismatch {
        /// The held session.
        session_id: PaperSessionId,
        /// The rebuilt engine's state.
        rebuilt: String,
        /// The log's replayed state.
        logged: String,
    },
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
            Self::StateMismatch {
                session_id,
                rebuilt,
                logged,
            } => write!(
                f,
                "paper runtime: session {session_id} is held: the rebuilt engine \
                 ({rebuilt}) disagrees with the log ({logged})"
            ),
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
            Self::StateMismatch { .. } => "state_mismatch",
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
    /// `None` takes every closed bar as final on its first read.
    settle: Option<SettlePolicy>,
    /// The counting reads per `(pair, timeframe)`, by bar `open_time`: the
    /// copy a later read must equal for a bar to be final, and when that copy
    /// was first read.
    reads: BTreeMap<(Pair, Timeframe), BTreeMap<i64, (Candle, i64)>>,
    /// When the last pass left a bar awaiting confirmation: the re-poll.
    repoll_at_ms: Option<i64>,
    /// First starts whose lead-in awaits its confirming read (#306): the
    /// session attaches once a read `repoll_ms` later agrees.
    pending_lead_in: BTreeMap<PaperSessionId, PendingLeadIn>,
    /// Bars already logged as unsettled past [`UNSETTLED_WARN_MS`].
    warned: BTreeSet<(Pair, Timeframe, i64)>,
    log: Arc<dyn RuntimeLog>,
    sessions: BTreeMap<PaperSessionId, RunningSession>,
    /// Sessions a stop was issued for whose `stop` append has not committed:
    /// never attached again by this process, so a refused append cannot let
    /// the next wake's discovery restart them.
    halted: BTreeSet<PaperSessionId>,
    /// Sessions whose rebuilt engine disagreed with their log at attach:
    /// never attached again by this process (their `data_event` is written
    /// once). A held session is still stopped through the direct path.
    held: BTreeSet<PaperSessionId>,
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
    /// The attach snapshot's owed primary history while the restart catch-up
    /// obligation is open (R3-C): the newest primary bar closed when this
    /// session attached. Cleared only by a successful shadow check of the
    /// state that committed that history contiguously — a zero-consumed pass,
    /// a pre-backlog check or the daily timestamp never discharge it.
    catch_up_to: Option<i64>,
    /// The refusals and data disagreements already written, by bar `open_time`
    /// — at most one `data_event` per distinct bar and refusal.
    reported: BTreeMap<i64, String>,
    /// A failed append rebuilds from the log before the next attempt.
    needs_rebuild: bool,
}

/// The replayed trade state the attach-time check compares exactly: the
/// closed-trade count and the open position's side, quantity and entry price.
#[derive(Debug, PartialEq)]
struct TradeState {
    closed_trades: usize,
    open: Option<(PaperSide, Decimal, Decimal)>,
}

impl core::fmt::Display for TradeState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} closed trades, ", self.closed_trades)?;
        match &self.open {
            Some((side, qty, entry_price)) => {
                write!(f, "open {side:?} qty {qty} at {entry_price}")
            }
            None => write!(f, "no open position"),
        }
    }
}

/// The paper log's side for an engine direction.
fn side_of_direction(direction: crate::domain::Direction) -> PaperSide {
    match direction {
        crate::domain::Direction::Long => PaperSide::Long,
        crate::domain::Direction::Short => PaperSide::Short,
    }
}

/// What one bar's processing produced.
enum StepOutcome {
    /// The bar stepped; its batch is ready to append.
    Consumed {
        bars: Vec<(Timeframe, Candle, bool)>,
        events: Vec<PaperEvent>,
    },
    /// The step refused; the session holds at this bar. `report` is the
    /// `reported` entry to record once the `data_event` append commits.
    Refused {
        summary: String,
        first_time: bool,
        report: (i64, String),
    },
    /// A higher bar that closes with the next primary bar is not ready: a read
    /// saw it unsettled, or every read so far closed without it (R2-4). The
    /// primary waits — and keeps the short re-poll alive — rather than step on
    /// a higher input a rebuild would not reproduce.
    AwaitingHigher,
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
            settle: Some(SettlePolicy::DEFAULT),
            reads: BTreeMap::new(),
            repoll_at_ms: None,
            pending_lead_in: BTreeMap::new(),
            warned: BTreeSet::new(),
            log,
            sessions: BTreeMap::new(),
            halted: BTreeSet::new(),
            held: BTreeSet::new(),
        }
    }

    /// Replace the settle policy ([`SettlePolicy::DEFAULT`] unless set).
    #[must_use]
    pub fn with_settle(mut self, policy: SettlePolicy) -> Self {
        self.settle = Some(policy);
        self
    }

    /// Take every closed bar as final on its first read — the intake before
    /// #306. For suites whose scripted source never revises a bar; the server
    /// never calls it.
    #[must_use]
    pub fn without_settle_gate(mut self) -> Self {
        self.settle = None;
        self
    }

    /// The next instant any attached session's timeframes need a poll (the
    /// first poll past a close waits out the settle time), or the re-poll a
    /// bar awaiting confirmation needs — a primary bar waiting for its due
    /// higher bar included — or, with nothing running, the idle re-scan
    /// instant (a session promoted later must be picked up without a
    /// restart).
    #[must_use]
    pub fn next_wake_ms(&self) -> Option<i64> {
        let now = self.clock.now_ms();
        let offset = self
            .settle
            .map_or(self.grace_ms, |policy| self.grace_ms.max(policy.settle_ms));
        let mut next: Option<i64> = None;
        for run in self.sessions.values() {
            for timeframe in run.session.timeframes() {
                let probe = boundaries(timeframe.duration_ms(), now, offset);
                next = Some(next.map_or(probe.next_poll_ms, |cur| cur.min(probe.next_poll_ms)));
            }
        }
        if let Some(repoll) = self.repoll_at_ms {
            next = Some(earliest(next, repoll));
        }
        if let Some(policy) = self.settle {
            for pending in self.pending_lead_in.values() {
                next = Some(earliest(next, pending.read_at_ms + policy.repoll_ms));
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
        failures.extend(self.catch_up_checkpoints().await);
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
        failures.extend(self.catch_up_checkpoints().await);
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
        // A failed append left the engine a bar ahead of the log: rebuild
        // first, so the check never compares a bar that never committed.
        if self
            .sessions
            .get(session_id)
            .is_some_and(|run| run.needs_rebuild)
        {
            self.rebuild(session_id).await?;
        }
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
    /// failure cannot keep it trading, and the session stays halted — this
    /// process never attaches it again. The append's error still comes back
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
        self.append_stop(session_id, event).await
    }

    /// Append a `stop`, holding the session halted until it commits.
    async fn append_stop(
        &mut self,
        session_id: &PaperSessionId,
        event: PaperEvent,
    ) -> Result<(), PaperRuntimeError> {
        self.halted.insert(session_id.clone());
        self.repo.append_bar(session_id, &[], &[event]).await?;
        self.halted.remove(session_id);
        Ok(())
    }

    /// Stop every running session with `stop_all`. A failure on one session
    /// never leaves the others running: every stop is attempted, and the
    /// failures come back (and are logged).
    pub async fn stop_all(&mut self, issuer: NonEmptyLabel) -> Vec<SessionFailure> {
        self.stop_all_reply(issuer).await.failures
    }

    /// [`Self::stop_all`] with the stopped ids. The sweep covers the attached
    /// sessions AND every persisted session that is not attached (its attach
    /// failed, or it is halted): an unattached running one is stopped through
    /// the direct path, so a later wake or boot cannot start it.
    pub async fn stop_all_reply(&mut self, issuer: NonEmptyLabel) -> StopAllReply {
        let mut failures = Vec::new();
        let mut ids: Vec<PaperSessionId> = self.sessions.keys().cloned().collect();
        match self.repo.list_sessions().await {
            Ok(persisted) => {
                for session in persisted {
                    if !self.sessions.contains_key(&session.id) {
                        ids.push(session.id);
                    }
                }
            }
            Err(error) => {
                self.log.write(format!(
                    "paper runtime: stop_all: listing the persisted sessions failed: {error}"
                ));
                failures.push(SessionFailure {
                    session_id: PaperSessionId::new(String::new()),
                    error: PaperRuntimeError::Data(error),
                });
            }
        }
        let mut stopped = Vec::new();
        for id in ids {
            let actor = StopActor::StopAll {
                issuer: issuer.clone(),
            };
            match self.stop_or_unattached(&id, actor).await {
                Ok(StopReply::Stopped | StopReply::StoppedWithoutShadow) => stopped.push(id),
                Ok(StopReply::AlreadyStopped | StopReply::Unknown) => {}
                Err(error) => {
                    self.log.write(format!(
                        "paper runtime: stop_all: session {id} failed: {error}"
                    ));
                    failures.push(SessionFailure {
                        session_id: id,
                        error,
                    });
                }
            }
        }
        StopAllReply { stopped, failures }
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
        if self.sessions.contains_key(id) || self.halted.contains(id) || self.held.contains(id) {
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
        self.append_stop(id, event).await?;
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
                let result = self.stop_all_reply(issuer).await;
                let _ = reply.send(result);
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
        let listed: BTreeSet<&PaperSessionId> =
            sessions.iter().map(|session| &session.id).collect();
        let (halted, held) = (&self.halted, &self.held);
        self.pending_lead_in
            .retain(|id, _| listed.contains(id) && !halted.contains(id) && !held.contains(id));
        for session in sessions {
            if self.sessions.contains_key(&session.id)
                || self.halted.contains(&session.id)
                || self.held.contains(&session.id)
            {
                continue;
            }
            let id = session.id.clone();
            if let Err(error) = self.attach(session).await {
                // Only a session this process can never attach again (held for
                // a state mismatch) gives up its pinned window: a transient
                // failure keeps it, so the retry resumes over the SAME eligible
                // history instead of re-probing past it and moving the cutoff.
                if self.held.contains(&id) {
                    self.pending_lead_in.remove(&id);
                }
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
    #[allow(clippy::too_many_lines)] // one linear sequence: replay, epoch, first start, rebuild, catch up, check
    async fn attach(&mut self, session: PaperSession) -> Result<(), PaperRuntimeError> {
        let id = session.id.clone();
        let log = self.repo.events(&id).await?;
        let state = PaperSessionState::replay(&session, &log)?;
        if state.status == PaperSessionStatus::Stopped {
            self.pending_lead_in.remove(&id);
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
        if !self.first_start(&session, &compiled).await? {
            return Ok(());
        }
        let loaded = self
            .load_and_build(&session, &compiled, config, &filters)
            .await?;
        // The rebuilt engine must reproduce the log's replayed trades exactly
        // (an engine upgrade that changed an earlier decision, or any
        // non-determinism, would otherwise corrupt the trade record with the
        // next delta). On a mismatch the session is held: not attached, one
        // `data_event`, still stoppable. No reconciliation is attempted.
        let rebuilt_open = loaded
            .recorded
            .get(&session.primary_timeframe)
            .and_then(|bars| bars.values().next_back())
            .and_then(|last| loaded.engine.open_position_mark(last));
        let rebuilt = TradeState {
            closed_trades: loaded.engine.closed_trades().len(),
            open: rebuilt_open.map(|mark| {
                (
                    side_of_direction(mark.direction),
                    mark.qty,
                    mark.entry_price,
                )
            }),
        };
        let logged = TradeState {
            closed_trades: state.closed_trades.len(),
            open: state
                .open_position
                .as_ref()
                .map(|position| (position.side, position.qty, position.entry_price)),
        };
        if rebuilt != logged {
            let summary = format!(
                "session {id} is held: the engine rebuilt from its recorded bars under engine {} \
                 disagrees with its log (rebuilt: {rebuilt}; log: {logged}); stop it and promote \
                 again",
                EngineFingerprint::current().as_str()
            );
            if !self.held.contains(&id) {
                let event = PaperEvent::DataEvent {
                    seq: 0,
                    at: self.now_text(),
                    summary: summary.clone(),
                };
                self.repo.append_bar(&id, &[], &[event]).await?;
                self.held.insert(id.clone());
                self.log.write(format!("paper runtime: {summary}"));
            }
            return Err(PaperRuntimeError::StateMismatch {
                session_id: id,
                rebuilt: rebuilt.to_string(),
                logged: logged.to_string(),
            });
        }
        // R3-C: pin the attach snapshot's owed primary history — the newest
        // primary bar closed at this instant, including input still awaiting
        // its confirming read. A zero-consumed catch-up pass is not completion:
        // the obligation holds until the recorded history is contiguous through
        // the watermark, and only then does the mandatory post-catch-up
        // checkpoint run. The check below covers the state it runs over; if the
        // obligation is already complete here, that check IS the required one.
        let watermark = boundaries(
            session.primary_timeframe.duration_ms(),
            self.clock.now_ms(),
            0,
        )
        .last_closed_open_ms;
        // The obligation is pinned unconditionally: whether the loaded history
        // already covers the watermark or the catch-up completes inside the
        // loop below, only a SUCCESSFUL check of the state this attach leaves
        // behind may clear it (R3 sweep).
        let catch_up_to = Some(watermark);
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
                catch_up_to,
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
        // The check below covers the state this attach leaves behind: when the
        // pinned catch-up obligation is already complete (the backlog was
        // consumed in the loop above, or the loaded history already covered the
        // watermark) that check IS the mandatory post-catch-up checkpoint and
        // discharges it — on SUCCESS only. When the backlog is still owed, the
        // obligation stays and the wake that commits it runs the checkpoint.
        // A session whose first start only landed lead-in has nothing to check
        // yet; the check still runs (an empty comparison) so the boot's shape is
        // uniform.
        let complete = self.sessions.get(&id).is_some_and(|run| {
            run.catch_up_to.is_some_and(|watermark| {
                catch_up_reached(&run.recorded, run.session.primary_timeframe, watermark)
            })
        });
        match self.shadow_check(&id).await {
            Ok(_) => {
                if complete && let Some(run) = self.sessions.get_mut(&id) {
                    run.catch_up_to = None;
                }
            }
            Err(error) => {
                // The required checkpoint stays pending, on a bounded wake.
                if let Some(policy) = self.settle {
                    self.repoll_at_ms = Some(earliest(
                        self.repoll_at_ms,
                        self.clock.now_ms() + policy.repoll_ms,
                    ));
                }
                return Err(error);
            }
        }
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

    /// Read the session's recorded bars and fold them into a fresh engine (the
    /// `run_backtest` order: drain each higher series by `close_time <=
    /// primary.close_time`). A first start's lead-in is appended by `attach`.
    async fn load_and_build(
        &self,
        session: &PaperSession,
        compiled: &CompiledStrategy,
        config: BacktestConfig,
        filters: &SymbolFilters,
    ) -> Result<LoadedSession, PaperRuntimeError> {
        let primary = session.primary_timeframe;
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
    /// `now`. The window starts `depth` primary bars back and doubles until
    /// [`first_fully_warm_bar_ms`] finds a fully warm bar inside it — the probe
    /// series IS the windows the live engine will step, so a warm bar in it
    /// proves the engine arrives warm at `L` (warmth is monotone), and no
    /// constant is invented. A source that cannot supply more history ends the
    /// loop honestly: the engine then warms up live.
    ///
    /// `previous_len` seeds the growth test with a candidate already in hand
    /// (R3-A's deepened re-probe); `None` is a fresh probe. Only bars read
    /// `settle_ms` past their close and strictly before `cutoff_ms` are in the
    /// window (#306, R3-A); [`Self::settled_lead_in`] confirms them.
    async fn probe_lead_in_from(
        &self,
        session: &PaperSession,
        compiled: &CompiledStrategy,
        cutoff_ms: i64,
        depth: i64,
        previous_len: Option<usize>,
    ) -> Result<LeadInWindow, PaperRuntimeError> {
        let step = session.primary_timeframe.duration_ms();
        let settle_ms = self.settle.map_or(0, |policy| policy.settle_ms);
        let mut depth = depth;
        let mut previous_len = previous_len;
        loop {
            let since = cutoff_ms - depth * step - 1;
            let mut raw: BTreeMap<Timeframe, Vec<Candle>> = BTreeMap::new();
            for timeframe in session.timeframes() {
                raw.insert(
                    timeframe,
                    self.source
                        .closed_since(&session.pair, timeframe, since)
                        .await?,
                );
            }
            // The window's instant is when its last read RETURNED: a slow
            // response must not leave an already-due confirmation deadline, and
            // only bars it saw settled are eligible (R2-1, R3 sweep).
            let read_at = self.clock.now_ms();
            let probe: BTreeMap<Timeframe, Vec<Candle>> = raw
                .into_iter()
                .map(|(timeframe, bars)| {
                    (
                        timeframe,
                        eligible_window(bars, cutoff_ms, read_at, settle_ms),
                    )
                })
                .collect();
            let len = probe.get(&session.primary_timeframe).map_or(0, Vec::len);
            let warm = lead_in_warm(&session.pair, session, compiled, &probe);
            // Growth is only evidence when the read actually held bars: an
            // empty response proves nothing, so the search keeps deepening to
            // its cap instead of ending on it (R3 sweep).
            let grew = previous_len.is_none_or(|previous| len > previous);
            let exhausted = len > 0 && !grew;
            if warm || exhausted || depth >= PROBE_MAX_DEPTH {
                return Ok(LeadInWindow {
                    since,
                    cutoff_ms,
                    depth,
                    read_at_ms: read_at,
                    bars: probe,
                });
            }
            previous_len = Some(len);
            depth *= 2;
        }
    }

    /// Hand a confirmed lead-in window to the first start (R3-A): the pin stays
    /// — refreshed to this read's instant — until the append it feeds has
    /// committed, so a failed append resumes over the same eligible history.
    fn release_lead_in(
        &mut self,
        id: &PaperSessionId,
        pin: PendingLeadIn,
    ) -> BTreeMap<Timeframe, Vec<Candle>> {
        let released = pin.bars.clone();
        self.pending_lead_in.insert(id.clone(), pin);
        released
    }

    /// Keep the probe growing over the SAME pinned obligation (R3-A): a deeper
    /// window is read and merged into the candidate. A complete, non-empty
    /// deeper read that served no more history releases the window — it is what
    /// exists — while an empty or growing one keeps the pin waiting, so absence
    /// is never taken for completeness. The pin's own deadline keeps the retry
    /// bounded, and a failed deeper read leaves it exactly as it was.
    async fn deepen_lead_in(
        &mut self,
        session: &PaperSession,
        compiled: &CompiledStrategy,
        pending: PendingLeadIn,
    ) -> Result<Option<BTreeMap<Timeframe, Vec<Candle>>>, PaperRuntimeError> {
        let candidate_len = pending
            .bars
            .get(&session.primary_timeframe)
            .map_or(0, Vec::len);
        let deeper = match self
            .probe_lead_in_from(
                session,
                compiled,
                pending.cutoff_ms,
                (pending.depth * 2).min(PROBE_MAX_DEPTH),
                Some(candidate_len),
            )
            .await
        {
            Ok(deeper) => deeper,
            Err(error) => {
                // A failed read is no evidence: the pin stays, retrying on a
                // deadline stamped now.
                self.pending_lead_in.insert(
                    session.id.clone(),
                    PendingLeadIn {
                        read_at_ms: self.clock.now_ms(),
                        ..pending
                    },
                );
                return Err(error);
            }
        };
        let grown = merge_lead_in(&pending.bars, &deeper.bars);
        let deeper_has_primary = deeper
            .bars
            .get(&session.primary_timeframe)
            .is_some_and(|bars| !bars.is_empty());
        if grown == pending.bars && deeper_has_primary {
            let released = pending.bars.clone();
            self.pending_lead_in.insert(
                session.id.clone(),
                PendingLeadIn {
                    since: deeper.since,
                    cutoff_ms: pending.cutoff_ms,
                    depth: deeper.depth,
                    read_at_ms: deeper.read_at_ms,
                    bars: released.clone(),
                },
            );
            return Ok(Some(released));
        }
        self.pending_lead_in.insert(
            session.id.clone(),
            PendingLeadIn {
                since: deeper.since,
                cutoff_ms: pending.cutoff_ms,
                depth: deeper.depth,
                read_at_ms: deeper.read_at_ms,
                bars: grown,
            },
        );
        Ok(None)
    }

    /// One confirming read over a pinned lead-in window: the raw bars, then the
    /// instant the read returned, then the eligibility filter — never a stamp
    /// taken before the I/O (R2-1, R3 sweep). R3-A keeps the pin regardless of
    /// the read's outcome; the caller re-inserts it on failure.
    async fn confirm_read(
        &self,
        session: &PaperSession,
        pending: &PendingLeadIn,
        settle_ms: i64,
    ) -> Result<(i64, BTreeMap<Timeframe, Vec<Candle>>), PaperRuntimeError> {
        let mut raw: BTreeMap<Timeframe, Vec<Candle>> = BTreeMap::new();
        for timeframe in session.timeframes() {
            raw.insert(
                timeframe,
                self.source
                    .closed_since(&session.pair, timeframe, pending.since)
                    .await?,
            );
        }
        let read_at = self.clock.now_ms();
        let read = raw
            .into_iter()
            .map(|(timeframe, bars)| {
                (
                    timeframe,
                    eligible_window(bars, pending.cutoff_ms, read_at, settle_ms),
                )
            })
            .collect();
        Ok((read_at, read))
    }

    /// A fresh probe: the window for this instant's first live bar.
    async fn probe_lead_in(
        &self,
        session: &PaperSession,
        compiled: &CompiledStrategy,
    ) -> Result<LeadInWindow, PaperRuntimeError> {
        let cutoff_ms =
            first_open_bar_ms(session.primary_timeframe.duration_ms(), self.clock.now_ms());
        self.probe_lead_in_from(session, compiled, cutoff_ms, 2, None)
            .await
    }

    /// A session with no recorded bars records its lead-in once it is
    /// settled. Returns whether the session can attach now; until then it
    /// stays unattached and the next wake tries again.
    async fn first_start(
        &mut self,
        session: &PaperSession,
        compiled: &CompiledStrategy,
    ) -> Result<bool, PaperRuntimeError> {
        if !self
            .repo
            .bars(&session.id, session.primary_timeframe)
            .await?
            .is_empty()
        {
            self.pending_lead_in.remove(&session.id);
            return Ok(true);
        }
        let Some(lead_in) = self.settled_lead_in(session, compiled).await? else {
            return Ok(false);
        };
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
        // Only a committed lead-in discharges the pinned window: a failed
        // append keeps it, so the retry resumes over the same eligible history.
        self.pending_lead_in.remove(&session.id);
        Ok(true)
    }

    /// A first start's lead-in, once settled (#306, R3-A): the pinned eligible
    /// window is read again `repoll_ms` or more later and compared, bar for bar,
    /// against the candidate over that SAME window — later live bars lie past
    /// the cutoff and never enter the comparison. Only exact equality over a
    /// non-vacuous primary window can discharge the obligation: an empty,
    /// shortened, changed or failed read keeps the pin (with its window and
    /// cutoff, whatever cadence boundaries passed), recovered eligible history
    /// joins the candidate, and the confirmation starts over — its deadline
    /// always stamped when a read RETURNED, never before it. While the window is
    /// still not warm and the source can supply more history, the probe keeps
    /// doubling (the same stopping rules); an empty response is never evidence
    /// that history is exhausted. `None` while no confirmed window is ready.
    /// Without a gate the probe is the lead-in.
    async fn settled_lead_in(
        &mut self,
        session: &PaperSession,
        compiled: &CompiledStrategy,
    ) -> Result<Option<BTreeMap<Timeframe, Vec<Candle>>>, PaperRuntimeError> {
        let Some(policy) = self.settle else {
            return Ok(Some(self.probe_lead_in(session, compiled).await?.bars));
        };
        let due = self.clock.now_ms();
        let Some(pending) = self.pending_lead_in.remove(&session.id) else {
            let probe = self.probe_lead_in(session, compiled).await?;
            self.pending_lead_in.insert(
                session.id.clone(),
                PendingLeadIn {
                    since: probe.since,
                    cutoff_ms: probe.cutoff_ms,
                    depth: probe.depth,
                    read_at_ms: probe.read_at_ms,
                    bars: probe.bars,
                },
            );
            return Ok(None);
        };
        if due < pending.read_at_ms + policy.repoll_ms {
            self.pending_lead_in.insert(session.id.clone(), pending);
            return Ok(None);
        }
        let (read_at, read) = match self.confirm_read(session, &pending, policy.settle_ms).await {
            Ok(read) => read,
            Err(error) => {
                // A failed read is no evidence: the pin stays, and its own
                // deadline (stamped now) keeps the retry bounded, never overdue.
                self.pending_lead_in.insert(
                    session.id.clone(),
                    PendingLeadIn {
                        read_at_ms: self.clock.now_ms(),
                        ..pending
                    },
                );
                return Err(error);
            }
        };
        let merged = merge_lead_in(&pending.bars, &read);
        if read == pending.bars {
            let primary_non_empty = pending
                .bars
                .get(&session.primary_timeframe)
                .is_some_and(|bars| !bars.is_empty());
            if primary_non_empty && lead_in_warm(&session.pair, session, compiled, &pending.bars) {
                return Ok(Some(self.release_lead_in(
                    &session.id,
                    PendingLeadIn {
                        read_at_ms: read_at,
                        ..pending
                    },
                )));
            }
            return self.deepen_lead_in(session, compiled, pending).await;
        }
        // Shortened, changed or newly recovered eligible history: keep the
        // strongest candidate and restart its timed confirmation.
        self.pending_lead_in.insert(
            session.id.clone(),
            PendingLeadIn {
                read_at_ms: read_at,
                bars: merged,
                ..pending
            },
        );
        Ok(None)
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
        self.reads.retain(|key, _| needs.contains_key(key));
        let awaiting = self.repoll_at_ms.take().is_some();
        let mut unsettled: BTreeMap<(Pair, Timeframe), i64> = BTreeMap::new();
        let mut fetched: BTreeMap<(Pair, Timeframe), Vec<Candle>> = BTreeMap::new();
        let mut fetch_errors: BTreeMap<(Pair, Timeframe), DataError> = BTreeMap::new();
        for ((pair, timeframe), since) in needs {
            match self.source.closed_since(&pair, timeframe, since).await {
                Ok(bars) => {
                    let closed: Vec<Candle> = bars
                        .into_iter()
                        .filter(|bar| bar.close_time < now)
                        .collect();
                    let key = (pair, timeframe);
                    // A read counts from when it returned, not from when the
                    // pass started: a slow request must not shorten the
                    // confirmation spacing.
                    let read_at = self.clock.now_ms();
                    let (bars, first_unsettled) = self.gate(&key, closed, read_at);
                    if let Some(close_time) = first_unsettled {
                        unsettled.insert(key.clone(), close_time);
                    }
                    fetched.insert(key, bars);
                }
                Err(error) => {
                    // A bar awaiting confirmation keeps its re-poll.
                    if let Some(policy) = self.settle.filter(|_| awaiting) {
                        self.repoll_at_ms =
                            Some(earliest(self.repoll_at_ms, now + policy.repoll_ms));
                    }
                    fetch_errors.insert((pair, timeframe), error);
                }
            }
        }

        let mut consumed_total = 0_usize;
        self.keep_due_primary_retry(&fetched);
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
                match self.consume_one_bar(&id, &fetched, &unsettled).await {
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

    /// Keep the short retry alive while a due eligible primary bar is missing
    /// (R3-B): an empty, truncated or failed FIRST read of the bar is no
    /// evidence it does not exist, and no counting copy needs to exist for the
    /// obligation to hold. Derived from the cadence and each session's fetch
    /// window — never from the counting cache — so the boundary read itself is
    /// covered; a session with no eligible primary bar due keeps the ordinary
    /// boundary schedule.
    fn keep_due_primary_retry(&mut self, fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>) {
        let Some(policy) = self.settle else {
            return;
        };
        let now = self.clock.now_ms();
        let missing = self
            .sessions
            .values()
            .any(|run| run.due_primary_missing(fetched, now, policy.settle_ms));
        if missing {
            self.repoll_at_ms = Some(earliest(self.repoll_at_ms, now + policy.repoll_ms));
        }
    }

    /// The mandatory post-catch-up shadow checkpoints (R3-C): a session whose
    /// pinned catch-up obligation is now complete is checked over exactly that
    /// caught-up state, immediately, whatever an earlier check or the daily
    /// cadence says. A failed check keeps the obligation pending on a bounded
    /// wake; one session's failure never blocks another's.
    async fn catch_up_checkpoints(&mut self) -> Vec<SessionFailure> {
        let now = self.clock.now_ms();
        let due: Vec<PaperSessionId> = self
            .sessions
            .iter()
            .filter(|(_, run)| {
                run.catch_up_to.is_some_and(|watermark| {
                    catch_up_reached(&run.recorded, run.session.primary_timeframe, watermark)
                })
            })
            .map(|(id, _)| id.clone())
            .collect();
        let mut failures = Vec::new();
        for id in due {
            match self.shadow_check(&id).await {
                Ok(_) => {
                    if let Some(run) = self.sessions.get_mut(&id) {
                        run.catch_up_to = None;
                    }
                }
                Err(error) => {
                    // The obligation stays: the checkpoint is required, and the
                    // short re-poll keeps it wakeable.
                    if let Some(policy) = self.settle {
                        self.repoll_at_ms =
                            Some(earliest(self.repoll_at_ms, now + policy.repoll_ms));
                    }
                    self.log.write(format!(
                        "paper runtime: session {id} catch-up checkpoint failed: {error}"
                    ));
                    failures.push(SessionFailure {
                        session_id: id.clone(),
                        error,
                    });
                }
            }
        }
        failures
    }

    /// Pass one `(pair, timeframe)` read through the settle gate: keep its
    /// counting reads for the next pass, schedule the re-poll a waiting bar
    /// needs, and return the settled bars (all of them without a gate) with
    /// the `close_time` of the first closed bar that is not final.
    fn gate(
        &mut self,
        key: &(Pair, Timeframe),
        closed: Vec<Candle>,
        now: i64,
    ) -> (Vec<Candle>, Option<i64>) {
        let Some(policy) = self.settle else {
            return (closed, None);
        };
        let previous = self.reads.remove(key).unwrap_or_default();
        let read = settle_read(closed, &previous, now, policy);
        self.reads.insert(key.clone(), read.reads);
        if let Some(at) = read.next_read_ms {
            self.repoll_at_ms = Some(earliest(self.repoll_at_ms, at));
        }
        // One log line per bar that stays unsettled past the bound; no state
        // changes, the bar keeps waiting.
        self.warned.retain(|(pair, timeframe, open_time)| {
            (pair, timeframe) != (&key.0, &key.1) || read.waiting.contains_key(open_time)
        });
        for (open_time, close_time) in &read.waiting {
            if now - (close_time + 1) >= UNSETTLED_WARN_MS
                && self.warned.insert((key.0.clone(), key.1, *open_time))
            {
                self.log.write(format!(
                    "paper runtime: {} {} bar {open_time} is not settled {} s after its close; \
                     still waiting",
                    key.0.as_str(),
                    key.1.binance_interval(),
                    (now - (close_time + 1)) / 1_000
                ));
            }
        }
        (read.settled, read.first_unsettled_close)
    }

    /// Consume the next new primary bar of one session, with its newly closed
    /// higher bars, as ONE append. Returns whether a bar was consumed.
    async fn consume_one_bar(
        &mut self,
        id: &PaperSessionId,
        fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>,
        unsettled: &BTreeMap<(Pair, Timeframe), i64>,
    ) -> Result<bool, PaperRuntimeError> {
        let at = self.now_text();
        let outcome = {
            let run = self
                .sessions
                .get_mut(id)
                .ok_or_else(|| PaperRuntimeError::NotRunning(id.clone()))?;
            let disagreements = scan_disagreements(run, fetched);
            if let Some((bar_open_time, summary, first_time, tag)) = disagreements.first() {
                // Report the first and hold this session for the pass; the
                // recorded bar is never replaced. At most one `data_event` per
                // distinct bar and refusal.
                StepOutcome::Refused {
                    summary: summary.clone(),
                    first_time: *first_time,
                    report: (*bar_open_time, tag.clone()),
                }
            } else {
                prepare_step(run, fetched, unsettled, &at)
            }
        };

        match outcome {
            StepOutcome::Idle => Ok(false),
            StepOutcome::AwaitingHigher => {
                // The higher bar can arrive at any read, so keep the short
                // re-poll alive: the next poll boundary may be a whole primary
                // bar away. Without a settle policy there is no re-poll to
                // keep, and the boundary cadence already wakes in time.
                if let Some(policy) = self.settle {
                    self.repoll_at_ms = Some(earliest(
                        self.repoll_at_ms,
                        self.clock.now_ms() + policy.repoll_ms,
                    ));
                }
                Ok(false)
            }
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
                report: (open_time, tag),
            } => {
                if first_time {
                    let event = PaperEvent::DataEvent {
                        seq: 0,
                        at,
                        summary: summary.clone(),
                    };
                    self.repo.append_bar(id, &[], &[event]).await?;
                    // Marked reported only once the event committed: a failed
                    // append leaves it unreported, so the next wake retries it.
                    if let Some(run) = self.sessions.get_mut(id) {
                        run.reported.insert(open_time, tag);
                    }
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
            // A session with no successful check yet (its attach-time check
            // failed) is due now, so a transient failure is retried.
            .filter(|(_, run)| {
                run.last_shadow_ms
                    .is_none_or(|last| daily_shadow_due(now, last))
            })
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

/// A first start's probe read, awaiting its confirming read (#306, R3-A).
struct PendingLeadIn {
    /// The fetch boundary the pinned window is read from. Set by the probe and
    /// only ever deepened, never moved forward: a later, shorter or empty read
    /// cannot shrink the history the session owes.
    since: i64,
    /// The eligibility cutoff: the first primary bar not closed at the probe
    /// instant. Bars at or after it are live input, never lead-in.
    cutoff_ms: i64,
    /// The probe depth behind `since` (the window-growth state).
    depth: i64,
    /// When the window was last read: its confirmation is due `repoll_ms`
    /// after this.
    read_at_ms: i64,
    /// Every eligible bar seen so far, per timeframe — the candidate, never
    /// shortened.
    bars: BTreeMap<Timeframe, Vec<Candle>>,
}

/// One probed lead-in window: where it was read from, the eligibility cutoff it
/// was filtered by, the probe depth, the instant its read RETURNED, and the
/// eligible bars it holds.
struct LeadInWindow {
    since: i64,
    cutoff_ms: i64,
    depth: i64,
    read_at_ms: i64,
    bars: BTreeMap<Timeframe, Vec<Candle>>,
}

/// The lead-in eligible part of one read: strictly before the pinned cutoff,
/// and settled `settle_ms` past its close (the #306 probe rule).
fn eligible_window(bars: Vec<Candle>, cutoff_ms: i64, read_at: i64, settle_ms: i64) -> Vec<Candle> {
    bars.into_iter()
        .filter(|bar| bar.open_time < cutoff_ms && bar.close_time + 1 + settle_ms <= read_at)
        .collect()
}

/// Fold a confirming read into the lead-in candidate: every bar either side
/// knows, the read's copy winning a shared `open_time` — so recovered history
/// joins the candidate and a changed eligible value restarts its confirmation,
/// while no candidate bar is ever dropped (R3-A).
fn merge_lead_in(
    candidate: &BTreeMap<Timeframe, Vec<Candle>>,
    read: &BTreeMap<Timeframe, Vec<Candle>>,
) -> BTreeMap<Timeframe, Vec<Candle>> {
    let mut merged: BTreeMap<Timeframe, BTreeMap<i64, Candle>> = BTreeMap::new();
    for (timeframe, bars) in candidate.iter().chain(read) {
        let entry = merged.entry(*timeframe).or_default();
        for bar in bars {
            entry.insert(bar.open_time, bar.clone());
        }
    }
    merged
        .into_iter()
        .map(|(timeframe, bars)| (timeframe, bars.into_values().collect()))
        .collect()
}

/// Whether a lead-in candidate holds a bar whose entry warm gate is satisfied
/// — the probe's own stopping rule, reused so a window is never accepted while
/// the source can still supply the history the engine is meant to arrive warm
/// on (R3-A).
fn lead_in_warm(
    pair: &Pair,
    session: &PaperSession,
    compiled: &CompiledStrategy,
    bars: &BTreeMap<Timeframe, Vec<Candle>>,
) -> bool {
    let primary = probe_series(pair, session.primary_timeframe, bars);
    let htf = session
        .htf_timeframe
        .map(|timeframe| probe_series(pair, timeframe, bars));
    let d1 = session
        .d1_timeframe()
        .map(|timeframe| probe_series(pair, timeframe, bars));
    first_fully_warm_bar_ms(compiled, &primary, htf.as_ref(), d1.as_ref()).is_some()
}

/// Whether the recorded primary history is contiguous from its first recorded
/// bar through `watermark` — every owed catch-up bar committed, never a partial
/// prefix (R3-C).
fn catch_up_reached(
    recorded: &BTreeMap<Timeframe, BTreeMap<i64, Candle>>,
    primary: Timeframe,
    watermark: i64,
) -> bool {
    let Some(bars) = recorded.get(&primary) else {
        return false;
    };
    let (Some(&first), Some(&newest)) = (bars.keys().next(), bars.keys().next_back()) else {
        return false;
    };
    if first > watermark {
        return true;
    }
    if newest < watermark {
        return false;
    }
    // Contiguous bars on the cadence's grid: the count is the span.
    let expected = (newest - first) / primary.duration_ms() + 1;
    i64::try_from(bars.len()).is_ok_and(|len| len == expected)
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

    /// Whether any primary bar this session owes — the span from the first bar
    /// after its recorded frontier (or its fetch floor) through the newest bar
    /// already past its counting time — is missing from the settled read
    /// (R3-B).
    ///
    /// Derived from the cadence and the recording window, never from the
    /// counting cache: an empty, truncated or failed read leaves the span
    /// unfilled, a response that carries the newest bar but skipped an older
    /// owed one is incomplete, and a read that carries the whole span raises no
    /// retry at all — so a caught-up session keeps the ordinary boundary
    /// schedule.
    fn due_primary_missing(
        &self,
        fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>,
        now: i64,
        settle_ms: i64,
    ) -> bool {
        let primary = self.session.primary_timeframe;
        let duration = primary.duration_ms();
        // `now - settle_ms` is the cutoff: a bar whose counting read is not due
        // yet belongs to the settle gate's own deadline, not to this retry.
        let due_open = boundaries(duration, now - settle_ms, 0).last_closed_open_ms;
        let owed_from = self
            .recorded
            .get(&primary)
            .and_then(|bars| bars.keys().next_back().copied())
            .map_or(self.since_floor_ms, |last| last + duration);
        if due_open < owed_from {
            // Nothing owed: everything up to the newest counting-eligible bar is
            // already recorded.
            return false;
        }
        let expected = (due_open - owed_from) / duration + 1;
        let present = fetched
            .get(&(self.session.pair.clone(), primary))
            .map_or(0, |bars| {
                i64::try_from(
                    bars.iter()
                        .filter(|bar| bar.open_time >= owed_from && bar.open_time <= due_open)
                        .count(),
                )
                .unwrap_or(i64::MAX)
            });
        present != expected
    }

    /// Whether the higher bar this primary bar needs — the newest one whose
    /// `close_time` is at or before `primary_close`, the one
    /// [`Self::new_higher_bars`] and the rebuild both drain — is missing from
    /// the read.
    ///
    /// `fetched` holds only settled bars, so a bar the gate is still
    /// confirming is not in it; the caller judges that case from its
    /// `unsettled` marker. What this answers is the read that closed WITHOUT
    /// the bar at all (R2-4): no marker exists for it, and stepping the primary
    /// would hand the engine a higher input a rebuild would not reproduce.
    ///
    /// **The fetch-floor exception.** Eligibility is the session's own fetch
    /// window: [`Self::since_for`] is the last recorded higher `open_time`
    /// minus one, or the recording floor minus one while no higher bar is
    /// recorded. A due bar opening at or before that boundary can never be
    /// delivered by any read of this window — it is either recorded already, or
    /// it opened before the session's first live bar and so never entered its
    /// recorded lead-in — and waiting for it would deadlock the session for
    /// good. Such a bar is NOT required: the rebuild drains recorded bars only,
    /// and lacks it exactly as the live session does.
    ///
    /// Inside the window the due bar ITSELF must be in hand: a read that
    /// carried a newer bar but skipped this one is still a read that never
    /// returned it, and stepping on it would feed the engine a higher bar on a
    /// later primary bar than the rebuild's drain does.
    fn due_higher_missing(
        &self,
        timeframe: Timeframe,
        fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>,
        pair: &Pair,
        primary_close: i64,
    ) -> bool {
        let duration = timeframe.duration_ms();
        // Derived from the cadence and this primary bar's close, never from
        // wall time: the bar closing at or before that close.
        let due_open = ((primary_close + 1).div_euclid(duration) - 1) * duration;
        if due_open <= self.since_for(timeframe) {
            return false;
        }
        if self
            .recorded
            .get(&timeframe)
            .is_some_and(|bars| bars.contains_key(&due_open))
        {
            return false;
        }
        !fetched
            .get(&(pair.clone(), timeframe))
            .is_some_and(|bars| bars.iter().any(|bar| bar.open_time == due_open))
    }
}

/// One `(pair, timeframe)` read split by the settle gate (#306).
struct SettleRead {
    /// The bars final at this read, ascending and contiguous from the first:
    /// the only bars the session may record, step or compare.
    settled: Vec<Candle>,
    /// This read's counting copies and when each was first read, by
    /// `open_time` (the next read's reference).
    reads: BTreeMap<i64, (Candle, i64)>,
    /// A closed bar is not final yet: when the runtime should read again.
    next_read_ms: Option<i64>,
    /// The `close_time` of the first closed bar that is not final.
    first_unsettled_close: Option<i64>,
    /// Every closed bar that is not final: `open_time` → `close_time`.
    waiting: BTreeMap<i64, i64>,
}

/// The earlier of an optional instant and `at`.
fn earliest(current: Option<i64>, at: i64) -> i64 {
    current.map_or(at, |cur| cur.min(at))
}

/// Gate one read: a closed bar is final when this read lies `settle_ms` or
/// more past its close and equals a counting read taken `repoll_ms` or more
/// earlier. A changed copy restarts that clock. The settled bars stop at the
/// first bar that is not final, so a session never records past a
/// provisional bar.
fn settle_read(
    closed: Vec<Candle>,
    previous: &BTreeMap<i64, (Candle, i64)>,
    now: i64,
    policy: SettlePolicy,
) -> SettleRead {
    let mut read = SettleRead {
        settled: Vec::new(),
        reads: BTreeMap::new(),
        next_read_ms: None,
        first_unsettled_close: None,
        waiting: BTreeMap::new(),
    };
    for bar in closed {
        // `close_time` is the bar's last millisecond: it closes one later.
        let counts_from = bar.close_time + 1 + policy.settle_ms;
        if now < counts_from {
            read.next_read_ms = Some(earliest(read.next_read_ms, counts_from));
            read.first_unsettled_close.get_or_insert(bar.close_time);
            read.waiting.insert(bar.open_time, bar.close_time);
            continue;
        }
        let first_read_ms = match previous.get(&bar.open_time) {
            Some((copy, at)) if *copy == bar => *at,
            _ => now,
        };
        let confirms_at = first_read_ms + policy.repoll_ms;
        if now < confirms_at {
            read.next_read_ms = Some(earliest(read.next_read_ms, confirms_at));
            read.first_unsettled_close.get_or_insert(bar.close_time);
            read.waiting.insert(bar.open_time, bar.close_time);
        } else if read.first_unsettled_close.is_none() {
            read.settled.push(bar.clone());
        }
        read.reads.insert(bar.open_time, (bar, first_read_ms));
    }
    // An empty or truncated response is no evidence about the bars it left
    // out: keep their counting reads and read again on the re-poll spacing.
    let newest = read.reads.keys().next_back().copied();
    for (open_time, entry) in previous {
        if newest.is_none_or(|newest| *open_time > newest) {
            read.reads.insert(*open_time, entry.clone());
            read.next_read_ms = Some(earliest(read.next_read_ms, now + policy.repoll_ms));
        }
    }
    read
}

/// Every fetched bar that is already recorded with DIFFERENT values — the A6
/// data disagreement. Each entry is `(bar open_time, summary, first_time,
/// tag)`; `first_time` is false when this exact `(bar, refusal)` was already
/// written, and `tag` is the `reported` entry to record once it is.
fn scan_disagreements(
    run: &RunningSession,
    fetched: &BTreeMap<(Pair, Timeframe), Vec<Candle>>,
) -> Vec<(i64, String, bool, String)> {
    let pair = run.session.pair.clone();
    let mut disagreements: Vec<(i64, String, bool, String)> = Vec::new();
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
                disagreements.push((
                    bar.open_time,
                    format!(
                        "re-fetched {} bar {} differs from the recorded bar; \
                         the recorded bar is kept",
                        timeframe.binance_interval(),
                        bar.open_time
                    ),
                    first_time,
                    tag,
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
    unsettled: &BTreeMap<(Pair, Timeframe), i64>,
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
    // A higher bar that closes with this one and is not final yet must ride
    // this bar's batch, as the rebuild drains it (#306): wait for it. Two ways
    // it is not ready: a read has seen it unsettled, or the reads have closed
    // without it at all (R2-4) — the second leaves no marker, so it is derived
    // from the higher cadence and this bar's close.
    let higher_waiting = [run.session.htf_timeframe, run.session.d1_timeframe()]
        .into_iter()
        .flatten()
        .any(|timeframe| {
            let unsettled_close = unsettled
                .get(&(pair.clone(), timeframe))
                .is_some_and(|close_time| *close_time <= bar.close_time);
            unsettled_close || run.due_higher_missing(timeframe, fetched, &pair, bar.close_time)
        });
    if higher_waiting {
        return StepOutcome::AwaitingHigher;
    }
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
        return StepOutcome::Refused {
            summary: format!("step refused at bar {}: {error}", bar.open_time),
            first_time,
            report: (bar.open_time, tag),
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
