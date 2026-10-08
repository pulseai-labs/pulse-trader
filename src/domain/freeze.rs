//! r4.s1.w4 (F1, C4) — the certification-freeze domain types.
//!
//! The freeze is the record that makes the holdout something the tools
//! enforce: the operator opens one with `pulse certify freeze` (holdout start,
//! hypothesis budget H, the measured alpha, the C1 holdout test's name and z
//! rule), every entry point's holdout guard reads the OPEN row's holdout start,
//! and `pulse certify close-freeze` closes it once at the campaign's end. A
//! spent holdout is never reused (F1), so a later freeze must start its
//! holdout strictly after every earlier close.
//!
//! Pure types + a typed refusal vocabulary, zero I/O like the rest of the ring.
//! The SQLite store lives in `adapters::db::certification_freeze_repo`; its
//! schema triggers hold the same two laws the store names here, so a raw
//! INSERT cannot talk its way past them either.

use crate::domain::DataError;

/// One `certification_freeze` row — the record of one holdout freeze, open
/// (`closed_at_ms == None`) or closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreezeRecord {
    /// The row id (a UUID minted by the store).
    pub id: String,
    /// The holdout's inclusive start, epoch ms — what the guard clamps to and
    /// refuses past.
    pub holdout_start_ms: i64,
    /// The hypothesis budget H (`1..=12`) the campaign runs under.
    pub h: u8,
    /// The measured alpha, a normalized decimal string (Decimal-as-TEXT, NFR-2).
    pub alpha: String,
    /// The C1 holdout test's name and z rule, verbatim.
    pub holdout_test: String,
    /// When the freeze was opened, epoch ms (the injected `Clock`).
    pub opened_at_ms: i64,
    /// When the freeze was closed, epoch ms; `None` while open.
    pub closed_at_ms: Option<i64>,
}

impl FreezeRecord {
    /// Whether this freeze is still open — the guard is active only while one is.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.closed_at_ms.is_none()
    }

    /// The holdout guard's view of this record: the holdout start, and nothing
    /// else. The guard never reads the budget or alpha — those are the
    /// certification step's (w5) and the ADR's business.
    #[must_use]
    pub fn holdout(&self) -> HoldoutFreeze {
        HoldoutFreeze {
            holdout_start_ms: self.holdout_start_ms,
        }
    }
}

/// The open freeze as the holdout guard consumes it.
///
/// A small value type on purpose: the guard is one application-layer decision
/// over `(pair, requested window, holdout start)`, and it is wired into every
/// entry point by passing this — `None` means no freeze is open (or the caller
/// is an exemption, grill Q4), which makes the guard inert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldoutFreeze {
    /// The holdout's inclusive start, epoch ms.
    pub holdout_start_ms: i64,
}

/// What `pulse certify freeze` asks the store to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenFreezeRequest {
    /// The holdout's inclusive start, epoch ms (the CLI floors the date to UTC
    /// midnight).
    pub holdout_start_ms: i64,
    /// The hypothesis budget H; the store refuses anything outside `1..=12`.
    pub h: u8,
    /// The measured alpha as a decimal string (already parsed by the caller).
    pub alpha: String,
    /// The C1 holdout test's name and z rule, verbatim.
    pub holdout_test: String,
}

/// The freeze store's typed refusals — the two laws F1 installs, the budget
/// bound, and the wrapped [`DataError`].
///
/// The schema holds the same laws by trigger/index, so these are the *named*
/// half of a refusal that would otherwise surface as an opaque constraint
/// error; the store checks them first so the operator reads why.
#[derive(Debug, thiserror::Error)]
pub enum FreezeStoreError {
    /// A freeze is already open — at most one may be (the schema's partial
    /// unique index says so too).
    #[error(
        "a freeze is already open (opened at {opened_at_ms} ms); close it before opening another"
    )]
    FreezeOpen {
        /// The open freeze's `opened_at_ms`.
        opened_at_ms: i64,
    },
    /// The new holdout would start at or before the last freeze's close — a
    /// spent holdout is never reused (F1).
    #[error(
        "holdout start {holdout_start_ms} ms is not later than the last freeze's close \
         {last_closed_at_ms} ms — a spent holdout is never reused (F1)"
    )]
    HoldoutStartNotAfterLastClose {
        /// The requested holdout start.
        holdout_start_ms: i64,
        /// The latest earlier `closed_at_ms`.
        last_closed_at_ms: i64,
    },
    /// The hypothesis budget is outside `1..=12` (the Q2 rule; lower-only).
    #[error("H = {h} is outside the legal 1..=12 hypothesis budget")]
    HOutOfRange {
        /// The requested budget.
        h: u8,
    },
    /// `close-freeze` was asked to close when nothing is open.
    #[error("no freeze is open")]
    NoOpenFreeze,
    /// A database error underneath the refusal.
    #[error(transparent)]
    Db(#[from] DataError),
}
