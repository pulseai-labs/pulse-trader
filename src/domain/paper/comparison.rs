//! The out-of-sample comparison (r3.s4.w4, spec §3; A8/A12/E3).
//!
//! [`comparison`] is the whole comparison as one pure function: given the
//! session row, its replayed state, the certifying walk-forward run (when the
//! graduation names one) and this build's fingerprint, it renders the live
//! mean R against the certifying run's fold-expectancy range — or the typed
//! reason there is nothing to compare.
//!
//! The range is the min..max of the folds' `mean_r` (A8); the live side is the
//! mean over the closed trades that recorded a `realized_r` (spec §2 — a
//! w3-era fill without one is not counted, so it can neither inflate nor
//! deflate `n`). Fewer than `session.min_trades` such trades is `Pending`.
//!
//! It also carries `engine_builds` (`epochs.len()` — the UI's "spans N engine
//! builds" note) and `certification_stale`: true when the certifying run's
//! fingerprint is not this build's. A stale certification is SHOWN, never
//! enforced (E3) — the comparison still renders.
//!
//! Pure: no store, no clock.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::EngineFingerprint;
use crate::domain::backtest::WalkForwardRun;
use crate::domain::paper::session::{Graduation, PaperSession};
use crate::domain::paper::state::PaperSessionState;

/// The A12 text a fixture-certified session shows instead of a comparison.
const FIXTURE_NOT_APPLICABLE: &str = "OOS comparison: n/a, certified on fixture data";

/// The text an override session shows: shadow identity still applies, but the
/// promotion named no certification to compare against.
const OVERRIDE_NOT_APPLICABLE: &str = "OOS comparison: n/a, override session; shadow identity only";

/// What the comparison found — the tagged verdict half of [`OosComparison`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ComparisonVerdict {
    /// No comparison applies; `reason` is the sentence to show.
    NotApplicable {
        /// Why there is nothing to compare.
        reason: String,
    },
    /// Fewer than `of` counted closed trades so far.
    Pending {
        /// The counted closed trades with a `realized_r`.
        n: u32,
        /// The floor (`session.min_trades`, A8).
        of: u32,
    },
    /// The live mean R sits inside the fold range.
    Within {
        /// The live mean R over the counted trades.
        live_mean_r: Decimal,
        /// The counted closed trades with a `realized_r`.
        n: u32,
        /// The certifying run's lowest fold `mean_r`.
        fold_min: Decimal,
        /// The certifying run's highest fold `mean_r`.
        fold_max: Decimal,
    },
    /// The live mean R is below the fold range.
    Below {
        /// The live mean R over the counted trades.
        live_mean_r: Decimal,
        /// The counted closed trades with a `realized_r`.
        n: u32,
        /// The certifying run's lowest fold `mean_r`.
        fold_min: Decimal,
        /// The certifying run's highest fold `mean_r`.
        fold_max: Decimal,
    },
    /// The live mean R is above the fold range.
    Above {
        /// The live mean R over the counted trades.
        live_mean_r: Decimal,
        /// The counted closed trades with a `realized_r`.
        n: u32,
        /// The certifying run's lowest fold `mean_r`.
        fold_min: Decimal,
        /// The certifying run's highest fold `mean_r`.
        fold_max: Decimal,
    },
}

/// The session's OOS comparison: the verdict plus the two notes the UI reads
/// (the engine-build span and the stale-certification badge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OosComparison {
    /// The verdict — flattened on the wire, so `status` sits beside the notes.
    #[serde(flatten)]
    pub verdict: ComparisonVerdict,
    /// How many engine builds the session has run under (`epochs.len()`).
    pub engine_builds: usize,
    /// Whether the certifying run's fingerprint is not this build's (E3:
    /// shown, never enforced).
    pub certification_stale: bool,
}

/// Render the comparison (spec §3). See the module docs for the rules.
#[must_use]
pub fn comparison(
    session: &PaperSession,
    state: &PaperSessionState,
    certifying_run: Option<&WalkForwardRun>,
    current_fingerprint: &EngineFingerprint,
) -> OosComparison {
    let engine_builds = state.epochs.len();
    let certification_stale = match (&session.graduation, certifying_run) {
        (Graduation::Certified { .. }, Some(run)) => {
            EngineFingerprint::from_stored(run.engine_fingerprint.clone()) != *current_fingerprint
        }
        // An override names no certification; nothing can be stale.
        _ => false,
    };
    let verdict = if session.oos_comparable() {
        match certifying_run {
            Some(run) => verdict_over(session, state, run),
            // A certified session whose run cannot be read has no baseline;
            // the store keeps the FK, so this is a corruption arm, not a path.
            None => ComparisonVerdict::NotApplicable {
                reason: "OOS comparison: n/a, the certifying run is unavailable".to_owned(),
            },
        }
    } else {
        match &session.graduation {
            Graduation::Certified { .. } => ComparisonVerdict::NotApplicable {
                reason: FIXTURE_NOT_APPLICABLE.to_owned(),
            },
            Graduation::Override { .. } => ComparisonVerdict::NotApplicable {
                reason: OVERRIDE_NOT_APPLICABLE.to_owned(),
            },
        }
    };
    OosComparison {
        verdict,
        engine_builds,
        certification_stale,
    }
}

/// The comparison over a readable certifying run.
fn verdict_over(
    session: &PaperSession,
    state: &PaperSessionState,
    run: &WalkForwardRun,
) -> ComparisonVerdict {
    let rs: Vec<Decimal> = state
        .closed_trades
        .iter()
        .filter_map(|trade| trade.realized_r)
        .collect();
    let n = u32::try_from(rs.len()).unwrap_or(u32::MAX);
    if n < session.min_trades {
        return ComparisonVerdict::Pending {
            n,
            of: session.min_trades,
        };
    }
    let live_mean_r = rs.iter().copied().sum::<Decimal>() / Decimal::from(n);
    let fold_min = run.folds.iter().map(|fold| fold.verdict.mean_r).min();
    let fold_max = run.folds.iter().map(|fold| fold.verdict.mean_r).max();
    let (Some(fold_min), Some(fold_max)) = (fold_min, fold_max) else {
        return ComparisonVerdict::NotApplicable {
            reason: "OOS comparison: n/a, the certifying run has no folds".to_owned(),
        };
    };
    if live_mean_r < fold_min {
        ComparisonVerdict::Below {
            live_mean_r,
            n,
            fold_min,
            fold_max,
        }
    } else if live_mean_r > fold_max {
        ComparisonVerdict::Above {
            live_mean_r,
            n,
            fold_min,
            fold_max,
        }
    } else {
        ComparisonVerdict::Within {
            live_mean_r,
            n,
            fold_min,
            fold_max,
        }
    }
}
