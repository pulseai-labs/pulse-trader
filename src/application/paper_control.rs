//! The control handle into the live paper runtime (r3.s4.w4, spec §1).
//!
//! One cloneable [`PaperControl`] over an mpsc channel of [`PaperCommand`]s,
//! each carrying a oneshot reply: the server's routes hold the sender, the
//! runtime thread holds the receiver and applies each command between wakes
//! (never during an `append_bar` — the loop is single-task, so a wake runs to
//! completion before the next command is received). The production loop lives
//! in `server::bind`; the application-side command semantics live in
//! [`PaperRuntime::handle_command`](crate::application::paper_runtime::PaperRuntime::handle_command).
//!
//! **No runtime.** With no receiver (tests, or the runtime failed to start)
//! every request answers [`PaperControlError::RuntimeUnavailable`] — a dropped
//! sender, never a hang. A live receiver that stops answering is bounded by the
//! reply timeout the handle carries ([`DEFAULT_PAPER_REPLY_TIMEOUT_MS`], the
//! `ServeConfig` default), and answers [`PaperControlError::Timeout`].
//!
//! The command replies are outcome enums, not errors: "the session is already
//! stopped" is an expected answer the routes map to 409, not a failure.

use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::application::paper_runtime::{PaperRuntimeError, SessionFailure};
use crate::domain::backtest::OpenPositionStops;
use crate::domain::paper::event::StopActor;
use crate::domain::paper::runtime::ShadowResult;
use crate::domain::paper::session::{NonEmptyLabel, PaperSessionId};

/// How long a control request waits for its reply by default (spec §1), used
/// by `ServeConfig` when no override is set.
pub const DEFAULT_PAPER_REPLY_TIMEOUT_MS: u64 = 30_000;

/// How long the read path's position query waits for the runtime's reply —
/// the reply half of its end-to-end bound; the enqueue half is non-blocking
/// by construction ([`POSITION_ENQUEUE_RESERVE`]) (r4.s4.w1, spec §3: "a few
/// seconds, a named constant"). Short on purpose — a read must never stall on
/// a busy runtime, and an unanswered query simply answers without the stops —
/// while leaving one runtime turn ample room.
const POSITION_REPLY_TIMEOUT_MS: u64 = 2_000;

/// How many slots of the command channel the read path's position query keeps
/// free for control commands (r4.s4.w2, E2): a `Position` query only enqueues
/// while MORE than this many of the channel's 64 slots are free. Otherwise it
/// answers without the stops at once — its existing degrade path — so a
/// wedged runtime plus a flood of summary reads cannot fill the bounded
/// channel the kill switch's `stop`/`stop_all` must enqueue into. Sized well
/// above a burst of concurrent reads and well below the channel.
const POSITION_ENQUEUE_RESERVE: usize = 16;

/// One control command, with its reply channel. `Attach` is the promote
/// path's; `Stop`/`StopAll` are the kill switch's; `ShadowCheck` is the
/// on-demand check's.
pub enum PaperCommand {
    /// Attach one promoted session now (its lead-in is fetched at once rather
    /// than at the next idle scan).
    Attach {
        /// The session to attach.
        session_id: PaperSessionId,
        /// `Ok(())` once attached (or already attached); the runtime's typed
        /// refusal otherwise.
        reply: oneshot::Sender<Result<(), PaperRuntimeError>>,
    },
    /// Stop one session (attached or not — see `StopReply`).
    Stop {
        /// The session to stop.
        session_id: PaperSessionId,
        /// Who is stopping it (from the caller's token label).
        actor: StopActor,
        /// The typed outcome.
        reply: oneshot::Sender<Result<StopReply, PaperRuntimeError>>,
    },
    /// Stop every session the runtime runs, as one sweep.
    StopAll {
        /// The token label that issued the sweep.
        issuer: NonEmptyLabel,
        /// The stopped ids and the per-session failures.
        reply: oneshot::Sender<StopAllReply>,
    },
    /// Run one session's shadow check now.
    ShadowCheck {
        /// The session to check.
        session_id: PaperSessionId,
        /// The typed outcome.
        reply: oneshot::Sender<Result<ShadowCheckReply, PaperRuntimeError>>,
    },
    /// Answer one session's open-position stop geometry now, read-only — the
    /// paper read's live stops (r4.s4.w1). Answered from the runtime's own
    /// attached sessions; `None` when the runtime does not hold the session,
    /// it is flat, or its engine owes a rebuild (r4.s4.w2). Never touches the
    /// store.
    Position {
        /// The session to read.
        session_id: PaperSessionId,
        /// The stops with the log seq they describe, or `None` when flat /
        /// not attached / awaiting a rebuild.
        reply: oneshot::Sender<Option<PositionReply>>,
    },
}

