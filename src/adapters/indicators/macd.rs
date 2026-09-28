//! MACD adapter — wraps ta-rs `MovingAverageConvergenceDivergence` behind the
//! domain [`Indicator`] port over `candle.close`.
//!
//! **Multi-output convention (resolves #18; selector lands in schema 1.2.0,
//! r3.s2 — b2).** ta-rs emits a 3-field output (MACD line, signal line,
//! histogram). `IndicatorSpec::Macd` carries an `output` selector
//! (`line` | `signal` | `histogram`), defaulted to the **MACD line**
//! (`macd = EMA(fast) − EMA(slow)`) — the historical ≤1.1.0 behaviour. This
//! adapter emits exactly the selected field; the engine builds one slot per
//! distinct spec, so `line` and `signal` over the same periods are two slots.
//!
//! **Warmup convention.** Like ta-rs's other indicators, the underlying EMAs
//! (fast, slow, AND the signal EMA over the MACD line) are *seeded* and emit
//! from candle 1. The port suppresses output until the selected series is
//! fully defined:
//!
//! - `line`:      `max(fast, slow)` candles (the slower EMA's seed horizon).
//! - `signal` / `histogram`: `max(fast, slow) + signal − 1` candles — the line
//!   first exists at candle `max(fast, slow)`, and the seeded signal EMA needs
//!   `signal` line values, so its first fully-defined value is candle
//!   `max(fast, slow) + signal − 1`.
//!
//! We feed *every* candle to the underlying ta-rs MACD (warming its recursive
//! state — including the signal EMA, which ta-rs advances from the very first
//! candle) but gate emission on a candle counter. The warmup counts are pinned
//! by AC tests.

use crate::adapters::indicators::convert::{decimal_to_f64, f64_to_decimal_rounded};
use crate::domain::{Candle, Indicator, MacdOutput};
use rust_decimal::Decimal;
use ta::Next;
use ta::indicators::MovingAverageConvergenceDivergence;

/// MACD over closing prices, wrapping ta-rs behind the [`Indicator`] port.
///
/// Emits the [`MacdOutput`]-selected ta-rs field. Output is rounded to
/// scale-8.
pub struct Macd {
    inner: MovingAverageConvergenceDivergence,
    /// Warmup bar-count: the first `warmup - 1` candles are suppressed.
    /// `line` → `max(fast, slow)`; `signal`/`histogram` →
    /// `max(fast, slow) + signal − 1`. The `max` is defense-in-depth for the
    /// `pub` constructor against inverted periods (the engine factory already
    /// rejects `fast >= slow`).
    warmup: u32,
    /// Number of candles fed so far.
    seen: u32,
    /// Which ta-rs output this instance emits.
    output: MacdOutput,
}

impl Macd {
    /// Build a MACD from `fast`/`slow`/`signal` periods and an output selector.
    ///
    /// Returns `None` if any period is 0 (ta-rs rejects a zero period).
    /// Constructed from concrete `u32`s (the `Fixed`-extraction factory is
    /// 3.03's concern). Panic-free: ta-rs's constructor error maps to `None`.
    #[must_use]
    pub fn new(fast: u32, slow: u32, signal: u32, output: MacdOutput) -> Option<Self> {
        let inner =
            MovingAverageConvergenceDivergence::new(fast as usize, slow as usize, signal as usize)
                .ok()?;
        let warmup = match output {
            MacdOutput::Line => fast.max(slow),
            MacdOutput::Signal | MacdOutput::Histogram => {
                fast.max(slow).saturating_add(signal.saturating_sub(1))
            }
        };
        Some(Self {
            inner,
            warmup,
            seen: 0,
            output,
        })
    }
}

impl Indicator for Macd {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        // Convert BEFORE advancing the warmup counter: a non-representable price
        // (`None`) must not desync `seen`/readiness from the inner ta-rs EMA
        // state (a phase shift in the determinism layer). Feed every valid candle
        // so the recursive state warms even while output is suppressed.
        let input = decimal_to_f64(candle.close)?;
        self.seen = self.seen.saturating_add(1);
        let out = self.inner.next(input);

