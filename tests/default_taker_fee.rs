//! #527 — the default taker fee is 5 bps (0.0500%, the Binance USDⓈ-M base
//! rate, PDR-0052). A default backtest request and a new paper session start
//! from the same number; neither may drift from the other.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{BacktestConfig, TAKER_FEE_BPS};
use rust_decimal::Decimal;

#[test]
fn the_backtest_default_charges_five_bps_taker() {
    assert_eq!(
        BacktestConfig::default().taker_fee_bps,
        Decimal::new(5, 0),
        "BacktestConfig::default() is the cost basis of every app, MCP and walk-forward run"
    );
    assert_eq!(
        BacktestConfig::default().slippage_bps,
        Decimal::new(1, 0),
        "slippage does not change with the fee"
    );
}

#[test]
fn a_new_paper_session_takes_the_same_five_bps() {
    assert_eq!(TAKER_FEE_BPS, 5);
    assert_eq!(
        Decimal::from(TAKER_FEE_BPS),
        BacktestConfig::default().taker_fee_bps,
        "the paper constant and the backtest default are one number"
    );
}
