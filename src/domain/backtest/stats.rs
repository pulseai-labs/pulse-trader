//! `SummaryStats` + the derived equity curve (VS-1.2.4 work-4.01, FR-6 / NFR-2,
//! BACKLOG-4).
//!
//! The **derived, read-only** headline read of a finished backtest. Everything
//! here is a pure `Decimal`/`usize` function of the *already-computed*
//! [`BacktestResult`](super::BacktestResult) totals + the final trade log + the
//! run's `starting_equity` — it never mutates the trade log, never re-sums
//! `net_pnl`/the cost totals, and (the slice's HARD invariant, README C3/C8) is
//! **excluded from both** `result_content_hash()` and `money_math_hash()`: not a
//! single byte of [`SummaryStats`] or [`EquityCurve`] reaches either hasher, so
//! the frozen baseline stays frozen *by construction* (#69 deferred).
//!
//! The slice's ONLY `f64`-derived statistics live here (4.02, D1/D2): `sharpe`
//! and `sortino`, computed from per-trade `realized_r`. The single allowed
//! transcendental — `sqrt` — is used exactly here: the variance and downside
//! sums are accumulated in `Decimal` (byte-exact), converted to `f64` ONCE, then
//! `sqrt`ed (no `f64` arithmetic precedes the single conversion, D2). The two
//! ratios are `Option<f64>` carrying ONLY a finite `f64` or `None` — never
//! `NaN`/`Inf` (D3, audit C10) — and are **oracle-excluded** (D4): like every
//! other `SummaryStats` field they never reach `result_content_hash()` /
//! `money_math_hash()`, so the frozen baseline stays frozen by construction.
//! All Decimal ratios are guarded against a zero denominator (D4): `profit_factor`
//! is `None` when `gross_loss == 0`; `win_rate`/`avg_win`/`avg_loss`/`expectancy`
//! are `0` on their respective zero denominators — never a panic / divide-by-zero.

use std::cmp::Ordering;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::trade::Trade;

/// The pure-`Decimal`/`usize` summary statistics for one backtest run (README
/// C1). Derived read-only over the final trade log + run totals in
/// [`SummaryStats::from_trades`]; attached to
/// [`BacktestResult`](super::BacktestResult) in `LoopState::into_result` and
/// surfaced in the `--json` object.
///
/// **Excluded from the determinism oracle** (README C3/C8, mirrors the
/// `engine_fingerprint` exclusion): two results differing only in their
/// `summary` hash identically — including the two `f64` ratios below (D4).
///
/// Derives `PartialEq` but **not** `Eq`/`Copy`: the `sharpe`/`sortino` `f64`
/// fields make `Eq` un-derivable (4.02, C1). `#[serde(default)]` (4.01, C5)
/// covers the two new fields too — they default to `None` for a pre-4.02 shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SummaryStats {
    /// Number of completed (closed) trades in the run.
    pub trade_count: usize,
    /// Number of trades with `realized_pnl > 0`.
    pub win_count: usize,
    /// Number of trades with `realized_pnl < 0`.
    pub loss_count: usize,
    /// Fraction of completed trades that were winners (`win_count / trade_count`),
    /// in `[0, 1]`. `0` when there are no trades (D4 zero-denominator guard).
    pub win_rate: Decimal,
    /// Sum of the positive `realized_pnl` across winning trades (always `>= 0`).
    pub gross_profit: Decimal,
    /// Sum of the magnitudes of the negative `realized_pnl` across losing trades
    /// (a positive magnitude, always `>= 0`).
    pub gross_loss: Decimal,
    /// Net P&L across the run — the alias of
    /// [`BacktestResult::net_pnl`](super::BacktestResult::net_pnl) (D1; not
    /// re-summed independently of the engine total).
    pub net_pnl: Decimal,
    /// `gross_profit / gross_loss`, or `None` when `gross_loss == 0` (D4 — no
    /// invented `Decimal` infinity).
    pub profit_factor: Option<Decimal>,
    /// Mean P&L of the winning trades (`gross_profit / win_count`); `0` when there
    /// are no winners (D4).
    pub avg_win: Decimal,
    /// Mean *loss magnitude* of the losing trades (`gross_loss / loss_count`, a
    /// positive magnitude); `0` when there are no losers (D4).
    pub avg_loss: Decimal,
    /// Mean P&L per trade (`net_pnl / trade_count`); `0` when there are no trades
    /// (D4).
    pub expectancy: Decimal,
    /// Maximum peak-to-trough decline over the derived equity curve, as a
    /// **positive magnitude** (`0` when the curve is monotonic or has < 2 points).
    pub max_drawdown: Decimal,
    /// The longest run of consecutive winning trades (in trade `seq` order).
    pub max_win_streak: usize,
    /// The longest run of consecutive losing trades (in trade `seq` order).
    pub max_loss_streak: usize,
    /// Total taker commission paid across the run — the alias of
    /// [`BacktestResult::fees_total`](super::BacktestResult::fees_total) (README
    /// C1 row 59; not re-summed, D1).
    pub commission_total: Decimal,
    /// Total signed funding P&L delta across the run — the alias of
    /// [`BacktestResult::funding_total`](super::BacktestResult::funding_total)
    /// (README C1 row 60; not re-summed, D1).
    pub funding_total: Decimal,
    /// Sharpe ratio over the per-trade `realized_r` series (risk-free `= 0`, NOT
    /// annualized): `mean / sample_stddev` with **Bessel `N−1`** sample stddev
    /// (D2). `None` when `trade_count < 2` OR `sample_stddev == 0`. The first of
    /// the slice's two `f64` statistics — finite-or-`None`, never `NaN`/`Inf`
    /// (D3); **oracle-excluded** (D4).
    pub sharpe: Option<f64>,
    /// Sortino ratio over the per-trade `realized_r` series (MAR `= 0`, NOT
    /// annualized): `mean / downside_deviation` with `downside_deviation =
    /// sqrt(Σ min(rᵢ,0)² / N)` — **divide by `N`**, not by the count-of-negatives
    /// (D2). `None` when `trade_count < 2` OR `downside_deviation == 0` (no
    /// negative `realized_r`). Finite-or-`None`, never `NaN`/`Inf` (D3);
    /// **oracle-excluded** (D4).
    pub sortino: Option<f64>,
}

