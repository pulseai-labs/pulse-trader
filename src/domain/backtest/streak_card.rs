//! The streak and drawdown card (r5.s4.w1, PDR-0035): a seeded Monte Carlo over
//! a backtest run's OWN closed trades, answering what a normal bad run of that
//! strategy looks like — the longest losing streak and the max drawdown that
//! trade population produces over [`HORIZON_TRADES`] trades, across [`PATHS`]
//! drawn paths, at the 50th ("typical") and 95th ("bad but still normal")
//! percentile, with the run's own observed values beside them.
//!
//! Pure and exact: `Decimal` and integer draws only — no clock, no I/O, no
//! `f64`. The whole card rides ONE [`SplitMix64`] stream seeded by
//! [`STREAK_CARD_SEED`], so the same trades give the same card on every run and
//! under parallel execution.
//!
//! The card is a READ (SPINE risk posture): it moves no money and opens no
//! exposure. Nothing an existing strategy, run or paper session computes
//! changes.

use rust_decimal::{Decimal, RoundingStrategy};

use super::Trade;
use crate::domain::rng::SplitMix64;

/// The horizon: every path draws this many closed trades (PDR-0035 Q1).
pub const HORIZON_TRADES: usize = 100;

/// The number of seeded paths behind every percentile (PDR-0035 Q1).
pub const PATHS: usize = 10_000;

/// Fewer closed trades than this and the card says so, with no figures
/// (PDR-0035 Q2).
pub const MIN_CLOSED_TRADES: usize = 30;

/// The two reported percentiles, in ascending order (PDR-0035 Q1).
pub const PERCENTILES: [u32; 2] = [50, 95];

/// The fixed equity the USDT drawdown's percentage is taken of (PDR-0035 Q3),
/// non-compounding. `Decimal::new` is not a `const fn`, so the const
/// constructor spells the same `10_000` here.
pub const FIXED_EQUITY_USDT: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// The card's seed — PDR-0035's "one fixed, documented constant". **Fixed**:
/// every figure below is a function of it, so changing it changes every card
/// and every frozen golden.
pub const STREAK_CARD_SEED: u64 = 0x5235_5334_5354_524B;

/// One closed trade as the card consumes it.
///
/// A draw picks ONE trade and reads BOTH of its values (PDR-0035 G2), so the R
/// and USDT measures of a path always come from the same drawn trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClosedTrade {
    /// The trade's realized R-multiple.
    pub realized_r: Decimal,
    /// The trade's realized P&L, in USDT.
    pub realized_pnl: Decimal,
}

impl From<&Trade> for ClosedTrade {
    fn from(trade: &Trade) -> Self {
        Self {
            realized_r: trade.realized_r,
            realized_pnl: trade.realized_pnl,
        }
    }
}

/// One count measure's three figures: the two percentiles over the drawn paths
/// and the run's own observed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreakPercentiles {
    /// The 50th percentile of the drawn paths' longest losing streaks.
    pub p50: usize,
    /// The 95th percentile of the drawn paths' longest losing streaks.
    pub p95: usize,
    /// The run's own longest losing streak, over its stored trade order.
    pub observed: usize,
}

/// One `Decimal` measure's three figures: the two percentiles over the drawn
/// paths and the run's own observed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecimalPercentiles {
    /// The 50th percentile over the drawn paths.
    pub p50: Decimal,
    /// The 95th percentile over the drawn paths.
    pub p95: Decimal,
    /// The run's own value, over its stored trade order.
    pub observed: Decimal,
}

/// The card itself: either "not enough trades" or the figures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreakCard {
    /// The run has too few closed trades for a card (PDR-0035 Q2).
    NotEnoughTrades {
        /// How many closed trades the run does have.
        closed_trades: usize,
    },
    /// The card's figures.
    Ready {
        /// How many closed trades the run has.
        closed_trades: usize,
        /// The horizon every path drew: [`HORIZON_TRADES`].
        horizon_trades: usize,
        /// The number of drawn paths: [`PATHS`].
        paths: usize,
        /// The seed the stream ran under: [`STREAK_CARD_SEED`].
        seed: u64,
        /// The longest losing streak, in trades.
        streak: StreakPercentiles,
        /// The max drawdown of the cumulative `realized_r`, as a magnitude.
        drawdown_r: DecimalPercentiles,
        /// The max drawdown of the cumulative `realized_pnl`, as a magnitude.
        drawdown_usdt: DecimalPercentiles,
        /// [`drawdown_usdt`](Self::Ready::drawdown_usdt) as a percentage of
        /// [`FIXED_EQUITY_USDT`], rounded half-even to 2 decimals.
        drawdown_pct: DecimalPercentiles,
    },
}

