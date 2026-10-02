//! The paper-session aggregate's row-facing value types (r3.s4.w2, ADR-0027).
//!
//! A live paper session is one `paper_session` row (this module) plus an
//! append-only `paper_event` log (`event.rs`, replay in `state.rs`). The row
//! is written ONCE, at promotion — every column is immutable by trigger
//! (`0018`), so the type has no mutation surface and the graduation is a
//! two-variant sum: `Certified` names the certifying walk-forward run and the
//! exact data versions it certified; `Override` records a human's reasoned
//! manual promotion, which is never a live basis (A5) and never
//! OOS-comparable (A12).
//!
//! Pure types only: nothing here reads the store or the clock.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::backtest::WalkForwardRunId;
use crate::domain::strategy::VersionId;
use crate::domain::{DataVersion, EngineFingerprint, Pair, Timeframe};

/// The session's opening equity, in USDT (A1 — every session, no exceptions).
pub const STARTING_EQUITY_USDT: i64 = 10_000;

/// The session's taker fee, in basis points (A1).
pub const TAKER_FEE_BPS: i64 = 4;

/// The session's adverse-fill slippage, in basis points (A1).
pub const SLIPPAGE_BPS: i64 = 1;

/// The trade count the OOS COMPARISON waits for (SPINE A8/A12): a session
/// shows "pending N of 20" until 20 closed trades, and 20 is wf-v1's
/// `N_MIN`. It is not a shadow floor — the shadow identity check applies to
/// every session (fixture and override included) regardless of trade count.
/// Stored in w2 for w4's comparison.
pub const MIN_TRADES: u32 = 20;

/// Text that must not be empty or whitespace-only: an override's reason, a
/// promoting client token's label. The refusal is the TYPE's — serde's
/// `try_from` hook re-runs it at every deserialization boundary, so an empty
/// payload field fails the same way an empty constructor argument does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct NonEmptyText(String);

/// Why [`NonEmptyText::try_new`] refused a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmptyTextError;

impl core::fmt::Display for EmptyTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "text must not be empty or whitespace-only")
    }
}

impl std::error::Error for EmptyTextError {}

impl NonEmptyText {
    /// Accepts any string with at least one non-whitespace character.
    ///
    /// # Errors
    ///
    /// [`EmptyTextError`] on empty or whitespace-only input.
    pub fn try_new(text: &str) -> Result<Self, EmptyTextError> {
        if text.trim().is_empty() {
            Err(EmptyTextError)
        } else {
            Ok(Self(text.to_owned()))
        }
    }

    /// The accepted text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NonEmptyText {
    type Error = EmptyTextError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::try_new(&text)
    }
}

/// An override promotion's reason (spec §4.2) — a [`NonEmptyText`] by domain.
pub type NonEmptyReason = NonEmptyText;

/// The promoting client token's label (`0016`'s `client_token.label`, audit
/// #6) — a [`NonEmptyText`] by domain, and a `length(trim(..)) > 0` CHECK by
/// table.
pub type NonEmptyLabel = NonEmptyText;

/// One certified `(timeframe, data_version)` pair — the session's
/// `certified_data_versions` JSON entries, and a `shadow_checked` payload's
/// version list entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertifiedDataVersion {
    /// The timeframe the version belongs to.
    pub timeframe: Timeframe,
    /// The content-hash `data_version`.
    pub data_version: DataVersion,
}

/// How the session was promoted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "graduation", rename_all = "snake_case")]
pub enum Graduation {
    /// Promoted by a passing walk-forward on the current build: names the
    /// run and the exact `(timeframe, data_version)` pairs its fold runs
    /// consumed, in fold order.
    Certified {
        /// The certifying `walk_forward_run`.
        walk_forward_run_id: WalkForwardRunId,
        /// The distinct certified versions, in fold order.
        data_versions: Vec<CertifiedDataVersion>,
    },
    /// Promoted by a human override: a non-empty reason and the instant it
    /// was taken (from the injected clock). Never a live basis (A5).
    Override {
        /// Why the human overrode.
        reason: NonEmptyReason,
        /// The RFC3339 instant of the override decision.
        at: String,
    },
}