impl SummaryStats {
    /// Compute the summary statistics from the final trade log + the run's
    /// already-computed totals (D1, pure / deterministic / `O(trades)`).
    ///
    /// `net_pnl`, `fees_total`, `funding_total` are the *engine* totals (passed
    /// through as aliases — `commission_total = fees_total`, `funding_total =
    /// funding_total` — never re-summed, D1). `max_drawdown` is read off the
    /// already-built [`EquityCurve`] so the curve construction is the single
    /// source of truth for the drawdown.
    ///
    /// Every ratio is guarded against a zero denominator (D4): no panic, no
    /// divide-by-zero, no `Decimal` infinity.
    #[must_use]
    pub fn from_trades(
        trades: &[Trade],
        net_pnl: Decimal,
        fees_total: Decimal,
        funding_total: Decimal,
        equity_curve: &EquityCurve,
    ) -> Self {
        let trade_count = trades.len();
        let mut win_count = 0usize;
        let mut loss_count = 0usize;
        let mut gross_profit = Decimal::ZERO;
        let mut gross_loss = Decimal::ZERO;
        let mut max_win_streak = 0usize;
        let mut max_loss_streak = 0usize;
        let mut cur_win_streak = 0usize;
        let mut cur_loss_streak = 0usize;

        for trade in trades {
            let pnl = trade.realized_pnl;
            match pnl.cmp(&Decimal::ZERO) {
                Ordering::Greater => {
                    win_count += 1;
                    gross_profit += pnl;
                    cur_win_streak += 1;
                    cur_loss_streak = 0;
                    if cur_win_streak > max_win_streak {
                        max_win_streak = cur_win_streak;
                    }
                }
                Ordering::Less => {
                    loss_count += 1;
                    // Accumulate the loss as a positive magnitude.
                    gross_loss += -pnl;
                    cur_loss_streak += 1;
                    cur_win_streak = 0;
                    if cur_loss_streak > max_loss_streak {
                        max_loss_streak = cur_loss_streak;
                    }
                }
                Ordering::Equal => {
                    // A break-even trade (`realized_pnl == 0`) breaks both streaks
                    // but counts as neither a win nor a loss.
                    cur_win_streak = 0;
                    cur_loss_streak = 0;
                }
            }
        }

        let win_rate = ratio(Decimal::from(win_count), Decimal::from(trade_count));
        let profit_factor = if gross_loss.is_zero() {
            None
        } else {
            Some(gross_profit / gross_loss)
        };
        let avg_win = ratio(gross_profit, Decimal::from(win_count));
        let avg_loss = ratio(gross_loss, Decimal::from(loss_count));
        let expectancy = ratio(net_pnl, Decimal::from(trade_count));
        let max_drawdown = equity_curve.max_drawdown();
        let (sharpe, sortino) = sharpe_sortino(trades);

        Self {
            trade_count,
            win_count,
            loss_count,
            win_rate,
            gross_profit,
            gross_loss,
            net_pnl,
            profit_factor,
            avg_win,
            avg_loss,
            expectancy,
            max_drawdown,
            max_win_streak,
            max_loss_streak,
            commission_total: fees_total,
            funding_total,
            sharpe,
            sortino,
        }
    }
}

