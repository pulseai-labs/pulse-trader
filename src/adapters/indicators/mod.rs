//! Indicator adapters (VS-1.1.3) — concrete [`crate::domain::Indicator`] impls
//! wrapping ta-rs, plus the `Decimal↔f64` conversion seam.
//!
//! This is the **only** module in the crate where `f64` is permitted: floats are
//! quarantined behind the domain port. `convert` is the conversion contract; `ema`
//! is the walking-skeleton adapter (3.01). RSI/ADX/MACD land in 3.02; the
//! multi-indicator engine / `EvalContext` impl in 3.03.

pub mod adx;
pub mod atr;
pub mod convert;
pub mod ema;
pub mod engine;
pub mod macd;
pub mod rsi;
pub mod wilder;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod determinism_tests {
    use super::macd::Macd;
    use super::rsi::Rsi;
    use crate::domain::{Candle, Indicator};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn candle_close(close: &str) -> Candle {
        let c = Decimal::from_str(close).unwrap();
        Candle {
            open_time: 0,
            close_time: 0,
            open: c,
            high: c,
            low: c,
            close: c,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    /// NFR-2: each adapter streamed twice over the same series yields a
    /// byte-identical `Vec<Option<Decimal>>` (exact `Decimal` equality). The
    /// fixed-scale-8 half-even rounding in `convert` is what makes this hold by
    /// construction over `f64`-smoothed internals. Covers both 3.02 adapters.
    #[test]
    fn rsi_macd_deterministic_across_repeated_runs() {
        let closes = [
            "100.123456789",
            "101.5",
            "99.987654321",
            "102.25",
            "103.0",
            "101.75",
            "104.5",
            "103.875",
            "105.0",
            "106.25",
            "104.9",
            "107.3",
        ];

        let run_rsi = || -> Vec<Option<Decimal>> {
            let mut rsi = Rsi::new(5).expect("period >= 1");
            closes.iter().map(|c| rsi.next(&candle_close(c))).collect()
        };
        let run_macd = || -> Vec<Option<Decimal>> {
            let mut macd = Macd::new(3, 6, 4).expect("periods >= 1");
            closes.iter().map(|c| macd.next(&candle_close(c))).collect()
        };

        assert_eq!(
            run_rsi(),
            run_rsi(),
            "RSI repeated runs yield identical Vec<Option<Decimal>> (NFR-2)"
        );
        assert_eq!(
            run_macd(),
            run_macd(),
            "MACD repeated runs yield identical Vec<Option<Decimal>> (NFR-2)"
        );
    }
}
