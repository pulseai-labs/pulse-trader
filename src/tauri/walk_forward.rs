//! The walk-forward command surface (r2.s3.w5) — the ring-owned DTOs the three
//! commands exchange, the pure projection that builds them, and the drivable
//! cores behind the `#[tauri::command]` wrappers in `commands.rs`.
//!
//! **One projection, two commands.** `run_walk_forward_version` and
//! `get_walk_forward_run` both answer with [`WalkForwardRunDto`] built by the
//! same [`walk_forward_run_dto`] — the read-back DTO is the run DTO, field for
//! field (AC-2's cold-run equality is the proof).
//!
//! **Folds are ordinary runs.** A fold's `backtest_run_id` resolves through
//! [`get_backtest_run_core`], which reads the persisted row back through the
//! SAME `read_back` + [`backtest_run_dto`] pipeline `run_backtest_version`
//! answers with — so a fold's run view carries its `walkForward` membership
//! for free, and no walk-forward screen re-derives a number the database
//! already holds.
//!
//! **Timestamp shape.** Span and window bounds cross as RFC 3339 millisecond
//! text — a `to` bound is a candle's `close_time` (`…:59.999`), so seconds
//! precision would truncate a real value. Decimals cross as exact strings
//! (NFR-2), the same discipline `backtest.rs` carries.

use serde::{Deserialize, Serialize};

use crate::adapters::broker::BinanceAdapter;
use crate::application::backtest::{read_back, resolve_default_request};
use crate::application::walk_forward::{WalkForwardRequest, run_walk_forward};
use crate::application::walk_forward_read::{load_fold_runs, rfc3339_ms};
use crate::domain::backtest::FoldVerdict;
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestRunId, BacktestRunRepository, PersistedRun, WalkForwardRun, WalkForwardRunId,
    WalkForwardRunRepository,
};

use super::backtest::{BacktestRunDto, backtest_run_dto, dec};
use super::commands::{DesktopState, OperationKey};
use super::error::{BusError, BusErrorCode};

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// What `run_walk_forward_version` is asked for: one persisted strategy
/// version, plus the `rolling-oos/v1` knobs the wire may set.
///
/// Pair, timeframes and costs are **not** here — the shared
/// [`resolve_default_request`] seam supplies them from the version's recorded
/// lineage (parent's latest run, then its own, then the app defaults), so a
/// walk-forward runs a version identically on every surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WalkForwardRunRequest {
    /// The immutable strategy version to walk forward.
    pub version_id: String,
    /// The counted span's inclusive start, RFC 3339. `None` defaults to the
    /// first fully-warm bar; an explicit earlier value refuses.
    #[serde(default)]
    pub from: Option<String>,
    /// The counted span's exclusive end, RFC 3339. `None` defaults to the
    /// snapshot's last candle's `close_time`. Independent of `from` — either
    /// bound may be given alone.
    #[serde(default)]
    pub to: Option<String>,
    /// The fold count `k`, in `2..=12`. `None` defaults to 6.
    #[serde(default)]
    pub k: Option<u8>,
}

/// What `get_walk_forward_run` is asked for: one persisted walk-forward run
/// id. A request struct rather than a bare `String` — the
/// `CompareChildRunRequest` shape — so the wire argument names itself
/// (`{ walkForwardRunId }`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct GetWalkForwardRunRequest {
    /// The walk-forward run id.
    pub walk_forward_run_id: String,
}

/// What `get_backtest_run` is asked for: one persisted run id — a standalone
/// run or a walk-forward fold alike (a fold IS an ordinary `backtest_run`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct GetBacktestRunRequest {
    /// The backtest run id.
    pub run_id: String,
}

// ---------------------------------------------------------------------------
// The DTO
// ---------------------------------------------------------------------------

/// One `wf-v1` verdict block — a fold's verdict and the pooled verdict share
/// this shape. `mean_r` crosses as exact decimal text; `lower_bound` is the
/// one genuinely `f64` value the rule computes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct FoldVerdictDto {
    /// The number of out-of-sample trades the verdict saw.
    pub n: u32,
    /// `Σ rᵢ / n`, exact decimal string.
    pub mean_r: String,
    /// `mean − 1.645 · sqrt(var / n)` — the one-sided expectancy lower bound
    /// in R.
    pub lower_bound: f64,
    /// `n >= 20 && lower_bound > 0` — `wf-v1`'s fold rule.
    pub holds: bool,
}

