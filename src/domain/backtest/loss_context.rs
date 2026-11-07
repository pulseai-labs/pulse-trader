//! Losses and losing streaks by context at entry (r6.s3.w4, PDR-0046 decision 4).
//!
//! A pure fold. It takes a run's trades, in run order, with one [`TradeContext`] per
//! trade, and counts trades, losses, net R and losing streaks in four groupings: the
//! 4H swing trend relative to the trade's side, the 15m [`Regime`], the volatility
//! tercile and the UTC hour. It reads nothing, stores nothing and decides nothing
//! (STR-0003: "They describe; they do not decide").
//!
//! The application ring rebuilds each trade's context from the run's own pinned
//! candle snapshots (`application::loss_context`). Nothing here knows about candles.

use rust_decimal::Decimal;
use serde::Serialize;

use super::regime::Regime;
use super::trade::Trade;

/// The 4H swing trend at entry, relative to the trade's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrendSide {
    /// The swings trend in the trade's direction.
    With,
    /// The swings do not trend.
    Ranging,
    /// The swings trend against the trade's direction.
    Against,
    /// The trend cannot be read: a swing is not yet confirmed, the bar is missing, or
    /// the run has no higher timeframe.
    Unknown,
}

/// The run's volatility tercile of a trade's `ATR ÷ price` at entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VolatilityBand {
    /// Below the first cut point.
    Low,
    /// At or above the first cut point, below the second.
    Mid,
    /// At or above the second cut point.
    High,
    /// The ratio is unknown, or fewer than three ratios are known.
    Unknown,
}

/// One trade's context at its entry signal bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TradeContext {
    /// The 4H swing trend relative to the trade's side.
    pub trend: TrendSide,
    /// ATR(14, primary) ÷ the signal bar's close; `None` while ATR is unwarm.
    pub atr_ratio: Option<Decimal>,
    /// UTC hour (0–23) of `entry_fill_time`.
    pub hour_utc: u8,
}

/// One cell of one grouping.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContextCell {
    /// Trades in the cell.
    pub trades: usize,
    /// Losing trades in the cell (`realized_pnl < 0`).
    pub losses: usize,
    /// Sum of `realized_r` (net R, w1) over the cell's trades.
    pub net_r: Decimal,
    /// Sum of `realized_pnl` over the cell's trades.
    pub net_pnl: Decimal,
    /// Losing streaks (≥ 2 losses in a row) whose first loss falls in this cell.
    pub losing_streaks: usize,
    /// The longest such streak, in trades; 0 when none.
    pub longest_losing_streak: usize,
}

/// The fold's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LossContextReport {
    /// Trades in the run.
    pub trades: usize,
    /// Losing trades in the run.
    pub losses: usize,
    /// By trend relative to the side. Order: with, ranging, against, unknown.
    pub by_trend: Vec<(TrendSide, ContextCell)>,
    /// By the 15m regime at entry. Order: `trending_up`, `trending_down`, `ranging`,
    /// `unknown`.
    pub by_regime: Vec<(Regime, ContextCell)>,
    /// The tercile cut points of `atr_ratio` (low < `cut_1` ≤ mid < `cut_2` ≤ high);
    /// `None` with fewer than 3 known ratios.
    pub volatility_cuts: Option<(Decimal, Decimal)>,
    /// By volatility tercile. Order: low, mid, high, unknown.
    pub by_volatility: Vec<(VolatilityBand, ContextCell)>,
    /// By UTC hour of the entry fill. Hours 0..=23, all 24 present.
    pub by_hour: Vec<(u8, ContextCell)>,
}

/// `trades` and `contexts` had different lengths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("loss_context: {trades} trades but {contexts} contexts")]
pub struct ContextLengthMismatch {
    /// The number of trades.
    pub trades: usize,
    /// The number of contexts.
    pub contexts: usize,
}

const TRENDS: [TrendSide; 4] = [
    TrendSide::With,
    TrendSide::Ranging,
    TrendSide::Against,
    TrendSide::Unknown,
];
const REGIMES: [Regime; 4] = [
    Regime::TrendingUp,
    Regime::TrendingDown,
    Regime::Ranging,
    Regime::Unknown,
];
const BANDS: [VolatilityBand; 4] = [
    VolatilityBand::Low,
    VolatilityBand::Mid,
    VolatilityBand::High,
    VolatilityBand::Unknown,
];
const HOURS: usize = 24;