/// Compute `(sharpe, sortino)` over the per-trade `realized_r` series (D2 — the
/// BINDING formula contract, pinned verbatim from README C1; NOT a re-derived
/// textbook variant). Risk-free `= 0`, MAR `= 0`, **NOT annualized**.
///
/// Let `N = trades.len()`, `rᵢ = trade.realized_r`, `mean = Σrᵢ / N`:
/// - **Sharpe** `= mean / sample_stddev`, `sample_stddev = sqrt(Σ(rᵢ−mean)² /
///   (N−1))` — **Bessel `N−1`**. `None` when `N < 2` OR `sample_stddev == 0`.
/// - **Sortino** `= mean / downside_deviation`, `downside_deviation =
///   sqrt(Σ min(rᵢ,0)² / N)` — **divide by `N`** (NOT count-of-negatives).
///   `None` when `N < 2` OR `downside_deviation == 0` (no negative `realized_r`).
///
/// **f64 quarantine (D1/D2):** the variance and downside sums are accumulated in
/// `Decimal` (byte-exact); each is converted to `f64` exactly ONCE before the
/// single `sqrt`/division. No `f64` arithmetic precedes that conversion.
///
/// **Finite-or-`None` (D3, audit C10):** every returned `Some(x)` is finite —
/// the `< 2` floor and the `== 0`-denominator floor together exclude the only
/// paths that could otherwise yield `NaN`/`Inf`; a non-finite result (defensive)
/// degrades to `None`, never escapes as `NaN`/`Inf`.
fn sharpe_sortino(trades: &[Trade]) -> (Option<f64>, Option<f64>) {
    let n = trades.len();
    // Fewer than two observations: sample stddev (N−1) is undefined; both None.
    if n < 2 {
        return (None, None);
    }
    let n_dec = Decimal::from(n);

    // mean = Σ rᵢ / N (Decimal, byte-exact).
    let mut sum = Decimal::ZERO;
    for trade in trades {
        sum += trade.realized_r;
    }
    let mean = sum / n_dec;

    // Σ(rᵢ−mean)² (sample variance numerator) and Σ min(rᵢ,0)² (downside numerator),
    // both accumulated in Decimal so no f64 arithmetic precedes the single conversion.
    let mut variance_num = Decimal::ZERO;
    let mut downside_num = Decimal::ZERO;
    for trade in trades {
        let r = trade.realized_r;
        let dev = r - mean;
        variance_num += dev * dev;
        if r < Decimal::ZERO {
            downside_num += r * r;
        }
    }

    // sample_stddev = sqrt( Σ(rᵢ−mean)² / (N−1) ) — Bessel N−1.
    let sample_var = variance_num / (n_dec - Decimal::ONE);
    let sample_stddev = decimal_to_f64(sample_var).sqrt();
    // downside_deviation = sqrt( Σ min(rᵢ,0)² / N ) — divide by N.
    let downside_var = downside_num / n_dec;
    let downside_deviation = decimal_to_f64(downside_var).sqrt();

    let mean_f64 = decimal_to_f64(mean);
    let sharpe = finite_ratio(mean_f64, sample_stddev);
    let sortino = finite_ratio(mean_f64, downside_deviation);
    (sharpe, sortino)
}

/// `numerator / denominator` as a finite `f64`, or `None` when the denominator is
/// `0` (the `sample_stddev == 0` / `downside_deviation == 0` floor, D2/D3) or the
/// quotient is not finite (defensive — never let `NaN`/`Inf` escape, D3/C10).
fn finite_ratio(numerator: f64, denominator: f64) -> Option<f64> {
    if denominator == 0.0 {
        return None;
    }
    let ratio = numerator / denominator;
    ratio.is_finite().then_some(ratio)
}

/// Convert a `Decimal` to `f64` for the single permitted `sqrt`/division (D2).
/// `Decimal`'s magnitude is bounded (≤ ~7.9e28), so the value is always
/// representable as a finite `f64`; `to_f64` is documented infallible for an
/// in-range `Decimal`, but a defensive `unwrap_or(0.0)` keeps the path total
/// (a `0.0` here only ever yields a `None` ratio downstream, never `NaN`/`Inf`).
fn decimal_to_f64(value: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    value.to_f64().unwrap_or(0.0)
}

/// A single point on the derived equity curve: the account equity at a point in
/// time (README C2). `Decimal`-only (NFR-2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EquityPoint {
    /// Point time, UTC epoch milliseconds. The leading point carries the run
    /// start time; each subsequent point carries a closed trade's `exit_fill_time`.
    pub time_ms: i64,
    /// Account equity at this point (`starting_equity + Σ realized_pnl` over the
    /// closed trades up to and including this one — **non-compounding**, constant
    /// base).
    pub equity: Decimal,
}