/// The `wf-v1` run verdict on the wire: the fold tallies plus the pooled
/// bound over every out-of-sample trade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WalkForwardVerdictDto {
    /// `folds_holding >= folds_required && pooled.holds`.
    pub pass: bool,
    /// How many of the run's folds held.
    pub folds_holding: u32,
    /// `⌈2K/3⌉` — how many must hold for the run to pass.
    pub folds_required: u32,
    /// The verdict over every fold's trades concatenated in fold order.
    pub pooled: FoldVerdictDto,
}

/// One fold's row: its counted window, the `wf-v1` verdict recorded on it, the
/// ordinary `backtest_run` it ran as, and that run's own headline stats — the
/// numbers the Backtest Lab's per-fold table renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WalkForwardFoldDto {
    /// The fold's position in the scheme (`0..k`).
    pub index: u32,
    /// The counted window's inclusive start, RFC 3339 ms.
    pub window_from: String,
    /// The counted window's exclusive end, RFC 3339 ms.
    pub window_to: String,
    /// The ordinary `backtest_run` this fold ran as — `get_backtest_run`'s id.
    pub backtest_run_id: String,
    /// The trades the `wf-v1` verdict saw (equals `trades`).
    pub n: u32,
    /// `Σ rᵢ / n`, exact decimal string.
    pub mean_r: String,
    /// The one-sided expectancy lower bound in R.
    pub lower_bound: f64,
    /// Whether the fold holds under `wf-v1`.
    pub holds: bool,
    /// The fold run's persisted trade count.
    pub trades: u32,
    /// The fold run's mean P&L per trade, exact decimal string.
    pub expectancy: String,
    /// The fold run's win rate, exact decimal string.
    pub win_rate: String,
}

/// The walk-forward run both `run_walk_forward_version` and
/// `get_walk_forward_run` answer with: the persisted parent's provenance and
/// verdict plus one row per fold. `PartialEq` (not `Eq`) — `lower_bound` is
/// `f64`, the same reason [`BacktestRunDto`] carries only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WalkForwardRunDto {
    /// The walk-forward run's opaque id.
    pub walk_forward_run_id: String,
    /// The `strategy_version` the run was produced against.
    pub version_id: String,
    /// The fold scheme's pinned name (`rolling-oos/v1`).
    pub scheme: String,
    /// The fold count.
    pub k: u32,
    /// The verdict rule's pinned name (`wf-v1`).
    pub rule: String,
    /// The counted span's inclusive start, RFC 3339 ms.
    pub span_from: String,
    /// The counted span's exclusive end, RFC 3339 ms.
    pub span_to: String,
    /// Whether `span_from` was defaulted to the first fully-warm bar rather
    /// than requested explicitly.
    pub from_defaulted: bool,
    /// The engine fingerprint the fold runs share.
    pub engine_fingerprint: String,
    /// The recorded `wf-v1` run verdict.
    pub verdict: WalkForwardVerdictDto,
    /// The folds in `fold_index` order.
    pub folds: Vec<WalkForwardFoldDto>,
}

// ---------------------------------------------------------------------------
// The projection
// ---------------------------------------------------------------------------

/// Narrow a stored count to the wire's `u32`, refusing rather than clamping —
/// the same discipline `backtest_run_dto`'s `count` carries, spelled against
/// the walk-forward run id it belongs to.
fn count_u32(run_id: &WalkForwardRunId, field: &str, value: usize) -> Result<u32, BusError> {
    u32::try_from(value).map_err(|_| {
        BusError::new(
            BusErrorCode::Data,
            format!(
                "walk-forward run `{}`: stored `{field}` = {value} does not fit the wire's u32",
                run_id.as_str()
            ),
        )
    })
}

/// Project one [`FoldVerdict`] onto its wire block.
fn fold_verdict_dto(
    run_id: &WalkForwardRunId,
    verdict: &FoldVerdict,
) -> Result<FoldVerdictDto, BusError> {
    Ok(FoldVerdictDto {
        n: count_u32(run_id, "verdict.n", verdict.n)?,
        mean_r: dec(verdict.mean_r),
        lower_bound: verdict.lower_bound,
        holds: verdict.holds,
    })
}