/// Where one trade falls in each grouping: an index into that grouping's cells.
#[derive(Debug, Clone, Copy)]
struct Placement {
    trend: usize,
    regime: usize,
    band: usize,
    hour: usize,
}

/// The four groupings' cells, indexed by [`Placement`].
struct Grid {
    trend: [ContextCell; 4],
    regime: [ContextCell; 4],
    band: [ContextCell; 4],
    hour: [ContextCell; HOURS],
}

impl Grid {
    fn new() -> Self {
        Self {
            trend: Default::default(),
            regime: Default::default(),
            band: Default::default(),
            hour: std::array::from_fn(|_| ContextCell::default()),
        }
    }

    /// Apply `update` to the one cell of each grouping the placement names.
    fn each(&mut self, at: Placement, mut update: impl FnMut(&mut ContextCell)) {
        update(&mut self.trend[at.trend]);
        update(&mut self.regime[at.regime]);
        update(&mut self.band[at.band]);
        update(&mut self.hour[at.hour]);
    }
}

/// The tercile cut points of the known ratios, or `None` with fewer than three.
fn tercile_cuts(contexts: &[TradeContext]) -> Option<(Decimal, Decimal)> {
    let mut ratios: Vec<Decimal> = contexts.iter().filter_map(|c| c.atr_ratio).collect();
    let n = ratios.len();
    if n < 3 {
        return None;
    }
    ratios.sort_unstable();
    Some((ratios[n / 3], ratios[2 * n / 3]))
}

/// A known ratio's band under `cuts`.
fn band_of(ratio: Decimal, (cut_1, cut_2): (Decimal, Decimal)) -> VolatilityBand {
    if ratio < cut_1 {
        VolatilityBand::Low
    } else if ratio < cut_2 {
        VolatilityBand::Mid
    } else {
        VolatilityBand::High
    }
}

/// Fold `trades` (in run order) with their `contexts` (same length, same order).
///
/// - A **loss** is `realized_pnl < 0` (the [`SummaryStats`](super::SummaryStats)
///   convention). A break-even trade is not a loss and ends a streak.
/// - A **losing streak** is a maximal run of at least two consecutive losses in trade
///   order. It counts once, in each grouping, in the cell of its **first** loss;
///   `longest_losing_streak` is the longest streak counted in that cell.
/// - **Volatility terciles** come from the trades' known ratios, sorted ascending,
///   `n` of them. With `n ≥ 3`, `cut_1` is the value at index `n / 3` and `cut_2` the
///   value at index `2 * n / 3`; a ratio below `cut_1` is `Low`, below `cut_2` is
///   `Mid`, else `High`. With `n < 3` there are no cuts and every trade is `Unknown`;
///   a trade with no ratio is always `Unknown`.
/// - Every listed cell is present, even when empty, in a fixed order.
///
/// # Errors
///
/// [`ContextLengthMismatch`] when `contexts` and `trades` differ in length. No partial
/// report is built.
pub fn loss_context(
    trades: &[Trade],
    contexts: &[TradeContext],
) -> Result<LossContextReport, ContextLengthMismatch> {
    if trades.len() != contexts.len() {
        return Err(ContextLengthMismatch {
            trades: trades.len(),
            contexts: contexts.len(),
        });
    }
    let cuts = tercile_cuts(contexts);
    let placements: Vec<Placement> = trades
        .iter()
        .zip(contexts)
        .map(|(trade, context)| Placement {
            trend: TRENDS.iter().position(|t| *t == context.trend).unwrap_or(3),
            regime: REGIMES.iter().position(|r| *r == trade.regime).unwrap_or(3),
            band: match (context.atr_ratio, cuts) {
                (Some(ratio), Some(cuts)) => band_of(ratio, cuts) as usize,
                _ => VolatilityBand::Unknown as usize,
            },
            hour: usize::from(context.hour_utc).min(HOURS - 1),
        })
        .collect();

    let mut grid = Grid::new();
    let mut losses = 0_usize;
    // The streak being walked: its first loss's placement and its length so far.
    let mut streak: Option<(Placement, usize)> = None;
    for (trade, at) in trades.iter().zip(&placements) {
        let is_loss = trade.realized_pnl < Decimal::ZERO;
        losses += usize::from(is_loss);
        grid.each(*at, |cell| {
            cell.trades += 1;
            cell.losses += usize::from(is_loss);
            cell.net_r += trade.realized_r;
            cell.net_pnl += trade.realized_pnl;
        });
        if is_loss {
            streak = Some(match streak {
                Some((first, length)) => (first, length + 1),
                None => (*at, 1),
            });
        } else if let Some((first, length)) = streak.take() {
            close_streak(&mut grid, first, length);
        }
    }
    if let Some((first, length)) = streak {
        close_streak(&mut grid, first, length);
    }

    let Grid {
        trend,
        regime,
        band,
        hour,
    } = grid;
    Ok(LossContextReport {
        trades: trades.len(),
        losses,
        by_trend: TRENDS.into_iter().zip(trend).collect(),
        by_regime: REGIMES.into_iter().zip(regime).collect(),
        volatility_cuts: cuts,
        by_volatility: BANDS.into_iter().zip(band).collect(),
        by_hour: (0..=23_u8).zip(hour).collect(),
    })
}

