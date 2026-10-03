//! The paper UI's wire DTOs and its stream channel (r3.s4.w5, spec §1).
//!
//! Every type here **mirrors w4's JSON exactly** — the paper routes answer
//! with the domain/application types' own field names, so a `rename_all` on
//! this side would stop mirroring the wire (`ServerStatus` is `snake_case` in
//! `bindings.ts` for the same reason). Values the server computed are never
//! re-derived: decimals cross as the server's own exact text, counts as
//! numbers, and the two fields that genuinely carry arbitrary JSON cross as
//! that JSON's exact text (see [`PaperJsonText`]).
//!
//! **Three specta limits shape the shapes below** (probed against the pinned
//! `=2.0.0-rc.25` trio):
//!
//! - `i64`/`u64` are refused by the TypeScript exporter (`BigInt` precision), so
//!   epoch milliseconds and sequence numbers cross as exact text or `i32`, and
//!   counts as `u32`;
//! - `serde_json::Value` is NOT exportable — even with specta's `serde_json`
//!   feature its inlined definition carries `serde_json::Number`'s `i64`, and
//!   the export panics on the `BigInt` guard. Fields that carry arbitrary JSON
//!   are therefore a newtype over that text ([`PaperJsonText`]);
//! - `#[serde(flatten)]` IS supported: the exporter emits an intersection
//!   (`{…} & Verdict`), which TypeScript distributes for narrowing.
//!
//! The eight command wrappers in `commands.rs` stay thin: this module holds
//! the shapes, `src/client` holds the transport.

use serde::{Deserialize, Deserializer, Serialize};

use super::error::{BusError, BusErrorCode};

/// An arbitrary JSON value carried as its compact text.
///
/// The `shadow_checked` verdict's two sides and a stream frame's payload are
/// the only arbitrary-JSON fields on this wire. They cross as text because
/// specta refuses `serde_json::Value` (module docs); deserialization accepts
/// ANY JSON value and stores its compact form, and serialization writes that
/// text back. The screen parses the text where it renders it — it never
/// re-derives a value the server computed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
#[serde(transparent)]
pub struct PaperJsonText(String);

impl PaperJsonText {
    /// Wrap already-compact JSON text.
    #[must_use]
    pub fn new(text: String) -> Self {
        Self(text)
    }

    /// The text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PaperJsonText {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(
            serde_json::Value::deserialize(deserializer)?.to_string(),
        ))
    }
}

/// An epoch-millisecond integer carried as its exact text — `i64` is refused
/// by the bindings export (the `src/tauri/backtest.rs` `ms()` rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, specta::Type)]
#[serde(transparent)]
pub struct PaperEpochMs(String);

impl<'de> Deserialize<'de> for PaperEpochMs {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(i64::deserialize(deserializer)?.to_string()))
    }
}

// ---------------------------------------------------------------------------
// One session
// ---------------------------------------------------------------------------

/// One certified `(timeframe, data_version)` pair — `CertifiedDataVersion`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperCertifiedDataVersion {
    /// The timeframe the version belongs to (`15m`/`4h`/`1d`).
    pub timeframe: String,
    /// The content-hash `data_version`.
    pub data_version: String,
}

/// How the session was promoted — `Graduation`, internally tagged on
/// `graduation`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "graduation", rename_all = "snake_case")]
pub enum PaperGraduation {
    /// Promoted by a passing walk-forward on the current build.
    Certified {
        /// The certifying `walk_forward_run`.
        walk_forward_run_id: String,
        /// The distinct certified versions, in fold order.
        data_versions: Vec<PaperCertifiedDataVersion>,
    },
    /// Promoted by a human override: the reason and the instant it was taken.
    Override {
        /// Why the human overrode the gate.
        reason: String,
        /// The RFC3339 instant of the override decision.
        at: String,
    },
}

/// Who stopped a session — `StopActor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum PaperStopActor {
    /// A single session stopped by this client token.
    Token {
        /// The stopping token's label.
        label: String,
    },
    /// Every running session stopped by a `stop_all` sweep.
    StopAll {
        /// The token label that issued the sweep.
        issuer: String,
    },
}

/// Whether a session runs or has stopped — `SessionStatus`, tagged on `state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PaperStatus {
    /// Live: bars are consumed and orders fill.
    Running,
    /// Stopped: the log is read-only.
    Stopped {
        /// The recorded stopping actor, when the log names one.
        stopped_by: Option<PaperStopActor>,
    },
}

/// The open position — `PaperPosition`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperPosition {
    /// The position's side (`long`/`short`).
    pub side: String,
    /// The filled quantity, exact decimal text.
    pub qty: String,
    /// The entry fill price, exact decimal text.
    pub entry_price: String,
    /// The entry fill instant (RFC3339).
    pub entry_fill_time: String,
}

