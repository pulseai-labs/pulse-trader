//! Walk-forward as a run kind (r2.s3.w3 — `rolling-oos/v1` + `wf-v1`, ADR-0025).
//!
//! A strategy version is **walked forward**: its counted span `[from, to)` is
//! cut into K equal-length contiguous out-of-sample folds ([`fold_windows`]),
//! each fold runs as an ordinary persisted windowed backtest with full-history
//! lead-in, and the run is judged by the versioned rule [`VerdictRule::WfV1`]:
//! a fold holds when it has at least [`N_MIN`] trades and its expectancy lower
//! bound in R is strictly above zero; the run passes when at least
//! `⌈2K/3⌉` folds hold and the pooled lower bound over every out-of-sample
//! trade is above zero.
//!
//! **The constants are the rule.** `Z = 1.645`, `N_MIN = 20`,
//! `folds_required(k) = ⌈2k/3⌉` are pinned by `tests/walk_forward_verdict.rs`;
//! changing any of them is a new rule name (`wf-v2`), not an edit.
//!
//! **Money-math discipline (the `stats.rs` quarantine).** `FoldVerdict`'s mean
//! and sample variance accumulate in `Decimal` (byte-exact); `variance / n` is
//! converted to `f64` exactly once, then `sqrt`ed once — the only
//! transcendental this file uses, the same single-`sqrt` shape as
//! `sharpe_sortino`. No `f64` arithmetic precedes that conversion.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::run::{BacktestRunId, CandleWindow};
use super::stats::decimal_to_f64;
use crate::domain::strategy::VersionId;

/// The lowest fold count a walk-forward accepts (L4).
pub const K_MIN: u8 = 2;
/// The highest fold count a walk-forward accepts (L4).
pub const K_MAX: u8 = 12;
/// The default fold count when a request does not name one.
pub const K_DEFAULT: u8 = 6;

/// The fold scheme — how the counted span becomes folds. `rolling-oos/v1` is
/// the only scheme this version of the rule knows: K equal-length contiguous
/// out-of-sample windows, evaluated in order, never re-fit. A scheme that
/// re-anchors, nests in-sample segments, or overlaps folds is a different name.
///
/// Serde: internally tagged so the persisted/serialized form carries the
/// pinned name beside `k` (`{"name":"rolling-oos/v1","k":6}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "name")]
pub enum FoldScheme {
    /// `rolling-oos/v1` — K contiguous equal-length OOS folds.
    #[serde(rename = "rolling-oos/v1")]
    RollingOos {
        /// The fold count, in `2..=12` (validated by [`FoldScheme::new`]).
        k: u8,
    },
}

impl FoldScheme {
    /// Build the scheme, refusing a `k` outside `2..=12` (L4).
    ///
    /// # Errors
    ///
    /// [`WalkForwardError::KOutOfRange`] naming the bounds.
    pub fn rolling_oos(k: u8) -> Result<Self, WalkForwardError> {
        if !(K_MIN..=K_MAX).contains(&k) {
            return Err(WalkForwardError::KOutOfRange {
                k,
                min: K_MIN,
                max: K_MAX,
            });
        }
        Ok(Self::RollingOos { k })
    }

    /// The versioned scheme name persisted on the run (`scheme` TEXT column).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::RollingOos { .. } => "rolling-oos/v1",
        }
    }

    /// The fold count the scheme carries.
    #[must_use]
    pub fn k(&self) -> u8 {
        match self {
            Self::RollingOos { k } => *k,
        }
    }
}

/// The verdict rule — how fold outcomes become a run verdict. `wf-v1` is the
/// only rule: its constants are the rule, so a changed constant is a new name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerdictRule {
    /// `wf-v1` — `n >= 20` and `lower_bound > 0` per fold; `⌈2K/3⌉` folds and a
    /// positive pooled lower bound for the run.
    #[serde(rename = "wf-v1")]
    WfV1,
}

impl VerdictRule {
    /// The versioned rule name persisted on the run (`rule` TEXT column).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::WfV1 => "wf-v1",
        }
    }
}

/// `wf-v1`'s one-sided confidence constant (≈ the 95% z-score).
pub const Z: f64 = 1.645;
/// `wf-v1`'s minimum per-fold trade count.
pub const N_MIN: usize = 20;

/// `wf-v1`'s required holding-fold count: `⌈2k/3⌉`.
#[must_use]
pub fn folds_required(k: u8) -> u8 {
    (2 * k).div_ceil(3)
}

/// Why a walk-forward argument refused (the domain-level taxonomy; the
/// application layer adds the request/persist failures around it).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WalkForwardError {
    /// `k` fell outside `2..=12`.
    #[error("the fold count must be in {min}..={max}, got {k}")]
    KOutOfRange {
        /// The refused fold count.
        k: u8,
        /// The lowest legal count.
        min: u8,
        /// The highest legal count.
        max: u8,
    },
}

