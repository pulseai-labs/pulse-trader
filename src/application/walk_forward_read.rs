//! The walk-forward read projection (r2.s3.w5) — the shared read seam every
//! walk-forward surface consumes.
//!
//! Two jobs, both read-only and both shared so no delivery ring re-derives its
//! own:
//!
//! 1. [`load_fold_runs`] — the L8 seam's read half: a walk-forward fold IS an
//!    ordinary `backtest_run` row, so it loads through
//!    [`BacktestRunRepository::get_run`] like any other run. A fold whose row
//!    is missing or unreadable is a refusal naming the parent — never a
//!    shortened folds list.
//! 2. [`WalkForwardRunDetail`] + [`walk_forward_run_detail`] — the ONE wire
//!    shape both `pulse mcp` walk-forward tools return (`run_walk_forward` and
//!    `get_walk_forward_run` are byte-identical for the same run, ruling (iv)).
//!    Every value traces to a persisted row: the parent's recorded provenance
//!    and verdict, the fold rows' windows and verdicts, and each fold's own
//!    [`RunSummary`] catalog row — never a recomputation.
//!
//! The application ring names no adapter namespace other than
//! `crate::adapters::backtest` (`tests/tauri_backtest.rs`'s source scan), so
//! everything here is pure: domain values in, `serde`-serializable structs out.

use rust_decimal::Decimal;
use serde::Serialize;

use crate::application::walk_forward::WalkForwardAppError;
use crate::domain::{
    BacktestRunRepository, DataError, FoldVerdict, PersistedRun, RunSummary, RunVerdict,
    WalkForwardRun,
};

/// Epoch ms → RFC 3339 with **millisecond** precision — the wire's timestamp
/// shape for a span or fold-window bound. A `to` bound is a candle's
/// `close_time` (`…:59.999`), so seconds precision would truncate a real
/// value. A non-representable ms renders as the raw integer rather than
/// fabricating a date (the `ms_rfc3339` fallback convention).
#[must_use]
pub(crate) fn rfc3339_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || format!("{ms}ms"),
        |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

/// Epoch ms → RFC 3339 with **seconds** precision — the prose timestamp shape
/// a refusal names a bound with (`describe_input_differences`'s convention).
/// The value it renders is always a candle's `open_time` (whole seconds), so
/// nothing is lost.
#[must_use]
pub(crate) fn rfc3339_secs(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || format!("{ms}ms"),
        |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

/// The fold runs of `run`, each read through the ordinary run log, in
/// `fold_index` order. A `get_run` `None` or `Err` is a
/// [`WalkForwardAppError::SavedButReadBackFailed`] naming the parent — the
/// same honest refusal `read_back_outcome` gives a fold missing from the
/// version's catalog, extended to the cold read path.
///
/// # Errors
///
/// Returns a [`WalkForwardAppError`] when any fold's run row fails to load.
pub(crate) async fn load_fold_runs<R>(
    runs: &R,
    run: &WalkForwardRun,
) -> Result<Vec<PersistedRun>, WalkForwardAppError>
where
    R: BacktestRunRepository,
{
    let mut fold_runs = Vec::with_capacity(run.folds.len());
    for fold in &run.folds {
        let fold_run = runs
            .get_run(&fold.backtest_run_id)
            .await
            .map_err(|source| WalkForwardAppError::SavedButReadBackFailed {
                run_id: run.id.clone(),
                source,
            })?
            .ok_or_else(|| WalkForwardAppError::SavedButReadBackFailed {
                run_id: run.id.clone(),
                source: DataError::Db(format!(
                    "fold {}'s backtest_run `{}` is not in the run log",
                    fold.index,
                    fold.backtest_run_id.as_str(),
                )),
            })?;
        fold_runs.push(fold_run);
    }
    Ok(fold_runs)
}

/// The [`RunSummary`] one persisted run's catalog row carries — the same ten
/// fields `row_to_run_summary` reads, rebuilt off the full run so a caller
/// holding `PersistedRun`s (rather than a version-scoped catalog listing)
/// projects the identical row.
#[must_use]
pub(crate) fn run_summary_of(run: &PersistedRun) -> RunSummary {
    RunSummary {
        id: run.id.clone(),
        strategy_version_id: run.strategy_version_id.clone(),
        schema_version: run.schema_version,
        created_at: run.created_at.clone(),
        engine_fingerprint: run.engine_fingerprint.clone(),
        engine_target: run.engine_target.clone(),
        result_content_hash: run.result_content_hash.clone(),
        net_pnl: run.net_pnl,
        expectancy: run.summary.expectancy,
        trade_count: run.summary.trade_count,
    }
}

/// One `[from, to)` bound pair on the wire — RFC 3339 millisecond text.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalkForwardWindowWire {
    /// The window's inclusive lower bound.
    pub from: String,
    /// The window's exclusive upper bound.
    pub to: String,
}