/// A closed trade — `PaperClosedTrade`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperClosedTrade {
    /// The side that closed.
    pub side: String,
    /// The closed quantity, exact decimal text.
    pub qty: String,
    /// The entry price, when the log recorded one.
    pub entry_price: Option<String>,
    /// The entry instant, when the log recorded one.
    pub entry_fill_time: Option<String>,
    /// The exit fill price, exact decimal text.
    pub exit_price: String,
    /// The exit fill instant (RFC3339).
    pub exit_fill_time: String,
    /// Why the position closed.
    pub exit_reason: String,
    /// The trade's realized R-multiple, when the fill recorded one.
    pub realized_r: Option<String>,
}

/// One shadow check's verdict — `ShadowResult`, tagged on `verdict`. `live` and
/// `shadow` are the server's own JSON values as text ([`json_text`]): the
/// server may put a trade, a count or a position mark there, so no shape may
/// be assumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum PaperShadowResult {
    /// The live epoch and the shadow agree.
    Identical {
        /// How many closed trades the check compared.
        closed_trades: u32,
        /// Whether an open position was compared (and agreed).
        open_position: bool,
    },
    /// They disagree; the first divergence is named in the server's own words.
    Drift {
        /// What diverged first, as the server wrote it.
        first_divergence: String,
        /// The live side of the divergence, JSON text.
        live: PaperJsonText,
        /// The shadow side of the divergence, JSON text.
        shadow: PaperJsonText,
    },
}

/// One epoch's latest shadow verdict — `EpochShadowCheck`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperShadowCheck {
    /// The epoch's engine fingerprint.
    pub engine_fingerprint: String,
    /// The epoch's latest verdict.
    pub result: PaperShadowResult,
}

/// The out-of-sample comparison's verdict — `ComparisonVerdict`, tagged on
/// `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PaperComparisonVerdict {
    /// No comparison applies; `reason` is the sentence to show.
    NotApplicable {
        /// Why there is nothing to compare (the server's own text).
        reason: String,
    },
    /// Fewer than `of` counted closed trades so far.
    Pending {
        /// The counted closed trades with a realized R.
        n: u32,
        /// The floor (`session.min_trades`).
        of: u32,
    },
    /// The live mean R sits inside the fold range.
    Within {
        /// The live mean R, exact decimal text.
        live_mean_r: String,
        /// The counted closed trades.
        n: u32,
        /// The certifying run's lowest fold mean R.
        fold_min: String,
        /// The certifying run's highest fold mean R.
        fold_max: String,
    },
    /// The live mean R is below the fold range.
    Below {
        /// The live mean R, exact decimal text.
        live_mean_r: String,
        /// The counted closed trades.
        n: u32,
        /// The certifying run's lowest fold mean R.
        fold_min: String,
        /// The certifying run's highest fold mean R.
        fold_max: String,
    },
    /// The live mean R is above the fold range.
    Above {
        /// The live mean R, exact decimal text.
        live_mean_r: String,
        /// The counted closed trades.
        n: u32,
        /// The certifying run's lowest fold mean R.
        fold_min: String,
        /// The certifying run's highest fold mean R.
        fold_max: String,
    },
}

/// The session's comparison — `OosComparison`. The verdict is flattened, so
/// `status` sits beside the two notes exactly as it does on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperComparison {
    /// The verdict, flattened onto this object.
    #[serde(flatten)]
    pub verdict: PaperComparisonVerdict,
    /// How many engine builds the session has run under.
    pub engine_builds: u32,
    /// Whether the certifying run's fingerprint is not this build's (E3:
    /// shown, never enforced).
    pub certification_stale: bool,
}

/// One session's summary — `SessionSummary`, the shape both the promote and
/// the read routes answer with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperSessionSummary {
    /// The session id.
    pub id: String,
    /// The promoted strategy version.
    pub strategy_version_id: String,
    /// The traded pair.
    pub pair: String,
    /// The primary timeframe.
    pub primary_timeframe: String,
    /// The higher timeframe, when the session uses one.
    pub htf_timeframe: Option<String>,
    /// Whether the session consumes the fixed daily series.
    pub uses_d1: bool,
    /// How the session was promoted.
    pub graduation: PaperGraduation,
    /// Whether every certified data version is a fixture snapshot.
    pub fixture: bool,
    /// The promoting token's label.
    pub promoted_by: String,
    /// Running or stopped, with the stop actor.
    pub status: PaperStatus,
    /// The engine fingerprints in order (E3).
    pub epochs: Vec<String>,
    /// The newest consumed bar's `open_time`, as exact epoch-millisecond text.
    pub last_bar_open_time: Option<PaperEpochMs>,
    /// How many closed trades the log holds.
    pub closed_trade_count: u32,
    /// The open position, when one stands.
    pub open_position: Option<PaperPosition>,
    /// The latest shadow verdict per epoch, in epoch order.
    pub shadow_checks: Vec<PaperShadowCheck>,
    /// Whether the certifying run's fingerprint is not this build's.
    pub certification_stale: bool,
    /// The OOS comparison.
    pub comparison: PaperComparison,
}