/// Cut the counted span into `k` contiguous half-open windows of equal length
/// (`len = (to − from) / k`, integer division; **the last fold absorbs the
/// remainder**). The union of the returned windows is exactly `[from, to)` and
/// each is non-empty in milliseconds (`step >= 1` is guaranteed by `k <= 12`
/// versus any span the caller has already proven holds candles — a span under
/// `k` milliseconds wide is refused by [`CandleWindow`]'s own construction or
/// degenerates to `step == 0`, which this function refuses to emit by falling
/// through to the last fold's remainder).
///
/// The caller validates `k` through [`FoldScheme::rolling_oos`] before calling;
/// this function is total for `k >= 1` and a valid `span`.
#[must_use]
pub fn fold_windows(span: &CandleWindow, k: u8) -> Vec<CandleWindow> {
    let k64 = i64::from(k);
    let step = (span.to_ms - span.from_ms) / k64;
    let mut folds = Vec::with_capacity(usize::from(k));
    for i in 0..k64 {
        let from_ms = span.from_ms + i * step;
        let to_ms = if i == k64 - 1 {
            span.to_ms
        } else {
            span.from_ms + (i + 1) * step
        };
        folds.push(CandleWindow { from_ms, to_ms });
    }
    folds
}

/// `wf-v1`'s per-fold verdict over one fold's `realized_r` series.
///
/// `PartialEq` (not `Eq`): `lower_bound` is the single `f64` the money-math
/// quarantine permits, exactly as `SummaryStats::sharpe`/`sortino` are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FoldVerdict {
    /// The number of out-of-sample trades the verdict saw.
    pub n: usize,
    /// `Σ rᵢ / n` in `Decimal` (byte-exact; `0` when `n < 2`).
    pub mean_r: Decimal,
    /// `mean − 1.645 · sqrt(var / n)` — the one-sided lower bound on the
    /// expectancy in R. `0.0` when `n < 2` (the bound is undefined, and
    /// undefined does not hold).
    pub lower_bound: f64,
    /// `n >= 20 && lower_bound > 0.0` — a lower bound of exactly zero does not
    /// hold (the bound must be STRICTLY above zero).
    pub holds: bool,
}

impl FoldVerdict {
    /// Assess one fold's trades under `wf-v1`.
    ///
    /// The mean and the **sample variance (Bessel `N−1`)** accumulate in
    /// `Decimal`; `variance / n` converts to `f64` once and is `sqrt`ed once —
    /// `lower_bound = decimal_to_f64(mean) − 1.645 · sqrt(var/n)`. `n < 2`
    /// yields `mean_r = 0`, `lower_bound = 0.0`, `holds = false`.
    #[must_use]
    pub fn from_rs(rs: &[Decimal]) -> Self {
        let n = rs.len();
        if n < 2 {
            return Self {
                n,
                mean_r: Decimal::ZERO,
                lower_bound: 0.0,
                holds: false,
            };
        }
        let n_dec = Decimal::from(n);
        let sum: Decimal = rs.iter().copied().sum();
        let mean = sum / n_dec;
        let mut variance_num = Decimal::ZERO;
        for r in rs {
            let dev = *r - mean;
            variance_num += dev * dev;
        }
        let sample_var = variance_num / (n_dec - Decimal::ONE);
        let var_over_n = sample_var / n_dec;
        let standard_error = decimal_to_f64(var_over_n).sqrt();
        let lower_bound = decimal_to_f64(mean) - Z * standard_error;
        Self {
            n,
            mean_r: mean,
            lower_bound,
            holds: n >= N_MIN && lower_bound > 0.0,
        }
    }
}

/// `wf-v1`'s run verdict: the per-fold tallies plus the pooled bound over every
/// out-of-sample trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunVerdict {
    /// How many of the run's folds held.
    pub folds_holding: u8,
    /// `⌈2K/3⌉` — how many must hold for the run to pass.
    pub folds_required: u8,
    /// [`FoldVerdict::from_rs`] over every fold's trades concatenated in fold
    /// order (L5).
    pub pooled: FoldVerdict,
    /// `folds_holding >= folds_required && pooled.holds`.
    pub pass: bool,
}

impl RunVerdict {
    /// Assess a finished run's folds under `wf-v1`. `folds` is the per-fold
    /// verdicts in fold order; `pooled_rs` is every fold's `realized_r`
    /// concatenated in fold order (L5).
    #[must_use]
    pub fn assess(folds: &[FoldVerdict], pooled_rs: &[Decimal]) -> Self {
        // `u8` saturation can only engage past 255 folds — a scheme cap of 12
        // makes it unreachable, and saturating still fails the run honestly.
        let holding = u8::try_from(folds.iter().filter(|f| f.holds).count()).unwrap_or(u8::MAX);
        let required = folds_required(u8::try_from(folds.len()).unwrap_or(u8::MAX));
        let pooled = FoldVerdict::from_rs(pooled_rs);
        Self {
            folds_holding: holding,
            folds_required: required,
            pass: holding >= required && pooled.holds,
            pooled,
        }
    }
}