/// Project one persisted [`WalkForwardRun`] plus its folds' run rows onto the
/// DTO both walk-forward commands answer with.
///
/// `fold_runs` must hold each fold's [`PersistedRun`] in `fold_index` order —
/// [`load_fold_runs`]'s contract. A length or id mismatch refuses `data`
/// naming the parent run: a seam that does not line up is corrupt, and a
/// silently shortened or misattributed fold table is the plausible-false-answer
/// shape the read-back discipline exists to prevent.
///
/// # Errors
///
/// Returns a [`BusError`] with [`BusErrorCode::Data`] when the fold rows do
/// not line up with the loaded runs, or a stored count does not fit `u32`.
pub fn walk_forward_run_dto(
    run: &WalkForwardRun,
    fold_runs: &[PersistedRun],
) -> Result<WalkForwardRunDto, BusError> {
    if fold_runs.len() != run.folds.len() {
        return Err(BusError::new(
            BusErrorCode::Data,
            format!(
                "walk-forward run {} carries {} fold rows but {} fold runs arrived",
                run.id.as_str(),
                run.folds.len(),
                fold_runs.len(),
            ),
        ));
    }
    let mut folds = Vec::with_capacity(run.folds.len());
    for (fold, persisted) in run.folds.iter().zip(fold_runs.iter()) {
        if persisted.id != fold.backtest_run_id {
            return Err(BusError::new(
                BusErrorCode::Data,
                format!(
                    "walk-forward run {} fold {}'s backtest_run `{}` does not match \
                     the loaded run `{}`",
                    run.id.as_str(),
                    fold.index,
                    fold.backtest_run_id.as_str(),
                    persisted.id.as_str(),
                ),
            ));
        }
        folds.push(WalkForwardFoldDto {
            index: u32::from(fold.index),
            window_from: rfc3339_ms(fold.window.from_ms),
            window_to: rfc3339_ms(fold.window.to_ms),
            backtest_run_id: fold.backtest_run_id.as_str().to_owned(),
            n: count_u32(&run.id, "verdict.n", fold.verdict.n)?,
            mean_r: dec(fold.verdict.mean_r),
            lower_bound: fold.verdict.lower_bound,
            holds: fold.verdict.holds,
            trades: count_u32(&run.id, "trade_count", persisted.summary.trade_count)?,
            expectancy: dec(persisted.summary.expectancy),
            win_rate: dec(persisted.summary.win_rate),
        });
    }
    Ok(WalkForwardRunDto {
        walk_forward_run_id: run.id.as_str().to_owned(),
        version_id: run.strategy_version_id.as_str().to_owned(),
        scheme: run.scheme.name().to_owned(),
        k: u32::from(run.scheme.k()),
        rule: run.rule.name().to_owned(),
        span_from: rfc3339_ms(run.span.from_ms),
        span_to: rfc3339_ms(run.span.to_ms),
        from_defaulted: run.from_defaulted,
        engine_fingerprint: run.engine_fingerprint.clone(),
        verdict: WalkForwardVerdictDto {
            pass: run.verdict.pass,
            folds_holding: u32::from(run.verdict.folds_holding),
            folds_required: u32::from(run.verdict.folds_required),
            pooled: fold_verdict_dto(&run.id, &run.verdict.pooled)?,
        },
        folds,
    })
}

// ---------------------------------------------------------------------------
// The cores
// ---------------------------------------------------------------------------

/// Parse one independent RFC 3339 request bound — `from`/`to` are each
/// optional on their own (unlike `run_backtest`'s both-or-neither window).
fn parse_rfc3339_bound(field: &str, raw: &str) -> Result<i64, BusError> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp_millis())
        .map_err(|e| {
            BusError::new(
                BusErrorCode::Validation,
                format!("`{field}` is not an RFC 3339 timestamp: {e}"),
            )
        })
}

