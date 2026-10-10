//! Pattern indicators (schema **1.3.0**, r4.s6.w3) — double top/bottom
//! necklines and trendlines as streaming
//! [`Indicator`](crate::domain::Indicator) impls, built on w1's confirmed-pivot
//! tracker.
//!
//! **No `f64` anywhere, and no `convert` seam** — the `structure` rule. Every
//! value is exact [`Decimal`] arithmetic on prices the candles themselves
//! supplied: a neckline is one of the between-pivot bars' own low (or high),
//! and a trendline is the two-point line through two pivot prices. The one
//! division — the trendline's slope — is rounded to the scale-8 half-even grid
//! `convert` pins, so the value is byte-identical across runs and platforms.
//!
//! **No look-ahead, no repaint.** A value at bar t uses only pivots confirmed at
//! or before bar t and candles at or before t: the tracker reports a pivot
//! exactly `right` bars late, and the neckline's running extreme folds a bar
//! only once it is `right + 1` bars old — so the values on a series truncated at
//! any bar equal the full series' values over the overlap (the truncation
//! no-repaint property `tests/pattern_indicators.rs` asserts per variant).
//!
//! **Bounded memory.** A neckline keeps two pivots, one running scalar and a
//! delay line of `right + 1` entries — never the series; a trendline keeps two
//! pivots. A degenerate `left`/`right` (or a tolerance outside `(0, 0.10]`)
//! refuses at construction, so the engine turns it into a typed
//! `InvalidPeriod`.

use std::collections::VecDeque;

use rust_decimal::{Decimal, RoundingStrategy};

use super::convert::INDICATOR_SCALE;
use super::structure::{Extreme, Pivot, PivotTracker, within_tolerance};
use crate::domain::{Candle, Indicator};

/// Which neckline a [`NecklineIndicator`] computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NecklineKind {
    /// `DoubleTopNeckline`: the last two confirmed pivot HIGHS within
    /// tolerance; the value is the LOWEST LOW strictly between them.
    DoubleTop,
    /// `DoubleBottomNeckline`: the last two confirmed pivot LOWS within
    /// tolerance; the value is the HIGHEST HIGH strictly between them.
    DoubleBottom,
}

impl NecklineKind {
    /// The extreme this neckline's pivots are: highs for a double top, lows for
    /// a double bottom.
    fn pivot_extreme(self) -> Extreme {
        match self {
            Self::DoubleTop => Extreme::High,
            Self::DoubleBottom => Extreme::Low,
        }
    }

    /// The between-pivot extreme the neckline's value is made of: the LOWEST low
    /// for a double top, the HIGHEST high for a double bottom.
    fn between_extreme(self) -> Extreme {
        match self {
            Self::DoubleTop => Extreme::Low,
            Self::DoubleBottom => Extreme::High,
        }
    }
}

/// `DoubleTopNeckline` / `DoubleBottomNeckline`: the between-pivot extreme of
/// the last two within-tolerance pivots.
///
/// The value at bar t is the lowest low (highest high) of the bars strictly
/// between the last two confirmed pivots, when those pivots are within
/// `tolerance_pct` of each other and at least two bars apart; `None` otherwise,
/// and `None` until a second pivot confirms. It is recomputed at each
/// confirmation, so it holds until a newer pivot arrives — the spec's rule.
#[derive(Debug, Clone)]
pub struct NecklineIndicator {
    /// The shared confirmed-pivot tracker (the same one `PivotHigh`/`PivotLow`
    /// run on).
    tracker: PivotTracker,
    /// Which neckline this is.
    kind: NecklineKind,
    /// How close two pivots must be to be a double top/bottom, as a decimal
    /// fraction (`0.003` = 0.3%).
    tolerance_pct: Decimal,
    /// The tracker's right span — the delay the between-pivot extreme runs at.
    right: u32,
    /// The earlier (A) of the pair in force; `None` until a second pivot
    /// confirms, which is also what `is_ready` reports.
    earlier: Option<Pivot>,
    /// The later (B) of the pair in force — the most recent confirmed pivot.
    latest: Option<Pivot>,
    /// The value in force; recomputed only when a pivot confirms.
    value: Option<Decimal>,
    /// Bars not yet folded into `running`, oldest first — bounded to
    /// `right + 1` entries and carrying the absolute bar, so a confirmation
    /// drops exactly the bars at or before the new pivot.
    pending: VecDeque<(u64, Decimal)>,
    /// The running extreme over the bars strictly after the last pivot.
    running: Option<Decimal>,
    /// Bars fed so far.
    seen: u64,
}

impl NecklineIndicator {
    /// Build the indicator for a fixed parameter tuple; `None` when the span is
    /// degenerate (`left`/`right` 0) or `tolerance_pct` sits outside
    /// `(0, 0.10]` — the engine turns that into a typed `InvalidPeriod` (the
    /// `LevelIndicator` rule set).
    #[must_use]
    pub fn new(left: u32, right: u32, tolerance_pct: Decimal, kind: NecklineKind) -> Option<Self> {
        if left == 0
            || right == 0
            || tolerance_pct <= Decimal::ZERO
            || tolerance_pct > Decimal::new(10, 2)
        {
            return None;
        }
        Some(Self {
            tracker: PivotTracker::new(left, right, kind.pivot_extreme()),
            kind,
            tolerance_pct,
            right,
            earlier: None,
            latest: None,
            value: None,
            pending: VecDeque::new(),
            running: None,
            seen: 0,
        })
    }

