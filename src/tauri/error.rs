//! The **one** error shape that crosses the Tauri boundary (ADR-0020, bus contract
//! clause 1).
//!
//! Every domain error family the desktop can provoke — `DataError` first, and
//! `ValidationErrors`, `BacktestError`, `ExchangeError`, `LlmError`, `ComposerError`
//! behind it — maps to a single [`BusError`]. The frontend renders errors with one code
//! path and never sniffs which family an error came from.
//!
//! **Three things deliberately never cross.**
//!
//! - **A stringified `Debug`.** [`BusError::message`] is the source error's `Display`
//!   rendering, which is prose a human can read. `Debug` leaks Rust variant syntax and
//!   sometimes internal payloads.
//! - **A panic.** Commands return `Result<_, BusError>`; nothing in the bus unwraps.
//!   The crate-wide `clippy::unwrap_used` / `expect_used` denials hold that structurally.
//! - **A numeric discriminant.** [`BusErrorCode`] serializes as a string, so inserting a
//!   variant cannot silently renumber the ones the frontend already branches on.
//!
//! `tests/tauri_bus_contract.rs::domain_error_maps_to_one_serializable_shape` (AC-4) is
//! the gate on all of it.

use serde::{Deserialize, Serialize};

use crate::agent::ComposerError;
use crate::application::backtest::BacktestAppError;
use crate::domain::{BacktestError, DataError, ExchangeError, LlmError, ValidationErrors};

/// Which family a [`BusError`] came from — a closed set the frontend can branch on.
///
/// Serializes as a lower-case string discriminant (`"data"`, `"backtest"`, ...), never
/// as an index, so a variant inserted later cannot renumber the existing ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum BusErrorCode {
    /// The data pipeline / persistence layer (`DataError`).
    Data,
    /// Semantic DSL validation (`ValidationErrors`) — the correctable-rejection family.
    Validation,
    /// The backtest engine (`BacktestError`).
    Backtest,
    /// Exchange metadata (`ExchangeError`).
    Exchange,
    /// The LLM transport (`LlmError`).
    Llm,
    /// The composer agent loop (`ComposerError`).
    Composer,
    /// An operation the app is **already running** was invoked again (r1.s4.w3,
    /// `pulseai-labs/pulse-trader#141`).
    ///
    /// A family of its own rather than an `Internal`, because it is the one
    /// refusal that is not a fault: nothing broke, nothing failed, and there is
    /// nothing to retry differently — the answer the caller wants is already on
    /// its way from the invocation that holds the key. The coach rail renders it
    /// as "already running" rather than as an error, and it can only do that if
    /// the code says so.
    ///
    /// The discriminant serializes as a string (`"busy"`), so its position
    /// carries no wire meaning; it sits beside the other non-domain code for
    /// readability.
    Busy,
    /// A named thing the command was asked about is not there (r2.s1.w4).
    ///
    /// A family of its own rather than `Data`: `Data` says the store faulted,
    /// which prescribes a retry, while this says the lookup came back empty —
    /// the remedy is a different name, not another attempt at a store that is
    /// working fine. First used by `compare_child_run` for an unknown run id,
    /// a version with no parent, or a parent with no run to compare against.
    NotFound,
    /// The shell itself: a dead channel, a failed startup, a bug. Not a domain family.
    Internal,
}