/// `run_walk_forward_version`'s transport-free core (r2.s3.w5).
///
/// The whole call is held under the operation latch keyed
/// [`OperationKey::WalkForward`] on the version, released through the RAII
/// guard on every exit path — the `#141` single-flight rule `run_backtest`
/// carries, on its own key so a `Busy` refusal names the true operation.
///
/// # Errors
///
/// Returns a [`BusError`]; see [`From<WalkForwardAppError>`](super::error) for
/// the variant-to-code mapping.
pub async fn run_walk_forward_version_core(
    state: &DesktopState,
    request: WalkForwardRunRequest,
) -> Result<WalkForwardRunDto, BusError> {
    let _operation = state.begin_operation(OperationKey::WalkForward(VersionId::new(
        &request.version_id,
    )))?;
    let from_ms = request
        .from
        .as_deref()
        .map(|raw| parse_rfc3339_bound("from", raw))
        .transpose()?;
    let to_ms = request
        .to
        .as_deref()
        .map(|raw| parse_rfc3339_bound("to", raw))
        .transpose()?;
    let strategies = state.strategy_repo();
    let runs = state.backtest_run_repo();
    // The same resolver `run_backtest_version` uses — pair, timeframes, costs
    // and the exact snapshot pins come from the version's recorded lineage.
    // `window: None`: the walk-forward's independent `from`/`to` are not the
    // backtest window; the counted span resolves inside `run_walk_forward`.
    let resolved = resolve_default_request(
        &strategies,
        &runs,
        &VersionId::new(&request.version_id),
        None,
    )
    .await?;
    let outcome = run_walk_forward(
        &strategies,
        &state.candles(),
        &BinanceAdapter::new(),
        &runs,
        &WalkForwardRequest {
            version_id: resolved.version_id,
            pair: resolved.pair,
            primary_timeframe: resolved.primary_timeframe,
            htf_timeframe: resolved.htf_timeframe,
            config: resolved.config,
            snapshots: resolved.snapshots,
            from_ms,
            to_ms,
            k: request.k,
        },
    )
    .await?;
    // The DTO's per-fold stats come from the fold's own persisted run — its
    // full `PersistedRun`, not the outcome's lighter `RunSummary` catalog row,
    // because the wire table also renders `win_rate`.
    let fold_runs = load_fold_runs(&runs, &outcome.run).await?;
    walk_forward_run_dto(&outcome.run, &fold_runs)
}

/// `get_walk_forward_run`'s transport-free core (r2.s3.w5): read the persisted
/// parent + its fold runs back and project the SAME DTO the run command
/// answers with — the read-back DTO is the run DTO, field for field.
///
/// # Errors
///
/// Returns a [`BusError`]: [`BusErrorCode::NotFound`] when the id names no
/// walk-forward run, [`BusErrorCode::Data`] when a stored row or fold seam
/// refuses to read back.
pub async fn get_walk_forward_run_core(
    state: &DesktopState,
    request: GetWalkForwardRunRequest,
) -> Result<WalkForwardRunDto, BusError> {
    let runs = state.backtest_run_repo();
    let run = runs
        .get_walk_forward_run(&WalkForwardRunId::new(&request.walk_forward_run_id))
        .await?
        .ok_or_else(|| {
            BusError::new(
                BusErrorCode::NotFound,
                format!("no walk-forward run {}", request.walk_forward_run_id),
            )
        })?;
    let fold_runs = load_fold_runs(&runs, &run).await?;
    walk_forward_run_dto(&run, &fold_runs)
}

/// `get_backtest_run`'s transport-free core (r2.s3.w5): one persisted run id
/// in, the same [`BacktestRunDto`] `run_backtest_version` answers with out —
/// a walk-forward fold's id therefore opens as an ordinary run carrying its
/// `walkForward` membership.
///
/// # Errors
///
/// Returns a [`BusError`]: [`BusErrorCode::NotFound`] when the id names no run
/// (the asked-for id rides `child_run_id`, the same slot
/// `compare_child_run`'s refusal uses), and the read-back's own
/// [`BacktestAppError`](crate::application::backtest::BacktestAppError)
/// mapping otherwise — including its `run_id` carry when a saved row exists
/// but its projection fails.
pub async fn get_backtest_run_core(
    state: &DesktopState,
    request: GetBacktestRunRequest,
) -> Result<BacktestRunDto, BusError> {
    let runs = state.backtest_run_repo();
    let run_id = BacktestRunId::new(&request.run_id);
    // An id that names nothing is `not_found`, not a read-back fault — the
    // same refusal `compare_child_run` gives.
    if runs.get_run(&run_id).await?.is_none() {
        return Err(BusError::with_child_run_id(
            BusErrorCode::NotFound,
            format!("no backtest run {}", request.run_id),
            request.run_id,
        ));
    }
    let outcome = read_back(state.candles(), &runs, run_id, None).await?;
    Ok(backtest_run_dto(&outcome)?)
}
