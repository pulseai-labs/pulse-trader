//! r4.s1.w5 (G7, C1, C4, C5) — the certification record's pure types.
//!
//! The record is the audit trail's atom (ADR-0025/0028): ONE immutable row per
//! certification call, holding what the hypothesis scored on both halves —
//! the search-span `wf-v2` verdict that made it a candidate, and the ONE
//! holdout backtest the campaign may never repeat. Whatever the outcome, the
//! call is recorded; a refused or errored call writes nothing at all, so the
//! hypothesis budget (Q2: one call = one hypothesis, H read from the OPEN
//! freeze) can never disagree with the calls that ran.
//!
//! Pure types + a typed refusal vocabulary, zero I/O like the rest of the ring.
//! The SQLite store lives in `adapters::db::certification_repo`; its two schema
//! laws (immutable rows, one `(freeze_id, hypothesis_index)`) hold there too,
//! so a raw INSERT cannot talk its way past them either.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::backtest::SnapshotSelection;
use crate::domain::strategy::VersionId;
use crate::domain::{DataError, Pair, WalkForwardRunId};

/// One side's recorded `(timeframe, data_version)` selections — the search
/// span's or the holdout's. Each side names its own triple because the two runs
/// load their snapshots independently; in practice they share the pins, and the
/// record says so rather than assuming it.
///
/// Serialized as the `certification.search_inputs` / `holdout_inputs` JSON
/// column — the `0006_backtest_inputs` precedent for recording run provenance,
/// which is exactly the shape [`SnapshotSelection`] already round-trips.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertificationInputs {
    /// The primary timeframe's exact snapshot.
    pub primary: SnapshotSelection,
    /// The higher timeframe's exact snapshot, when the run used one.
    pub htf: Option<SnapshotSelection>,
    /// The fixed daily series' exact snapshot, when the run consumed one.
    pub d1: Option<SnapshotSelection>,
}

/// One `certification` row — the record of one hypothesis.
#[derive(Debug, Clone, PartialEq)]
pub struct CertificationRecord {
    /// The row id (a UUID minted by the store).
    pub id: String,
    /// The version that was certified.
    pub version_id: VersionId,
    /// The freeze the call ran under (its H is the budget).
    pub freeze_id: String,
    /// The hypothesis's position under that freeze, `1..=H` — the budget count
    /// itself, and unique with `freeze_id`.
    pub hypothesis_index: u32,
    /// The search rule's persisted name (`"wf-v2"` for this item's step).
    pub rule: String,
    /// The pair both halves evaluated.
    pub pair: Pair,
    /// The persisted search-span walk-forward run (what the promotion gate
    /// reads its provenance and fingerprint through).
    pub search_walk_forward_run_id: WalkForwardRunId,
    /// Whether the search-span `wf-v2` verdict passed.
    pub search_pass: bool,
    /// The holdout window's inclusive start (epoch ms).
    pub holdout_start_ms: i64,
    /// The holdout window's exclusive end (epoch ms) — the primary snapshot's
    /// last candle's `close_time` (C5).
    pub holdout_end_ms: i64,
    /// The holdout's trade count (C5).
    pub holdout_n: usize,
    /// The holdout's mean expectancy in R (Decimal, NFR-2).
    pub holdout_mean_r: Decimal,
    /// The C1 test's quantile `z(1 − 0.05/H)` at this freeze's H.
    pub holdout_z: f64,
    /// The one-sided lower bound on the holdout expectancy.
    pub holdout_lower_bound: f64,
    /// Whether the C1 holdout test passed.
    pub holdout_passes: bool,
    /// `search_pass AND holdout_passes` — the schema holds the same law.
    pub certified: bool,
    /// The search span's recorded data versions, per timeframe.
    pub search_inputs: CertificationInputs,
    /// The holdout's recorded data versions, per timeframe.
    pub holdout_inputs: CertificationInputs,
    /// The search run's engine fingerprint (all folds share it).
    pub engine_fingerprint: String,
    /// When the record was written (RFC3339 UTC, the injected `Clock`).
    pub created_at: String,
    /// The calling token's label — the authenticated request context's, never a
    /// tool argument (grill Q5; the risk gate's audit trail).
    pub called_by: String,
}

impl CertificationRecord {
    /// The hypotheses the freeze's budget still allows after this record:
    /// `H − (this record's index)`, saturating at zero. `hypothesis_index` is
    /// the budget count by construction (the store mints it as
    /// `MAX(index)+1` inside the write transaction), so this needs no second
    /// read.
    #[must_use]
    pub fn hypotheses_left(&self, h: u8) -> u32 {
        u32::from(h).saturating_sub(self.hypothesis_index)
    }
}