/// The derived, non-compounding equity curve over a run (README C2).
///
/// A leading point `(run_start_time_ms, starting_equity)` then one point per
/// **closed trade** in `seq` order, the equity stepping by each trade's
/// `realized_pnl` off a **constant** `starting_equity` base (matching the
/// constant-equity sizing — no compounding). Derived read-only; never persisted
/// as its own table (4.05 rebuilds it on read via the same [`from_trades`]).
///
/// [`from_trades`]: EquityCurve::from_trades
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct EquityCurve(pub Vec<EquityPoint>);

impl EquityCurve {
    /// Build the equity curve from the run-start time, the constant starting
    /// equity, and the final trade log (README C2 — the SINGLE reusable
    /// constructor: `into_result` calls it in-memory and 4.05 reuses it on the
    /// read path over the persisted trades).
    ///
    /// Pure, deterministic, `O(trades)`. The series is a leading point
    /// `(run_start_time_ms, starting_equity)` then one point per closed trade in
    /// order: `time_ms = trade.exit_fill_time`, `equity = starting_equity + Σ
    /// realized_pnl(0..=i)` — **non-compounding** (the base never changes; only
    /// the realized-P&L prefix sum accumulates).
    #[must_use]
    pub fn from_trades(run_start_time_ms: i64, starting_equity: Decimal, trades: &[Trade]) -> Self {
        let mut points = Vec::with_capacity(trades.len() + 1);
        points.push(EquityPoint {
            time_ms: run_start_time_ms,
            equity: starting_equity,
        });
        let mut running = starting_equity;
        for trade in trades {
            running += trade.realized_pnl;
            points.push(EquityPoint {
                time_ms: trade.exit_fill_time,
                equity: running,
            });
        }
        Self(points)
    }

    /// The maximum peak-to-trough decline over the curve, as a **positive
    /// magnitude** (README C1 row 56). `0` for a monotonic-non-decreasing curve or
    /// a curve with fewer than 2 points. A single pure `O(points)` pass tracking
    /// the running peak.
    #[must_use]
    pub fn max_drawdown(&self) -> Decimal {
        let mut peak = Decimal::ZERO;
        let mut max_dd = Decimal::ZERO;
        for (i, point) in self.0.iter().enumerate() {
            if i == 0 || point.equity > peak {
                peak = point.equity;
            }
            let drawdown = peak - point.equity;
            if drawdown > max_dd {
                max_dd = drawdown;
            }
        }
        max_dd
    }
}

/// Guarded ratio: `numerator / denominator`, or `Decimal::ZERO` when the
/// denominator is zero (D4 — total function, never a divide-by-zero panic).
fn ratio(numerator: Decimal, denominator: Decimal) -> Decimal {
    if denominator.is_zero() {
        Decimal::ZERO
    } else {
        numerator / denominator
    }
}

#[cfg(test)]
// `cast_precision_loss` / `cast_possible_wrap`: the hand-computed Sharpe/Sortino
// oracle helpers cast small `usize` counts to `f64`/`i64` — benign in test code
// over O(1)-sized series, and isolated to this `#[cfg(test)]` module (production
// `f64` goes through the `Decimal::to_f64` quarantine, never a raw `usize` cast).
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap
)]
mod tests {
    use super::{EquityCurve, EquityPoint, SummaryStats};
    use crate::domain::backtest::Regime;
    use crate::domain::{Direction, ExitReason, Trade, TradeSource};
    use rust_decimal::Decimal;

    fn d(value: i64) -> Decimal {
        Decimal::new(value, 0)
    }

    /// A `Trade` carrying only the fields the stats math reads (`realized_pnl`,
    /// `exit_fill_time`); the rest are filled with inert defaults.
    fn trade_with(realized_pnl: Decimal, exit_fill_time: i64) -> Trade {
        trade_with_r(realized_pnl, Decimal::ZERO, exit_fill_time)
    }

    /// A `Trade` carrying `realized_pnl`, `realized_r` (the Sharpe/Sortino input,
    /// `trade.rs:109`), and `exit_fill_time`; the rest inert defaults.
    fn trade_with_r(realized_pnl: Decimal, realized_r: Decimal, exit_fill_time: i64) -> Trade {
        Trade {
            direction: Direction::Long,
            qty: Decimal::ONE,
            entry_price: d(100),
            exit_price: d(100),
            entry_signal_time: 0,
            entry_fill_time: 0,
            exit_signal_time: exit_fill_time,
            exit_fill_time,
            fills: Vec::new(),
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            realized_pnl,
            realized_r,
            mfe_r: Decimal::ZERO,
            mae_r: Decimal::ZERO,
            exit_reason: ExitReason::Signal,
            source: TradeSource::Backtest,
            regime: Regime::Unknown,
            stop_price: None,
        }
    }

