//! The paper read model (r3.s4.w4, spec §4): one source of truth for the HTTP
//! routes and the MCP read tools.
//!
//! [`session_summary`] and [`list_summaries`] replay each session's log
//! (`PaperSessionState`), attach the per-epoch shadow verdicts, and render the
//! OOS comparison — so a route and a tool that ask the same question answer
//! from the same code. [`session_trades`] is the trades half, and
//! [`session_events`] the raw log the SSE route tails.
//!
//! Pure projection over the ports: no store, no clock, no adapters.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::application::paper_control::PaperControl;
use crate::domain::backtest::OpenPositionStops;
use crate::domain::paper::comparison::{OosComparison, comparison};
use crate::domain::paper::event::{PaperEvent, StopActor};
use crate::domain::paper::pnl::{
    MarkBar, PaperPnl, PaperPnlEntry, PaperPnlOpenPosition, PaperPnlTrade,
};
use crate::domain::paper::runtime::ShadowResult;
use crate::domain::paper::session::{Graduation, NonEmptyLabel, PaperSession, PaperSessionId};
use crate::domain::paper::state::{
    PaperPosition, PaperSessionState, PaperSessionStatus, ReplayError,
};
use crate::domain::strategy::VersionId;
use crate::domain::{
    DataError, EngineFingerprint, Pair, PaperSessionRepository, Timeframe, WalkForwardRun,
    WalkForwardRunRepository,
};

/// Whether a session runs or has stopped, and — when it stopped — who stopped
/// it (audit #6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionStatus {
    /// Live: bars are consumed and orders fill.
    Running,
    /// Stopped: the log is read-only. `stopped_by` is the actor the `stop`
    /// event recorded (every stopped state replayed one).
    Stopped {
        /// The recorded stopping actor.
        stopped_by: Option<StopActor>,
    },
}

/// The latest `shadow_checked` verdict recorded in one engine epoch (E3:
/// shadow identity is judged per epoch).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EpochShadowCheck {
    /// The epoch's engine fingerprint.
    pub engine_fingerprint: EngineFingerprint,
    /// The epoch's latest verdict.
    pub result: ShadowResult,
}

/// One session's summary — the shape both `GET /api/v1/paper/sessions/{id}`
/// and the `get_paper_session` MCP tool return (spec §4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// The session id.
    pub id: PaperSessionId,
    /// The promoted strategy version.
    pub strategy_version_id: VersionId,
    /// The traded pair.
    pub pair: Pair,
    /// The primary timeframe.
    pub primary_timeframe: Timeframe,
    /// The higher timeframe, when the session uses one.
    pub htf_timeframe: Option<Timeframe>,
    /// Whether the session consumes the fixed daily series.
    pub uses_d1: bool,
    /// How the session was promoted (carries the override reason or the
    /// certifying run).
    pub graduation: Graduation,
    /// Whether every certified data version is a `fixture_snapshot` row.
    pub fixture: bool,
    /// The promoting token's label.
    pub promoted_by: NonEmptyLabel,
    /// Running or stopped, with the stop actor.
    pub status: SessionStatus,
    /// The engine fingerprints in order (E3).
    pub epochs: Vec<EngineFingerprint>,
    /// The newest consumed bar's `open_time`, when one was consumed.
    pub last_bar_open_time: Option<i64>,
    /// How many closed trades the log holds.
    pub closed_trade_count: u64,
    /// Σ realized USDT over the closed trades that carry a figure (0 when none
    /// does — [`Self::realized_usdt_count`] says how many did).
    pub realized_usdt_total: Decimal,
    /// How many closed trades carry a USDT figure ("n of m" with
    /// [`Self::closed_trade_count`]).
    pub realized_usdt_count: u64,
    /// Σ realized R over the closed trades that recorded one (0 when none
    /// did).
    pub realized_r_total: Decimal,
    /// How many closed trades carry a realized R.
    pub realized_r_count: u64,
    /// The open position's unrealized USDT, net of the costs paid so far —
    /// `None` when the session is flat (and in the unmarkable hand-seeded
    /// case: a position the log holds but no bar to mark it against).
    pub unrealized_usdt: Option<Decimal>,
    /// The session's running total: [`Self::realized_usdt_total`] + the
    /// unrealized figure when a position stands, else the realized total
    /// alone. Always present.
    pub session_pnl_usdt: Decimal,
    /// The open position, when one stands — the log's position marked at the
    /// last processed primary bar, with its costs so far and the live stops a
    /// running engine answered (absent when none did).
    pub open_position: Option<PaperPnlOpenPosition>,
    /// The latest shadow verdict per epoch, in epoch order.
    pub shadow_checks: Vec<EpochShadowCheck>,
    /// Whether the certifying run's fingerprint is not this build's (E3:
    /// shown, never enforced).
    pub certification_stale: bool,
    /// The OOS comparison (A8/A12).
    pub comparison: OosComparison,
}

