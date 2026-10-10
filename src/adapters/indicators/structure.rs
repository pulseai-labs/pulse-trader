//! Structure indicators (schema **1.3.0**, r4.s6.w1) — swing pivots,
//! support/resistance levels and candle anatomy as streaming
//! [`Indicator`](crate::domain::Indicator) impls.
//!
//! **No `f64` anywhere, and no `convert` seam.** Every value here is exact
//! [`Decimal`] arithmetic straight off the candles — a pivot price is one of the
//! inputs, a level is a mean of pivot prices, and the anatomy values are
//! differences of the current bar's own four prices. Nothing is smoothed, so
//! nothing needs the fixed-scale-8 rounding the ta-rs adapters use.
//!
//! **No look-ahead, no repaint.** The pivot tracker emits a pivot only once bar
//! `p + right` has closed; a value computed at bar t never reads a later bar, so
//! the values on a series truncated at any bar equal the full series' values
//! over the overlap (the truncation no-repaint property each variant is tested
//! against).
//!
//! **Lazy rings (the [`RollingExtremes`](super::rolling::RollingExtremes) rule).**
//! `pulse serve` accepts raw DSL over MCP from any authenticated client, so an
//! absurd `left`/`right`/`lookback` must never reserve memory up front: every
//! ring grows one entry per fed bar and simply never warms.

use std::collections::VecDeque;

use crate::domain::{Candle, Indicator};
use rust_decimal::Decimal;

/// The extreme a [`PivotTracker`] confirms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Extreme {
    /// A swing high (`PivotHigh`, `Resistance`).
    High,
    /// A swing low (`PivotLow`, `Support`).
    Low,
}

/// One confirmed swing pivot: the bar it sits on (absolute, 0-based) and its
/// price.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pivot {
    /// The pivot bar's absolute index in the fed series.
    pub(crate) bar: u64,
    /// The pivot's high (for [`Extreme::High`]) or low (for [`Extreme::Low`]).
    pub(crate) price: Decimal,
}

/// The confirmed-pivot tracker shared by `PivotHigh`/`PivotLow` and the level
/// indicators (and reused by the pattern indicators that build on them).
///
/// Bar p is a pivot for a `left`/`right` pair when its high is **strictly
/// greater** than the `left` highs before it and **greater-or-equal** to the
/// `right` highs after it (mirrored on lows). Equality is deliberate on the
/// right: a run of equal extremes therefore resolves to its FIRST bar, and every
/// later equal bar fails the strict-left test — a tie is never two pivots.
///
/// The tracker holds a ring of `left + right + 1` values, grown one per bar, and
/// reports a pivot only when the bar `right` places after it has closed — the
/// value it returns is `(pivot bar, price)` and the caller learns of it exactly
/// `right` bars late.
#[derive(Debug, Clone)]
pub(crate) struct PivotTracker {
    /// Bars the pivot must exceed to its left (strictly).
    left: u32,
    /// Bars the pivot must be at least equal to on its right.
    right: u32,
    /// Which extreme this tracker confirms.
    extreme: Extreme,
    /// The last `left + right + 1` values, oldest first.
    window: VecDeque<Decimal>,
    /// Bars fed so far.
    seen: u64,
}

impl PivotTracker {
    /// Build a tracker for `left`/`right` on `extreme`.
    #[must_use]
    pub(crate) fn new(left: u32, right: u32, extreme: Extreme) -> Self {
        Self {
            left,
            right,
            extreme,
            window: VecDeque::new(),
            seen: 0,
        }
    }

    /// The window span: the bars a pivot needs around it.
    fn span(&self) -> u64 {
        u64::from(self.left) + u64::from(self.right) + 1
    }

    /// Feed one closed bar; return the pivot it confirms, if any.
    pub(crate) fn push(&mut self, candle: &Candle) -> Option<Pivot> {
        let value = self.value_of(candle);
        self.seen = self.seen.saturating_add(1);
        self.window.push_back(value);
        while self.window.len() as u64 > self.span() {
            self.window.pop_front();
        }
        if (self.window.len() as u64) < self.span() {
            return None;
        }

        // The window is exactly `left + right + 1` values long, so the
        // candidate sits at index `left` with `left` values before it and
        // `right` after it.
        let left = self.left as usize;
        let candidate = self.window[left];
        let left_ok = self
            .window
            .iter()
            .take(left)
            .all(|value| self.beats(candidate, *value));
        let right_ok = self
            .window
            .iter()
            .skip(left + 1)
            .all(|value| self.meets(candidate, *value));
        if !left_ok || !right_ok {
            return None;
        }

        Some(Pivot {
            bar: self.seen - 1 - u64::from(self.right),
            price: candidate,
        })
    }