    /// Build a `SummaryStats` over a series of `realized_r` values (the only input
    /// Sharpe/Sortino read). `realized_pnl` mirrors `realized_r` (the stats math
    /// reads them independently — only `realized_r` feeds Sharpe/Sortino).
    fn stats_over_r(rs: &[Decimal]) -> SummaryStats {
        let trades: Vec<Trade> = rs
            .iter()
            .enumerate()
            .map(|(i, &r)| trade_with_r(r, r, i as i64))
            .collect();
        let net_pnl: Decimal = trades.iter().map(|t| t.realized_pnl).sum();
        let curve = EquityCurve::from_trades(0, d(1000), &trades);
        SummaryStats::from_trades(&trades, net_pnl, Decimal::ZERO, Decimal::ZERO, &curve)
    }

    /// Reference Sharpe over the same `realized_r` series, computed independently
    /// in plain `f64` with the Bessel `N−1` sample stddev — the hand-computed
    /// oracle the contract tests assert the production value matches (to a tight
    /// epsilon). Returns `None` for the same `N < 2` / zero-stddev floors.
    fn oracle_sharpe(rs: &[f64]) -> Option<f64> {
        let n = rs.len();
        if n < 2 {
            return None;
        }
        let mean = rs.iter().sum::<f64>() / n as f64;
        // `d * d` not `.powi(2)` — `powi` is a guard-banned call (the determinism
        // guard scans this whole file, test code included).
        let var = rs.iter().map(|r| (r - mean) * (r - mean)).sum::<f64>() / (n as f64 - 1.0);
        let sd = var.sqrt();
        if sd == 0.0 { None } else { Some(mean / sd) }
    }

    /// Reference Sortino over the same series: `mean / sqrt(Σ min(rᵢ,0)² / N)` —
    /// divide-by-`N` (NOT count-of-negatives). `None` for `N < 2` / zero-downside.
    fn oracle_sortino(rs: &[f64]) -> Option<f64> {
        let n = rs.len();
        if n < 2 {
            return None;
        }
        let mean = rs.iter().sum::<f64>() / n as f64;
        let dd = (rs
            .iter()
            .map(|r| {
                let m = r.min(0.0);
                m * m
            })
            .sum::<f64>()
            / n as f64)
            .sqrt();
        if dd == 0.0 { None } else { Some(mean / dd) }
    }

    /// AC-4 (`summary_stats_math`): the pure-Decimal/usize summary roll-ups —
    /// counts, win rate, gross profit/loss, profit factor, expectancy, win/loss
    /// streaks, and the commission/funding aliases — are computed correctly over a
    /// mixed win/loss/break-even trade log (NFR-2: all Decimal/usize, no f64).
    #[test]
    fn summary_stats_math() {
        // seq: +10, -4, +6, -2, -3, 0  → wins {10,6}=2, losses {4,2,3}=3,
        // break-even {0}=1 (counts as neither). 6 trades total.
        let trades = vec![
            trade_with(d(10), 10),
            trade_with(d(-4), 20),
            trade_with(d(6), 30),
            trade_with(d(-2), 40),
            trade_with(d(-3), 50),
            trade_with(d(0), 60),
        ];
        let net_pnl = d(10) - d(4) + d(6) - d(2) - d(3); // = 7
        let fees_total = d(12);
        let funding_total = d(-1);
        let curve = EquityCurve::from_trades(0, d(1000), &trades);
        let s = SummaryStats::from_trades(&trades, net_pnl, fees_total, funding_total, &curve);

        assert_eq!(s.trade_count, 6);
        assert_eq!(s.win_count, 2);
        assert_eq!(s.loss_count, 3);
        // win_rate = 2/6.
        assert_eq!(s.win_rate, d(2) / d(6));
        assert_eq!(s.gross_profit, d(16)); // 10 + 6
        assert_eq!(s.gross_loss, d(9)); // |−4| + |−2| + |−3| (positive magnitude)
        assert_eq!(s.net_pnl, d(7));
        // profit_factor = 16/9 (gross_loss != 0).
        assert_eq!(s.profit_factor, Some(d(16) / d(9)));
        assert_eq!(s.avg_win, d(16) / d(2)); // 8
        assert_eq!(s.avg_loss, d(9) / d(3)); // 3 (positive magnitude)
        assert_eq!(s.expectancy, d(7) / d(6)); // net / trade_count
        // streaks: longest win run = 1 (no two wins adjacent); longest loss run =
        // 2 (the −2, −3 back-to-back).
        assert_eq!(s.max_win_streak, 1);
        assert_eq!(s.max_loss_streak, 2);
        // Aliases — NOT re-summed (D1).
        assert_eq!(s.commission_total, fees_total);
        assert_eq!(s.funding_total, funding_total);
    }

