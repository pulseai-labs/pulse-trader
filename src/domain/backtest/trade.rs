//! Trade record value types (FR-6) — the immutable shape of one backtested
//! trade and its synthetic fills.
//!
//! This work item (1.01) **defines** the fields; the event loop (1.03)
//! **populates** them. Everything is `Decimal` (exact money-math, NFR-2) and
//! `i64` epoch-ms timestamps (`Binance`-native), matching the
//! [`Candle`](crate::domain::Candle) convention. serde-serializable so a trade
//! can cross the `Tauri` boundary and later persist (VS-1.2.4).
//!
//! A backtest [`Trade`] carries the four MASTER-SPEC timestamps (invariant #11:
//! `entry_signal_time` / `entry_fill_time` / `exit_signal_time` /
//! `exit_fill_time`) plus the "embedded fills sub-list" ([`Fill`]) — exactly two
//! synthetic fills (open, close) for a backtest trade — plus the cost-component
//! totals the loop accumulates from the [`cost`](super::cost) primitives.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::regime::Regime;
use crate::domain::Direction;

/// Why a trade closed. Only the four exit kinds this slice models appear —
/// `TrailingStop` / `TimeStop` are rejected fail-fast upstream
/// ([`BacktestError::UnsupportedExit`](super::BacktestError::UnsupportedExit),
/// C4), so they have no exit-reason here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The stop-loss level was reached (intra-bar, G2).
    StopLoss,
    /// The take-profit level was reached (intra-bar, G2).
    TakeProfit,
    /// A `SignalExit` condition became true (fills next-bar-open, G1).
    Signal,
    /// The series ended with the position open; force-closed at the final bar's
    /// close (S4).
    EndOfData,
}

/// How a trade originated. v1 is backtest-only; the variant exists so persisted
/// trades (VS-1.2.4) and later paper/live trades share one record shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TradeSource {
    /// Produced by the deterministic backtester.
    Backtest,
}

/// One synthetic fill — a price/quantity execution at a point in time, with the
/// taker fee booked against it. A backtest [`Trade`] has exactly two: open and
/// close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    /// The (slippage-adjusted) execution price.
    pub price: Decimal,
    /// The filled base-asset quantity.
    pub qty: Decimal,
    /// Fill time, UTC epoch milliseconds.
    pub time_ms: i64,
    /// The taker fee charged on this fill (quote currency, always `>= 0`).
    pub fee: Decimal,
}

