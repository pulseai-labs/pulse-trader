//! The streak and drawdown card's read contract (r5.s4.w1, PDR-0035) — the
//! **app's** view of what a normal bad run of one strategy version looks like.
//!
//! The server resolves the version's **latest full-span backtest run** (never a
//! walk-forward fold), loads its closed trades and runs the domain card over
//! them; this module is the DTO and the transport-free core behind
//! `POST /api/v1/streak-card` and the `streak_card` bus command.
//!
//! **Additive by design (ADR-0020).** The read is a NEW plain route, a NEW bus
//! command and a NEW client method; not one existing Tauri signature changes.
//!
//! **Wire shape.** One internally tagged enum on `state`: `noRun`,
//! `notEnoughTrades` (with the count) or `ready` (with the figures). Counts
//! cross as `u32` (specta refuses `u64`/`i64`), the seed as the exact text of
//! its documented constant, every `Decimal` as an exact string (NFR-2), and
//! `runCreatedAt` as the stored RFC 3339 millisecond text. The card carries no
//! `f64`.
//!
//! An **unknown** version id is a 422 `not_found` refusal, distinct from a
//! version that exists but has no run yet (`noRun`) — a version that does not
//! exist and a version with nothing to show are different answers.

use serde::{Deserialize, Serialize};

use crate::domain::backtest::StreakCard;
use crate::domain::strategy::VersionId;
use crate::domain::{BacktestRunRepository, ClosedTrade, StrategyRepository, streak_card};

use super::commands::DesktopState;
use super::error::{BusError, BusErrorCode};

/// What `streak_card` is asked for: one strategy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct StreakCardRequest {
    /// The version whose latest full-span run the card describes.
    pub version_id: String,
}

/// A count measure's three figures: the two percentiles over the drawn paths
/// and the run's own observed value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct StreakPercentilesDto {
    /// The 50th percentile ("typical").
    pub p50: u32,
    /// The 95th percentile ("bad but still normal").
    pub p95: u32,
    /// The run's own value.
    pub observed: u32,
}

/// A `Decimal` measure's three figures: the two percentiles and the run's own
/// observed value, each an exact decimal string (NFR-2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DecimalPercentilesDto {
    /// The 50th percentile ("typical").
    pub p50: String,
    /// The 95th percentile ("bad but still normal").
    pub p95: String,
    /// The run's own value.
    pub observed: String,
}

/// What `streak_card` answers for one version, tagged on `state`.
///
/// `Ready` is the wide variant and it carries the whole card's figures. Boxing
/// it would even the variants out and reshape the tagged wire this item's read
/// contract (and the generated TypeScript) pins; the DTO is built once per read
/// and immediately serialized, never held in a collection, so the size
/// difference costs one move of a few hundred bytes at the call boundary.
/// Stated rather than silenced blindly — the `coach_decision.rs` precedent.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(
    tag = "state",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum StreakCardDto {
    /// The version exists but has no readable backtest run yet.
    NoRun,
    /// The version's latest full-span run has too few closed trades.
    NotEnoughTrades {
        /// The run the figures would have come from.
        run_id: String,
        /// How many closed trades that run does have.
        closed_trades: u32,
    },
    /// The card's figures.
    Ready {
        /// The full-span run the figures come from (Q4: always named).
        run_id: String,
        /// The run's recorded timestamp, RFC 3339 milliseconds.
        run_created_at: String,
        /// How many closed trades the run has.
        closed_trades: u32,
        /// The horizon every drawn path used.
        horizon_trades: u32,
        /// The number of drawn paths.
        paths: u32,
        /// The card's fixed seed, as its documented constant's exact text.
        seed: String,
        /// The longest losing streak, in trades.
        streak: StreakPercentilesDto,
        /// The max drawdown of the cumulative `realized_r`.
        drawdown_r: DecimalPercentilesDto,
        /// The max drawdown of the cumulative `realized_pnl`, in USDT.
        drawdown_usdt: DecimalPercentilesDto,
        /// The USDT drawdown as a percentage of the fixed 10,000 USDT equity.
        drawdown_pct: DecimalPercentilesDto,
    },
}

/// `streak_card`'s transport-free core: one version id in, the card out.
///
/// # Errors
///
/// Returns [`BusErrorCode::NotFound`] when the version id names no strategy
/// version, and a store-level [`BusError`] when a read fails.
pub async fn streak_card_core(
    state: &DesktopState,
    request: StreakCardRequest,
) -> Result<StreakCardDto, BusError> {
    let version_id = VersionId::new(&request.version_id);
    // "No such version" is a refusal; "a version with no run yet" is `noRun`.
    if state
        .strategy_repo()
        .get_version(&version_id)
        .await?
        .is_none()
    {
        return Err(BusError::new(
            BusErrorCode::NotFound,
            format!("no strategy version {}", request.version_id),
        ));
    }

    let runs = state.backtest_run_repo();
    let Some(run) = runs.latest_readable_run_for_version(&version_id).await?.run else {
        return Ok(StreakCardDto::NoRun);
    };
    let trades = runs.get_trades(&run.id).await?;
    let outcomes: Vec<ClosedTrade> = trades.iter().map(ClosedTrade::from).collect();
    let run_id = run.id.as_str().to_owned();

    Ok(match streak_card(&outcomes) {
        StreakCard::NotEnoughTrades { closed_trades } => StreakCardDto::NotEnoughTrades {
            run_id,
            closed_trades: count(closed_trades),
        },
        StreakCard::Ready {
            closed_trades,
            horizon_trades,
            paths,
            seed,
            streak,
            drawdown_r,
            drawdown_usdt,
            drawdown_pct,
        } => StreakCardDto::Ready {
            run_id,
            run_created_at: run.created_at.clone(),
            closed_trades: count(closed_trades),
            horizon_trades: count(horizon_trades),
            paths: count(paths),
            seed: format!("{seed:#x}"),
            streak: StreakPercentilesDto {
                p50: count(streak.p50),
                p95: count(streak.p95),
                observed: count(streak.observed),
            },
            drawdown_r: decimal_percentiles(drawdown_r.p50, drawdown_r.p95, drawdown_r.observed),
            drawdown_usdt: decimal_percentiles(
                drawdown_usdt.p50,
                drawdown_usdt.p95,
                drawdown_usdt.observed,
            ),
            drawdown_pct: decimal_percentiles(
                drawdown_pct.p50,
                drawdown_pct.p95,
                drawdown_pct.observed,
            ),
        },
    })
}

/// A count as the wire's `u32` — specta refuses 64-bit integers.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// One `Decimal` row → its wire shape, exact strings throughout.
fn decimal_percentiles(
    p50: rust_decimal::Decimal,
    p95: rust_decimal::Decimal,
    observed: rust_decimal::Decimal,
) -> DecimalPercentilesDto {
    DecimalPercentilesDto {
        p50: p50.to_string(),
        p95: p95.to_string(),
        observed: observed.to_string(),
    }
}
