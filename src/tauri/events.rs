//! The typed event stream (ADR-0020, bus contract clause 5) — **per-invocation
//! channels, never a global event bus**.
//!
//! Streaming uses a Tauri v2 `Channel<T>` handed to the command that starts the run.
//! **The channel *is* the correlation.** A second compose run gets a second channel and
//! cannot be mistaken for the first, because there is no shared bus for the two to
//! share (grill A2).
//!
//! A global event bus with correlation ids bolted on was the rejected alternative. It
//! fails for a reason worth writing down: it makes every *subscriber* responsible for
//! filtering, and a missed filter is a cross-run data leak that type-checks. With a
//! per-invocation channel, delivering run B's tokens to run A's screen is not a bug you
//! can write — there is no wire between them.
//!
//! [`BusEvent::run_id`] is still carried on every event. That is belt-and-braces, not
//! the mechanism: it makes the correlation *assertable* (AC-5) and lets a log line name
//! its run.
//!
//! [`EventSink`] exists so the streaming core is testable and so the domain-facing half
//! of the bus never names a Tauri type. `Channel<BusEvent>` implements it; so does any
//! test double.

use serde::{Deserialize, Serialize};

use super::error::{BusError, BusErrorCode};

/// A per-invocation run identifier, minted when a streaming command starts.
///
/// Opaque on the wire (`#[serde(transparent)]` over a UUID string) so the frontend
/// compares it and never parses it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Mint a fresh run id.
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for RunId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RunId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a streamed event is *about*.
///
/// Internally tagged on `kind`, so the frontend switches on one discriminated union
/// rather than sniffing which optional fields are present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum BusEventPayload {
    /// The run has begun. Always the first event on a channel.
    Started,
    /// Incremental progress — one step, one token, one line of output.
    Progress {
        /// Human-readable progress text.
        message: String,
    },
    /// A composer tool call was dispatched to a builder tool (r1.s1.w4).
    ///
    /// Mirrors [`ComposerEvent::ToolCallStarted`](crate::agent::ComposerEvent):
    /// the payload carries the composer's OWN fields — the tool name and its
    /// non-secret arguments preview — never invented ones. Mapping the composer's
    /// stream onto the channel is rendering, not faking.
    ///
    /// The variant-level `rename_all` exists because the ENUM-level one renames
    /// variants only, not their fields — without it `arguments_preview` would
    /// cross the wire in `snake_case` against the file's `camelCase` convention
    /// (a nothing-word like `message` never exposed this before multi-word
    /// fields).
    #[serde(rename_all = "camelCase")]
    ToolCallStarted {
        /// The builder tool the model called (e.g. `add_filter`).
        name: String,
        /// A short, non-secret preview of the call arguments.
        arguments_preview: String,
    },
    /// A composer tool call produced an outcome (r1.s1.w4).
    ///
    /// Mirrors [`ComposerEvent::ToolCallResult`](crate::agent::ComposerEvent) —
    /// the `Ok` summary or the serialized correctable `FieldError`s, appended to
    /// the same step its `ToolCallStarted` opened.
    #[serde(rename_all = "camelCase")]
    ToolCallResult {
        /// The builder tool the model called.
        name: String,
        /// The `Ok` summary or the serialized correctable errors.
        outcome: String,
    },
    /// The composer's own prose for one turn (r3.s2.w5, Q5 option 1).
    ///
    /// Mirrors
    /// [`ComposerEvent::AssistantText`](crate::agent::ComposerEvent) — the
    /// text has already crossed the scrub-then-bound seam inside the composer,
    /// so what rides the bus is safe to display and store.
    AssistantText {
        /// The scrubbed, bounded assistant text.
        text: String,
    },
    /// The run finished successfully. Always the last event on a channel.
    ///
    /// For a compose run (r1.s1.w4) the closing summary is the composer's own
    /// finalize line — `ComposerEvent::Finalized`'s `version_summary`.
    Finished {
        /// A closing summary line.
        message: String,
    },
}

/// One event on one run's channel.
///
/// `run_id` + `seq` together let the frontend detect a dropped event rather than
/// silently rendering a shorter stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct BusEvent {
    /// The run this event belongs to — the id the command was invoked with.
    pub run_id: RunId,
    /// Monotonic from 0 within a run.
    pub seq: u32,
    /// What happened.
    pub payload: BusEventPayload,
}

impl BusEvent {
    /// Build an event for `run_id` at position `seq`.
    #[must_use]
    pub fn new(run_id: &RunId, seq: u32, payload: BusEventPayload) -> Self {
        Self {
            run_id: run_id.clone(),
            seq,
            payload,
        }
    }
}

/// Somewhere a streaming command can push [`BusEvent`]s.
///
/// The production implementor is `tauri::ipc::Channel<BusEvent>`; tests supply their
/// own. Keeping the streaming core generic over this trait is what lets
/// `tests/tauri_bus_contract.rs` assert cancellation behaviour without a running app —
/// and it keeps the core from naming a Tauri type, which matters because the same core
/// will later be driven by the composer.
pub trait EventSink {
    /// Deliver one event.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] when the far end is gone — which the streaming core reads
    /// as **cancellation**, not as a failure to report (see
    /// [`super::commands::demo_stream_core`]).
    fn send_event(&self, event: BusEvent) -> Result<(), BusError>;
}

impl EventSink for tauri::ipc::Channel<BusEvent> {
    fn send_event(&self, event: BusEvent) -> Result<(), BusError> {
        self.send(event)
            .map_err(|e| BusError::new(BusErrorCode::Internal, e.to_string()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{BusEvent, BusEventPayload, RunId};

    #[test]
    fn run_ids_are_unique_per_mint() {
        assert_ne!(RunId::new(), RunId::new());
    }

    #[test]
    fn the_wire_shape_is_camel_case_with_a_tagged_payload() {
        let run_id = RunId::new();
        let event = BusEvent::new(
            &run_id,
            7,
            BusEventPayload::Progress {
                message: "step".to_owned(),
            },
        );
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["runId"], serde_json::json!(run_id.as_str()));
        assert_eq!(json["seq"], serde_json::json!(7));
        assert_eq!(json["payload"]["kind"], serde_json::json!("progress"));
        assert_eq!(json["payload"]["message"], serde_json::json!("step"));
    }
}
