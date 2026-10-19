//! r5.s5.w1 (PDR-0040, ADR-0032) — the power review's pure types and maths.
//!
//! Before a certify attempt spends one of an open freeze's hypotheses, the operator sees what C1
//! (ADR-0028) is worth on this candidate: the holdout trade **count** it would test, and the chance C1
//! would pass a true `+0.25R` edge at that count. This module holds the arithmetic and the record's value
//! types — no I/O, like the rest of the ring.
//!
//! **Deliberately outside `src/domain/backtest/`**: the engine fingerprint's source set
//! (`build_support/engine_source_set.rs`) does not cover this file, so the engine stays byte-identical
//! (SPINE G7). `z` is still C1's own quantile — read from [`holdout_test`], never re-derived.
//!
//! **The constants are the campaign's pre-commitment** (SPINE G4, ADR-0032): a request field, CLI flag or
//! environment variable must never be able to move them.

use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;

use crate::domain::strategy::VersionId;
use crate::domain::{CertificationInputs, Pair, WalkForwardRunId, holdout_test};

/// The pass chance an attempt must reach before it may spend a hypothesis (PDR-0040 ruling 1).
pub const POWER_BAR: f64 = 0.50;

/// The edge the pass chance is computed at, in R — ADR-0028's reference edge, fixed before any candidate
/// was seen (PDR-0040 ruling 1).
pub const ASSUMED_EDGE_R: f64 = 0.25;

/// The holdout trade count below which an attempt is refused whatever the pass chance says — `wf-v2`'s own
/// per-fold minimum, the count below which a single trade's R dominates the mean (PDR-0040 ruling 1).
pub const MIN_HOLDOUT_TRADES: usize = 20;

/// The standard normal CDF, Φ(x), by Abramowitz & Stegun **26.2.17** — a rational approximation with the
/// published `|ε| < 7.5e-8`, well inside the 1e-6 this module's tests ask for. No new crate, and one copy:
/// the certify path and the app read the same Φ.
#[must_use]
pub fn normal_cdf(x: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.231_641_9 * x.abs());
    let phi = (-x * x / 2.0).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let q = phi
        * t
        * (0.319_381_530
            + t * (-0.356_563_782
                + t * (1.781_477_937 + t * (-1.821_255_978 + t * 1.330_274_429))));
    if x >= 0.0 { 1.0 - q } else { q }
}

/// The normal approximation of `P(C1 passes | n, z, μ, σ)` (ADR-0032): `1 − Φ(z − μ·√n / σ)`.
///
/// Zero when the verdict is undefined or hopeless: fewer than two trades, or a σ that is not strictly
/// positive. `z` is [`c1_z`]'s value at the freeze's `H`; `μ` is [`ASSUMED_EDGE_R`]; `σ` is the candidate's
/// own search-span sample σ in R.
#[must_use]
pub fn pass_chance(n: usize, z: f64, mu_r: f64, sigma_r: f64) -> f64 {
    let has_dispersion = sigma_r > 0.0;
    if n < 2 || !has_dispersion {
        return 0.0;
    }
    // A trade count is far inside `f64`'s exact integer range, so this cast is exact.
    #[allow(clippy::cast_precision_loss)]
    let n_f = n as f64;
    1.0 - normal_cdf(z - mu_r * n_f.sqrt() / sigma_r)
}

/// C1's own quantile `z(1 − 0.05/H)` at the freeze's budget `H` — read from [`holdout_test`], so the
/// review can never drift from the test the certify step will apply. Two trades are enough to reach the
/// quantile (it does not read the series), and `holdout.rs` is not edited for this.
#[must_use]
pub fn c1_z(h: u8) -> f64 {
    holdout_test(&[Decimal::ZERO, Decimal::TWO], h).z
}

/// The sample standard deviation (Bessel `N−1`) of a series of R-multiples, or `None` when there are fewer
/// than two trades or the `Decimal` does not convert.
///
/// The mean and the variance accumulate in `Decimal` exactly as `holdout_test` and `FoldVerdict::from_rs`
/// do; one conversion to `f64` and one `sqrt` follow, the same single-`sqrt` shape.
#[must_use]
pub fn sample_sigma(rs: &[Decimal]) -> Option<f64> {
    let n = rs.len();
    if n < 2 {
        return None;
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
    sample_var.to_f64().map(f64::sqrt)
}

/// Why an attempt is not allowed, in the order [`verdict`] checks them. The stored string is the record's
/// `reason` column value; `as_str` is the one place the wire/DB spelling lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerBlock {
    /// The search span's `wf-v2` verdict did not pass.
    SearchSpanFails,
    /// Fewer than [`MIN_HOLDOUT_TRADES`] trades would fall in the holdout.
    TooFewHoldoutTrades,
    /// The pass chance is below [`POWER_BAR`].
    BelowBar,
}

impl PowerBlock {
    /// The persisted/wire spelling of this reason (`0022`'s CHECK names the same three).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SearchSpanFails => "search_span_fails",
            Self::TooFewHoldoutTrades => "too_few_holdout_trades",
            Self::BelowBar => "below_bar",
        }
    }
}

/// Whether a power review allows a certify attempt, and if not, why (the first failing reason).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerVerdict {
    /// Whether an attempt may spend a hypothesis.
    pub allowed: bool,
    /// The first failing reason; `None` exactly when `allowed`.
    pub reason: Option<PowerBlock>,
}