/// What a `Position` query answered (r4.s4.w2): the open position's stop
/// geometry as the live engine holds it, paired with the last log seq the
/// runtime committed for the session. The read compares that seq with the
/// last seq of the log it replayed and drops the stops when the two do not
/// describe the same state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PositionReply {
    /// The open position's stop geometry.
    pub stops: OpenPositionStops,
    /// The session's last committed log seq when the answer was built.
    pub last_seq: i64,
}

/// What a `Stop` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReply {
    /// The session was attached: the final shadow check is attempted, then the
    /// `stop` event — a failed check is logged and does not veto the stop.
    Stopped,
    /// The session was not attached: a direct final `Stop` append with no
    /// shadow check (spec §1 — a session can always be stopped).
    StoppedWithoutShadow,
    /// No such session row.
    Unknown,
    /// The session's log already ends in `stop`.
    AlreadyStopped,
}

/// What a `ShadowCheck` did.
#[derive(Debug, Clone, PartialEq)]
pub enum ShadowCheckReply {
    /// The check ran; the verdict is the payload.
    Checked(ShadowResult),
    /// No such session row.
    Unknown,
    /// The session is stopped; nothing to check.
    Stopped,
    /// The session runs but this runtime does not hold it (for example its
    /// replay failed), so there is no live state to check.
    NotAttached,
}

/// What a `StopAll` did: the ids it stopped and the per-session failures. A
/// failure on one session never leaves the others running (spec §5).
#[derive(Debug)]
pub struct StopAllReply {
    /// The sessions that stopped.
    pub stopped: Vec<PaperSessionId>,
    /// The sessions that did not, with their typed error.
    pub failures: Vec<SessionFailure>,
}

/// Why a control request could not deliver its command's outcome.
#[derive(Debug)]
pub enum PaperControlError {
    /// No runtime is attached (no handle installed, or its receiver is gone).
    RuntimeUnavailable,
    /// The runtime did not answer inside the reply timeout.
    Timeout,
    /// The runtime answered with its own typed refusal.
    Runtime(PaperRuntimeError),
}

