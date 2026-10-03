//! Stateful regime detector (EMA50/200 + ADX14) — the **adapter** half of the
//! regime classifier (FR-5 / FR-6, BACKLOG-5).
//!
//! [`RegimeDetector`] composes the three VS-1.1.3 indicator adapters ([`Ema`]
//! ×2, [`Adx`]) **independently of the strategy's `IndicatorEngine`** — so the
//! regime is computed regardless of which indicators the strategy declares
//! (mirrors how `engine.rs` composes `IndicatorEngine`). Each [`RegimeDetector::step`]
//! advances all three indicators by exactly one candle and recomputes the
//! current [`Regime`]; the pure classification + value types live in
//! [`crate::domain::backtest::regime`].
//!
//! # Warmup → `Unknown` (issue #16)
//!
//! EMA200 is the binding warmup (~200 bars); ADX(14) warms at 28, EMA50 at 50.
//! Until **all three** have emitted at least one `Some`, `current` is
//! [`Regime::Unknown`] — never a silently-defaulted [`Regime::Ranging`]. The
//! pre-warm prefix is therefore a genuine "undetermined" state, not a live one.
//!
//! # Timeframe-agnostic
//!
//! The detector holds no notion of M15 vs H4; 2.04 chooses the series it steps
//! (v1 wires the primary M15 — slice README C7). No `f64` arithmetic here: it
//! consumes the indicators' `Decimal` outputs and the pure `classify` (NFR-2).
//!
//! # Construction (#26 context)
//!
//! The ADX reuses the VS-1.1.3 `Adx` (SMA-seeded Wilder); the short-series
//! ADX-seed divergence (#26) shifts early ADX values slightly but does not
//! change the coarse `> 25` threshold. Noted, not reconciled here.

use crate::domain::backtest::{Regime, classify};
use crate::domain::{Candle, Indicator};
use crate::{Adx, Ema};

/// Streaming regime detector: EMA50 vs EMA200 (direction) gated by ADX(14)
/// (strength), advanced one candle at a time. Holds the three indicator adapters
/// + the current [`Regime`] (the warmup-aware classification of the latest bar).
pub struct RegimeDetector {
    /// `Some` once constructed; `None` only in the impossible all-period-zero
    /// case, where the detector stays permanently [`Regime::Unknown`] (a
    /// panic-free degenerate — `unwrap`/`expect` are crate-denied).
    indicators: Option<Indicators>,
    current: Regime,
}

/// The three composed indicator adapters (held together so a failed construction
/// degrades the whole triple to "permanently warming" rather than panicking).
struct Indicators {
    ema50: Ema,
    ema200: Ema,
    adx: Adx,
}

impl Default for RegimeDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl RegimeDetector {
    /// Build a detector with `Ema::new(50)`, `Ema::new(200)`, `Adx::new(14)`.
    ///
    /// All three periods are non-zero literals, so the underlying adapter
    /// constructors (which only reject period 0) always succeed; on the
    /// impossible `None` the detector holds no indicators and stays
    /// [`Regime::Unknown`] forever rather than panicking (`unwrap`/`expect` are
    /// crate-denied).
    #[must_use]
    pub fn new() -> Self {
        // `?` short-circuits the impossible period-0 case to `None` without any
        // panic; the non-zero period literals guarantee `Some` in practice.
        // EMA50 vs EMA200 trend direction, ADX(14) trend strength (README C7).
        let indicators = (|| {
            Some(Indicators {
                ema50: Ema::new(50)?,
                ema200: Ema::new(200)?,
                adx: Adx::new(14)?,
            })
        })();
        Self {
            indicators,
            current: Regime::Unknown,
        }
    }

    /// Advance all three indicators by one candle and recompute the current
    /// regime. If **any** indicator is still warming (emits `None`), the regime
    /// is [`Regime::Unknown`] (#16); otherwise it is [`classify`]d from the three
    /// warm `Decimal` values.
    pub fn step(&mut self, candle: &Candle) {
        let Some(ind) = self.indicators.as_mut() else {
            // No indicators (impossible degenerate) → stays Unknown.
            self.current = Regime::Unknown;
            return;
        };
        // Advance ALL three every bar (their recursive state must warm in
        // lock-step with the others, exactly as the engine steps every declared
        // indicator each candle). Do not short-circuit on the first `None`.
        let ema50 = ind.ema50.next(candle);
        let ema200 = ind.ema200.next(candle);
        let adx = ind.adx.next(candle);

        self.current = match (ema50, ema200, adx) {
            (Some(e50), Some(e200), Some(a)) => classify(e50, e200, a),
            // Any warming indicator → undetermined (never silent `Ranging`).
            _ => Regime::Unknown,
        };
    }

    /// The regime of the most-recently-stepped candle ([`Regime::Unknown`] before
    /// any step, and while warming).
    #[must_use]
    pub fn current(&self) -> Regime {
        self.current
    }
}