/// Identifier of a persisted [`WalkForwardRun`] — a `#[serde(transparent)]`
/// `String` newtype (mirror [`BacktestRunId`]); the adapter mints the
/// UUID-hyphenated value that is the `walk_forward_run.id` TEXT primary key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WalkForwardRunId(String);

impl WalkForwardRunId {
    /// Wrap a raw (adapter-generated) id string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying id string (for SQL binding).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A persisted fold row's typed projection: its window, the ordinary
/// `backtest_run` it ran as, and the `wf-v1` verdict recorded against it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForwardFold {
    /// The fold's position in the scheme (`0..k`).
    pub index: u8,
    /// The counted window the fold's run covered.
    pub window: CandleWindow,
    /// The persisted `backtest_run` this fold ran as.
    pub backtest_run_id: BacktestRunId,
    /// The verdict recorded on the fold row.
    pub verdict: FoldVerdict,
}

/// The walk-forward membership a `backtest_run` carries (0013's two columns):
/// the parent run and the fold's position in it. Both are present or neither
/// is — the `0013` pair trigger refuses a half-set row, and a run that is not a
/// fold carries `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalkForwardMembership {
    /// The parent `walk_forward_run` this run is a fold of.
    pub run_id: WalkForwardRunId,
    /// The run's position in that parent's scheme (`0..k`).
    pub fold_index: u8,
}

/// The persisted walk-forward run: identity + provenance (scheme, rule, span,
/// whether `from` was defaulted, the shared engine fingerprint) + the recorded
/// `wf-v1` verdict + its fold rows in `fold_index` order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WalkForwardRun {
    /// The run's opaque id.
    pub id: WalkForwardRunId,
    /// The `strategy_version` this run was produced against (FK).
    pub strategy_version_id: VersionId,
    /// The injected-Clock run timestamp (RFC 3339 UTC ms text on the column).
    pub created_at: String,
    /// The scheme the folds were cut under.
    pub scheme: FoldScheme,
    /// The rule the verdicts were judged under.
    pub rule: VerdictRule,
    /// The counted span the folds cover (union of their windows).
    pub span: CandleWindow,
    /// Whether `span.from_ms` was defaulted to the first fully-warm bar rather
    /// than requested explicitly.
    pub from_defaulted: bool,
    /// The engine fingerprint the fold runs share (all equal by construction;
    /// asserted at save).
    pub engine_fingerprint: String,
    /// The `wf-v1` run verdict recorded on the row.
    pub verdict: RunVerdict,
    /// The fold rows, ordered by `index`.
    pub folds: Vec<WalkForwardFold>,
}

/// What [`save_walk_forward_run`](crate::domain::port::WalkForwardRunRepository::save_walk_forward_run)
/// persists: the run's provenance + verdict, and per fold the same pieces
/// [`save_run`](crate::domain::port::BacktestRunRepository::save_run) persists
/// for an ordinary windowed run.
#[derive(Debug)]
pub struct WalkForwardRunDraft {
    /// The scheme the folds were cut under.
    pub scheme: FoldScheme,
    /// The rule the verdicts were judged under.
    pub rule: VerdictRule,
    /// The counted span the folds cover.
    pub span: CandleWindow,
    /// Whether `span.from_ms` was defaulted to the first fully-warm bar.
    pub from_defaulted: bool,
    /// The engine fingerprint the fold runs share.
    pub engine_fingerprint: String,
    /// The `wf-v1` run verdict.
    pub verdict: RunVerdict,
    /// The fold payloads in `fold_index` order (`folds[i].index == i`).
    pub folds: Vec<WalkForwardFoldDraft>,
}

/// One fold's persistable payload — the window and verdict it is recorded
/// under, plus the ordinary-run pieces `insert_run_row`/`insert_trade_rows`
/// already know how to write.
#[derive(Debug)]
pub struct WalkForwardFoldDraft {
    /// The fold's position in the scheme.
    pub index: u8,
    /// The counted window the fold's run covered.
    pub window: CandleWindow,
    /// The `wf-v1` fold verdict.
    pub verdict: FoldVerdict,
    /// The inputs the fold ran with (window + recorded lead-in — w2).
    pub inputs: super::run::BacktestInputs,
    /// The fold's engine result.
    pub result: super::result::BacktestResult,
    /// The derived summary persisted on the fold's row.
    pub summary: super::stats::SummaryStats,
    /// The equity-curve base.
    pub starting_equity: Decimal,
}