    /// AC-4 zero-trade guard: an empty run has all-zero stats and NO panic /
    /// divide-by-zero (D4). `profit_factor` is `None` (`gross_loss == 0`) and every
    /// ratio is `Decimal::ZERO`.
    #[test]
    fn summary_stats_math_empty_run_is_all_zero() {
        let trades: Vec<Trade> = Vec::new();
        let curve = EquityCurve::from_trades(0, d(1000), &trades);
        let s =
            SummaryStats::from_trades(&trades, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, &curve);
        assert_eq!(s.trade_count, 0);
        assert_eq!(s.win_rate, Decimal::ZERO);
        assert_eq!(s.avg_win, Decimal::ZERO);
        assert_eq!(s.avg_loss, Decimal::ZERO);
        assert_eq!(s.expectancy, Decimal::ZERO);
        assert_eq!(s.profit_factor, None);
        assert_eq!(s.max_drawdown, Decimal::ZERO);
        assert_eq!(s.max_win_streak, 0);
        assert_eq!(s.max_loss_streak, 0);
    }

    /// AC-5 (`equity_curve_construction`): the curve is a leading
    /// `(run_start, starting_equity)` point then one non-compounding point per
    /// closed trade, `time_ms = exit_fill_time` and `equity = starting_equity + Σ
    /// realized_pnl(0..=i)` over a CONSTANT base. Final equity == starting + net.
    #[test]
    fn equity_curve_construction() {
        let trades = vec![
            trade_with(d(10), 100),
            trade_with(d(-4), 200),
            trade_with(d(6), 300),
        ];
        let start = d(1000);
        let curve = EquityCurve::from_trades(50, start, &trades);

        // 1 leading point + one per trade.
        assert_eq!(curve.0.len(), trades.len() + 1);
        // Leading point is exactly (run_start, starting_equity).
        assert_eq!(
            curve.0[0],
            EquityPoint {
                time_ms: 50,
                equity: start,
            }
        );
        // Non-compounding prefix sums off the constant base.
        assert_eq!(
            curve.0[1],
            EquityPoint {
                time_ms: 100,
                equity: d(1010),
            }
        );
        assert_eq!(
            curve.0[2],
            EquityPoint {
                time_ms: 200,
                equity: d(1006),
            }
        );
        assert_eq!(
            curve.0[3],
            EquityPoint {
                time_ms: 300,
                equity: d(1012),
            }
        );
        // Final equity == starting + net_pnl (10 − 4 + 6 = 12).
        let net: Decimal = trades.iter().map(|t| t.realized_pnl).sum();
        assert_eq!(curve.0.last().unwrap().equity, start + net);
    }

    /// AC-5 edge: a run with no trades yields a single leading point at
    /// `(run_start, starting_equity)`.
    #[test]
    fn equity_curve_construction_empty_run_is_single_leading_point() {
        let curve = EquityCurve::from_trades(7, d(500), &[]);
        assert_eq!(curve.0.len(), 1);
        assert_eq!(
            curve.0[0],
            EquityPoint {
                time_ms: 7,
                equity: d(500),
            }
        );
    }

    /// AC-6 (`equity_curve_max_drawdown`): the max peak-to-trough decline is a
    /// positive magnitude; `0` for a monotonic-non-decreasing curve and for a
    /// curve with fewer than 2 points.
    #[test]
    fn equity_curve_max_drawdown() {
        // Equity path off 1000: +100 → 1100 (peak), −300 → 800 (trough −300 from
        // 1100), +50 → 850, −20 → 830, +400 → 1230. The deepest peak-to-trough
        // decline is 1100 − 800 = 300.
        let trades = vec![
            trade_with(d(100), 1),
            trade_with(d(-300), 2),
            trade_with(d(50), 3),
            trade_with(d(-20), 4),
            trade_with(d(400), 5),
        ];
        let curve = EquityCurve::from_trades(0, d(1000), &trades);
        assert_eq!(curve.max_drawdown(), d(300));

        // Monotonic non-decreasing → zero drawdown.
        let up = vec![
            trade_with(d(5), 1),
            trade_with(d(7), 2),
            trade_with(d(0), 3),
        ];
        let up_curve = EquityCurve::from_trades(0, d(100), &up);
        assert_eq!(up_curve.max_drawdown(), Decimal::ZERO);

        // Single leading point (no trades) → zero drawdown (< 2 points).
        let empty = EquityCurve::from_trades(0, d(100), &[]);
        assert_eq!(empty.max_drawdown(), Decimal::ZERO);
    }

