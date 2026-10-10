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
///
/// Shared with the pattern indicators (`super::patterns`), whose necklines ask
/// the same question of two pivot prices — the seam is this visibility word,
/// never a copy of the rule.
pub(crate) fn within_tolerance(anchor: Decimal, price: Decimal, tolerance_pct: Decimal) -> bool {
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
/// pivots within the last `lookback` bars. A candidate level is a **run of
/// sorted pivot prices whose highest is within `tolerance_pct` of their
/// lowest** — for each start `i`, the maximal run `p_i..=p_j` with
/// `p_j − p_i ≤ p_i × tolerance_pct`, kept when it holds at least
/// `min_touches` prices, with the run's mean as its value. `None` when no run
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
    /// The readiness latch (r4.s6 PR-362 fix round 2, rule A): `true` once the
    /// window has held `min_touches` pivots at any bar, and never back to
    /// `false`. Readiness must be monotonic because the engine's warm gate
    /// reads it: a pivot aging out of `lookback` must not make a warmed level
    /// slot cold again and freeze every unrelated entry branch behind it.
    ready: bool,
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
            ready: false,
        })
    }

    /// The current bar's level, from the in-window pivots and this close.
    ///
    /// Candidate levels are the runs of SORTED pivot prices whose highest is
    /// within `tolerance_pct` of their lowest: for each start `i`, the run
    /// `p_i..=p_j` with the largest `j` satisfying
    /// `within_tolerance(p_i, p_j, tolerance_pct)` — `p_j − p_i ≤ p_i × tol`,
    /// checked ops. A run of at least `min_touches` prices is a candidate with
    /// value their mean (the `group_mean` rounding); the value is the LOWEST
    /// candidate strictly above the close (the HIGHEST strictly below it for
    /// support), `None` when no run qualifies. Runs OVERLAP by construction: a
    /// price may be the top of one candidate and the anchor of the next, so a
    /// cluster the old disjoint sweep split — `[100, 109]` then a lone `118`
    /// at 10% tolerance — still forms `[109, 118]`.
    fn level(&self, close: Decimal) -> Option<Decimal> {
        let mut best: Option<Decimal> = None;
        self.for_each_candidate(|mean| {
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
        });
        best
    }

    /// Visit every candidate level — the mean of each maximal within-tolerance
    /// run of the sorted in-window pivot prices — in ascending-anchor order.
    /// [`LevelIndicator::level`] keeps the best of them; the scan's proptest
    /// compares the whole set against a naive reference scan.
    fn for_each_candidate(&self, mut visit: impl FnMut(Decimal)) {
        if self.pivots.len() < self.min_touches as usize {
            return;
        }
        let mut prices: Vec<Decimal> = self.pivots.iter().map(|pivot| pivot.price).collect();
        prices.sort();

        // The window `[start, end)` is the maximal run for the current start.
        // `end` moves forward only — the tolerance threshold `p_i × (1 + tol)`
        // grows with the anchor — so every price enters and leaves the window
        // once and the whole scan is linear in the pivot count (r4.s6 PR-362
        // fix round 2, rule C). The window's sum is carried across starts:
        // subtract the anchor that leaves, add the prices that enter.
        let mut end = 1_usize;
        let mut total = prices[0];
        let mut sum_ok = true;
        for start in 0..prices.len() {
            if end < start + 1 {
                // The previous run ended at its anchor: open a fresh window.
                end = start + 1;
                total = prices[start];
                sum_ok = true;
            } else if !sum_ok {
                // A run left `Decimal`'s range: rebuild this window's sum from
                // scratch (the checked-op contract) and carry on.
                let mut acc = Decimal::ZERO;
                let mut ok = true;
                for price in &prices[start..end] {
                    if let Some(sum) = acc.checked_add(*price) {
                        acc = sum;
                    } else {
                        ok = false;
                        break;
                    }
                }
                total = acc;
                sum_ok = ok;
            } else if start > 0 {
                // The anchor that leaves the window; subtracting from a
                // representable sum is exact.
                total = total
                    .checked_sub(prices[start - 1])
                    .unwrap_or(Decimal::ZERO);
            }
            while end < prices.len()
                && within_tolerance(prices[start], prices[end], self.tolerance_pct)
            {
                let Some(sum) = total.checked_add(prices[end]) else {
                    // The run's sum left `Decimal`'s range: this start has no
                    // candidate — never a phantom mean (R2-6). The next start's
                    // scan resumes here and rebuilds the sum from scratch.
                    sum_ok = false;
                    break;
                };
                total = sum;
                end += 1;
            }
            let count = u32::try_from(end - start).unwrap_or(u32::MAX);
            if !sum_ok || count < self.min_touches {
                continue;
            }
            if let Some(mean) = group_mean(total, count) {
                visit(mean);
            }
        }
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
        // Latch readiness (rule A): once the window has held `min_touches`
        // pivots, it stays ready — the eviction above must not un-warm a slot
        // the engine's gate already opened.
        self.ready = self.ready || self.pivots.len() >= self.min_touches as usize;
        self.level(candle.close)
    }

    fn is_ready(&self) -> bool {
        // The trait's `is_ready` is an exact "the next call returns Some" for
        // every other adapter; a level cannot promise that, because whether the
        // NEXT bar's close sits below (resistance) or above (support) a
        // qualifying run is the next bar's own fact. This reports the part the
        // indicator can know: enough pivots have been in the window for a level
        // to be possible at all. The engine's warm gate reads it as readiness
        // for a level/pattern slot (r4.s6 PR-362 fix rounds 1-2): past it, a
        // `None` value means "no level at this bar" and a comparison over it is
        // false — it can never be an entry by itself. LATCHED: once true it
        // never reverts (a pivot aging out of `lookback` cannot freeze the gate).
        self.ready
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
        // A bar's own anatomy is defined from the very first bar. Every step
        // is a checked operation — an overflow gives `None`, never a panic
        // (the ADR-0024 addendum's contract, the `group_mean` precedent).
        match self.kind {
            AnatomyKind::Body => candle.close.checked_sub(candle.open).map(|body| body.abs()),
            AnatomyKind::UpperWick => candle.high.checked_sub(candle.open.max(candle.close)),
            AnatomyKind::LowerWick => candle.open.min(candle.close).checked_sub(candle.low),
            AnatomyKind::Range => candle.high.checked_sub(candle.low),
        }
    }

    fn is_ready(&self) -> bool {
        // Every bar has anatomy, including the first: on every bar a real
        // candle can carry, the next call returns Some (the checked steps
        // above are the overflow contract, not an expected `None`).
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Extreme, LevelIndicator, Pivot, PivotTracker, group_mean, within_tolerance};
    use proptest::prelude::*;
    use proptest::test_runner::{Config as ProptestConfig, RngSeed};
    use rust_decimal::Decimal;
    use std::collections::VecDeque;

    /// The F4 rule written out naively — every start scanned from scratch, in
    /// the order the rule states — the oracle the two-pointer scan must
    /// reproduce exactly: same candidates, same order, same duplicates.
    fn naive_candidates(
        prices: &[Decimal],
        tolerance_pct: Decimal,
        min_touches: u32,
    ) -> Vec<Decimal> {
        let mut sorted = prices.to_vec();
        sorted.sort();
        let mut candidates = Vec::new();
        for start in 0..sorted.len() {
            let anchor = sorted[start];
            let mut total = anchor;
            let mut count = 1_u32;
            let mut overflow = false;
            for &price in &sorted[start + 1..] {
                if !within_tolerance(anchor, price, tolerance_pct) {
                    break;
                }
                match total.checked_add(price) {
                    Some(sum) => total = sum,
                    None => overflow = true,
                }
                count = count.saturating_add(1);
            }
            if overflow || count < min_touches {
                continue;
            }
            if let Some(mean) = group_mean(total, count) {
                candidates.push(mean);
            }
        }
        candidates
    }

    /// A `LevelIndicator` whose window holds exactly `prices`. The scan reads
    /// only `pivots`, `tolerance_pct` and `min_touches`, so the pivot tracker is
    /// bypassed and the run scan is exercised directly.
    fn indicator_with_prices(
        prices: &[Decimal],
        tolerance_pct: Decimal,
        min_touches: u32,
    ) -> LevelIndicator {
        LevelIndicator {
            tracker: PivotTracker::new(1, 1, Extreme::High),
            pivots: prices
                .iter()
                .enumerate()
                .map(|(bar, price)| Pivot {
                    bar: bar as u64,
                    price: *price,
                })
                .collect::<VecDeque<_>>(),
            tolerance_pct,
            min_touches,
            lookback: 60,
            above: true,
            seen: 0,
            ready: true,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 512,
            rng_seed: RngSeed::Fixed(0x725F_5EED),
            ..ProptestConfig::default()
        })]

        /// Rule C's proof (r4.s6 PR-362 fix round 2): over random sorted price
        /// sets (1..=24 prices in 1..=2000), tolerances in (0, 0.10] and
        /// `min_touches` 1..=5, the two-pointer scan yields exactly the naive
        /// reference scan's candidate list. Deterministic: fixed seed, 512 cases.
        #[test]
        fn the_two_pointer_scan_matches_the_naive_reference(
            mut prices in prop::collection::vec(1_i64..=2_000, 1..=24),
            tolerance_milli in 1_u32..=100,
            min_touches in 1_u32..=5,
        ) {
            prices.sort_unstable();
            let prices: Vec<Decimal> = prices.into_iter().map(Decimal::from).collect();
            let tolerance_pct = Decimal::new(i64::from(tolerance_milli), 3);
            let indicator = indicator_with_prices(&prices, tolerance_pct, min_touches);
            let mut got = Vec::new();
            indicator.for_each_candidate(|mean| got.push(mean));
            let want = naive_candidates(&prices, tolerance_pct, min_touches);
            prop_assert_eq!(got, want);
        }
    }
}