impl core::fmt::Display for PaperControlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::RuntimeUnavailable => write!(f, "the paper runtime is not available"),
            Self::Timeout => write!(f, "the paper runtime did not answer in time"),
            Self::Runtime(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PaperControlError {}

/// The cloneable handle the routes hold.
#[derive(Clone)]
pub struct PaperControl {
    sender: mpsc::Sender<PaperCommand>,
    reply_timeout: Duration,
}

impl PaperControl {
    /// The channel pair: the handle for the server, the receiver for the
    /// runtime loop. `reply_timeout_ms` bounds every reply wait.
    #[must_use]
    pub fn channel(reply_timeout_ms: u64) -> (Self, mpsc::Receiver<PaperCommand>) {
        let (sender, receiver) = mpsc::channel(64);
        (
            Self {
                sender,
                reply_timeout: Duration::from_millis(reply_timeout_ms),
            },
            receiver,
        )
    }

    /// Whether a runtime can still receive commands (its receiver is alive).
    /// A cheap pre-check for the promote route's no-runtime refusal.
    #[must_use]
    pub fn is_available(&self) -> bool {
        !self.sender.is_closed()
    }

    /// Attach one session now.
    ///
    /// # Errors
    ///
    /// [`PaperControlError`] when no runtime answers, the reply times out, or
    /// the runtime refuses the attach.
    pub async fn attach(&self, session_id: PaperSessionId) -> Result<(), PaperControlError> {
        self.request(|reply| PaperCommand::Attach { session_id, reply })
            .await?
            .map_err(PaperControlError::Runtime)
    }

    /// Stop one session.
    ///
    /// # Errors
    ///
    /// [`PaperControlError`] when no runtime answers, the reply times out, or
    /// the runtime's stop itself fails.
    pub async fn stop(
        &self,
        session_id: PaperSessionId,
        actor: StopActor,
    ) -> Result<StopReply, PaperControlError> {
        self.request(|reply| PaperCommand::Stop {
            session_id,
            actor,
            reply,
        })
        .await?
        .map_err(PaperControlError::Runtime)
    }

    /// Stop every running session.
    ///
    /// # Errors
    ///
    /// [`PaperControlError`] when no runtime answers or the reply times out.
    /// Per-session failures ride the reply's `failures`.
    pub async fn stop_all(&self, issuer: NonEmptyLabel) -> Result<StopAllReply, PaperControlError> {
        self.request(|reply| PaperCommand::StopAll { issuer, reply })
            .await
    }

    /// Run one session's shadow check now.
    ///
    /// # Errors
    ///
    /// [`PaperControlError`] when no runtime answers, the reply times out, or
    /// the check itself fails.
    pub async fn shadow_check(
        &self,
        session_id: PaperSessionId,
    ) -> Result<ShadowCheckReply, PaperControlError> {
        self.request(|reply| PaperCommand::ShadowCheck { session_id, reply })
            .await?
            .map_err(PaperControlError::Runtime)
    }

    /// Ask one session's open-position stops now, under the read path's SHORT
    /// timeout ([`POSITION_REPLY_TIMEOUT_MS`]) — never the command timeout,
    /// which bounds mutations a caller must actually see (r4.s4.w1). The
    /// enqueue NEVER blocks (r4.s4.w2, E2): the query only takes a channel
    /// slot while more than [`POSITION_ENQUEUE_RESERVE`] of them are free, so
    /// read traffic can never wedge the control commands that must go
    /// through; at or below the reserve it answers `Ok(None)` at once — the
    /// read then carries no stops, its existing degrade path. `Ok(None)` also
    /// when the runtime does not hold the session, it is flat, or it owes a
    /// rebuild.
    ///
    /// # Errors
    ///
    /// [`PaperControlError`] when no runtime is attached or the reply times
    /// out. Callers on the read path answer without the stops on either.
    pub async fn position(
        &self,
        session_id: PaperSessionId,
    ) -> Result<Option<PositionReply>, PaperControlError> {
        // The channel's tail belongs to the control commands: a read that
        // finds the reserve already eaten degrades instead of queueing.
        if self.sender.capacity() <= POSITION_ENQUEUE_RESERVE {
            return Ok(None);
        }
        let (reply, done) = oneshot::channel();
        let command = PaperCommand::Position { session_id, reply };
        if let Err(error) = self.sender.try_send(command) {
            return match error {
                // A racing sender took the last free slot: still a degrade,
                // never a failure — and never a block.
                mpsc::error::TrySendError::Full(_) => Ok(None),
                // The receiver is gone (the runtime is not attached).
                mpsc::error::TrySendError::Closed(_) => Err(PaperControlError::RuntimeUnavailable),
            };
        }
        // The enqueue landed; only the reply is left to wait for, under the
        // SHORT bound that leaves one runtime turn ample room.
        match tokio::time::timeout(Duration::from_millis(POSITION_REPLY_TIMEOUT_MS), done).await {
            Err(_) => Err(PaperControlError::Timeout),
            // The runtime dropped the reply without answering (it stopped).
            Ok(Err(_)) => Err(PaperControlError::RuntimeUnavailable),
            Ok(Ok(answer)) => Ok(answer),
        }
    }

    /// Send one command and await its reply under the handle's timeout.
    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> PaperCommand,
    ) -> Result<T, PaperControlError> {
        self.request_under(self.reply_timeout, make).await
    }

    /// Send one command and await its reply under `timeout` — [`Self::request`]
    /// with the bound as a parameter, so the read path's short position query
    /// shares the channel machinery without widening the command timeout.
    async fn request_under<T>(
        &self,
        timeout: Duration,
        make: impl FnOnce(oneshot::Sender<T>) -> PaperCommand,
    ) -> Result<T, PaperControlError> {
        let (reply, done) = oneshot::channel();
        if self.sender.send(make(reply)).await.is_err() {
            return Err(PaperControlError::RuntimeUnavailable);
        }
        match tokio::time::timeout(timeout, done).await {
            Err(_) => Err(PaperControlError::Timeout),
            // The runtime dropped the reply without answering (it stopped).
            Ok(Err(_)) => Err(PaperControlError::RuntimeUnavailable),
            Ok(Ok(value)) => Ok(value),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use tokio::sync::oneshot;

    use super::{
        DEFAULT_PAPER_REPLY_TIMEOUT_MS, POSITION_ENQUEUE_RESERVE, POSITION_REPLY_TIMEOUT_MS,
        PaperCommand, PaperControl, PaperControlError, PositionReply,
    };
    use crate::domain::paper::event::StopActor;
    use crate::domain::paper::session::{NonEmptyLabel, PaperSessionId};

    fn id() -> PaperSessionId {
        PaperSessionId::new("sess-1".to_owned())
    }

    /// Queue one `Position` command directly — the very command the read path
    /// enqueues — with its reply receiver dropped on this side.
    fn queue_position(control: &PaperControl) {
        let (reply, _done) = oneshot::channel::<Option<PositionReply>>();
        control
            .sender
            .try_send(PaperCommand::Position {
                session_id: id(),
                reply,
            })
            .expect("the test keeps the queue below capacity");
    }

    /// Fill the channel the way the read path would — queue `Position`
    /// commands — until exactly `free` slots are left.
    fn fill_to_free(control: &PaperControl, free: usize) {
        while control.sender.capacity() > free {
            queue_position(control);
        }
        assert_eq!(
            control.sender.capacity(),
            free,
            "the fill stops exactly at the wanted free capacity"
        );
    }

    /// A position query that found room keeps the SHORT bound: the command is
    /// queued, and with nothing draining the reply times out inside
    /// [`POSITION_REPLY_TIMEOUT_MS`]. (The round-1 test filled the channel to
    /// the brim; that case now degrades at once instead — the reserve tests
    /// below.)
    #[tokio::test]
    async fn an_enqueued_position_query_is_still_bounded_by_the_reply_timeout() {
        let (control, receiver) = PaperControl::channel(DEFAULT_PAPER_REPLY_TIMEOUT_MS);
        // Free capacity one above the reserve: the query still enqueues.
        fill_to_free(&control, POSITION_ENQUEUE_RESERVE + 1);
        let queued = receiver.len();

        let started = tokio::time::Instant::now();
        let result = control.position(id()).await;
        let elapsed = started.elapsed();
        assert!(
            matches!(result, Err(PaperControlError::Timeout)),
            "the reply bound expires: {result:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(POSITION_REPLY_TIMEOUT_MS),
            "the bound is the short one: {elapsed:?}"
        );
        assert!(
            elapsed <= Duration::from_millis(POSITION_REPLY_TIMEOUT_MS + 1_000),
            "the query returns with the bound, not later: {elapsed:?}"
        );
        assert_eq!(
            receiver.len(),
            queued + 1,
            "the query took a slot: its enqueue landed"
        );
    }

    /// At the reserve the query answers at once, WITHOUT enqueueing: the
    /// slots the kill switch needs stay free (r4.s4.w2, E2).
    #[tokio::test]
    async fn a_position_query_at_the_reserve_answers_without_enqueueing() {
        let (control, receiver) = PaperControl::channel(DEFAULT_PAPER_REPLY_TIMEOUT_MS);
        // Free capacity exactly equal to the reserve: the query degrades.
        fill_to_free(&control, POSITION_ENQUEUE_RESERVE);
        let queued = receiver.len();

        let started = tokio::time::Instant::now();
        let result = control.position(id()).await;
        assert!(
            matches!(result, Ok(None)),
            "no stops and no failure — the read's degrade path: {result:?}"
        );
        assert!(
            started.elapsed() < Duration::from_millis(POSITION_REPLY_TIMEOUT_MS),
            "answered at once, never waited for a reply: {:?}",
            started.elapsed()
        );
        assert_eq!(receiver.len(), queued, "nothing was enqueued");
    }

    /// A channel filled with position reads up to the reserve still takes a
    /// control command's enqueue: the stop is queued promptly, and only its
    /// reply waits — the kill switch never wedges behind read traffic
    /// (r4.s4.w2, E2; N1).
    #[tokio::test]
    async fn a_channel_filled_to_the_reserve_still_takes_a_stop() {
        // A short reply bound keeps the test fast; the ENQUEUE is what this
        // test measures, and it is not bounded by it.
        let (control, receiver) = PaperControl::channel(50);
        fill_to_free(&control, POSITION_ENQUEUE_RESERVE);
        let queued = receiver.len();

        let actor = StopActor::Token {
            label: NonEmptyLabel::try_new("test-token").expect("non-empty"),
        };
        let stopped = tokio::time::timeout(Duration::from_secs(5), control.stop(id(), actor)).await;
        assert!(
            matches!(stopped, Ok(Err(PaperControlError::Timeout))),
            "the stop enqueued and only its reply timed out: {stopped:?}"
        );
        assert_eq!(
            receiver.len(),
            queued + 1,
            "the stop's command sits in the queue"
        );
    }
}
