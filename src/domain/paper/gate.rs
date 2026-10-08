//! The promotion gate (r3.s4.w2, E2 — a refinement of ADR-0025's
//! certification seam).
//!
//! [`decide_promotion`] is the whole gate as one pure function: given the
//! version, its latest walk-forward run (if any), the fold runs' recorded
//! inputs, this build's fingerprint, an optional human override and the
//! promoting token's label, it either returns the promotion draft the
//! repository persists — or refuses, typed. It never reads the store or the
//! clock: the inputs arrive resolved (the application ring's job), and an
//! override carries its own `at` instant, read from the injected clock by the
//! caller before this function runs.
//!
//! The two graduation paths (spec §4):
//!
//! 1. **Certified** — the latest walk-forward run passed on THIS build. A
//!    foreign fingerprint refuses (`CertifiedUnderOtherEngine`), override or
//!    not: re-running the walk-forward is the only re-certification (E2). A
//!    fold run with unreadable inputs refuses (`CertificationUnreadable`) —
//!    a certification whose data provenance cannot be named is not a
//!    certification. Otherwise the session's certified data versions are the
//!    distinct `(timeframe, data_version)` pairs over every fold's
//!    `primary`/`htf`/`d1`, in fold order, and the session's timeframes come
//!    from those same inputs (A10).
//! 2. **Override** — no passing run: without an override the promotion
//!    refuses (`Uncertified`); with one, the session promotes as
//!    `Graduation::Override`.
//!
//! Every session gets the A1 settings (10,000 USDT, 4 bps taker, 1 bps
//! slippage, `min_trades = 20`). The `fixture` flag is NOT decided here — it
//! names a store fact (every certified version is a `fixture_snapshot` row),
//! so the application ring derives it after this function and sets it on the
//! draft it persists.

use rust_decimal::Decimal;

use crate::domain::backtest::{BacktestInputs, WalkForwardRun};
use crate::domain::certification::CertificationRecord;
use crate::domain::paper::session::{
    CertifiedDataVersion, Graduation, MIN_TRADES, NonEmptyLabel, NonEmptyReason, SLIPPAGE_BPS,
    STARTING_EQUITY_USDT, TAKER_FEE_BPS,
};
use crate::domain::strategy::StrategyVersion;
use crate::domain::{EngineFingerprint, Pair, Timeframe, VerdictRule};

/// An override promotion request: the human's non-empty reason, the instant
/// it was taken (RFC3339, from the injected clock — the caller stamps it, the
/// gate stays pure), and the session shape the request names (pair and
/// timeframes; spec §4.2 "the request names the primary and HTF timeframes").
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionOverride {
    /// Why the human overrode the gate.
    pub reason: NonEmptyReason,
    /// The RFC3339 instant of the override decision.
    pub at: String,
    /// The pair the session trades.
    pub pair: Pair,
    /// The session's primary timeframe.
    pub primary_timeframe: Timeframe,
    /// The session's higher timeframe, when the request names one.
    pub htf_timeframe: Option<Timeframe>,
    /// Whether the session consumes the fixed daily series.
    pub uses_d1: bool,
}

/// The promotion the gate accepted, ready for the repository to persist.
/// Carries the A1 session settings; the adapter mints `id`/`seq`/`created_at`
/// and the application derives `fixture`.
#[derive(Debug, Clone, PartialEq)]
pub struct PromotionDraft {
    /// The traded pair (A10: the certified inputs').
    pub pair: Pair,
    /// The primary timeframe (A10).
    pub primary_timeframe: Timeframe,
    /// The higher timeframe, when the certified inputs used one (A10).
    pub htf_timeframe: Option<Timeframe>,
    /// Whether the certified inputs consumed the fixed daily series (A10).
    pub uses_d1: bool,
    /// The opening equity (A1).
    pub starting_equity: Decimal,
    /// The taker fee, bps (A1).
    pub taker_fee_bps: Decimal,
    /// The slippage, bps (A1).
    pub slippage_bps: Decimal,
    /// The engine fingerprint at start — the build that made the decision.
    pub engine_fingerprint: EngineFingerprint,
    /// How the session was promoted.
    pub graduation: Graduation,
    /// The OOS comparison floor (A8/A12, stored for w4).
    pub min_trades: u32,
    /// The promoting client token's label (audit #6).
    pub promoted_by: NonEmptyLabel,
}

