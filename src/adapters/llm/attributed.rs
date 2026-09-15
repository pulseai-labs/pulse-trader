//! The attributed coach provider (r1.s4.w1, `#132` / PR #128 finding G1) — one
//! call, one ledger row, learned TOGETHER.
//!
//! **The wiring obligation this removes.** The sealed turn used to take a provider
//! and a capture handle independently, which made "the buffer the decorator writes
//! through is the buffer this turn reads" a promise a caller could keep or break
//! silently. Break it and a turn either names NO ledger row (a post-call session
//! with `llm_call_id = NULL`, which the audit trail reads as "no row was correlated
//! to this turn" about a call that was made and billed) or names ANOTHER turn's.
//!
//! Here the two are composed inside one adapter built by the composition root, and
//! the port hands the application module a `(response, LlmCallId)` pair or a typed
//! refusal. The module cannot be handed a mismatched pair because it is never handed
//! two things.
//!
//! **It still refuses rather than resolves.** Zero ids for a usable response is
//! [`AttributedCallError::LedgerRowMissing`]; several is
//! [`AttributedCallError::LedgerRowsAmbiguous`]. Taking the newest of several — what
//! the coach did before PR #128 — can name a different turn's row, and no choice
//! among them is honest. That is DETECTION, not proof of origin: a buffer shared
//! with a second provider that captures nothing still yields exactly one id, and
//! nothing here can tell whose it is. What removes that last case is the composition
//! root minting one buffer per turn, which is now one line in one place.

use std::sync::{Mutex, PoisonError};

use crate::domain::{AttributedCall, AttributedCallError, AttributedCoachProvider};
use crate::domain::{LlmCallId, LlmConfig, LlmProvider, Message, ToolDefinition};

use crate::agent::LlmCallCapture;

/// A [`LlmProvider`] (the redacting + cost-logging decorator, in production) paired
/// with the capture buffer that decorator's ledger repo pushes minted ids into.
///
/// Consumed generically (`<P: LlmProvider>`, never `dyn`) — the established port
/// style.
pub(crate) struct AttributedProvider<P> {
    inner: P,
    captured: LlmCallCapture,
    /// Where the buffer stood when the CURRENT call started, so a cancelled call
    /// (the timeout path) can still be asked what it correlated. `Mutex` rather than
    /// `Cell` because the port takes `&self` and the turn is polled on a runtime.
    call_start: Mutex<usize>,
}

impl<P> AttributedProvider<P> {
    /// Pair `inner` with the buffer its ledger decorator writes through.
    ///
    /// The composition root mints ONE buffer per turn and hands it to exactly one
    /// capturing repo and one of these.
    pub(crate) fn new(inner: P, captured: LlmCallCapture) -> Self {
        Self {
            inner,
            captured,
            call_start: Mutex::new(0),
        }
    }

    /// The current buffer length — the pre-call snapshot point.
    fn captured_len(&self) -> usize {
        self.captured
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The ids that appeared since `start`, in order.
    ///
    /// A buffer SHORTER than `start` is an error, not an empty slice. `get(start..)`
    /// returns `None` there, and defaulting that to "no ids" reports a call that
    /// reached the provider as having minted no ledger row — `llm_call_id = NULL` on
    /// a turn that was made and billed, which is the same false record
    /// `LedgerRowMissing` exists to refuse.
    fn ids_since(&self, start: usize) -> Result<Vec<LlmCallId>, AttributedCallError> {
        let guard = self.captured.lock().unwrap_or_else(PoisonError::into_inner);
        guard.get(start..).map(<[LlmCallId]>::to_vec).ok_or(
            AttributedCallError::CaptureBufferShrank {
                start,
                len: guard.len(),
            },
        )
    }

    /// Record where the buffer stood as this call began.
    fn mark_call_start(&self, start: usize) {
        *self
            .call_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = start;
    }

    /// Where the buffer stood when the last call began.
    fn call_start(&self) -> usize {
        *self
            .call_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// At most one id since `start` — the rule for a call that produced no usable
    /// response. Zero is legitimate (a timeout can strike before the decorator
    /// writes); one is the common case (a transport fault is still a billed call
    /// and still writes its row, PR #169 R1); several never is.
    fn at_most_one(&self, start: usize) -> Result<Option<LlmCallId>, AttributedCallError> {
        let mut ids = self.ids_since(start)?;
        match ids.len() {
            0 => Ok(None),
            1 => Ok(Some(ids.remove(0))),
            seen => Err(AttributedCallError::LedgerRowsAmbiguous { seen }),
        }
    }
}

impl<P: LlmProvider + Sync> AttributedCoachProvider for AttributedProvider<P> {
    async fn attributed_chat(
        &self,
        messages: Vec<Message>,
        tools: &[ToolDefinition],
        config: &LlmConfig,
    ) -> Result<AttributedCall, AttributedCallError> {
        let start = self.captured_len();
        self.mark_call_start(start);

        match self.inner.chat(messages, tools, config).await {
            Ok(response) => {
                // A turn that got a USABLE RESPONSE names exactly one ledger row, or
                // it is a wiring fault and not a coaching outcome.
                let mut ids = self.ids_since(start)?;
                match ids.len() {
                    1 => Ok(AttributedCall {
                        response,
                        llm_call_id: ids.remove(0),
                    }),
                    0 => Err(AttributedCallError::LedgerRowMissing),
                    seen => Err(AttributedCallError::LedgerRowsAmbiguous { seen }),
                }
            }
            Err(error) => {
                // The call failed, but it may still have minted a row before it did
                // — name it if so rather than asserting NULL.
                let llm_call_id = self.at_most_one(start)?;
                Err(AttributedCallError::Provider { error, llm_call_id })
            }
        }
    }

    fn attempted_call_id(&self) -> Result<Option<LlmCallId>, AttributedCallError> {
        self.at_most_one(self.call_start())
    }
}