/// Identifier of a persisted paper session — a UUID-hyphenated TEXT primary
/// key minted by the adapter.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PaperSessionId(String);

impl PaperSessionId {
    /// Wrap an id string (adapter-minted).
    #[must_use]
    pub fn new(id: String) -> Self {
        Self(id)
    }

    /// The id as its raw string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for PaperSessionId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The aggregate's row mirror. Every field maps one `paper_session` column;
/// the row is immutable by trigger, so the type is read-only by design.
#[derive(Debug, Clone, PartialEq)]
pub struct PaperSession {
    /// The row id.
    pub id: PaperSessionId,
    /// The insertion sequence (`MAX(seq)+1` in the write tx).
    pub seq: i64,
    /// The promoted strategy version.
    pub strategy_version_id: VersionId,
    /// The promotion instant (RFC3339, injected clock).
    pub created_at: String,
    /// The traded pair.
    pub pair: Pair,
    /// The primary timeframe.
    pub primary_timeframe: Timeframe,
    /// The higher timeframe, when the certified inputs used one.
    pub htf_timeframe: Option<Timeframe>,
    /// Whether the certified inputs consumed the fixed daily series.
    pub uses_d1: bool,
    /// The opening equity, USDT (A1).
    pub starting_equity: Decimal,
    /// The taker fee, bps (A1).
    pub taker_fee_bps: Decimal,
    /// The slippage, bps (A1).
    pub slippage_bps: Decimal,
    /// The engine fingerprint at start (E3: the first epoch).
    pub engine_fingerprint: EngineFingerprint,
    /// How the session was promoted.
    pub graduation: Graduation,
    /// Whether every certified data version is a `fixture_snapshot` row.
    pub fixture: bool,
    /// The OOS comparison floor (A8/A12, stored for w4) — not a shadow
    /// floor; shadow identity applies regardless.
    pub min_trades: u32,
    /// The promoting client token's label (audit #6).
    pub promoted_by: NonEmptyLabel,
}

impl PaperSession {
    /// Whether this session may be compared out-of-sample (A12): ONLY a
    /// non-fixture certified session. An override session or a
    /// fixture-certified one has no OOS comparison — w4 renders "OOS
    /// comparison: n/a" for both.
    #[must_use]
    pub fn oos_comparable(&self) -> bool {
        matches!(self.graduation, Graduation::Certified { .. }) && !self.fixture
    }
}

/// What the repository persists: a [`PaperSession`] minus the columns the
/// adapter mints (`id`, `seq`, `created_at`).
#[derive(Debug, Clone, PartialEq)]
pub struct PaperSessionDraft {
    /// The promoted strategy version.
    pub strategy_version_id: VersionId,
    /// The traded pair.
    pub pair: Pair,
    /// The primary timeframe.
    pub primary_timeframe: Timeframe,
    /// The higher timeframe, when the promotion used one.
    pub htf_timeframe: Option<Timeframe>,
    /// Whether the promotion consumed the fixed daily series.
    pub uses_d1: bool,
    /// The opening equity, USDT (A1).
    pub starting_equity: Decimal,
    /// The taker fee, bps (A1).
    pub taker_fee_bps: Decimal,
    /// The slippage, bps (A1).
    pub slippage_bps: Decimal,
    /// The engine fingerprint at start.
    pub engine_fingerprint: EngineFingerprint,
    /// How the session was promoted.
    pub graduation: Graduation,
    /// Whether every certified data version is a `fixture_snapshot` row.
    pub fixture: bool,
    /// The OOS comparison floor (A8/A12, stored for w4).
    pub min_trades: u32,
    /// The promoting client token's label (audit #6).
    pub promoted_by: NonEmptyLabel,
}