/// Why the gate refused a promotion. Typed at the domain; w4 maps these onto
/// the API's error surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromotionRefused {
    /// No walk-forward run, or the latest one did not pass, and no override
    /// was supplied.
    Uncertified,
    /// The certifying run passed under a DIFFERENT engine fingerprint. The
    /// override cannot bypass this (E2): re-run the walk-forward.
    CertifiedUnderOtherEngine {
        /// The fingerprint the version was certified under.
        certified_under: EngineFingerprint,
        /// This build's fingerprint.
        current: EngineFingerprint,
    },
    /// A fold run's recorded inputs are missing or disagree across folds —
    /// the certification's data provenance cannot be named.
    CertificationUnreadable,
}

impl core::fmt::Display for PromotionRefused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Uncertified => write!(
                f,
                "strategy version is not certified: its latest walk-forward run did not pass"
            ),
            Self::CertifiedUnderOtherEngine {
                certified_under,
                current,
            } => write!(
                f,
                "certified under engine {}; re-run walk-forward to re-certify (this build is {})",
                certified_under.as_str(),
                current.as_str()
            ),
            Self::CertificationUnreadable => write!(
                f,
                "the certifying walk-forward's fold inputs are unreadable; the certified data \
                 versions cannot be named"
            ),
        }
    }
}

/// Whether the inputs' shape (pair, timeframes, d1) is identical across every
/// fold — a certification whose folds ran different inputs is unreadable.
fn common_inputs<'a>(
    fold_inputs: &'a [Option<&'a BacktestInputs>],
) -> Result<&'a BacktestInputs, PromotionRefused> {
    let first = fold_inputs
        .first()
        .copied()
        .flatten()
        .ok_or(PromotionRefused::CertificationUnreadable)?;
    let consistent = fold_inputs.iter().all(|input| {
        input.is_some_and(|i| {
            i.pair == first.pair
                && i.primary.timeframe == first.primary.timeframe
                && i.primary.data_version == first.primary.data_version
                && i.htf.as_ref().map(|h| h.timeframe) == first.htf.as_ref().map(|h| h.timeframe)
                && i.htf.as_ref().map(|h| &h.data_version)
                    == first.htf.as_ref().map(|h| &h.data_version)
                && i.d1.as_ref().map(|d| d.timeframe) == first.d1.as_ref().map(|d| d.timeframe)
                && i.d1.as_ref().map(|d| &d.data_version)
                    == first.d1.as_ref().map(|d| &d.data_version)
        })
    });
    if consistent {
        Ok(first)
    } else {
        Err(PromotionRefused::CertificationUnreadable)
    }
}

/// Collect the distinct certified `(timeframe, data_version)` pairs over
/// every fold's primary/htf/d1, in fold order (first occurrence wins).
fn certified_versions(fold_inputs: &[Option<&BacktestInputs>]) -> Vec<CertifiedDataVersion> {
    let mut out: Vec<CertifiedDataVersion> = Vec::new();
    let push = |selection: &crate::domain::SnapshotSelection, out: &mut Vec<_>| {
        let candidate = CertifiedDataVersion {
            timeframe: selection.timeframe,
            data_version: selection.data_version.clone(),
        };
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    };
    for input in fold_inputs.iter().filter_map(|i| *i) {
        push(&input.primary, &mut out);
        if let Some(htf) = &input.htf {
            push(htf, &mut out);
        }
        if let Some(d1) = &input.d1 {
            push(d1, &mut out);
        }
    }
    out
}