/// One session's trades — `SessionTrades`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperTrades {
    /// The closed trades, in log order, with their realized R.
    pub closed_trades: Vec<PaperClosedTrade>,
    /// The open position, when one stands.
    pub open_position: Option<PaperPosition>,
}

// ---------------------------------------------------------------------------
// The command bodies
// ---------------------------------------------------------------------------

/// The typed override half of a promote — `OverrideRequest` on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PromoteOverride {
    /// Why the human overrode the gate (trimmed non-empty by the server).
    pub reason: String,
    /// The pair the session trades.
    pub pair: String,
    /// The session's primary timeframe.
    pub primary_timeframe: String,
    /// The session's higher timeframe, when named.
    pub htf_timeframe: Option<String>,
    /// Whether the session consumes the fixed daily series.
    pub uses_d1: bool,
}

/// What `paper_promote` is asked for — the route's body, literally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PromoteRequest {
    /// The strategy version to promote.
    pub version_id: String,
    /// The typed override, when the caller supplies one.
    pub r#override: Option<PromoteOverride>,
}

/// What `paper_stop` answered — the stop route's body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperStopResult {
    /// The session that stopped.
    pub session_id: String,
    /// Whether the stop skipped the final shadow check (an unattached
    /// session).
    pub stopped_without_shadow: bool,
}

/// One session `stop_all` could not stop — the route's `failures[]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct StopFailure {
    /// The session that did not stop.
    pub id: String,
    /// The runtime's typed error code for it.
    pub code: String,
}

/// What `paper_stop_all` answered — the kill switch's own shape. `failures` is
/// always present: a sweep that reports only its successes would hide exactly
/// the sessions a trader needs to see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct StopAllResult {
    /// The sessions that stopped.
    pub stopped: Vec<String>,
    /// The sessions that did not, with their typed code.
    pub failures: Vec<StopFailure>,
}

// ---------------------------------------------------------------------------
// The session stream
// ---------------------------------------------------------------------------

/// One `paper` frame from the session stream: the log event's sequence, its
/// `type` tag, and the event's own JSON as text ([`json_text`] — the payload
/// is an arbitrary event body).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct PaperEventFrame {
    /// The event's per-session sequence.
    pub seq: i32,
    /// The event's `type` tag (`bar_processed`, `fill`, `shadow_checked`, …).
    pub r#type: String,
    /// The event's own JSON, compact text.
    pub payload: PaperJsonText,
}

/// What arrives on a session stream's channel: one frame, in seq order, or the
/// terminal refusal. Internally tagged, so the frontend switches on one
/// discriminated union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum PaperStreamEvent {
    /// One log frame, in seq order.
    Frame(PaperEventFrame),
    /// The server revoked the presented token; the stream is over.
    #[serde(rename_all = "camelCase")]
    TokenRefused {
        /// The server's own reason text.
        reason: String,
    },
}

/// Somewhere a session-stream reader can push [`PaperStreamEvent`]s.
///
/// The production implementor is `tauri::ipc::Channel<PaperStreamEvent>`; the
/// test suites supply their own (a recorder, a refusing sink). Keeping the
/// reader generic over this trait is what lets `tests/tauri_paper.rs` prove
/// the dropped-channel stop without a running app — the `EventSink` precedent.
pub trait PaperStreamSink: Send + Sync {
    /// Deliver one event, or refuse when the far end is gone.
    ///
    /// # Errors
    ///
    /// A [`BusError`] when the channel is closed; the reader treats it as
    /// cancellation and stops.
    fn send_paper_event(&self, event: PaperStreamEvent) -> Result<(), BusError>;
}

impl PaperStreamSink for tauri::ipc::Channel<PaperStreamEvent> {
    fn send_paper_event(&self, event: PaperStreamEvent) -> Result<(), BusError> {
        self.send(event).map_err(|error| {
            BusError::new(
                BusErrorCode::Internal,
                format!("the paper session channel is gone: {error}"),
            )
        })
    }
}
