//! Cost-model + P&L primitives — the pure money-math the event loop (1.03)
//! composes per fill / per funding boundary / per closed trade.
//!
//! Every function is pure, `Decimal`-only (NFR-2), and side-effect-free; the
//! loop is responsible for *when* to call them (which fill, which boundary) and
//! for summing the components into a [`Trade`](super::trade::Trade)'s totals.
//! This module is the **money-math 100% coverage tier** (MASTER-SPEC Phase 9):
//! every function is unit-tested against hand-derived synthetic fixtures with
//! exact expected `Decimal` values.
//!
//! # Sign conventions (load-bearing — G3 / G4)
//!
//! - **Slippage (G3)** is *adverse on every fill*. A long pays up on entry and
//!   sells down on exit; a short is the mirror. See [`apply_slippage`] and
//!   [`Side`].
//! - **Funding (G4)** is a *signed P&L delta*. A long **pays** positive funding
//!   (`pnl -= rate × notional`), a short **receives** it (`pnl += rate ×
//!   notional`); a negative rate flips both. [`funding_payment`] returns the
//!   delta directly, so the caller always does `pnl += funding_payment(..)`.
//! - **Notional** is constant — `qty × entry_price` (G4) — and is the caller's
//!   responsibility to supply.

use rust_decimal::Decimal;

use super::error::BacktestError;
use crate::domain::Direction;

/// Basis-point denominator: 1 bp = `1/10_000`.
const BPS_DENOMINATOR: i64 = 10_000;

/// Which leg of a trade a fill belongs to. Slippage is adverse relative to the
/// leg: an entry pays the spread to get in, an exit pays it to get out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The opening fill of a position.
    Entry,
    /// The closing fill of a position.
    Exit,
}

/// The taker fee on a fill: `notional × fee_bps / 10_000`.
///
/// Always non-negative for a non-negative `notional` and `fee_bps` (the v1
/// default is 4 bps — Binance USDⓂ taker). Pure; the caller books it against a
/// [`Fill`](super::trade::Fill).
#[must_use]
pub fn taker_fee(notional: Decimal, fee_bps: Decimal) -> Decimal {
    notional * fee_bps / Decimal::from(BPS_DENOMINATOR)
}

/// Apply slippage to a fill price **adversely** (G3): the fill is always worse
/// for us than the trigger price.
///
/// - Long entry → fills **higher**; long exit → fills **lower**.
/// - Short entry → fills **lower**; short exit → fills **higher**.
///
/// The adjustment magnitude is `price × bps / 10_000`. Pure.
#[must_use]
pub fn apply_slippage(price: Decimal, bps: Decimal, direction: Direction, side: Side) -> Decimal {
    let delta = price * bps / Decimal::from(BPS_DENOMINATOR);
    // `worse_is_higher` is true exactly when an adverse fill raises the price:
    // a long pays up to enter; a short pays up to exit (buy back higher).
    let worse_is_higher = matches!(
        (direction, side),
        (Direction::Long, Side::Entry) | (Direction::Short, Side::Exit)
    );
    if worse_is_higher {
        price + delta
    } else {
        price - delta
    }
}

/// The signed funding P&L delta for one 8h boundary (G4).
///
/// Returns `pnl += funding_payment(..)`-ready value: a **long pays** positive
/// funding (`-rate × notional`), a **short receives** it (`+rate × notional`); a
/// negative `rate` flips both signs naturally. `notional = qty × entry_price`
/// (constant-notional, G4) is the caller's responsibility.
#[must_use]
pub fn funding_payment(rate: Decimal, notional: Decimal, direction: Direction) -> Decimal {
    let payment = rate * notional;
    match direction {
        // Long pays positive funding → negative P&L delta.
        Direction::Long => -payment,
        // Short receives positive funding → positive P&L delta.
        Direction::Short => payment,
    }
}

/// Gross realized P&L in quote currency for a closed position (pre-cost).
///
/// `Long`: `(exit − entry) × qty`; `Short`: `(entry − exit) × qty`. The caller
/// (1.03) subtracts fees/funding/slippage to get the net figure. Pure.
#[must_use]
pub fn realized_pnl(entry: Decimal, exit: Decimal, qty: Decimal, direction: Direction) -> Decimal {
    match direction {
        Direction::Long => (exit - entry) * qty,
        Direction::Short => (entry - exit) * qty,
    }
}