/// The counted span — a window plus whether its `from` was defaulted to the
/// first fully-warm bar (recorded on the row; `false` when the caller asked
/// for it explicitly).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalkForwardSpanWire {
    /// The counted span's inclusive lower bound.
    pub from: String,
    /// The counted span's exclusive upper bound.
    pub to: String,
    /// Whether `from` was defaulted (not requested explicitly).
    pub from_defaulted: bool,
}

/// One `wf-v1` verdict block — a fold's verdict and the pooled verdict share
/// this shape (`n` / `mean_r` / `lower_bound` / `holds`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FoldVerdictWire {
    /// The number of out-of-sample trades the verdict saw.
    pub n: usize,
    /// `Σ rᵢ / n` in `Decimal` — byte-exact.
    pub mean_r: Decimal,
    /// `mean − 1.645 · sqrt(var / n)` — the one-sided lower bound on the
    /// expectancy in R.
    pub lower_bound: f64,
    /// `n >= 20 && lower_bound > 0` — `wf-v1`'s fold rule.
    pub holds: bool,
}

/// The `wf-v1` run verdict on the wire: the fold tallies plus the pooled bound.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunVerdictWire {
    /// `folds_holding >= folds_required && pooled.holds`.
    pub pass: bool,
    /// How many of the run's folds held.
    pub folds_holding: u8,
    /// `⌈2K/3⌉` — how many must hold for the run to pass.
    pub folds_required: u8,
    /// The verdict over every fold's trades concatenated in fold order.
    pub pooled: FoldVerdictWire,
}

/// One fold's row in the detail: its window, its `wf-v1` verdict, the ordinary
/// `backtest_run` it ran as, and that run's own catalog summary (L8, a7).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalkForwardFoldDetail {
    /// The fold's position in the scheme (`0..k`).
    pub index: u8,
    /// The counted window the fold's run covered.
    pub window: WalkForwardWindowWire,
    /// The persisted `backtest_run` id this fold ran as — the same id
    /// `get_run` / `list_runs` resolve.
    pub backtest_run_id: String,
    /// The `wf-v1` verdict recorded on the fold row.
    pub verdict: FoldVerdictWire,
    /// The fold run's own catalog row — every [`RunSummary`] field.
    pub summary: RunSummary,
}

/// The ONE shape `run_walk_forward` and `get_walk_forward_run` both return
/// (ruling (iv)): the persisted parent's provenance and `wf-v1` verdict plus
/// one entry per fold.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WalkForwardRunDetail {
    /// The walk-forward run's opaque id.
    pub walk_forward_run_id: String,
    /// The `strategy_version` the run was produced against.
    pub version_id: String,
    /// The fold scheme's pinned name (`rolling-oos/v1`).
    pub scheme: String,
    /// The fold count.
    pub k: u8,
    /// The verdict rule's pinned name (`wf-v1`).
    pub rule: String,
    /// The counted span the folds cover (union of their windows).
    pub span: WalkForwardSpanWire,
    /// The engine fingerprint the fold runs share.
    pub engine_fingerprint: String,
    /// The recorded `wf-v1` run verdict.
    pub verdict: RunVerdictWire,
    /// The folds in `fold_index` order.
    pub folds: Vec<WalkForwardFoldDetail>,
}

