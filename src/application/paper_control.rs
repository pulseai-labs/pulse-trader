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
use crate::domain::paper::event::StopActor;
use crate::domain::paper::runtime::ShadowResult;
use crate::domain::paper::session::{NonEmptyLabel, PaperSessionId};

/// How long a control request waits for its reply by default (spec §1), used
/// by `ServeConfig` when no override is set.
pub const DEFAULT_PAPER_REPLY_TIMEOUT_MS: u64 = 30_000;

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
}

/// What a `Stop` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReply {
    /// The session was attached: a final shadow check, then the `stop` event.
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

    /// Send one command and await its reply under the handle's timeout.
    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<T>) -> PaperCommand,
    ) -> Result<T, PaperControlError> {
        let (reply, done) = oneshot::channel();
        if self.sender.send(make(reply)).await.is_err() {
            return Err(PaperControlError::RuntimeUnavailable);
        }
        match tokio::time::timeout(self.reply_timeout, done).await {
            Err(_) => Err(PaperControlError::Timeout),
            // The runtime dropped the reply without answering (it stopped).
            Ok(Err(_)) => Err(PaperControlError::RuntimeUnavailable),
            Ok(Ok(value)) => Ok(value),
        }
    }
}