/// Count a finished streak in the cell of its first loss, in each grouping. One loss
/// is not a streak.
fn close_streak(grid: &mut Grid, first: Placement, length: usize) {
    if length < 2 {
        return;
    }
    grid.each(first, |cell| {
        cell.losing_streaks += 1;
        cell.longest_losing_streak = cell.longest_losing_streak.max(length);
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        ContextCell, ContextLengthMismatch, LossContextReport, TradeContext, TrendSide,
        VolatilityBand, loss_context,
    };
    use crate::domain::Direction;
    use crate::domain::backtest::{ExitReason, Regime, Trade, TradeSource};
    use rust_decimal::Decimal;

    /// A trade with `pnl` and net R `pnl / 100`, in `regime`.
    fn trade(pnl: i64, regime: Regime) -> Trade {
        Trade {
            direction: Direction::Long,
            qty: Decimal::ONE,
            entry_price: Decimal::new(100, 0),
            exit_price: Decimal::new(100, 0),
            entry_signal_time: 0,
            entry_fill_time: 0,
            exit_signal_time: 0,
            exit_fill_time: 0,
            fills: vec![],
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            realized_pnl: Decimal::new(pnl, 0),
            realized_r: Decimal::new(pnl, 2),
            mfe_r: Decimal::ZERO,
            mae_r: Decimal::ZERO,
            exit_reason: ExitReason::StopLoss,
            source: TradeSource::Backtest,
            regime,
            stop_price: None,
            setup: None,
        }
    }

    /// A context: trend, ATR ratio in thousandths (`None` = unwarm), UTC hour.
    fn ctx(trend: TrendSide, ratio_milli: Option<i64>, hour_utc: u8) -> TradeContext {
        TradeContext {
            trend,
            atr_ratio: ratio_milli.map(|milli| Decimal::new(milli, 3)),
            hour_utc,
        }
    }

    /// Trades with `pnl`s, one per ratio, all in one regime and hour, ratios in
    /// the order given.
    fn by_ratio(ratios: &[Option<i64>]) -> (Vec<Trade>, Vec<TradeContext>) {
        let trades = ratios.iter().map(|_| trade(1, Regime::Ranging)).collect();
        let contexts = ratios
            .iter()
            .map(|ratio| ctx(TrendSide::With, *ratio, 0))
            .collect();
        (trades, contexts)
    }

    fn band(report: &LossContextReport, band: VolatilityBand) -> &ContextCell {
        &report
            .by_volatility
            .iter()
            .find(|(b, _)| *b == band)
            .expect("every band is listed")
            .1
    }

    fn trend(report: &LossContextReport, trend: TrendSide) -> &ContextCell {
        &report
            .by_trend
            .iter()
            .find(|(t, _)| *t == trend)
            .expect("every trend is listed")
            .1
    }

    /// The trade counts per band, `[low, mid, high, unknown]`.
    fn band_counts(report: &LossContextReport) -> [usize; 4] {
        [
            VolatilityBand::Low,
            VolatilityBand::Mid,
            VolatilityBand::High,
            VolatilityBand::Unknown,
        ]
        .map(|b| band(report, b).trades)
    }

    #[test]
    fn loss_context_a_streak_counts_in_the_cell_of_its_first_loss() {
        // Three losses in a row, each in a different cell of every grouping, then a win.
        let trades = [
            trade(-100, Regime::TrendingUp),
            trade(-100, Regime::TrendingDown),
            trade(-100, Regime::Ranging),
            trade(50, Regime::Ranging),
        ];
        let contexts = [
            ctx(TrendSide::With, Some(1), 1),
            ctx(TrendSide::Against, Some(2), 2),
            ctx(TrendSide::Ranging, Some(3), 3),
            ctx(TrendSide::With, Some(4), 4),
        ];
        let report = loss_context(&trades, &contexts).unwrap();

        // The streak (length 3) sits in the first loss's cell and nowhere else.
        let with = trend(&report, TrendSide::With);
        assert_eq!((with.losing_streaks, with.longest_losing_streak), (1, 3));
        for other in [TrendSide::Ranging, TrendSide::Against, TrendSide::Unknown] {
            let cell = trend(&report, other);
            assert_eq!((cell.losing_streaks, cell.longest_losing_streak), (0, 0));
        }
        let regimes: Vec<(usize, usize)> = report
            .by_regime
            .iter()
            .map(|(_, c)| (c.losing_streaks, c.longest_losing_streak))
            .collect();
        assert_eq!(
            regimes,
            [(1, 3), (0, 0), (0, 0), (0, 0)],
            "trending_up first"
        );
        let hours: Vec<usize> = report
            .by_hour
            .iter()
            .map(|(_, c)| c.losing_streaks)
            .collect();
        assert_eq!(hours.iter().sum::<usize>(), 1);
        assert_eq!(hours[1], 1, "the first loss was at hour 1");
        // Each loss still counts as a loss in its own cell.
        assert_eq!(trend(&report, TrendSide::Against).losses, 1);
        assert_eq!(trend(&report, TrendSide::Ranging).losses, 1);
    }

    #[test]
    fn loss_context_a_break_even_trade_splits_a_streak_and_is_not_a_loss() {
        // L L 0 L L L: two streaks (2 then 3), the break-even between is no loss.
        let pnls = [-10, -10, 0, -10, -10, -10];
        let trades: Vec<Trade> = pnls.iter().map(|p| trade(*p, Regime::Ranging)).collect();
        let contexts = [
            ctx(TrendSide::With, None, 0),
            ctx(TrendSide::With, None, 0),
            ctx(TrendSide::With, None, 0),
            ctx(TrendSide::Against, None, 0),
            ctx(TrendSide::Against, None, 0),
            ctx(TrendSide::Against, None, 0),
        ];
        let report = loss_context(&trades, &contexts).unwrap();
        assert_eq!((report.trades, report.losses), (6, 5));
        let with = trend(&report, TrendSide::With);
        assert_eq!((with.losing_streaks, with.longest_losing_streak), (1, 2));
        let against = trend(&report, TrendSide::Against);
        assert_eq!(
            (against.losing_streaks, against.longest_losing_streak),
            (1, 3)
        );
        // Unsplit, the same five losses would be ONE streak of 5 in `with`.
        assert_eq!(with.losses, 2, "the break-even trade is not a loss");
    }

    #[test]
    fn loss_context_a_single_loss_is_not_a_streak() {
        let trades = [
            trade(-10, Regime::Ranging),
            trade(10, Regime::Ranging),
            trade(-10, Regime::Ranging),
            trade(10, Regime::Ranging),
            trade(-10, Regime::Ranging),
        ];
        let contexts = [ctx(TrendSide::With, None, 5); 5];
        let report = loss_context(&trades, &contexts).unwrap();
        assert_eq!(report.losses, 3);
        for (_, cell) in &report.by_trend {
            assert_eq!((cell.losing_streaks, cell.longest_losing_streak), (0, 0));
        }
        for (_, cell) in &report.by_hour {
            assert_eq!(cell.losing_streaks, 0);
        }
    }

    #[test]
    fn loss_context_a_streak_still_open_at_the_last_trade_counts() {
        let trades = [
            trade(10, Regime::Ranging),
            trade(-10, Regime::Ranging),
            trade(-10, Regime::Ranging),
        ];
        let contexts = [
            ctx(TrendSide::With, None, 0),
            ctx(TrendSide::Against, None, 0),
            ctx(TrendSide::With, None, 0),
        ];
        let report = loss_context(&trades, &contexts).unwrap();
        let against = trend(&report, TrendSide::Against);
        assert_eq!(
            (against.losing_streaks, against.longest_losing_streak),
            (1, 2)
        );
    }

    #[test]
    fn loss_context_terciles_with_three_trades() {
        // Known ratios 1, 2, 3 (thousandths), given out of order: n = 3, so cut_1 is
        // index 1 and cut_2 is index 2 of the sorted values.
        let (trades, contexts) = by_ratio(&[Some(3), Some(1), Some(2)]);
        let report = loss_context(&trades, &contexts).unwrap();
        assert_eq!(
            report.volatility_cuts,
            Some((Decimal::new(2, 3), Decimal::new(3, 3)))
        );
        assert_eq!(band_counts(&report), [1, 1, 1, 0]);
    }

    #[test]
    fn loss_context_terciles_with_four_trades() {
        // n = 4: cut_1 at 4 / 3 = 1, cut_2 at 8 / 3 = 2 (integer division).
        let (trades, contexts) = by_ratio(&[Some(4), Some(1), Some(3), Some(2)]);
        let report = loss_context(&trades, &contexts).unwrap();
        assert_eq!(
            report.volatility_cuts,
            Some((Decimal::new(2, 3), Decimal::new(3, 3)))
        );
        assert_eq!(
            band_counts(&report),
            [1, 1, 2, 0],
            "low 1; mid 2; high 3 and 4"
        );
    }

    #[test]
    fn loss_context_terciles_with_nine_trades() {
        // n = 9: cut_1 at index 3, cut_2 at index 6 — three trades in each band.
        let (trades, contexts) = by_ratio(&[
            Some(9),
            Some(2),
            Some(7),
            Some(4),
            Some(1),
            Some(8),
            Some(5),
            Some(3),
            Some(6),
        ]);
        let report = loss_context(&trades, &contexts).unwrap();
        assert_eq!(
            report.volatility_cuts,
            Some((Decimal::new(4, 3), Decimal::new(7, 3)))
        );
        assert_eq!(band_counts(&report), [3, 3, 3, 0]);
    }

    #[test]
    fn loss_context_fewer_than_three_known_ratios_have_no_cuts() {
        for ratios in [vec![], vec![Some(1)], vec![Some(1), Some(2)]] {
            let (trades, contexts) = by_ratio(&ratios);
            let report = loss_context(&trades, &contexts).unwrap();
            assert_eq!(
                report.volatility_cuts,
                None,
                "{} known ratios",
                ratios.len()
            );
            assert_eq!(band_counts(&report), [0, 0, 0, ratios.len()]);
        }
        // Two known and one unwarm is still two known: no cuts.
        let (trades, contexts) = by_ratio(&[Some(1), None, Some(2)]);
        let report = loss_context(&trades, &contexts).unwrap();
        assert_eq!(report.volatility_cuts, None);
        assert_eq!(band_counts(&report), [0, 0, 0, 3]);
    }

    #[test]
    fn loss_context_an_unwarm_ratio_is_unknown_even_when_cuts_exist() {
        let (trades, contexts) = by_ratio(&[Some(1), Some(2), Some(3), None]);
        let report = loss_context(&trades, &contexts).unwrap();
        assert!(report.volatility_cuts.is_some(), "three known ratios");
        assert_eq!(band_counts(&report), [1, 1, 1, 1]);
    }

    #[test]
    fn loss_context_a_ratio_equal_to_a_cut_falls_in_the_upper_band() {
        // n = 6, sorted 1 1 2 2 3 3: cut_1 = 2 (index 2), cut_2 = 3 (index 4).
        // A ratio equal to cut_1 is Mid, and one equal to cut_2 is High.
        let (trades, contexts) = by_ratio(&[Some(1), Some(1), Some(2), Some(2), Some(3), Some(3)]);
        let report = loss_context(&trades, &contexts).unwrap();
        assert_eq!(
            report.volatility_cuts,
            Some((Decimal::new(2, 3), Decimal::new(3, 3)))
        );
        assert_eq!(band_counts(&report), [2, 2, 2, 0]);
    }

    #[test]
    fn loss_context_equal_cuts_leave_no_mid_band() {
        // n = 6, sorted 1 2 2 2 2 3: cut_1 = cut_2 = 2, so a ratio below 2 is Low and
        // every other is High — nothing is Mid.
        let (trades, contexts) = by_ratio(&[Some(2), Some(1), Some(2), Some(2), Some(3), Some(2)]);
        let report = loss_context(&trades, &contexts).unwrap();
        assert_eq!(
            report.volatility_cuts,
            Some((Decimal::new(2, 3), Decimal::new(2, 3)))
        );
        assert_eq!(band_counts(&report), [1, 0, 5, 0]);
    }

    #[test]
    fn loss_context_lists_every_cell_in_the_stated_order_even_with_no_trades() {
        let report = loss_context(&[], &[]).unwrap();
        assert_eq!((report.trades, report.losses), (0, 0));
        assert_eq!(report.volatility_cuts, None);
        let trends: Vec<TrendSide> = report.by_trend.iter().map(|(t, _)| *t).collect();
        assert_eq!(
            trends,
            [
                TrendSide::With,
                TrendSide::Ranging,
                TrendSide::Against,
                TrendSide::Unknown
            ]
        );
        let regimes: Vec<Regime> = report.by_regime.iter().map(|(r, _)| *r).collect();
        assert_eq!(
            regimes,
            [
                Regime::TrendingUp,
                Regime::TrendingDown,
                Regime::Ranging,
                Regime::Unknown
            ]
        );
        let bands: Vec<VolatilityBand> = report.by_volatility.iter().map(|(b, _)| *b).collect();
        assert_eq!(
            bands,
            [
                VolatilityBand::Low,
                VolatilityBand::Mid,
                VolatilityBand::High,
                VolatilityBand::Unknown
            ]
        );
        // All 24 hours are present, in order, each an empty cell.
        let hours: Vec<u8> = report.by_hour.iter().map(|(h, _)| *h).collect();
        assert_eq!(hours, (0..=23).collect::<Vec<u8>>());
        assert!(
            report
                .by_hour
                .iter()
                .all(|(_, c)| *c == ContextCell::default())
        );
    }

    #[test]
    fn loss_context_each_grouping_sums_to_the_totals() {
        let regimes = [
            Regime::TrendingUp,
            Regime::TrendingDown,
            Regime::Ranging,
            Regime::Unknown,
        ];
        let trends = [
            TrendSide::With,
            TrendSide::Ranging,
            TrendSide::Against,
            TrendSide::Unknown,
        ];
        let pnls = [-40, 25, -10, -10, 0, 70, -5, -5, -5, 12, -1];
        let trades: Vec<Trade> = pnls
            .iter()
            .enumerate()
            .map(|(i, pnl)| trade(*pnl, regimes[i % 4]))
            .collect();
        let contexts: Vec<TradeContext> = (0..pnls.len())
            .map(|i| {
                ctx(
                    trends[(i * 3) % 4],
                    if i % 5 == 0 {
                        None
                    } else {
                        Some(i64::try_from(i).unwrap())
                    },
                    u8::try_from((i * 7) % 24).unwrap(),
                )
            })
            .collect();
        let report = loss_context(&trades, &contexts).unwrap();

        let total_pnl: Decimal = trades.iter().map(|t| t.realized_pnl).sum();
        let total_r: Decimal = trades.iter().map(|t| t.realized_r).sum();
        let losses = pnls.iter().filter(|p| **p < 0).count();
        assert_eq!((report.trades, report.losses), (pnls.len(), losses));

        let groupings: [Vec<&ContextCell>; 4] = [
            report.by_trend.iter().map(|(_, c)| c).collect(),
            report.by_regime.iter().map(|(_, c)| c).collect(),
            report.by_volatility.iter().map(|(_, c)| c).collect(),
            report.by_hour.iter().map(|(_, c)| c).collect(),
        ];
        for cells in groupings {
            assert_eq!(cells.iter().map(|c| c.trades).sum::<usize>(), pnls.len());
            assert_eq!(cells.iter().map(|c| c.losses).sum::<usize>(), losses);
            assert_eq!(cells.iter().map(|c| c.net_pnl).sum::<Decimal>(), total_pnl);
            assert_eq!(cells.iter().map(|c| c.net_r).sum::<Decimal>(), total_r);
            // Every streak is counted exactly once in each grouping: the pnls hold
            // streaks (-10, -10) and (-5, -5, -5) and no more.
            assert_eq!(cells.iter().map(|c| c.losing_streaks).sum::<usize>(), 2);
            assert_eq!(cells.iter().map(|c| c.longest_losing_streak).max(), Some(3));
        }
    }

    #[test]
    fn loss_context_a_length_mismatch_is_an_error_not_a_partial_report() {
        let one = [trade(-1, Regime::Ranging)];
        let two = [ctx(TrendSide::With, None, 0), ctx(TrendSide::With, None, 0)];
        assert_eq!(
            loss_context(&one, &two),
            Err(ContextLengthMismatch {
                trades: 1,
                contexts: 2
            })
        );
        assert_eq!(
            loss_context(&[], &two[..1]),
            Err(ContextLengthMismatch {
                trades: 0,
                contexts: 1
            })
        );
        let error = loss_context(&one, &[]).unwrap_err();
        assert_eq!(error.to_string(), "loss_context: 1 trades but 0 contexts");
    }
}
