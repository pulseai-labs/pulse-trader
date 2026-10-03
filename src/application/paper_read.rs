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

use serde::{Deserialize, Serialize};

use crate::domain::paper::comparison::{OosComparison, comparison};
use crate::domain::paper::event::{PaperEvent, StopActor};
use crate::domain::paper::runtime::ShadowResult;
use crate::domain::paper::session::{Graduation, NonEmptyLabel, PaperSession, PaperSessionId};
use crate::domain::paper::state::{
    PaperClosedTrade, PaperPosition, PaperSessionState, PaperSessionStatus, ReplayError,
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
    /// The open position, when one stands.
    pub open_position: Option<PaperPosition>,
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
    /// The closed trades, in log order, with their realized R.
    pub closed_trades: Vec<PaperClosedTrade>,
    /// The open position, when one stands.
    pub open_position: Option<PaperPosition>,
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
/// # Errors
///
/// [`PaperReadError`] on a store or replay failure, or an undecodable shadow
/// payload.
pub async fn session_summary<P, W>(
    sessions: &P,
    walk_forwards: &W,
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
    let certifying_run = certifying_run(walk_forwards, &session).await?;
    summarize(&session, &events, &state, certifying_run.as_ref()).map(Some)
}

/// Every session's summary, in catalog order (the repository's `seq` order).
///
/// # Errors
///
/// [`PaperReadError`] on a store or replay failure, or an undecodable shadow
/// payload.
pub async fn list_summaries<P, W>(
    sessions: &P,
    walk_forwards: &W,
) -> Result<Vec<SessionSummary>, PaperReadError>
where
    P: PaperSessionRepository,
    W: WalkForwardRunRepository,
{
    let mut summaries = Vec::new();
    for session in sessions.list_sessions().await? {
        let events = sessions.events(&session.id).await?;
        let state = PaperSessionState::replay(&session, &events)?;
        let certifying_run = certifying_run(walk_forwards, &session).await?;
        summaries.push(summarize(
            &session,
            &events,
            &state,
            certifying_run.as_ref(),
        )?);
    }
    Ok(summaries)
}

/// One session's closed trades and open position, or `None` when no such row
/// exists.
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
    Ok(Some(SessionTrades {
        closed_trades: state.closed_trades,
        open_position: state.open_position,
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
        open_position: state.open_position.clone(),
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