/// The single serializable error shape that crosses the Tauri boundary.
///
/// **Five fields, always all present** (r1.s3.w3 added `run_id`; r1.s4.w3's review
/// added `session_id`; r2.s1.w4 added `child_run_id`), so the TypeScript
/// type generated from this struct describes every error the frontend can receive and
/// one rendering path handles all of them. An ordinary error serializes
/// `run_id: null` — the field is never skipped, because a field that sometimes
/// vanishes reaches TypeScript as `undefined` while its generated type says
/// `string | null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct BusError {
    /// The family this error came from.
    pub code: BusErrorCode,
    /// The source error's `Display` rendering — prose, safe to show a user.
    pub message: String,
    /// The id of a backtest run that **is persisted** despite this error
    /// (r1.s3.w3).
    ///
    /// `Some` only when a run was saved and something afterwards could not be read
    /// back. The message says so too, but a screen must not have to parse prose to
    /// tell a user "your run is saved, here is its id" — so the id crosses the bus
    /// as a field. `None` for every other error, including a save that never
    /// committed: reporting an id for a row that does not exist would be worse than
    /// reporting none.
    pub run_id: Option<String>,
    /// The coaching session this error is ABOUT, when the caller must act on that
    /// session rather than on the one it asked under.
    ///
    /// `Some` on a `Busy` refusal that defers to a turn already running for this
    /// run — the id being deferred TO. The rail's "Check again" has to reload that
    /// session, and reloading a freshly minted id instead starts a second billable
    /// turn, which is the opposite of what a button offering the other turn's
    /// result promises. The message names it too; the same rule as `run_id` applies,
    /// that a screen must not parse prose to learn an id it has to act on.
    pub session_id: Option<String>,
    /// The run id `compare_child_run` was asked about, on a `not_found` refusal
    /// (r2.s1.w4).
    ///
    /// `Some` only when the asked-for id itself is the thing that is missing —
    /// the same rule `run_id` follows: a screen must not parse prose to learn an
    /// id it has to report back. `None` for every other error, including the
    /// comparison's other refusals, where the run exists and it is the parent
    /// (or the parent's run) that does not.
    pub child_run_id: Option<String>,
}

impl BusError {
    /// Construct a [`BusError`] directly. Prefer the `From` impls for domain errors.
    #[must_use]
    pub fn new(code: BusErrorCode, message: String) -> Self {
        Self {
            code,
            message,
            run_id: None,
            session_id: None,
            child_run_id: None,
        }
    }

    /// The same, for an error that a persisted run survives (r1.s3.w3).
    #[must_use]
    pub fn with_run_id(code: BusErrorCode, message: String, run_id: String) -> Self {
        Self {
            code,
            message,
            run_id: Some(run_id),
            session_id: None,
            child_run_id: None,
        }
    }

    /// The same, naming the coaching session the caller must act on (r1.s4.w3).
    #[must_use]
    pub fn with_session_id(code: BusErrorCode, message: String, session_id: String) -> Self {
        Self {
            code,
            message,
            run_id: None,
            session_id: Some(session_id),
            child_run_id: None,
        }
    }

    /// The same, naming the run `compare_child_run` was asked about (r2.s1.w4).
    #[must_use]
    pub fn with_child_run_id(code: BusErrorCode, message: String, child_run_id: String) -> Self {
        Self {
            code,
            message,
            run_id: None,
            session_id: None,
            child_run_id: Some(child_run_id),
        }
    }

    /// A shell-level failure that is not a domain error family.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(BusErrorCode::Internal, message.into())
    }
}

impl std::fmt::Display for BusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for BusError {}

/// Generate a `From<$err> for BusError` that carries the source's **`Display`**.
///
/// A macro rather than six hand-written impls so the "Display, never Debug" rule is
/// stated once and cannot drift between families.
macro_rules! bus_error_from {
    ($($source:ty => $code:expr),+ $(,)?) => {
        $(
            impl From<$source> for BusError {
                fn from(err: $source) -> Self {
                    Self::new($code, err.to_string())
                }
            }
        )+
    };
}

bus_error_from! {
    DataError => BusErrorCode::Data,
    ValidationErrors => BusErrorCode::Validation,
    BacktestError => BusErrorCode::Backtest,
    ExchangeError => BusErrorCode::Exchange,
    LlmError => BusErrorCode::Llm,
    ComposerError => BusErrorCode::Composer,
}