    /// AC-7 (`profit_factor_none_on_zero_gross_loss`): when there are no losing
    /// trades (`gross_loss == 0`) `profit_factor` is `None` — no invented Decimal
    /// infinity (D4) — while a run WITH losses yields `Some(gross_profit /
    /// gross_loss)`.
    #[test]
    fn profit_factor_none_on_zero_gross_loss() {
        // All-winners (and a break-even): gross_loss == 0 ⇒ None.
        let winners = vec![
            trade_with(d(10), 1),
            trade_with(d(5), 2),
            trade_with(d(0), 3),
        ];
        let curve = EquityCurve::from_trades(0, d(1000), &winners);
        let s = SummaryStats::from_trades(&winners, d(15), Decimal::ZERO, Decimal::ZERO, &curve);
        assert_eq!(s.gross_loss, Decimal::ZERO);
        assert_eq!(
            s.profit_factor, None,
            "no losses ⇒ profit_factor is None (D4)"
        );

        // With a loss, profit_factor is Some(gross_profit / gross_loss).
        let mixed = vec![trade_with(d(10), 1), trade_with(d(-4), 2)];
        let curve2 = EquityCurve::from_trades(0, d(1000), &mixed);
        let s2 = SummaryStats::from_trades(&mixed, d(6), Decimal::ZERO, Decimal::ZERO, &curve2);
        assert_eq!(s2.profit_factor, Some(d(10) / d(4)));
    }

    /// How close two `f64`s must be to count as equal here. Sharpe/Sortino fold a
    /// few correctly-rounded ops over a `sqrt`; a tight absolute epsilon is ample
    /// (the values under test are O(1)).
    const EPS: f64 = 1e-12;

    fn dr(value: i64, scale: u32) -> Decimal {
        Decimal::new(value, scale)
    }

    /// AC-1 (`stats_sharpe`): Sharpe over a mixed `realized_r` series equals the
    /// hand-computed oracle (`mean / sample_stddev`, Bessel `N−1`), to a tight
    /// epsilon. NFR-2: the f64 is oracle-excluded, so a tiny per-arch ulp wobble
    /// here never touches the byte-identity oracle.
    #[test]
    fn stats_sharpe() {
        // realized_r = {2, -1, 0.5, 1.5, -0.5}.
        let rs = [dr(20, 1), dr(-10, 1), dr(5, 1), dr(15, 1), dr(-5, 1)];
        let rs_f = [2.0_f64, -1.0, 0.5, 1.5, -0.5];
        let s = stats_over_r(&rs);
        let want = oracle_sharpe(&rs_f).expect("non-degenerate series ⇒ Some");
        let got = s.sharpe.expect("non-degenerate series ⇒ Some");
        assert!(
            (got - want).abs() < EPS,
            "sharpe {got} != oracle {want} (mean/sample_stddev, Bessel N−1)"
        );
    }

    /// AC-2 (`stats_sortino`): Sortino over the same series equals the oracle
    /// (`mean / downside_deviation`, `Σ min(rᵢ,0)² / N`).
    #[test]
    fn stats_sortino() {
        let rs = [dr(20, 1), dr(-10, 1), dr(5, 1), dr(15, 1), dr(-5, 1)];
        let rs_f = [2.0_f64, -1.0, 0.5, 1.5, -0.5];
        let s = stats_over_r(&rs);
        let want = oracle_sortino(&rs_f).expect("has negatives ⇒ Some");
        let got = s.sortino.expect("has negatives ⇒ Some");
        assert!(
            (got - want).abs() < EPS,
            "sortino {got} != oracle {want} (mean/downside_deviation, ÷N)"
        );
    }

    /// AC-3 (`sharpe_sortino_none_below_two_trades`): with `< 2` trades the sample
    /// stddev (N−1) is undefined, so BOTH ratios are `None` (D3). Also: a series
    /// with no negative `realized_r` ⇒ `downside_deviation == 0` ⇒ `sortino` is
    /// `None`; a constant series ⇒ `sample_stddev == 0` ⇒ `sharpe` is `None`.
    #[test]
    fn sharpe_sortino_none_below_two_trades() {
        // Zero trades: both None.
        assert_eq!(stats_over_r(&[]).sharpe, None);
        assert_eq!(stats_over_r(&[]).sortino, None);
        // One trade: both None (N−1 = 0).
        let one = stats_over_r(&[dr(13, 1)]);
        assert_eq!(one.sharpe, None);
        assert_eq!(one.sortino, None);
        // Two equal (positive) trades: sample_stddev == 0 ⇒ sharpe None; no
        // negative ⇒ downside_deviation == 0 ⇒ sortino None.
        let flat = stats_over_r(&[dr(15, 1), dr(15, 1)]);
        assert_eq!(flat.sharpe, None, "zero stddev ⇒ sharpe None (D2/D3)");
        assert_eq!(flat.sortino, None, "no downside ⇒ sortino None (D2/D3)");
    }