/// The **gross** realized R-multiple: the favourable price move divided by the
/// stop distance (`|entry − stop_price|`).
///
/// `Long` move = `exit − entry`; `Short` move = `entry − exit`. A clean stop is
/// ≈ `−1R`. This is a pure price-geometry ratio — it does NOT subtract costs.
/// Closed trades record [`net_r`] (r6.s3.w1, #457), not this ratio; this one
/// stays for paper's unrealized R (#385) and for the cost-free comparisons.
///
/// # Errors
///
/// Returns [`BacktestError::NoStopLoss`] when `entry == stop_price` (a zero stop
/// distance has no R denominator — G5 / #20).
pub fn realized_r(
    entry: Decimal,
    exit: Decimal,
    stop_price: Decimal,
    direction: Direction,
) -> Result<Decimal, BacktestError> {
    let stop_distance = (entry - stop_price).abs();
    if stop_distance.is_zero() {
        return Err(BacktestError::NoStopLoss);
    }
    let price_move = match direction {
        Direction::Long => exit - entry,
        Direction::Short => entry - exit,
    };
    Ok(price_move / stop_distance)
}

/// The realized R-multiple **net of costs** (r6.s3.w1, #457, ADR-0028 amendment):
/// `net_pnl ÷ (qty × |entry − stop_price|)`, the trade's net P&L in units of its
/// initial risk. Fees and funding count; slippage is already inside `net_pnl`.
///
/// # Errors
///
/// [`BacktestError::NoStopLoss`] when the risk `qty × |entry − stop_price|` is zero.
pub fn net_r(
    net_pnl: Decimal,
    qty: Decimal,
    entry: Decimal,
    stop_price: Decimal,
) -> Result<Decimal, BacktestError> {
    let risk = qty * (entry - stop_price).abs();
    if risk.is_zero() {
        return Err(BacktestError::NoStopLoss);
    }
    Ok(net_pnl / risk)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        Side, apply_slippage, funding_payment, net_r, realized_pnl, realized_r, taker_fee,
    };
    use crate::domain::Direction;
    use crate::domain::backtest::error::BacktestError;
    use rust_decimal::Decimal;

    /// Helper: `n / 10^scale` as a `Decimal` (e.g. `dec(4, 0)` = 4, `dec(4, 4)`
    /// = 0.0004).
    fn dec(n: i64, scale: u32) -> Decimal {
        Decimal::new(n, scale)
    }

    // ---- taker_fee ---------------------------------------------------------

    #[test]
    fn taker_fee_is_notional_times_bps() {
        // notional 10_000, 4 bps → 10_000 * 4 / 10_000 = 4.
        assert_eq!(taker_fee(dec(10_000, 0), dec(4, 0)), dec(4, 0));
        // notional 250.5, 1 bp → 250.5 * 1 / 10_000 = 0.02505.
        assert_eq!(taker_fee(dec(2505, 1), dec(1, 0)), dec(2505, 5));
        // zero fee bps → zero fee.
        assert_eq!(taker_fee(dec(10_000, 0), Decimal::ZERO), Decimal::ZERO);
    }

    // ---- apply_slippage (G3: adverse on every fill) ------------------------

    #[test]
    fn slippage_is_adverse_long() {
        // price 100, 10 bps → delta = 100 * 10 / 10_000 = 0.1.
        let price = dec(100, 0);
        let bps = dec(10, 0);
        // Long entry fills HIGHER (worse): 100 + 0.1 = 100.1.
        assert_eq!(
            apply_slippage(price, bps, Direction::Long, Side::Entry),
            dec(1001, 1)
        );
        // Long exit fills LOWER (worse): 100 - 0.1 = 99.9.
        assert_eq!(
            apply_slippage(price, bps, Direction::Long, Side::Exit),
            dec(999, 1)
        );
    }

    #[test]
    fn slippage_is_adverse_short() {
        let price = dec(100, 0);
        let bps = dec(10, 0);
        // Short entry fills LOWER (worse — you sell to open at a worse price):
        // 100 - 0.1 = 99.9.
        assert_eq!(
            apply_slippage(price, bps, Direction::Short, Side::Entry),
            dec(999, 1)
        );
        // Short exit fills HIGHER (worse — you buy back higher): 100 + 0.1 = 100.1.
        assert_eq!(
            apply_slippage(price, bps, Direction::Short, Side::Exit),
            dec(1001, 1)
        );
    }

    #[test]
    fn slippage_zero_bps_is_identity() {
        let price = dec(42_000, 0);
        assert_eq!(
            apply_slippage(price, Decimal::ZERO, Direction::Long, Side::Entry),
            price
        );
    }

    // ---- funding_payment (G4: long pays positive, short receives) ----------

    #[test]
    fn funding_long_pays_positive_rate() {
        // rate 0.0001, notional 10_000 → payment = 1; long PAYS → -1.
        let rate = dec(1, 4); // 0.0001
        let notional = dec(10_000, 0);
        assert_eq!(
            funding_payment(rate, notional, Direction::Long),
            dec(-1, 0),
            "a long pays positive funding (negative P&L delta)"
        );
    }

    #[test]
    fn funding_short_receives_positive_rate() {
        let rate = dec(1, 4); // 0.0001
        let notional = dec(10_000, 0);
        assert_eq!(
            funding_payment(rate, notional, Direction::Short),
            dec(1, 0),
            "a short receives positive funding (positive P&L delta)"
        );
    }

    #[test]
    fn funding_negative_rate_flips_both_sides() {
        let rate = dec(-1, 4); // -0.0001
        let notional = dec(10_000, 0);
        // Long with negative rate now RECEIVES: -(-0.0001 * 10_000) = +1.
        assert_eq!(funding_payment(rate, notional, Direction::Long), dec(1, 0));
        // Short with negative rate now PAYS: (-0.0001 * 10_000) = -1.
        assert_eq!(
            funding_payment(rate, notional, Direction::Short),
            dec(-1, 0)
        );
    }

    // ---- realized_pnl ------------------------------------------------------

    #[test]
    fn realized_pnl_long_and_short() {
        // Long: (110 - 100) * 2 = 20.
        assert_eq!(
            realized_pnl(dec(100, 0), dec(110, 0), dec(2, 0), Direction::Long),
            dec(20, 0)
        );
        // Short: (100 - 110) * 2 = -20 (price up hurts a short).
        assert_eq!(
            realized_pnl(dec(100, 0), dec(110, 0), dec(2, 0), Direction::Short),
            dec(-20, 0)
        );
        // Short winning: (100 - 90) * 2 = 20.
        assert_eq!(
            realized_pnl(dec(100, 0), dec(90, 0), dec(2, 0), Direction::Short),
            dec(20, 0)
        );
    }

    // ---- realized_r --------------------------------------------------------

    #[test]
    fn realized_r_clean_stop_is_minus_one() {
        // Long, entry 100, stop 95 (5% stop): exit AT the stop → move -5, dist 5
        // → -1R.
        assert_eq!(
            realized_r(dec(100, 0), dec(95, 0), dec(95, 0), Direction::Long).unwrap(),
            dec(-1, 0)
        );
        // Short, entry 100, stop 105: exit AT the stop → move (100-105) = -5,
        // dist 5 → -1R.
        assert_eq!(
            realized_r(dec(100, 0), dec(105, 0), dec(105, 0), Direction::Short).unwrap(),
            dec(-1, 0)
        );
    }

    #[test]
    fn realized_r_two_r_win() {
        // Long, entry 100, stop 95 (dist 5), exit 110 → move +10 → +2R.
        assert_eq!(
            realized_r(dec(100, 0), dec(110, 0), dec(95, 0), Direction::Long).unwrap(),
            dec(2, 0)
        );
        // Short, entry 100, stop 105 (dist 5), exit 90 → move +10 → +2R.
        assert_eq!(
            realized_r(dec(100, 0), dec(90, 0), dec(105, 0), Direction::Short).unwrap(),
            dec(2, 0)
        );
    }

    #[test]
    fn realized_r_zero_stop_distance_errors() {
        // entry == stop → NoStopLoss (G5 / #20), no divide-by-zero.
        let err = realized_r(dec(100, 0), dec(110, 0), dec(100, 0), Direction::Long).unwrap_err();
        assert_eq!(err, BacktestError::NoStopLoss);
    }

    // ---- net_r (r6.s3.w1, #457) --------------------------------------------

    #[test]
    fn net_r_long_stop_out_with_fees_and_negative_funding_is_below_minus_one() {
        // Long 20 @ 100, stop 95 (risk 20 × 5 = 100), stopped out at 95:
        // gross −100, fees 1.6, funding paid 6 → net −107.6 → −1.076R.
        let gross = realized_pnl(dec(100, 0), dec(95, 0), dec(20, 0), Direction::Long);
        let net = gross + dec(-6, 0) - dec(16, 1);
        let r = net_r(net, dec(20, 0), dec(100, 0), dec(95, 0)).unwrap();
        assert_eq!(r, dec(-1076, 3));
        assert!(r < Decimal::NEGATIVE_ONE);
    }

    #[test]
    fn net_r_short_with_funding_received_beats_its_gross_r() {
        // Short 20 @ 100, stop 105, exit 98: gross +40 (0.4R); funding
        // received +3, no fees → net +43 → 0.43R, above the gross 0.4R.
        let gross = realized_pnl(dec(100, 0), dec(98, 0), dec(20, 0), Direction::Short);
        let net = gross + dec(3, 0);
        let r = net_r(net, dec(20, 0), dec(100, 0), dec(105, 0)).unwrap();
        let gross_r = realized_r(dec(100, 0), dec(98, 0), dec(105, 0), Direction::Short).unwrap();
        assert_eq!(r, dec(43, 2));
        assert!(r > gross_r);
    }

    #[test]
    fn net_r_zero_risk_is_no_stop_loss() {
        // entry == stop: no risk, no denominator.
        let err = net_r(dec(10, 0), dec(2, 0), dec(100, 0), dec(100, 0)).unwrap_err();
        assert_eq!(err, BacktestError::NoStopLoss);
        // qty == 0: the risk in USDT is zero as well.
        let err = net_r(dec(10, 0), Decimal::ZERO, dec(100, 0), dec(95, 0)).unwrap_err();
        assert_eq!(err, BacktestError::NoStopLoss);
    }

    #[test]
    fn net_r_fees_only_is_gross_r_minus_fees_over_risk() {
        // Long 2 @ 100, stop 95, exit 110: gross +20, risk 10, fees 0.5, no
        // funding → net 19.5. Gross R 2, so net R = 2 − 0.5 ÷ 10 = 1.95.
        let gross = realized_pnl(dec(100, 0), dec(110, 0), dec(2, 0), Direction::Long);
        let fees = dec(5, 1);
        let r = net_r(gross - fees, dec(2, 0), dec(100, 0), dec(95, 0)).unwrap();
        let gross_r = realized_r(dec(100, 0), dec(110, 0), dec(95, 0), Direction::Long).unwrap();
        let risk = dec(10, 0);
        assert_eq!(r, gross_r - fees / risk);
        assert_eq!(r, dec(195, 2));
    }

    #[test]
    fn net_r_equals_realized_r_when_fees_and_funding_are_zero() {
        for (entry, exit, stop, direction) in [
            (dec(100, 0), dec(95, 0), dec(95, 0), Direction::Long),
            (dec(100, 0), dec(110, 0), dec(95, 0), Direction::Long),
            (dec(100, 0), dec(105, 0), dec(105, 0), Direction::Short),
            (dec(100, 0), dec(90, 0), dec(105, 0), Direction::Short),
        ] {
            let qty = dec(7, 0);
            let net = realized_pnl(entry, exit, qty, direction);
            assert_eq!(
                net_r(net, qty, entry, stop).unwrap(),
                realized_r(entry, exit, stop, direction).unwrap()
            );
        }
    }
}