    /// STRICT dominance: a high beats a lower high, a low beats a higher low.
    fn beats(&self, value: Decimal, other: Decimal) -> bool {
        match self.extreme {
            Extreme::High => value > other,
            Extreme::Low => value < other,
        }
    }

    /// NON-STRICT dominance (the right-side test, where a tie still confirms).
    fn meets(&self, value: Decimal, other: Decimal) -> bool {
        match self.extreme {
            Extreme::High => value >= other,
            Extreme::Low => value <= other,
        }
    }

    /// The pivot price of a candle: its high or its low.
    fn value_of(&self, candle: &Candle) -> Decimal {
        match self.extreme {
            Extreme::High => candle.high,
            Extreme::Low => candle.low,
        }
    }
}

/// How close two pivot prices must be to count as one level, as an exact
/// comparison with no division: `price − anchor ≤ anchor × tolerance_pct`. The
/// DSL's `tolerance_pct` is a decimal FRACTION on the `distance_pct` convention
/// (`0.003` = 0.3%, `0.05` = 5%), so the allowance is `anchor` scaled by it —
/// no `/100` anywhere.
///
/// Prices inside a group are sorted ascending, so `price − anchor` is never
/// negative. A checked operation that overflows reports "not within tolerance":
/// no group, never a panic.
fn within_tolerance(anchor: Decimal, price: Decimal, tolerance_pct: Decimal) -> bool {
    let Some(spread) = price.checked_sub(anchor) else {
        return false;
    };
    let Some(allowance) = anchor.checked_mul(tolerance_pct) else {
        return false;
    };
    spread <= allowance
}

/// The level of a group of touching pivots: their arithmetic mean, or `None`
/// when the sum or the division overflows `Decimal` (a degenerate input never
/// panics and never produces a level).
fn group_mean(total: Decimal, count: u32) -> Option<Decimal> {
    let divisor = Decimal::from(count);
    if divisor.is_zero() {
        return None;
    }
    total.checked_div(divisor)
}

/// The pivot tracker as an indicator: `PivotHigh` / `PivotLow`.
///
/// The value at bar t is the price of the **most recent confirmed** pivot —
/// `None` until the first confirmation, which lands exactly `right` bars after
/// its pivot bar.
#[derive(Debug, Clone)]
pub struct PivotIndicator {
    tracker: PivotTracker,
    latest: Option<Decimal>,
}

impl PivotIndicator {
    /// Build the indicator for `left`/`right`; `None` when either is 0 (a
    /// degenerate window, refused by the engine as an invalid period).
    #[must_use]
    pub fn new(left: u32, right: u32, extreme: Extreme) -> Option<Self> {
        if left == 0 || right == 0 {
            return None;
        }
        Some(Self {
            tracker: PivotTracker::new(left, right, extreme),
            latest: None,
        })
    }
}

impl Indicator for PivotIndicator {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        if let Some(pivot) = self.tracker.push(candle) {
            self.latest = Some(pivot.price);
        }
        self.latest
    }

    fn is_ready(&self) -> bool {
        // Ready iff the NEXT call returns Some: a pivot is already confirmed.
        self.latest.is_some()
    }
}

/// `Resistance` / `Support`: the nearest qualifying level from confirmed pivots.
///
/// The value at bar t is the lowest level **strictly above** the current close
/// (the highest **strictly below** it for support), built from the confirmed
/// pivots within the last `lookback` bars, grouped by `tolerance_pct`, keeping
/// only groups with at least `min_touches` pivots. `None` when no group
/// qualifies.
#[derive(Debug, Clone)]
pub struct LevelIndicator {
    /// The shared confirmed-pivot tracker (the same one `PivotHigh`/`PivotLow`
    /// run on).
    tracker: PivotTracker,
    /// The confirmed pivots still inside the window, oldest first.
    pivots: VecDeque<Pivot>,
    /// The level's half-width as a decimal fraction (`0.003` = 0.3%).
    tolerance_pct: Decimal,
    /// How many touching pivots a level needs to qualify.
    min_touches: u32,
    /// How many bars back a confirmed pivot still counts.
    lookback: u32,
    /// Which side of the close the level sits on: `true` = resistance (above),
    /// `false` = support (below).
    above: bool,
    /// Bars fed so far (mirrors the tracker's counter so the window rule can be
    /// evaluated against the current bar).
    seen: u64,
}