        // Warmup: suppress the first `warmup - 1` candles; emit from candle
        // `warmup` onward.
        if self.seen < self.warmup {
            return None;
        }
        // The selected ta-rs output.
        match self.output {
            MacdOutput::Line => f64_to_decimal_rounded(out.macd),
            MacdOutput::Signal => f64_to_decimal_rounded(out.signal),
            MacdOutput::Histogram => f64_to_decimal_rounded(out.histogram),
        }
    }

    fn is_ready(&self) -> bool {
        // ready iff the NEXT call (the `seen + 1`-th candle) reaches candle
        // `warmup` or beyond.
        self.seen.saturating_add(1) >= self.warmup
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Macd;
    use crate::domain::{Candle, Indicator, MacdOutput};
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

    /// Independent oracle (architect-critic C4): a seeded-EMA recurrence computed
    /// *here in the test*, NOT read back from ta-rs's own EMA objects. ta-rs EMA
    /// is seeded (out[0] = close[0]); thereafter `out[t] = k*c + (1-k)*out[t-1]`,
    /// `k = 2/(p+1)`. The MACD line is `EMA(fast) − EMA(slow)` over the same
    /// series. Returns the full per-candle MACD-line reference.
    fn macd_line_reference(closes: &[f64], fast: u32, slow: u32) -> Vec<f64> {
        let ema = |period: u32| -> Vec<f64> {
            let k = 2.0 / (f64::from(period) + 1.0);
            let mut out = Vec::with_capacity(closes.len());
            let mut prev = closes[0];
            out.push(prev);
            for &c in &closes[1..] {
                prev = k * c + (1.0 - k) * prev;
                out.push(prev);
            }
            out
        };
        let fast_ema = ema(fast);
        let slow_ema = ema(slow);
        fast_ema
            .iter()
            .zip(slow_ema.iter())
            .map(|(f, s)| f - s)
            .collect()
    }

    /// The seeded-EMA signal reference: a seeded EMA of `period` over the FULL
    /// unblanked MACD line (mirroring ta-rs, which advances the signal EMA from
    /// candle 1), then `histogram = line − signal`.
    fn signal_reference(closes: &[f64], fast: u32, slow: u32, signal: u32) -> Vec<f64> {
        let line = macd_line_reference(closes, fast, slow);
        let k = 2.0 / (f64::from(signal) + 1.0);
        let mut out = Vec::with_capacity(line.len());
        let mut prev = line[0];
        out.push(prev);
        for &l in &line[1..] {
            prev = k * l + (1.0 - k) * prev;
            out.push(prev);
        }
        out
    }

    #[test]
    fn macd_returns_none_during_warmup_then_some() {
        let (fast, slow, signal) = (3u32, 6u32, 4u32);
        let warmup = slow; // MACD line first defined on the slow-period candle.
        let mut macd = Macd::new(fast, slow, signal, MacdOutput::Line).expect("periods >= 1");

        // Feed exactly `warmup - 1` candles → all None.
        for i in 1..warmup {
            let out = macd.next(&candle_close(&format!("{}", 100 + i)));
            assert_eq!(out, None, "candle {i} is warmup → None");
        }
        // Having fed exactly `slow - 1` candles, the NEXT call is candle
        // `warmup` → first defined MACD line.
        assert!(macd.is_ready(), "ready right before candle warmup");

        let out = macd.next(&candle_close("200"));
        assert!(out.is_some(), "candle warmup → Some");
        assert!(macd.is_ready(), "stays ready once warm");
    }

    #[test]
    fn macd_warmup_uses_max_of_fast_and_slow() {
        // Defense-in-depth: the engine factory rejects `fast >= slow`, but the
        // `pub` constructor (and ta-rs's own `MACD::new`) does not. With inverted
        // periods the warmup must gate on `max(fast, slow)` — the slower EMA — NOT
        // the bare `slow`, or it would emit before the longer EMA has reached its
        // nominal warmup.
        let (fast, slow, signal) = (6u32, 3u32, 4u32); // fast > slow (inverted)
        let warmup = fast.max(slow); // 6, not slow (3)
        let mut macd = Macd::new(fast, slow, signal, MacdOutput::Line).expect("periods >= 1");

        for i in 1..warmup {
            let out = macd.next(&candle_close(&format!("{}", 100 + i)));
            assert_eq!(out, None, "candle {i} (< max(fast,slow)) → None");
            assert_eq!(
                macd.is_ready(),
                i >= warmup - 1,
                "readiness tracks max warmup"
            );
        }
        // Candle `max(fast, slow)` is the first defined emission.
        let out = macd.next(&candle_close("200"));
        assert!(out.is_some(), "candle max(fast,slow) → first Some");
        assert!(macd.is_ready(), "stays ready once warm");
    }

    #[test]
    fn macd_resolves_to_macd_line() {
        let (fast, slow, signal) = (3u32, 6u32, 4u32);
        let warmup = slow;
        // A series comfortably longer than warmup so we exercise warm output.
        let closes = [
            2.0_f64, 3.0, 4.2, 7.0, 6.7, 6.5, 8.1, 9.4, 8.8, 10.2, 11.5, 10.9, 12.3, 13.0, 12.1,
        ];
        let reference = macd_line_reference(&closes, fast, slow);

        let mut macd = Macd::new(fast, slow, signal, MacdOutput::Line).expect("periods >= 1");
        let mut idx: u32 = 0;
        for (i, &c) in closes.iter().enumerate() {
            idx += 1;
            let out = macd.next(&candle_close(&c.to_string()));
            if idx < warmup {
                assert_eq!(out, None, "warmup candle {idx} → None");
            } else {
                let got: f64 = out.expect("warm → Some").to_string().parse().unwrap();
                let expected = reference[i];
                assert!(
                    (got - expected).abs() < 1e-6,
                    "candle {idx}: macd line got {got}, expected {expected}"
                );
            }
        }
    }

    /// r3.s2 — b2: the signal and histogram warm up over
    /// `max(fast, slow) + signal − 1` candles (the line first exists at
    /// `max(fast, slow)`; the seeded signal EMA needs `signal` line values).
    #[test]
    fn macd_signal_and_histogram_warmup_extends_to_max_plus_signal_minus_one() {
        let (fast, slow, signal) = (3u32, 6u32, 4u32);
        let warmup = fast.max(slow) + signal - 1; // 9, not 6

        for output in [MacdOutput::Signal, MacdOutput::Histogram] {
            let mut macd = Macd::new(fast, slow, signal, output).expect("periods >= 1");
            for i in 1..warmup {
                let out = macd.next(&candle_close(&format!("{}", 100 + i)));
                assert_eq!(out, None, "{output:?}: candle {i} (< warmup) → None");
                assert_eq!(
                    macd.is_ready(),
                    i >= warmup - 1,
                    "{output:?}: readiness tracks the extended warmup"
                );
            }
            let out = macd.next(&candle_close("200"));
            assert!(out.is_some(), "{output:?}: candle warmup → first Some");
        }
    }

    /// r3.s2 — b2: the emitted signal/histogram values match the independent
    /// seeded-EMA oracle (signal = seeded EMA(signal) of the full unblanked
    /// line, mirroring ta-rs advancing the signal EMA from candle 1;
    /// histogram = line − signal).
    #[test]
    fn macd_signal_and_histogram_match_the_seeded_ema_oracle() {
        let (fast, slow, signal) = (3u32, 6u32, 4u32);
        let warmup = fast.max(slow) + signal - 1;
        let closes = [
            2.0_f64, 3.0, 4.2, 7.0, 6.7, 6.5, 8.1, 9.4, 8.8, 10.2, 11.5, 10.9, 12.3, 13.0, 12.1,
        ];
        let line_ref = macd_line_reference(&closes, fast, slow);
        let signal_ref = signal_reference(&closes, fast, slow, signal);

        let mut macd_signal = Macd::new(fast, slow, signal, MacdOutput::Signal).expect("periods");
        let mut macd_hist = Macd::new(fast, slow, signal, MacdOutput::Histogram).expect("periods");
        for (i, &c) in closes.iter().enumerate() {
            let idx = u32::try_from(i + 1).expect("row count fits u32");
            let got_signal = macd_signal.next(&candle_close(&c.to_string()));
            let got_hist = macd_hist.next(&candle_close(&c.to_string()));
            if idx < warmup {
                assert_eq!(got_signal, None, "signal warmup candle {idx} → None");
                assert_eq!(got_hist, None, "histogram warmup candle {idx} → None");
            } else {
                let got: f64 = got_signal
                    .expect("warm → Some")
                    .to_string()
                    .parse()
                    .unwrap();
                assert!(
                    (got - signal_ref[i]).abs() < 1e-6,
                    "candle {idx}: signal got {got}, expected {}",
                    signal_ref[i]
                );
                let got_h: f64 = got_hist.expect("warm → Some").to_string().parse().unwrap();
                let expected_h = line_ref[i] - signal_ref[i];
                assert!(
                    (got_h - expected_h).abs() < 1e-6,
                    "candle {idx}: histogram got {got_h}, expected {expected_h}"
                );
            }
        }
    }
}