/// The application ring's backtest failures (r1.s3.w3).
///
/// Hand-written rather than macro-generated because this is the one family where the
/// mapping is not "one type, one code": the variants fan out across five existing
/// codes, and exactly one of them additionally carries a run id. **No new
/// `BusErrorCode` variant is added** — the code set is a closed discriminant every
/// existing screen already branches on, and "saved but unreadable" is a `data`
/// failure that happens to know something extra, not a new family. r2.s1's
/// `WindowEmpty` maps to `validation`, not `data`: it is a caller-correctable
/// bad-argument refusal (the MCP surface reports the same variant as
/// `field_error("window", …)`), while `data` is storage/read-back failure.
impl From<BacktestAppError> for BusError {
    fn from(err: BacktestAppError) -> Self {
        let code = match &err {
            BacktestAppError::DslInvalid(_)
            | BacktestAppError::CompileFailed(_)
            // r2.s2.w2 + round-1 fix F1: a missing or non-higher HTF selection
            // is a caller-correctable input refusal like `WindowEmpty` —
            // `validation`, and the message names `inputs.htf` (the MCP
            // surface reports the same variants as `field_error("inputs.htf", …)`).
            | BacktestAppError::HtfRequired { .. }
            | BacktestAppError::HtfNotHigher { .. }
            // r2.s2 round-2 fix G1: a different-pair HTF series refusal is the
            // same caller-correctable `inputs.htf` family — `validation`, not
            // `backtest`.
            | BacktestAppError::Engine(BacktestError::HtfPairMismatch { .. })
            | BacktestAppError::WindowEmpty { .. } => BusErrorCode::Validation,
            BacktestAppError::ExchangeFilters(_) => BusErrorCode::Exchange,
            BacktestAppError::Engine(_) => BusErrorCode::Backtest,
            BacktestAppError::Internal(_) => BusErrorCode::Internal,
            BacktestAppError::VersionNotFound(_)
            | BacktestAppError::SnapshotMissing { .. }
            | BacktestAppError::SeriesGapped { .. }
            | BacktestAppError::PreSaveRead { .. }
            | BacktestAppError::Persist(_)
            | BacktestAppError::SavedButReadBackFailed { .. } => BusErrorCode::Data,
        };
        let run_id = err.persisted_run_id().map(|id| id.as_str().to_owned());
        let message = err.to_string();
        match run_id {
            Some(run_id) => Self::with_run_id(code, message, run_id),
            None => Self::new(code, message),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{BusError, BusErrorCode};
    use crate::application::backtest::BacktestAppError;
    use crate::domain::{BacktestError, DataError, Pair, Timeframe};

    #[test]
    fn code_serializes_as_a_string_discriminant() {
        let json = serde_json::to_value(BusErrorCode::Backtest).unwrap();
        assert_eq!(json, serde_json::json!("backtest"));
    }

    #[test]
    fn display_is_the_message_so_anyhow_context_reads_cleanly() {
        let err = BusError::from(DataError::Parse("nope".to_owned()));
        assert_eq!(err.to_string(), "parse error: nope");
    }

    /// r2.s2 round-1 fix F1: a non-higher `inputs.htf` refusal maps to the
    /// caller-correctable `validation` family, exactly like `HtfRequired`, and
    /// its message names the field.
    #[test]
    fn htf_not_higher_maps_to_validation_and_names_the_field() {
        let err = BusError::from(BacktestAppError::HtfNotHigher {
            field: "inputs.htf",
            primary: Timeframe::H4,
            htf: Timeframe::M15,
        });
        assert_eq!(err.code, BusErrorCode::Validation);
        assert!(err.message.contains("inputs.htf"), "{}", err.message);
    }

    /// r2.s2 round-2 fix G1: a different-pair HTF series refusal arrives as
    /// `Engine(HtfPairMismatch)` (the request carries only one pair, so no
    /// app-layer variant exists) but is still the caller-correctable
    /// `inputs.htf` family — `validation`, not `backtest` — and the message
    /// names both pairs.
    #[test]
    fn htf_pair_mismatch_maps_to_validation() {
        let err = BusError::from(BacktestAppError::Engine(BacktestError::HtfPairMismatch {
            primary: Pair::new("BTCUSDT"),
            htf: Pair::new("ETHUSDT"),
        }));
        assert_eq!(err.code, BusErrorCode::Validation);
        assert!(err.message.contains("BTCUSDT"), "{}", err.message);
        assert!(err.message.contains("ETHUSDT"), "{}", err.message);
    }
}