/// One backtested trade (FR-6). Immutable once the loop (1.03) populates it.
///
/// The four timestamps separate **signal** time (when the condition fired, the
/// bar close) from **fill** time (when it executed, the next bar open for
/// signal-driven fills — the G1 latency gap) per invariant #11. Cost components
/// (`fees_total` / `funding_total` / `slippage_total`) are summed by the loop
/// from the [`cost`](super::cost) primitives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trade {
    /// Trade side (reuses the DSL [`Direction`]).
    pub direction: Direction,
    /// Filled position size in base asset.
    pub qty: Decimal,
    /// Entry fill price (slippage-adjusted).
    pub entry_price: Decimal,
    /// Exit fill price (slippage-adjusted).
    pub exit_price: Decimal,

    /// When the entry condition fired (bar close), epoch ms.
    pub entry_signal_time: i64,
    /// When the entry actually executed (next bar open for signal fills), epoch ms.
    pub entry_fill_time: i64,
    /// When the exit condition fired, epoch ms.
    pub exit_signal_time: i64,
    /// When the exit actually executed, epoch ms.
    pub exit_fill_time: i64,

    /// The synthetic fills (open, close) backing this trade.
    pub fills: Vec<Fill>,

    /// Total taker fees paid across all fills (quote currency).
    pub fees_total: Decimal,
    /// Total funding as a signed P&L delta — the signed sum of per-boundary
    /// [`funding_payment`](super::cost::funding_payment) values: **negative when
    /// the position paid funding, positive when it received** (a long paying
    /// positive-rate funding accrues a negative delta).
    pub funding_total: Decimal,
    /// Total adverse slippage cost across all fills (quote currency).
    pub slippage_total: Decimal,

    /// Net realized P&L in quote currency (gross P&L minus the cost totals; the
    /// loop computes the aggregate).
    pub realized_pnl: Decimal,
    /// Realized R-multiple (price move ÷ stop distance); a clean stop ≈ −1R, but
    /// costs can push it past −1R (G3).
    pub realized_r: Decimal,

    /// Maximum **favorable** excursion in R-multiples (FR-6, C5): the largest
    /// intra-bar move in the trade's direction across every held bar (entry-fill
    /// to exit-fill inclusive), measured from the entry fill price and normalized
    /// by the initial stop distance. Initialized to 0 at entry, so `mfe_r >= 0` by
    /// construction. The **full exit-bar range** folds in (no intra-bar path
    /// reconstruction), so `mfe_r >= realized_r` is NOT guaranteed (documented
    /// overshoot) and is not asserted.
    ///
    /// **Semantics — full-bar *potential* excursion, NOT experienced excursion.**
    /// Because the entire exit bar's high/low are folded in, a trade that exits at
    /// the bar **open** (a signal-exit fill, or a gap-through-stop/TP at the open)
    /// still records the rest of that bar's range — price action that occurred
    /// *after* the position was already closed. So `mfe_r`/`mae_r` bound the
    /// regime's *potential* excursion over the held span, not the path the trade
    /// actually experienced. Coaching/analytics (MASTER-SPEC Phase 1 §(e)) must
    /// read them as potential-excursion bounds, not as proof of an experienced
    /// price path. (Deliberate v1 simplification, grill amendment 3; a separate
    /// realized-hold excursion is deferred — see close-audit finding.)
    pub mfe_r: Decimal,
    /// Maximum **adverse** excursion in R-multiples (FR-6, C5): the largest
    /// intra-bar move against the trade, measured + normalized like `mfe_r`.
    /// Initialized to 0 at entry, so `mae_r <= 0` by construction. Same
    /// **full-bar potential** semantics as [`mfe_r`](Self::mfe_r) — includes the
    /// full exit bar (post-close range for an at-open exit), so it is a
    /// potential-excursion bound, not the experienced path.
    pub mae_r: Decimal,

    /// Why the trade closed.
    pub exit_reason: ExitReason,
    /// Where the trade originated.
    pub source: TradeSource,

    /// The market [`Regime`] in effect at the entry-fill bar (FR-6, VS-1.2.2
    /// work-2.04). The engine steps a [`RegimeDetector`](crate::RegimeDetector)
    /// on the primary M15 series once per bar and tags the position with the
    /// then-current regime at open; it is `Unknown` while the EMA200/ADX warm.
    /// `RegimeBreakdown` aggregates `(regime, realized_pnl)` over the trade log.
    pub regime: Regime,

    /// The recorded per-trade stop price (r2.s2.w2) — the level the position's
    /// stop order sat at, frozen at the fill. For a `StopLoss` it is
    /// `entry × (1 ± distance_pct)`; for an `AtrStop` it is `entry ∓ multiple ×
    /// ATR(period)` evaluated at the **signal** bar. `Option` because a pre-`0011`
    /// persisted row carries no such column and reads back `NULL`; a fresh trade
    /// always records `Some`.
    ///
    /// `#[serde(default)]` (#68 / README C5): an old-shape trade missing this
    /// field deserializes as `None`. The content hash feeds it CONDITIONALLY —
    /// only when `Some` — so a pre-`0011` row re-derives the identical byte
    /// stream it hashed under (the r2.s1 G1(b) precedent).
    #[serde(default)]
    pub stop_price: Option<Decimal>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{ExitReason, Fill, Trade, TradeSource};
    use crate::domain::Direction;
    use crate::domain::backtest::Regime;
    use rust_decimal::Decimal;

    fn sample_trade() -> Trade {
        let open = Fill {
            price: Decimal::new(100, 0),
            qty: Decimal::new(2, 0),
            time_ms: 1_700_000_000_000,
            fee: Decimal::new(8, 2), // 0.08
        };
        let close = Fill {
            price: Decimal::new(110, 0),
            qty: Decimal::new(2, 0),
            time_ms: 1_700_000_900_000,
            fee: Decimal::new(88, 3), // 0.088
        };
        Trade {
            direction: Direction::Long,
            qty: Decimal::new(2, 0),
            entry_price: Decimal::new(100, 0),
            exit_price: Decimal::new(110, 0),
            entry_signal_time: 1_699_999_100_000,
            entry_fill_time: 1_700_000_000_000,
            exit_signal_time: 1_700_000_000_000,
            exit_fill_time: 1_700_000_900_000,
            fills: vec![open, close],
            fees_total: Decimal::new(168, 3),
            funding_total: Decimal::new(5, 2),
            slippage_total: Decimal::new(2, 2),
            realized_pnl: Decimal::new(20, 0),
            realized_r: Decimal::new(2, 0),
            mfe_r: Decimal::new(25, 1), // 2.5
            mae_r: Decimal::new(-5, 1), // -0.5
            exit_reason: ExitReason::TakeProfit,
            source: TradeSource::Backtest,
            regime: Regime::TrendingUp,
            stop_price: Some(Decimal::new(99, 0)),
        }
    }

    #[test]
    fn trade_holds_two_fills_and_four_timestamps() {
        let t = sample_trade();
        assert_eq!(t.fills.len(), 2, "a backtest trade has open + close fills");
        // The four MASTER-SPEC timestamps are distinct fields (invariant #11).
        assert!(t.entry_signal_time <= t.entry_fill_time);
        assert!(t.exit_signal_time <= t.exit_fill_time);
        assert_eq!(t.direction, Direction::Long);
    }

    #[test]
    fn trade_carries_mfe_mae_with_correct_signs() {
        // C5 invariant: the favorable excursion is non-negative and the adverse
        // excursion is non-positive (holds by the init-0 running sample). The
        // round-trip below also proves the new Decimal fields survive serde.
        let t = sample_trade();
        assert!(t.mfe_r >= Decimal::ZERO, "mfe_r must be >= 0");
        assert!(t.mae_r <= Decimal::ZERO, "mae_r must be <= 0");
    }

    #[test]
    fn trade_serde_round_trips() {
        let t = sample_trade();
        let json = serde_json::to_string(&t).expect("serialize Trade");
        let back: Trade = serde_json::from_str(&json).expect("deserialize Trade");
        assert_eq!(t, back);
    }

    #[test]
    fn exit_reason_serializes_snake_case() {
        let json = serde_json::to_string(&ExitReason::StopLoss).expect("serialize");
        assert_eq!(json, "\"stop_loss\"");
        let json = serde_json::to_string(&ExitReason::EndOfData).expect("serialize");
        assert_eq!(json, "\"end_of_data\"");
    }

    #[test]
    fn trade_source_serializes_snake_case() {
        let json = serde_json::to_string(&TradeSource::Backtest).expect("serialize");
        assert_eq!(json, "\"backtest\"");
    }
}