/// One session's trades — the `GET .../trades` and `get_paper_trades` shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTrades {
    /// The closed trades, in log order, with their realized R and USDT.
    pub closed_trades: Vec<PaperPnlTrade>,
    /// The open position, when one stands (the log's own shape — the summary
    /// carries the marked one).
    pub open_position: Option<PaperPosition>,
    /// Every `Order` and `Fill` event, in log order — the entry fills included
    /// (#344: the trade log's orders and fills).
    pub entries: Vec<PaperPnlEntry>,
}

/// Why a read failed. A store or replay failure is an honest error — never a
/// silently empty projection.
#[derive(Debug)]
pub enum PaperReadError {
    /// A store read failed.
    Data(DataError),
    /// A session's log does not replay.
    Replay(ReplayError),
    /// A `shadow_checked` payload does not decode into a verdict.
    ShadowPayload(String),
}

impl core::fmt::Display for PaperReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Data(error) => write!(f, "paper read store failure: {error}"),
            Self::Replay(error) => write!(f, "paper read replay refused: {error}"),
            Self::ShadowPayload(reason) => {
                write!(f, "paper read: shadow_checked payload refused: {reason}")
            }
        }
    }
}

impl std::error::Error for PaperReadError {}

impl From<DataError> for PaperReadError {
    fn from(error: DataError) -> Self {
        Self::Data(error)
    }
}

impl From<ReplayError> for PaperReadError {
    fn from(error: ReplayError) -> Self {
        Self::Replay(error)
    }
}

/// One session's summary, or `None` when no such row exists.
///
/// `control` is the installed paper control handle, when the caller has one
/// (`pulse serve`'s routes and its `/mcp` mount do; the stdio MCP process
/// does not). The read asks it for the open position's live stops — under a
/// short timeout, and never failing because of them (r4.s4.w1).
///
/// # Errors
///
/// [`PaperReadError`] on a store or replay failure, or an undecodable shadow
/// payload.
pub async fn session_summary<P, W>(
    sessions: &P,
    walk_forwards: &W,
    control: Option<&PaperControl>,
    id: &PaperSessionId,
) -> Result<Option<SessionSummary>, PaperReadError>
where
    P: PaperSessionRepository,
    W: WalkForwardRunRepository,
{
    let Some(session) = sessions.get_session(id).await? else {
        return Ok(None);
    };
    let events = sessions.events(id).await?;
    let state = PaperSessionState::replay(&session, &events)?;
    let pnl = pnl_for(&session, &events, &state, sessions, control).await?;
    let certifying_run = certifying_run(walk_forwards, &session).await?;
    summarize(&session, &events, &state, certifying_run.as_ref(), &pnl).map(Some)
}

/// Every session's summary, in catalog order (the repository's `seq` order).
///
/// `control` rides exactly as in [`session_summary`], once per session that
/// holds an open position.
///
/// # Errors
///
/// [`PaperReadError`] on a store or replay failure, or an undecodable shadow
/// payload.
pub async fn list_summaries<P, W>(
    sessions: &P,
    walk_forwards: &W,
    control: Option<&PaperControl>,
) -> Result<Vec<SessionSummary>, PaperReadError>
where
    P: PaperSessionRepository,
    W: WalkForwardRunRepository,
{
    let mut summaries = Vec::new();
    for session in sessions.list_sessions().await? {
        let events = sessions.events(&session.id).await?;
        let state = PaperSessionState::replay(&session, &events)?;
        let pnl = pnl_for(&session, &events, &state, sessions, control).await?;
        let certifying_run = certifying_run(walk_forwards, &session).await?;
        summaries.push(summarize(
            &session,
            &events,
            &state,
            certifying_run.as_ref(),
            &pnl,
        )?);
    }
    Ok(summaries)
}

/// Derive one session's money view (r4.s4.w1, ADR-0027 addendum).
///
/// A flat session pays neither the mark-bar store read nor a runtime query;
/// a position that stands takes the newest `paper_bar` row of the session's
/// primary timeframe as its mark, and the runtime's answer — under its own
/// short timeout — as its live stops. The read NEVER fails because of the
/// stops: an absent runtime, a gone one, or one that does not answer leaves
/// the answer without them (spec §3, grill Q3).
async fn pnl_for<P: PaperSessionRepository>(
    session: &PaperSession,
    events: &[PaperEvent],
    state: &PaperSessionState,
    sessions: &P,
    control: Option<&PaperControl>,
) -> Result<PaperPnl, PaperReadError> {
    let (mark, stops) = if state.open_position.is_some() {
        let mark = sessions
            .latest_bar(&session.id, session.primary_timeframe)
            .await?
            .map(|bar| MarkBar {
                price: bar.close,
                close_time_ms: bar.close_time,
            });
        (mark, live_stops(control, &session.id).await)
    } else {
        (None, None)
    };
    Ok(PaperPnl::derive(session, events, mark, stops)?)
}

/// The open position's live stops, when a runtime answers in time.
async fn live_stops(
    control: Option<&PaperControl>,
    id: &PaperSessionId,
) -> Option<OpenPositionStops> {
    let control = control?;
    control.position(id.clone()).await.ok().flatten()
}