    /// AC-15 (`sharpe_uses_sample_stddev_bessel`): the denominator is the SAMPLE
    /// stddev (Bessel `N−1`), NOT the population stddev (`N`). The test pins this
    /// by computing BOTH variants and asserting the production value matches the
    /// `N−1` one and is strictly distinguishable from the `N` one.
    #[test]
    fn sharpe_uses_sample_stddev_bessel() {
        let rs = [dr(20, 1), dr(-10, 1), dr(5, 1), dr(15, 1), dr(-5, 1)];
        let rs_f = [2.0_f64, -1.0, 0.5, 1.5, -0.5];
        let n = rs_f.len() as f64;
        let mean = rs_f.iter().sum::<f64>() / n;
        let ss = rs_f.iter().map(|r| (r - mean) * (r - mean)).sum::<f64>();
        let sharpe_bessel = mean / (ss / (n - 1.0)).sqrt(); // N−1 (correct)
        let sharpe_population = mean / (ss / n).sqrt(); // N (wrong variant)

        let got = stats_over_r(&rs).sharpe.expect("Some");
        assert!(
            (got - sharpe_bessel).abs() < EPS,
            "sharpe must use the Bessel N−1 sample stddev: got {got}, N−1 {sharpe_bessel}"
        );
        assert!(
            (got - sharpe_population).abs() > 1e-6,
            "sharpe must NOT use the population (÷N) stddev {sharpe_population}"
        );
    }

    /// AC-16 (`sortino_downside_deviation_divides_by_n`): `downside_deviation`
    /// divides the squared-downside sum by `N` (total trades), NOT by the
    /// count-of-negatives. Pins it by computing both and asserting ÷N matches
    /// while ÷count-of-negatives is strictly distinguishable.
    #[test]
    fn sortino_downside_deviation_divides_by_n() {
        // 4 trades, only 1 negative ⇒ ÷N (4) and ÷neg-count (1) diverge sharply.
        let rs = [dr(30, 1), dr(10, 1), dr(-20, 1), dr(5, 1)];
        let rs_f = [3.0_f64, 1.0, -2.0, 0.5];
        let n = rs_f.len() as f64;
        let neg_count = rs_f.iter().filter(|r| **r < 0.0).count() as f64;
        let mean = rs_f.iter().sum::<f64>() / n;
        let downside_sumsq = rs_f
            .iter()
            .map(|r| {
                let m = r.min(0.0);
                m * m
            })
            .sum::<f64>();
        let sortino_div_n = mean / (downside_sumsq / n).sqrt(); // ÷N (correct)
        let sortino_div_negcount = mean / (downside_sumsq / neg_count).sqrt(); // wrong

        let got = stats_over_r(&rs).sortino.expect("Some");
        assert!(
            (got - sortino_div_n).abs() < EPS,
            "sortino downside_deviation must divide by N: got {got}, ÷N {sortino_div_n}"
        );
        assert!(
            (got - sortino_div_negcount).abs() > 1e-6,
            "sortino must NOT divide by the count-of-negatives {sortino_div_negcount}"
        );
    }

    /// AC-17 (`sharpe_sortino_finite_or_none_never_nan_inf`): at the type boundary
    /// every returned `Some(x)` is finite — never `NaN`/`Inf` (D3, audit C10) —
    /// across a battery of series including the degenerate ones (empty, single,
    /// all-equal, all-positive, all-negative, large-magnitude).
    #[test]
    fn sharpe_sortino_finite_or_none_never_nan_inf() {
        let series: Vec<Vec<Decimal>> = vec![
            vec![],
            vec![dr(5, 1)],
            vec![dr(15, 1), dr(15, 1)], // constant ⇒ sharpe None, no downside ⇒ sortino None
            vec![dr(10, 1), dr(20, 1), dr(30, 1)], // all positive
            vec![dr(-10, 1), dr(-20, 1), dr(-30, 1)], // all negative
            vec![dr(20, 1), dr(-10, 1), dr(5, 1), dr(15, 1), dr(-5, 1)],
            vec![Decimal::new(999_999_999, 0), Decimal::new(-999_999_999, 0)],
        ];
        for rs in series {
            let s = stats_over_r(&rs);
            if let Some(x) = s.sharpe {
                assert!(x.is_finite(), "sharpe must be finite-or-None, got {x}");
            }
            if let Some(x) = s.sortino {
                assert!(x.is_finite(), "sortino must be finite-or-None, got {x}");
            }
        }
    }
}