/// The promotion gate (spec §4). See the module docs for the two paths.
///
/// **`certification` is the version's certified record, when it has one**
/// (r4.s1.w5, spec A5). A `wf-v2` run's pass is a SEARCH-span verdict, and the
/// campaign's candidates are tuned towards it — so it certifies nothing by
/// itself: a `wf-v2` run promotes as `Certified` only when `certification`
/// names that very run and says `certified`. The `wf-v1` path is unchanged
/// (today's rule, kept for the fixture and older lineages), and so is the
/// override path.
///
/// # Errors
///
/// [`PromotionRefused::Uncertified`] when there is no run that certifies (a
/// failing run, or a `wf-v2` run with no certified record behind it) and no
/// override, [`PromotionRefused::CertifiedUnderOtherEngine`] when the
/// certifying run predates this build (with or without an override), and
/// [`PromotionRefused::CertificationUnreadable`] when a fold run's inputs are
/// missing or disagree across folds.
pub fn decide_promotion(
    _version: &StrategyVersion,
    certifying_run: Option<&WalkForwardRun>,
    certification: Option<&CertificationRecord>,
    fold_inputs: &[Option<&BacktestInputs>],
    current_fingerprint: &EngineFingerprint,
    promotion_override: Option<PromotionOverride>,
    promoted_by: NonEmptyLabel,
) -> Result<PromotionDraft, PromotionRefused> {
    // A run certifies when it passed AND the version carries the certification
    // that names it: `wf-v1` runs are their own certification (the pre-wf-v2
    // rule), while a `wf-v2` search-span pass needs the record.
    let certifying = certifying_run
        .filter(|run| run.verdict.pass)
        .filter(|run| match run.rule {
            VerdictRule::WfV1 => true,
            VerdictRule::WfV2 => certification.is_some_and(|record| {
                record.certified && record.search_walk_forward_run_id == run.id
            }),
        });
    if let Some(run) = certifying {
        let certified_under = EngineFingerprint::from_stored(run.engine_fingerprint.clone());
        if certified_under != *current_fingerprint {
            return Err(PromotionRefused::CertifiedUnderOtherEngine {
                certified_under,
                current: current_fingerprint.clone(),
            });
        }
        let inputs = common_inputs(fold_inputs)?;
        let data_versions = certified_versions(fold_inputs);
        return Ok(PromotionDraft {
            pair: inputs.pair.clone(),
            primary_timeframe: inputs.primary.timeframe,
            htf_timeframe: inputs.htf.as_ref().map(|h| h.timeframe),
            uses_d1: inputs.d1.is_some(),
            starting_equity: Decimal::from(STARTING_EQUITY_USDT),
            taker_fee_bps: Decimal::from(TAKER_FEE_BPS),
            slippage_bps: Decimal::from(SLIPPAGE_BPS),
            engine_fingerprint: current_fingerprint.clone(),
            graduation: Graduation::Certified {
                walk_forward_run_id: run.id.clone(),
                data_versions,
            },
            min_trades: MIN_TRADES,
            promoted_by,
        });
    }
    if let Some(request) = promotion_override {
        let PromotionOverride {
            reason,
            at,
            pair,
            primary_timeframe,
            htf_timeframe,
            uses_d1,
        } = request;
        return Ok(PromotionDraft {
            pair,
            primary_timeframe,
            htf_timeframe,
            uses_d1,
            starting_equity: Decimal::from(STARTING_EQUITY_USDT),
            taker_fee_bps: Decimal::from(TAKER_FEE_BPS),
            slippage_bps: Decimal::from(SLIPPAGE_BPS),
            engine_fingerprint: current_fingerprint.clone(),
            graduation: Graduation::Override { reason, at },
            min_trades: MIN_TRADES,
            promoted_by,
        });
    }
    Err(PromotionRefused::Uncertified)
}