impl LevelIndicator {
    /// Build the indicator for a fixed parameter tuple; `None` when any
    /// parameter is degenerate (`left`/`right` 0, `min_touches` < 2,
    /// `lookback <= left + right`, `tolerance_pct` outside `(0, 0.10]`) — the
    /// engine turns that into a typed `InvalidPeriod` refusal.
    #[must_use]
    pub fn new(
        left: u32,
        right: u32,
        tolerance_pct: Decimal,
        min_touches: u32,
        lookback: u32,
        above: bool,
    ) -> Option<Self> {
        let span = u64::from(left) + u64::from(right);
        if left == 0
            || right == 0
            || min_touches < 2
            || u64::from(lookback) <= span
            || tolerance_pct <= Decimal::ZERO
            || tolerance_pct > Decimal::new(10, 2)
        {
            return None;
        }
        let extreme = if above { Extreme::High } else { Extreme::Low };
        Some(Self {
            tracker: PivotTracker::new(left, right, extreme),
            pivots: VecDeque::new(),
            tolerance_pct,
            min_touches,
            lookback,
            above,
            seen: 0,
        })
    }

    /// The current bar's level, from the in-window pivots and this close.
    fn level(&self, close: Decimal) -> Option<Decimal> {
        if self.pivots.len() < self.min_touches as usize {
            return None;
        }
        let mut prices: Vec<Decimal> = self.pivots.iter().map(|pivot| pivot.price).collect();
        prices.sort();

        let mut best: Option<Decimal> = None;
        let mut index = 0;
        while index < prices.len() {
            // The group's anchor is its LOWEST price; every later price joins
            // while it sits within `tolerance_pct` of that anchor.
            let anchor = prices[index];
            let mut total = anchor;
            let mut count = 1u32;
            let mut cursor = index + 1;
            let mut overflow = false;
            while cursor < prices.len()
                && within_tolerance(anchor, prices[cursor], self.tolerance_pct)
            {
                match total.checked_add(prices[cursor]) {
                    Some(sum) => total = sum,
                    None => overflow = true,
                }
                count = count.saturating_add(1);
                cursor += 1;
            }
            if overflow {
                index = cursor;
                continue;
            }
            if count >= self.min_touches
                && let Some(mean) = group_mean(total, count)
            {
                let qualifies = if self.above {
                    mean > close
                } else {
                    mean < close
                };
                if qualifies {
                    best = Some(match best {
                        None => mean,
                        Some(current) if self.above => current.min(mean),
                        Some(current) => current.max(mean),
                    });
                }
            }
            index = cursor;
        }
        best
    }
}

impl Indicator for LevelIndicator {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        self.seen = self.seen.saturating_add(1);
        if let Some(pivot) = self.tracker.push(candle) {
            self.pivots.push_back(pivot);
        }
        let current = self.seen - 1;
        while let Some(oldest) = self.pivots.front() {
            if current - oldest.bar < u64::from(self.lookback) {
                break;
            }
            self.pivots.pop_front();
        }
        self.level(candle.close)
    }

    fn is_ready(&self) -> bool {
        // The trait's `is_ready` is an exact "the next call returns Some" for
        // every other adapter; a level cannot promise that, because whether the
        // NEXT bar's close sits below (resistance) or above (support) the
        // qualifying group is the next bar's own fact. This reports the part
        // the indicator can know: enough touches are in the window for a level
        // to be possible at all. The direction is the safe one — it never lets
        // the entry gate fire on an indicator that has not yet produced any
        // level, and a bar whose close disqualifies the group simply evaluates
        // `None` (a comparison over it is false, never an entry).
        self.pivots.len() >= self.min_touches as usize
    }
}

/// Which candle-anatomy value an [`Anatomy`] indicator computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnatomyKind {
    /// `|close − open|` — the bar's body height.
    Body,
    /// `high − max(open, close)` — the upper wick.
    UpperWick,
    /// `min(open, close) − low` — the lower wick.
    LowerWick,
    /// `high − low` — the bar's full range.
    Range,
}

/// The four candle-anatomy indicators: direct `Decimal` arithmetic on the
/// current bar, defined from the very first bar.
#[derive(Debug, Clone, Copy)]
pub struct Anatomy {
    /// Which anatomy value to compute.
    kind: AnatomyKind,
}

impl Anatomy {
    /// Build the anatomy indicator for `kind`.
    #[must_use]
    pub fn new(kind: AnatomyKind) -> Self {
        Self { kind }
    }
}

impl Indicator for Anatomy {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        // A bar's own anatomy is always defined — the first bar already has one.
        Some(match self.kind {
            AnatomyKind::Body => (candle.close - candle.open).abs(),
            AnatomyKind::UpperWick => candle.high - candle.open.max(candle.close),
            AnatomyKind::LowerWick => candle.open.min(candle.close) - candle.low,
            AnatomyKind::Range => candle.high - candle.low,
        })
    }

    fn is_ready(&self) -> bool {
        // Every bar has anatomy, including the first: the next call always
        // returns Some.
        true
    }
}