/// Project a [`FoldVerdict`] onto its wire block — field-for-field.
#[must_use]
fn fold_verdict_wire(verdict: &FoldVerdict) -> FoldVerdictWire {
    FoldVerdictWire {
        n: verdict.n,
        mean_r: verdict.mean_r,
        lower_bound: verdict.lower_bound,
        holds: verdict.holds,
    }
}

/// Project a [`RunVerdict`] onto its wire block — field-for-field.
#[must_use]
fn run_verdict_wire(verdict: &RunVerdict) -> RunVerdictWire {
    RunVerdictWire {
        pass: verdict.pass,
        folds_holding: verdict.folds_holding,
        folds_required: verdict.folds_required,
        pooled: fold_verdict_wire(&verdict.pooled),
    }
}

/// Pair one persisted [`WalkForwardRun`] with its folds' catalog rows into the
/// one wire detail.
///
/// `fold_summaries` must be in fold order, one per `run.folds` entry, each
/// naming its fold's `backtest_run_id` — the contract
/// `run_walk_forward`'s `fold_summaries` and [`load_fold_runs`] +
/// [`run_summary_of`] both already satisfy. A length or id mismatch is refused
/// as [`WalkForwardAppError::SavedButReadBackFailed`] — a seam that does not
/// line up is corrupt, and a silently shortened or mismatched table is the
/// plausible-false-answer shape the read-back discipline exists to prevent.
///
/// # Errors
///
/// Returns a [`WalkForwardAppError`] when the summaries do not line up with
/// the run's fold rows.
pub fn walk_forward_run_detail(
    run: &WalkForwardRun,
    fold_summaries: &[RunSummary],
) -> Result<WalkForwardRunDetail, WalkForwardAppError> {
    if fold_summaries.len() != run.folds.len() {
        return Err(WalkForwardAppError::SavedButReadBackFailed {
            run_id: run.id.clone(),
            source: DataError::Db(format!(
                "walk-forward run {} carries {} fold rows but {} fold summaries arrived",
                run.id.as_str(),
                run.folds.len(),
                fold_summaries.len(),
            )),
        });
    }
    let mut folds = Vec::with_capacity(run.folds.len());
    for (fold, summary) in run.folds.iter().zip(fold_summaries.iter()) {
        if summary.id != fold.backtest_run_id {
            return Err(WalkForwardAppError::SavedButReadBackFailed {
                run_id: run.id.clone(),
                source: DataError::Db(format!(
                    "fold {}'s backtest_run `{}` does not match summary `{}`",
                    fold.index,
                    fold.backtest_run_id.as_str(),
                    summary.id.as_str(),
                )),
            });
        }
        folds.push(WalkForwardFoldDetail {
            index: fold.index,
            window: WalkForwardWindowWire {
                from: rfc3339_ms(fold.window.from_ms),
                to: rfc3339_ms(fold.window.to_ms),
            },
            backtest_run_id: fold.backtest_run_id.as_str().to_owned(),
            verdict: fold_verdict_wire(&fold.verdict),
            summary: summary.clone(),
        });
    }
    Ok(WalkForwardRunDetail {
        walk_forward_run_id: run.id.as_str().to_owned(),
        version_id: run.strategy_version_id.as_str().to_owned(),
        scheme: run.scheme.name().to_owned(),
        k: run.scheme.k(),
        rule: run.rule.name().to_owned(),
        span: WalkForwardSpanWire {
            from: rfc3339_ms(run.span.from_ms),
            to: rfc3339_ms(run.span.to_ms),
            from_defaulted: run.from_defaulted,
        },
        engine_fingerprint: run.engine_fingerprint.clone(),
        verdict: run_verdict_wire(&run.verdict),
        folds,
    })
}