/// What the certification step asks the store to write: everything the row
/// carries except the pieces the store mints (`id`, `created_at`) and the
/// `hypothesis_index` it derives from the freeze inside the write transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct CertificationDraft {
    /// The version that was certified.
    pub version_id: VersionId,
    /// The freeze the call ran under.
    pub freeze_id: String,
    /// The search rule's persisted name.
    pub rule: String,
    /// The pair both halves evaluated.
    pub pair: Pair,
    /// The persisted search-span walk-forward run.
    pub search_walk_forward_run_id: WalkForwardRunId,
    /// Whether the search-span verdict passed.
    pub search_pass: bool,
    /// The holdout window's inclusive start.
    pub holdout_start_ms: i64,
    /// The holdout window's exclusive end.
    pub holdout_end_ms: i64,
    /// The holdout's trade count.
    pub holdout_n: usize,
    /// The holdout's mean expectancy in R.
    pub holdout_mean_r: Decimal,
    /// The C1 test's quantile at this freeze's H.
    pub holdout_z: f64,
    /// The one-sided lower bound on the holdout expectancy.
    pub holdout_lower_bound: f64,
    /// Whether the C1 holdout test passed.
    pub holdout_passes: bool,
    /// The search span's recorded data versions.
    pub search_inputs: CertificationInputs,
    /// The holdout's recorded data versions.
    pub holdout_inputs: CertificationInputs,
    /// The search run's engine fingerprint.
    pub engine_fingerprint: String,
    /// The calling token's label.
    pub called_by: String,
}

impl CertificationDraft {
    /// `search_pass AND holdout_passes` — the one derived cell.
    #[must_use]
    pub fn certified(&self) -> bool {
        self.search_pass && self.holdout_passes
    }
}

/// The certification step's typed refusals (C4, Q2) — every one names its
/// reason, and every one means **nothing was written and no hypothesis was
/// spent**. The step returns these before it runs anything; an engine or data
/// failure after the refusals is a different error (nothing written then too,
/// and the budget untouched).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CertifyRefusal {
    /// No freeze is open, so there is no holdout to certify against and no
    /// budget to count against (C4, F1).
    #[error(
        "no certification freeze is open: the certification step requires an open freeze \
         (`pulse certify freeze`, F1) — nothing was written"
    )]
    NoOpenFreeze,
    /// The version's **lineage root** was created before the freeze opened: the
    /// campaign only certifies lineages started after the freeze (C4, Q1), so a
    /// pre-freeze strategy cannot be slipped in by refining it.
    #[error(
        "version `{}` descends from lineage root `{}`, created at {} ms — before the freeze \
         opened at {} ms (C4: only lineages rooted after the freeze may be certified) — \
         nothing was written",
        version_id.as_str(),
        root_version_id.as_str(),
        root_created_at_ms,
        freeze_opened_at_ms
    )]
    PreFreezeLineage {
        /// The version the call named.
        version_id: VersionId,
        /// The root of its `parent_version_id` chain.
        root_version_id: VersionId,
        /// When that root was created (epoch ms).
        root_created_at_ms: i64,
        /// When the freeze opened (epoch ms).
        freeze_opened_at_ms: i64,
    },
    /// The freeze's `H` certifications are already recorded: one call is one
    /// hypothesis and the `(H+1)`th is refused by name (Q2).
    #[error(
        "the freeze's hypothesis budget is spent: all {h} hypotheses are recorded — \
         nothing was written"
    )]
    HypothesisBudgetSpent {
        /// The freeze's budget `H`.
        h: u8,
    },
}

/// The certification step's errors: a typed refusal, a missing version, or a
/// failure from one of the use cases it composes. Every arm means **nothing was
/// written and no hypothesis was spent** — the record write is the step's last
/// action.
#[derive(Debug, thiserror::Error)]
pub enum CertifyError {
    /// One of the step's typed refusals (C4, Q2).
    #[error(transparent)]
    Refused(#[from] CertifyRefusal),
    /// No such version.
    #[error("no such strategy version `{}`", .0.as_str())]
    VersionNotFound(VersionId),
    /// A repository read or the record write failed.
    #[error(transparent)]
    Store(#[from] DataError),
    /// The search-span walk-forward failed (a missing snapshot, a gapped
    /// series, an engine error): nothing was written and nothing counts.
    #[error("the search-span walk-forward failed before any record was written: {0}")]
    WalkForward(#[from] crate::application::walk_forward::WalkForwardAppError),
    /// The holdout backtest failed (a missing snapshot, a gapped series, an
    /// engine error): nothing was written and nothing counts.
    #[error("the holdout backtest failed before any record was written: {0}")]
    Backtest(#[from] crate::application::backtest::BacktestAppError),
    /// A defect in this layer (a lineage cycle, a failed task join).
    #[error("internal: {0}")]
    Internal(String),
}