/// The verdict, in its fixed order: the search span first (no candidate at all), then the trade-count floor,
/// then the bar. A chance exactly at [`POWER_BAR`] is allowed — the comparison is a strict below. The bar
/// test fails **closed**: a NaN or non-finite chance is not allowed, and refuses with [`PowerBlock::BelowBar`].
#[must_use]
pub fn verdict(search_pass: bool, holdout_n: usize, pass_chance: f64) -> PowerVerdict {
    let reason = if !search_pass {
        Some(PowerBlock::SearchSpanFails)
    } else if holdout_n < MIN_HOLDOUT_TRADES {
        Some(PowerBlock::TooFewHoldoutTrades)
    } else if !pass_chance.is_finite() || pass_chance < POWER_BAR {
        Some(PowerBlock::BelowBar)
    } else {
        None
    };
    PowerVerdict {
        allowed: reason.is_none(),
        reason,
    }
}

/// One `power_review` row — the immutable record of one count-only holdout read (ADR-0032).
///
/// **Nothing about the holdout's outcomes is a field** (SPINE G1): the row carries the count, the window,
/// the pinned inputs, the candidate's search-span σ and the bar's arithmetic — never an R, mean, bound,
/// P&L, win rate, trade time or verdict.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PowerReviewRecord {
    /// The row id (a UUID minted by the store).
    pub id: String,
    /// The version the review is about.
    pub version_id: VersionId,
    /// The freeze whose holdout was counted.
    pub freeze_id: String,
    /// The pair the count was taken on.
    pub pair: Pair,
    /// The freeze's hypothesis budget `H` (which fixes `z`).
    pub h: u8,
    /// C1's quantile at `h` ([`c1_z`]).
    pub z: f64,
    /// The holdout window's inclusive start (epoch ms).
    pub holdout_start_ms: i64,
    /// The holdout window's exclusive end (epoch ms).
    pub holdout_end_ms: i64,
    /// The holdout's trade count — the one thing this read reports about the holdout.
    pub holdout_n: usize,
    /// The data versions the holdout read pinned, per timeframe.
    pub holdout_inputs: CertificationInputs,
    /// The persisted search-span walk-forward the σ and `search_n` come from.
    pub search_walk_forward_run_id: WalkForwardRunId,
    /// Whether that search span's `wf-v2` verdict passed.
    pub search_pass: bool,
    /// The pooled out-of-sample trade count of the search span's folds.
    pub search_n: usize,
    /// The candidate's search-span sample σ in R, as a decimal string (`x.xxxxxx`).
    pub sigma_r: String,
    /// [`ASSUMED_EDGE_R`] as stored (`"0.25"`).
    pub assumed_edge_r: String,
    /// [`POWER_BAR`] as stored (`"0.50"`).
    pub bar: String,
    /// [`MIN_HOLDOUT_TRADES`] as stored.
    pub min_holdout_trades: usize,
    /// The pass chance the decision was made on.
    pub pass_chance: f64,
    /// Whether an attempt is allowed.
    pub allowed: bool,
    /// The first failing reason's stored spelling, or `None` when allowed.
    pub reason: Option<String>,
    /// The search run's engine fingerprint (the same source the certification record uses).
    pub engine_fingerprint: String,
    /// The caller's label — `"cli"` for the CLI arm; never a credential.
    pub called_by: String,
    /// When the row was written (RFC 3339 UTC, the injected `Clock`).
    pub created_at: String,
}

/// What the use case asks the store to write: everything the row carries except the pieces the store mints
/// (`id`, `created_at`).
#[derive(Debug, Clone, PartialEq)]
pub struct PowerReviewDraft {
    /// The version the review is about.
    pub version_id: VersionId,
    /// The freeze whose holdout was counted.
    pub freeze_id: String,
    /// The pair the count was taken on.
    pub pair: Pair,
    /// The freeze's hypothesis budget `H`.
    pub h: u8,
    /// C1's quantile at `h`.
    pub z: f64,
    /// The holdout window's inclusive start (epoch ms).
    pub holdout_start_ms: i64,
    /// The holdout window's exclusive end (epoch ms).
    pub holdout_end_ms: i64,
    /// The holdout's trade count.
    pub holdout_n: usize,
    /// The holdout read's pinned data versions.
    pub holdout_inputs: CertificationInputs,
    /// The persisted search-span walk-forward.
    pub search_walk_forward_run_id: WalkForwardRunId,
    /// Whether the search span passed.
    pub search_pass: bool,
    /// The search span's pooled trade count.
    pub search_n: usize,
    /// The candidate's search-span sample σ in R, as a decimal string.
    pub sigma_r: String,
    /// The assumed edge in R, as stored.
    pub assumed_edge_r: String,
    /// The bar, as stored.
    pub bar: String,
    /// The trade-count floor, as stored.
    pub min_holdout_trades: usize,
    /// The pass chance.
    pub pass_chance: f64,
    /// Whether an attempt is allowed.
    pub allowed: bool,
    /// The first failing reason's stored spelling.
    pub reason: Option<String>,
    /// The search run's engine fingerprint.
    pub engine_fingerprint: String,
    /// The caller's label.
    pub called_by: String,
}