/// The card for one run's closed trades, in run order (PDR-0035).
///
/// Fewer than [`MIN_CLOSED_TRADES`] trades answers
/// [`StreakCard::NotEnoughTrades`]. Otherwise: one [`SplitMix64`] stream seeded
/// by [`STREAK_CARD_SEED`]; every path draws [`HORIZON_TRADES`] trades WITH
/// replacement by index `next_u64() % n` (G4), and the same walk produces the
/// observed values over the run's own order.
#[must_use]
pub fn streak_card(outcomes: &[ClosedTrade]) -> StreakCard {
    let closed_trades = outcomes.len();
    if closed_trades < MIN_CLOSED_TRADES {
        return StreakCard::NotEnoughTrades { closed_trades };
    }

    let mut rng = SplitMix64::new(STREAK_CARD_SEED);
    let mut streaks = Vec::with_capacity(PATHS);
    let mut drawdowns_r = Vec::with_capacity(PATHS);
    let mut drawdowns_usdt = Vec::with_capacity(PATHS);
    for _ in 0..PATHS {
        let figures = path_figures(&mut rng, outcomes);
        streaks.push(figures.streak);
        drawdowns_r.push(figures.drawdown_r);
        drawdowns_usdt.push(figures.drawdown_usdt);
    }

    let mut observed = Figures::default();
    for trade in outcomes {
        observed.push(trade);
    }
    let (observed_streak, observed_r, observed_usdt) = observed.finish();

    let (streak_p50, streak_p95) = nearest_ranks(&mut streaks);
    let (drawdown_r_p50, drawdown_r_p95) = nearest_ranks(&mut drawdowns_r);
    let (drawdown_usdt_p50, drawdown_usdt_p95) = nearest_ranks(&mut drawdowns_usdt);

    StreakCard::Ready {
        closed_trades,
        horizon_trades: HORIZON_TRADES,
        paths: PATHS,
        seed: STREAK_CARD_SEED,
        streak: StreakPercentiles {
            p50: streak_p50,
            p95: streak_p95,
            observed: observed_streak,
        },
        drawdown_r: DecimalPercentiles {
            p50: drawdown_r_p50,
            p95: drawdown_r_p95,
            observed: observed_r,
        },
        drawdown_usdt: DecimalPercentiles {
            p50: drawdown_usdt_p50,
            p95: drawdown_usdt_p95,
            observed: observed_usdt,
        },
        drawdown_pct: DecimalPercentiles {
            p50: percent_of_equity(drawdown_usdt_p50),
            p95: percent_of_equity(drawdown_usdt_p95),
            observed: percent_of_equity(observed_usdt),
        },
    }
}

/// One path: [`HORIZON_TRADES`] draws with replacement (PDR-0035 Q2), each by
/// index `next_u64() % n` (PDR-0035 G4), walked by the same [`Figures`] the
/// observed values use. The caller has already refused fewer than
/// [`MIN_CLOSED_TRADES`] trades, so the modulo's width is never zero.
fn path_figures(rng: &mut SplitMix64, outcomes: &[ClosedTrade]) -> PathFigures {
    let width = u64::try_from(outcomes.len()).unwrap_or(u64::MAX);
    let mut figures = Figures::default();
    for _ in 0..HORIZON_TRADES {
        let index = usize::try_from(rng.next_u64() % width).unwrap_or(0);
        figures.push(&outcomes[index]);
    }
    let (streak, dip_r, dip_usdt) = figures.finish();
    PathFigures {
        streak,
        drawdown_r: dip_r,
        drawdown_usdt: dip_usdt,
    }
}

/// One drawn path's three figures, as `path_figures` returns them.
struct PathFigures {
    streak: usize,
    drawdown_r: Decimal,
    drawdown_usdt: Decimal,
}

/// The accumulator BOTH walks share: one sequence of trades in, the three
/// figures out. A drawn path pushes its drawn trades; the observed walk pushes
/// the run's own trades in order — one implementation, so the two rules cannot
/// drift apart.
#[derive(Default)]
struct Figures {
    current_streak: usize,
    longest_streak: usize,
    cumulative_r: Decimal,
    peak_r: Decimal,
    drawdown_r: Decimal,
    cumulative_usdt: Decimal,
    peak_usdt: Decimal,
    drawdown_usdt: Decimal,
}

impl Figures {
    /// Take one trade. A loss is `realized_pnl < 0`; a breakeven or a winning
    /// trade ends the current losing run (G1 — the same rule
    /// [`SummaryStats`](super::SummaryStats) applies). Both cumulative measures
    /// step by THIS trade (G2: one draw, both measures).
    fn push(&mut self, trade: &ClosedTrade) {
        if trade.realized_pnl < Decimal::ZERO {
            self.current_streak += 1;
            self.longest_streak = self.longest_streak.max(self.current_streak);
        } else {
            self.current_streak = 0;
        }
        self.cumulative_r += trade.realized_r;
        self.peak_r = self.peak_r.max(self.cumulative_r);
        self.drawdown_r = self.drawdown_r.max(self.peak_r - self.cumulative_r);
        self.cumulative_usdt += trade.realized_pnl;
        self.peak_usdt = self.peak_usdt.max(self.cumulative_usdt);
        self.drawdown_usdt = self
            .drawdown_usdt
            .max(self.peak_usdt - self.cumulative_usdt);
    }