    /// Push this bar's own extreme behind the delay line and fold every bar that
    /// has aged out of it (older than `right` bars) into `running`.
    ///
    /// The fold is what makes the value at a confirmation exact: when the
    /// tracker confirms the pivot at bar `p` (at bar `t = p + right`), `running`
    /// covers the bars `[anchor + 1, p − 1]` — precisely the bars strictly
    /// between the anchor and the new pivot.
    fn advance_extreme(&mut self, candle: &Candle) {
        let own = match self.kind.between_extreme() {
            Extreme::Low => candle.low,
            Extreme::High => candle.high,
        };
        self.pending.push_back((self.seen - 1, own));
        while self.pending.len() as u64 > u64::from(self.right) + 1 {
            let Some((_, value)) = self.pending.pop_front() else {
                break;
            };
            self.running = Some(match (self.running, self.kind.between_extreme()) {
                (None, _) => value,
                (Some(current), Extreme::Low) => current.min(value),
                (Some(current), Extreme::High) => current.max(value),
            });
        }
    }
}

impl Indicator for NecklineIndicator {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        self.seen = self.seen.saturating_add(1);
        self.advance_extreme(candle);
        if let Some(pivot) = self.tracker.push(candle) {
            self.value = match self.latest {
                None => None,
                Some(previous) => {
                    let within = within_tolerance(
                        previous.price.min(pivot.price),
                        previous.price.max(pivot.price),
                        self.tolerance_pct,
                    );
                    if within && pivot.bar.saturating_sub(previous.bar) >= 2 {
                        self.running
                    } else {
                        None
                    }
                }
            };
            self.earlier = self.latest;
            self.latest = Some(pivot);
            // The new pivot anchors the next pair: its own bar is excluded from
            // the next neckline, and the range between it and the pivot after it
            // starts empty.
            while let Some((bar, _)) = self.pending.front() {
                if *bar <= pivot.bar {
                    self.pending.pop_front();
                } else {
                    break;
                }
            }
            self.running = None;
        }
        self.value
    }

    fn is_ready(&self) -> bool {
        // A neckline cannot promise "the next call returns Some": whether the
        // NEXT pivot pair sits within tolerance is the next pivot's own fact.
        // This reports the part the indicator can know — a pair has confirmed, so
        // a value is possible at all — the `LevelIndicator` reading of the same
        // limit. The engine's warm gate reads this as genuine readiness for a
        // level/pattern slot (r4.s6 PR-362 fix round 1): past it, a `None` value
        // is a no-value reading, false in a comparison, never an entry itself.
        self.earlier.is_some()
    }
}

/// `TrendlineLows` / `TrendlineHighs`: the two-point line through the last two
/// confirmed pivots, extended to the current bar.
///
/// The value at bar t is `B.price + (B.price − A.price) × (t − B.bar) /
/// (B.bar − A.bar)` when the pair points the indicator's way — a RISING pair of
/// pivot lows (`B.price > A.price`) for `TrendlineLows`, a FALLING pair of
/// pivot highs (`B.price < A.price`) for `TrendlineHighs` — and `None`
/// otherwise. The division rounds to the scale-8 half-even grid `convert` pins
/// (the determinism rule for a real-valued quotient), and every step is a
/// checked operation, so an overflow gives `None` rather than a panic (the
/// `group_mean` precedent).
#[derive(Debug, Clone)]
pub struct TrendlineIndicator {
    /// The shared confirmed-pivot tracker.
    tracker: PivotTracker,
    /// `true` for `TrendlineLows`: the pair must rise.
    rising: bool,
    /// The earlier (A) of the pair in force.
    earlier: Option<Pivot>,
    /// The later (B) of the pair in force — the most recent confirmed pivot.
    latest: Option<Pivot>,
    /// Bars fed so far (the bar the line is evaluated at).
    seen: u64,
}

impl TrendlineIndicator {
    /// Build the indicator for `left`/`right`; `None` when either is 0 (a
    /// degenerate window, refused by the engine as an invalid period). `rising`
    /// selects `TrendlineLows` (lows, a rising pair) or `TrendlineHighs`
    /// (highs, a falling pair).
    #[must_use]
    pub fn new(left: u32, right: u32, rising: bool) -> Option<Self> {
        if left == 0 || right == 0 {
            return None;
        }
        let extreme = if rising { Extreme::Low } else { Extreme::High };
        Some(Self {
            tracker: PivotTracker::new(left, right, extreme),
            rising,
            earlier: None,
            latest: None,
            seen: 0,
        })
    }

    /// The line at `bar`, or `None` while no pair has confirmed, the pair points
    /// the wrong way, or an operation overflows.
    fn line_at(&self, bar: u64) -> Option<Decimal> {
        let (earlier, latest) = (self.earlier?, self.latest?);
        let delta = latest.price.checked_sub(earlier.price)?;
        let points_the_right_way = if self.rising {
            delta > Decimal::ZERO
        } else {
            delta < Decimal::ZERO
        };
        if !points_the_right_way {
            return None;
        }
        let bars = Decimal::from(bar.checked_sub(latest.bar)?);
        let span = Decimal::from(latest.bar.checked_sub(earlier.bar)?);
        let slope = delta
            .checked_mul(bars)?
            .checked_div(span)?
            .round_dp_with_strategy(INDICATOR_SCALE, RoundingStrategy::MidpointNearestEven);
        slope.checked_add(latest.price)
    }
}

impl Indicator for TrendlineIndicator {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        self.seen = self.seen.saturating_add(1);
        if let Some(pivot) = self.tracker.push(candle) {
            self.earlier = self.latest;
            self.latest = Some(pivot);
        }
        self.line_at(self.seen - 1)
    }

    fn is_ready(&self) -> bool {
        // The neckline's reading of the same limit: a pair has confirmed, so a
        // value is possible at all — and the engine's warm gate reads it as
        // readiness for this slot (r4.s6 PR-362 fix round 1).
        self.earlier.is_some()
    }
}