#[cfg(test)]
impl RegimeDetector {
    /// Test-only state digest (the r3.s4.w1 step-atomicity proof): the
    /// current classification plus the readiness of all three adapters.
    /// The adapters' recursion state is a deterministic function of the
    /// stepped candle sequence, and the detector only steps after every
    /// fallible point in `EngineSession::step`, so equal digests plus the
    /// structural ordering imply equal later behaviour.
    pub(crate) fn state_digest(&self) -> String {
        let readiness = self.indicators.as_ref().map_or("none".to_owned(), |ind| {
            format!(
                "{}/{}",
                ind.ema50.is_ready() && ind.ema200.is_ready(),
                ind.adx.is_ready()
            )
        });
        format!(
            "RegimeDetector{{current:{:?},ready:{readiness}}}",
            self.current
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::RegimeDetector;
    use crate::domain::Candle;
    use crate::domain::backtest::Regime;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    /// Build an OHLC candle from f64 inputs (test-only; the production path is
    /// `Decimal`-only). High = close + 1, low = close − 1 unless overridden.
    fn ohlc(high: f64, low: f64, close: f64) -> Candle {
        let to_d = |x: f64| Decimal::from_str(&format!("{x}")).unwrap();
        Candle {
            open_time: 0,
            close_time: 0,
            open: to_d(close),
            high: to_d(high),
            low: to_d(low),
            close: to_d(close),
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    /// EMA200 is the binding warmup: it first emits `Some` on the 200th candle.
    /// We must feed at least 200 candles before any non-`Unknown` regime appears.
    const WARM_BARS: usize = 200;

    #[test]
    fn pre_warm_prefix_is_unknown_not_ranging() {
        // Before any step, and through the entire warmup prefix, the regime is
        // Unknown — NEVER silently Ranging (#16). EMA200 binds at 200 bars, so
        // every candle up to and including the 199th leaves the detector warming.
        let mut det = RegimeDetector::new();
        assert_eq!(det.current(), Regime::Unknown, "Unknown before any step");

        let mut price = 100.0_f64;
        for i in 1..WARM_BARS {
            det.step(&ohlc(price + 1.0, price - 1.0, price));
            assert_eq!(
                det.current(),
                Regime::Unknown,
                "candle {i} is within warmup → Unknown, never Ranging"
            );
            price += 1.0;
        }
    }

    #[test]
    fn clear_uptrend_classifies_trending_up_once_warm() {
        // A strong, monotonic uptrend: rising closes pull EMA50 above EMA200 and
        // drive ADX above the 25 threshold. Once EMA200 warms (200+ bars) the
        // detector reports TrendingUp — and the prefix was Unknown, not Ranging.
        let mut det = RegimeDetector::new();
        let mut price = 100.0_f64;
        // Feed a long monotonic ramp so all three indicators warm and agree.
        let total = WARM_BARS + 80;
        for _ in 0..total {
            det.step(&ohlc(price + 2.0, price, price + 1.0));
            price += 5.0;
        }
        assert_eq!(
            det.current(),
            Regime::TrendingUp,
            "a sustained uptrend warms to TrendingUp"
        );
    }

    #[test]
    fn clear_downtrend_classifies_trending_down_once_warm() {
        // A strong, monotonic downtrend: falling closes pull EMA50 below EMA200
        // with a strong ADX → TrendingDown once warm.
        let mut det = RegimeDetector::new();
        let mut price = 2_000.0_f64;
        let total = WARM_BARS + 80;
        for _ in 0..total {
            det.step(&ohlc(price, price - 2.0, price - 1.0));
            price -= 5.0;
        }
        assert_eq!(
            det.current(),
            Regime::TrendingDown,
            "a sustained downtrend warms to TrendingDown"
        );
    }

    #[test]
    fn flat_series_classifies_ranging_once_warm() {
        // A perfectly flat series: EMA50 == EMA200 (degenerate) AND ADX collapses
        // to 0 (no directional movement) → Ranging once warm. Either path lands
        // on Ranging; the warm regime must NOT be a trend.
        let mut det = RegimeDetector::new();
        let total = WARM_BARS + 80;
        for _ in 0..total {
            // Tiny constant band so high/low differ but there is no drift.
            det.step(&ohlc(100.5, 99.5, 100.0));
        }
        assert_eq!(
            det.current(),
            Regime::Ranging,
            "a flat/choppy series warms to Ranging, never a trend"
        );
    }

    #[test]
    fn step_is_deterministic_across_repeated_runs() {
        // NFR-2: stepping a fresh detector twice over the same candle stream
        // yields the identical regime sequence (exact, no f64 nondeterminism).
        let stream: Vec<Candle> = (0..WARM_BARS + 40)
            .map(|i| {
                let p = 100.0 + f64::from(u16::try_from(i).unwrap()) * 3.0;
                ohlc(p + 1.5, p - 0.5, p + 1.0)
            })
            .collect();

        let run = || -> Vec<Regime> {
            let mut det = RegimeDetector::new();
            stream
                .iter()
                .map(|c| {
                    det.step(c);
                    det.current()
                })
                .collect()
        };

        assert_eq!(
            run(),
            run(),
            "repeated runs yield identical regime sequence"
        );
    }
}
