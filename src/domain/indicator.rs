//! The `Indicator` port (domain) — a streaming, append-only indicator over
//! candles (FR-5). This is the seam every concrete indicator adapter
//! (VS-1.1.3: EMA here, RSI/ADX/MACD in 3.02) implements, and the seam the
//! backtester / `EvalContext` (3.03) reads through.

use crate::domain::Candle;
use rust_decimal::Decimal;

/// A streaming indicator: fed one candle at a time, append-only, no look-ahead.
///
/// `next` advances by **exactly one candle**. It consumes the *current* candle
/// and may never peek forward; calling it N times over N candles is the only
/// way to advance the indicator's state. During **warmup** (before enough
/// history has accrued) it returns `None`; once warm it returns `Some(value)`.
///
/// The port returns `Decimal` — **`f64` never crosses it**. Floats are
/// quarantined behind the adapter (`src/adapters/indicators/convert.rs`); the
/// domain only ever sees exact decimals (NFR-2).
///
/// `is_ready()` is `true` iff the *next* call to `next` will return `Some`
/// (i.e. warmup has completed).
///
/// **The level/pattern carve-out (r4.s6 PR-362 fix round 2).** The six
/// level/pattern variants (`Resistance`, `Support`, `DoubleTopNeckline`,
/// `DoubleBottomNeckline`, `TrendlineLows`, `TrendlineHighs`) cannot promise
/// that: whether the next bar carries a level is the next bar's own fact, so
/// their `is_ready()` reports "a value is possible at all" instead — a `None`
/// after it is true is a no-value reading (a comparison over it is false), not
/// a warmup state. Readiness for those six is LATCHED: once true it stays true
/// for the life of the adapter. [`IndicatorEngine::is_warm`] reads it as the
/// warm gate for those slots.
///
/// [`IndicatorEngine::is_warm`]: crate::IndicatorEngine::is_warm
pub trait Indicator {
    /// Advance the indicator by one candle and return its value, or `None`
    /// while still in warmup.
    fn next(&mut self, candle: &Candle) -> Option<Decimal>;

    /// `true` when the indicator has the history it needs — exactly "the next
    /// [`Indicator::next`] call yields `Some`" for every adapter except the
    /// level/pattern carve-out above, whose post-readiness `None` bars are
    /// no-value readings (see the type doc).
    fn is_ready(&self) -> bool;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Indicator;
    use crate::domain::Candle;
    use rust_decimal::Decimal;

    /// A trivial in-test double: a 1-bar-warmup constant. It returns `None` on
    /// the first candle (warmup) and `Some(value)` on every candle thereafter.
    /// Exists only to lock the trait shape (`next`/`is_ready` contract).
    struct ConstantOneBarWarmup {
        value: Decimal,
        seen: u32,
    }

    impl ConstantOneBarWarmup {
        fn new(value: Decimal) -> Self {
            Self { value, seen: 0 }
        }
    }

    impl Indicator for ConstantOneBarWarmup {
        fn next(&mut self, _candle: &Candle) -> Option<Decimal> {
            self.seen += 1;
            if self.seen >= 2 {
                Some(self.value)
            } else {
                None
            }
        }

        fn is_ready(&self) -> bool {
            // ready iff the NEXT call returns Some, i.e. we've already seen >= 1.
            self.seen >= 1
        }
    }

    fn dummy_candle() -> Candle {
        Candle {
            open_time: 0,
            close_time: 0,
            open: Decimal::ZERO,
            high: Decimal::ZERO,
            low: Decimal::ZERO,
            close: Decimal::ZERO,
            volume: Decimal::ZERO,
            funding_rate: None,
        }
    }

    #[test]
    fn indicator_port_warmup_then_ready_contract() {
        let mut ind = ConstantOneBarWarmup::new(Decimal::from(42));
        let candle = dummy_candle();

        // First candle: warmup. Not ready before the first feed.
        assert!(!ind.is_ready(), "not ready before any candle");
        assert_eq!(ind.next(&candle), None, "first candle is warmup → None");

        // After one candle, the double promises the next call returns Some.
        assert!(ind.is_ready(), "ready after warmup candle");
        assert_eq!(
            ind.next(&candle),
            Some(Decimal::from(42)),
            "second candle → Some(value)"
        );
        assert!(ind.is_ready(), "stays ready once warm");
    }
}