/// One session's closed trades (with their USDT figures) and entries, or
/// `None` when no such row exists.
///
/// # Errors
///
/// [`PaperReadError`] on a store or replay failure.
pub async fn session_trades<P>(
    sessions: &P,
    id: &PaperSessionId,
) -> Result<Option<SessionTrades>, PaperReadError>
where
    P: PaperSessionRepository,
{
    let Some(session) = sessions.get_session(id).await? else {
        return Ok(None);
    };
    let events = sessions.events(id).await?;
    let state = PaperSessionState::replay(&session, &events)?;
    // No mark and no stops here: this read serves the log's trades and
    // entries, and its own `open_position` keeps the log's own shape.
    let pnl = PaperPnl::derive(&session, &events, None, None)?;
    Ok(Some(SessionTrades {
        closed_trades: pnl.closed_trades,
        open_position: state.open_position,
        entries: pnl.entries,
    }))
}

/// One session's decoded event log in `seq` order, or `None` when no such row
/// exists (the SSE route's existence check and backlog).
///
/// # Errors
///
/// [`PaperReadError`] on a store failure or an undecodable row.
pub async fn session_events<P>(
    sessions: &P,
    id: &PaperSessionId,
) -> Result<Option<Vec<PaperEvent>>, PaperReadError>
where
    P: PaperSessionRepository,
{
    let Some(_session) = sessions.get_session(id).await? else {
        return Ok(None);
    };
    Ok(Some(sessions.events(id).await?))
}

/// The certifying run a session's graduation names, when it names one.
async fn certifying_run<W>(
    walk_forwards: &W,
    session: &PaperSession,
) -> Result<Option<WalkForwardRun>, PaperReadError>
where
    W: WalkForwardRunRepository,
{
    let Graduation::Certified {
        walk_forward_run_id,
        ..
    } = &session.graduation
    else {
        return Ok(None);
    };
    Ok(walk_forwards
        .get_walk_forward_run(walk_forward_run_id)
        .await?)
}

/// Project one replayed session into its summary.
fn summarize(
    session: &PaperSession,
    events: &[PaperEvent],
    state: &PaperSessionState,
    certifying_run: Option<&WalkForwardRun>,
    pnl: &PaperPnl,
) -> Result<SessionSummary, PaperReadError> {
    let status = if state.status == PaperSessionStatus::Stopped {
        SessionStatus::Stopped {
            stopped_by: events.iter().rev().find_map(|event| match event {
                PaperEvent::Stop { actor, .. } => Some(actor.clone()),
                _ => None,
            }),
        }
    } else {
        SessionStatus::Running
    };
    let comparison = comparison(
        session,
        state,
        certifying_run,
        &EngineFingerprint::current(),
    );
    Ok(SessionSummary {
        id: session.id.clone(),
        strategy_version_id: session.strategy_version_id.clone(),
        pair: session.pair.clone(),
        primary_timeframe: session.primary_timeframe,
        htf_timeframe: session.htf_timeframe,
        uses_d1: session.uses_d1,
        graduation: session.graduation.clone(),
        fixture: session.fixture,
        promoted_by: session.promoted_by.clone(),
        status,
        epochs: state.epochs.clone(),
        last_bar_open_time: state.last_bar_open_time,
        closed_trade_count: u64::try_from(state.closed_trades.len()).unwrap_or(u64::MAX),
        realized_usdt_total: pnl.realized_usdt_total,
        realized_usdt_count: pnl.realized_usdt_count,
        realized_r_total: pnl.realized_r_total,
        realized_r_count: pnl.realized_r_count,
        unrealized_usdt: pnl.unrealized_usdt,
        session_pnl_usdt: pnl.session_pnl_usdt,
        open_position: pnl.open_position.clone(),
        shadow_checks: shadow_checks(events, state)?,
        certification_stale: comparison.certification_stale,
        comparison,
    })
}

/// The latest `shadow_checked` verdict per epoch, in epoch order.
fn shadow_checks(
    events: &[PaperEvent],
    state: &PaperSessionState,
) -> Result<Vec<EpochShadowCheck>, PaperReadError> {
    let mut by_epoch: Vec<Option<ShadowResult>> = vec![None; state.epochs.len()];
    let mut epoch = 0_usize;
    for event in events {
        match event {
            PaperEvent::EngineUpgraded { .. } => epoch = epoch.saturating_add(1),
            PaperEvent::ShadowChecked { result, .. } => {
                let verdict: ShadowResult =
                    serde_json::from_value(result.clone()).map_err(|e| {
                        PaperReadError::ShadowPayload(format!("shadow_checked payload: {e}"))
                    })?;
                if let Some(slot) = by_epoch.get_mut(epoch) {
                    *slot = Some(verdict);
                }
            }
            _ => {}
        }
    }
    Ok(state
        .epochs
        .iter()
        .enumerate()
        .filter_map(|(index, fingerprint)| {
            by_epoch[index].take().map(|result| EpochShadowCheck {
                engine_fingerprint: fingerprint.clone(),
                result,
            })
        })
        .collect())
}