    /// The three figures, as the walks return them.
    fn finish(self) -> (usize, Decimal, Decimal) {
        (self.longest_streak, self.drawdown_r, self.drawdown_usdt)
    }
}

/// Sort `values` ascending in place, then read both percentiles by nearest
/// rank (PDR-0035 G3).
fn nearest_ranks<T: Ord + Copy>(values: &mut [T]) -> (T, T) {
    values.sort_unstable();
    (
        values[percentile_index(PERCENTILES[0])],
        values[percentile_index(PERCENTILES[1])],
    )
}

/// PDR-0035 G3's nearest rank over [`PATHS`] drawn values:
/// `ceil(p × PATHS / 100) − 1` — index 4999 for p50, 9499 for p95.
fn percentile_index(p: u32) -> usize {
    (usize::try_from(p).unwrap_or(0) * PATHS).div_ceil(100) - 1
}

/// A USDT drawdown as a percentage of [`FIXED_EQUITY_USDT`], rounded half-even
/// to two decimals (PDR-0035 Q3, non-compounding).
fn percent_of_equity(usdt: Decimal) -> Decimal {
    (usdt * Decimal::from(100) / FIXED_EQUITY_USDT)
        .round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven)
}

#[cfg(test)]
mod tests {
    use super::{
        ClosedTrade, FIXED_EQUITY_USDT, HORIZON_TRADES, MIN_CLOSED_TRADES, PATHS, STREAK_CARD_SEED,
        StreakCard, percentile_index, streak_card,
    };
    use rust_decimal::Decimal;

    fn trade(realized_r: i64, realized_pnl: i64) -> ClosedTrade {
        ClosedTrade {
            realized_r: Decimal::new(realized_r, 0),
            realized_pnl: Decimal::new(realized_pnl, 0),
        }
    }

    /// A mixed run of `count` trades, alternating a winner and a loser.
    fn mixed(count: usize) -> Vec<ClosedTrade> {
        (0..count)
            .map(|i| {
                if i % 2 == 0 {
                    trade(2, 200)
                } else {
                    trade(-1, -100)
                }
            })
            .collect()
    }

    #[test]
    fn streak_card_below_the_floor_says_not_enough_trades() {
        assert_eq!(
            streak_card(&mixed(MIN_CLOSED_TRADES - 1)),
            StreakCard::NotEnoughTrades {
                closed_trades: MIN_CLOSED_TRADES - 1
            }
        );
        assert!(matches!(
            streak_card(&mixed(MIN_CLOSED_TRADES)),
            StreakCard::Ready { .. }
        ));
    }

    #[test]
    fn streak_card_carries_the_pinned_horizon_paths_and_seed() {
        let StreakCard::Ready {
            horizon_trades,
            paths,
            seed,
            ..
        } = streak_card(&mixed(MIN_CLOSED_TRADES))
        else {
            panic!("thirty trades is a ready card");
        };
        assert_eq!(horizon_trades, HORIZON_TRADES);
        assert_eq!(paths, PATHS);
        assert_eq!(seed, STREAK_CARD_SEED);
        // The seed's VALUE is the pinned constant itself. The anchor's own
        // percentiles turn out to be robust to a ±1 seed change on that
        // fixture (the distribution is tightly concentrated), so this is the
        // assertion that makes a seed change a visible, deliberate act.
        assert_eq!(
            STREAK_CARD_SEED, 0x5235_5334_5354_524B,
            "the seed is FIXED: changing it changes every card"
        );
    }

    #[test]
    fn streak_card_is_the_same_card_twice() {
        let trades = mixed(40);
        assert_eq!(streak_card(&trades), streak_card(&trades));
    }

    #[test]
    fn streak_card_percentiles_are_the_pinned_nearest_ranks() {
        assert_eq!(percentile_index(50), 4999);
        assert_eq!(percentile_index(95), 9499);
    }

    #[test]
    fn streak_card_observed_values_are_the_runs_own_order() {
        // 20 losers, a breakeven, 20 losers: the observed streak is 20 (the
        // breakeven resets it), while the observed USDT drawdown is the whole
        // 40 × −100 decline below the 0 peak — the breakeven adds nothing.
        let mut trades = vec![trade(-1, -100); 20];
        trades.push(trade(0, 0));
        trades.extend(vec![trade(-1, -100); 20]);
        let StreakCard::Ready {
            streak,
            drawdown_usdt,
            drawdown_pct,
            ..
        } = streak_card(&trades)
        else {
            panic!("41 trades is a ready card");
        };
        assert_eq!(streak.observed, 20);
        assert_eq!(drawdown_usdt.observed, Decimal::from(4_000));
        assert_eq!(drawdown_pct.observed, Decimal::from(40));
        assert_eq!(
            FIXED_EQUITY_USDT,
            Decimal::from(10_000),
            "the const constructor spells the same 10_000"
        );
    }
}
