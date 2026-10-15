//! Named candlestick patterns (schema **1.4.0**, r5.s1.w1) as a streaming
//! [`Indicator`](crate::domain::Indicator) impl.
//!
//! **The contract is `candle-pattern-definitions.md`**, not this file: this
//! module is a transcription of its §1 primitives, §2 settings, §3 (this item's
//! 13 patterns) and §5 lookbacks. Every value is **TA-Lib's own integer** —
//! `+100` bullish, `-100` bearish, `0` no pattern, and `±80` for the partial
//! forms of `Engulfing` and `Harami`, where one body end coincides with the
//! prior body's edge.
//!
//! **No `f64`, no `convert` seam.** Every comparison is exact [`Decimal`]
//! arithmetic on prices the candles supplied — the `structure`/`patterns`
//! discipline (ADR-0019). The definitions' §2.2 tip is taken literally: a
//! condition `x ⋛ avg(S, k)` is evaluated as `N × x ⋛ f × sum`, which is the
//! same test with **no division**, because `N > 0` and every factor is a short
//! decimal. Every operation is checked: an overflow makes a condition false,
//! never a panic.
//!
//! **Warm-up is a plain lookback, and the value never goes back to `None`.**
//! For the 0-based bar index `i < L` the indicator yields `None`; from `i >= L`
//! it yields the pattern's integer on **every** bar. `L` is TA-Lib's declared
//! lookback (§5), used *exactly* as listed — for hammer, hanging man, inverted
//! hammer and shooting star it is one bar more than the conditions need, and
//! for engulfing it is 2 while the conditions need only 1. Deriving it from the
//! widest window would shift the first ready bar. Because a value is always
//! available after warm-up, this variant is deliberately **not** in the engine's
//! `is_level_or_pattern` carve-out: the engine's ordinary all-`Some` warm rule
//! is exactly right for it.
//!
//! **No look-ahead, no repaint.** A value at bar t reads only bars at or before
//! t, so the values on a series truncated at any bar equal the full series'
//! values over the overlap.
//!
//! **The candle ring, and how w2 lands on it.** One `CandleRing` holds the last
//! [`RING_LEN`] bars, newest last, plus the per-setting trailing averages of
//! §2. `RING_LEN` covers the deepest window any pattern of this family reads —
//! `avg(S, t-1)` over `N = 10` reaches `t-11`, and w2's three-bar patterns reach
//! `t-12`. Each pattern is **one function of the ring** in
//! [`CandlePatternIndicator::evaluate`], so w2 adds a three-bar pattern by
//! adding one evaluator and one `CandlePatternName` value: the ring, the
//! settings and the averaging rule are shared, and no engine, validation,
//! mutation or formatter arm is added again.

use std::collections::VecDeque;

use rust_decimal::Decimal;

use crate::domain::{Candle, CandlePatternName, Indicator};

/// The deepest bar offset any pattern of this family reads: `avg(S, t-1)` over
/// `N = 10` reaches `t-11` (offset 11), and w2's `MorningStar` /
/// `ThreeBlackCrows` windows reach `t-12` (offset 12). The ring is one longer
/// than the offset, so 13 bars in total.
const RING_LEN: usize = 13;

/// A cheap, wide-bound guard on the ring: the deepest offset any evaluator may
/// ask for. A read past it is a programming error in an evaluator, caught by the
/// debug assertion in [`CandleRing::at`], never a silent `None`.
const MAX_WINDOW_OFFSET: usize = RING_LEN - 1;

// ---------------------------------------------------------------------------
// §1 — candle primitives
// ---------------------------------------------------------------------------

/// One bar's four prices, with the §1 primitives over them. Every primitive is
/// a checked operation; `None` means "the arithmetic left `Decimal`'s range",
/// which an evaluator reads as "the condition does not hold".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bar {
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
}

impl Bar {
    fn of(candle: &Candle) -> Self {
        Self {
            open: candle.open,
            high: candle.high,
            low: candle.low,
            close: candle.close,
        }
    }

    /// §1 real body: `|close − open|`.
    fn body(self) -> Option<Decimal> {
        self.close.checked_sub(self.open).map(|body| body.abs())
    }

    /// §1 body top: `max(open, close)`.
    fn top(self) -> Decimal {
        self.open.max(self.close)
    }

    /// §1 body bottom: `min(open, close)`.
    fn bot(self) -> Decimal {
        self.open.min(self.close)
    }

    /// §1 upper shadow: `high − top`.
    fn upper(self) -> Option<Decimal> {
        self.high.checked_sub(self.top())
    }

    /// §1 lower shadow: `bot − low`.
    fn lower(self) -> Option<Decimal> {
        self.bot().checked_sub(self.low)
    }

    /// §1 high-low range: `high − low`.
    fn hl(self) -> Option<Decimal> {
        self.high.checked_sub(self.low)
    }

    /// §1 colour: `+1` (white) when `close >= open`, `-1` (black) otherwise.
    /// A bar with `open = close` counts as **white**.
    fn colour(self) -> i64 {
        if self.close >= self.open { 1 } else { -1 }
    }
}

// ---------------------------------------------------------------------------
// §2 — candle settings and the averaging rule
// ---------------------------------------------------------------------------

/// Which per-bar quantity a setting averages (§2.1 "Range measured").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RangeKind {
    /// `range_S(k) = body(k)`.
    Body,
    /// `range_S(k) = hl(k)` — the high-low range.
    HighLow,
}

/// One of §2.1's settings: which quantity it averages, its period `N` and its
/// factor `f`. The factor is held as the exact pair `(mantissa, scale)` — every
/// factor here is a short decimal (`1.0`, `0.1`, `0.2`), and `Decimal::new` is
/// not a `const fn`, so the const table stores the pair and materializes the
/// value at use. The `d = 2` divisor of §2.2 rule 3 applies only to the
/// "shadows" range, which no pattern here measures; the rule is stated in
/// [`range_divisor`] rather than silently dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Setting {
    kind: RangeKind,
    period: u32,
    /// Factor mantissa, e.g. `2`.
    factor_mantissa: i64,
    /// Factor scale, e.g. `1` for `0.2`.
    factor_scale: u32,
}

impl Setting {
    /// §2.1's factor `f`, exactly.
    fn factor(self) -> Decimal {
        Decimal::new(self.factor_mantissa, self.factor_scale)
    }
}

/// `BodyLong` — real body, `N = 10`, `f = 1.0`.
const BODY_LONG: Setting = Setting {
    kind: RangeKind::Body,
    period: 10,
    factor_mantissa: 1,
    factor_scale: 0,
};

/// `BodyShort` — real body, `N = 10`, `f = 1.0`.
const BODY_SHORT: Setting = Setting {
    kind: RangeKind::Body,
    period: 10,
    factor_mantissa: 1,
    factor_scale: 0,
};

/// `BodyDoji` — high-low range, `N = 10`, `f = 0.1`.
const BODY_DOJI: Setting = Setting {
    kind: RangeKind::HighLow,
    period: 10,
    factor_mantissa: 1,
    factor_scale: 1,
};

/// `ShadowLong` — real body, `N = 0`, `f = 1.0`. With `N = 0` the rule is
/// rule 2, so `avg(ShadowLong, k) = body(k)` — "a lower shadow longer than the
/// body" is what the hammer family's second condition means.
const SHADOW_LONG: Setting = Setting {
    kind: RangeKind::Body,
    period: 0,
    factor_mantissa: 1,
    factor_scale: 0,
};

/// `ShadowVeryShort` — high-low range, `N = 10`, `f = 0.1`.
const SHADOW_VERY_SHORT: Setting = Setting {
    kind: RangeKind::HighLow,
    period: 10,
    factor_mantissa: 1,
    factor_scale: 1,
};

/// `Near` — high-low range, `N = 5`, `f = 0.2`.
const NEAR: Setting = Setting {
    kind: RangeKind::HighLow,
    period: 5,
    factor_mantissa: 2,
    factor_scale: 1,
};

/// `Far` — high-low range, `N = 5`, `f = 0.6`. w1's 13 patterns did not need
/// it; `ThreeWhiteSoldiers` does (§3.16's two "not far below" body tests).
const FAR: Setting = Setting {
    kind: RangeKind::HighLow,
    period: 5,
    factor_mantissa: 6,
    factor_scale: 1,
};

/// §1/§2.1: the per-bar quantity a setting's window sums.
fn range_value(kind: RangeKind, bar: Bar) -> Option<Decimal> {
    match kind {
        RangeKind::Body => bar.body(),
        RangeKind::HighLow => bar.hl(),
    }
}

/// §2.2 rule 3's divisor: `2` for the "shadows" range (the sum of both shadows
/// of one bar), `1` for every other range. No pattern in this family measures
/// "shadows", so this is always `1` — named so the rule has one home rather than
/// being silently absent.
const fn range_divisor(kind: RangeKind) -> u32 {
    match kind {
        RangeKind::Body | RangeKind::HighLow => 1,
    }
}

/// The comparison a condition makes against an average. Only the three this
/// family's 13 patterns express: every "at or above" shape in the definitions
/// is written as a difference compared against the average with `<=` (the
/// hammer's `bot(t) <= low(t−1) + avg`, the hanging man's
/// `bot(t) >= high(t−1) − avg`), so a fourth operator would be dead code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
}

/// Apply `cmp` to two exact decimals.
fn holds(lhs: Decimal, rhs: Decimal, cmp: Cmp) -> bool {
    match cmp {
        Cmp::Lt => lhs < rhs,
        Cmp::Le => lhs <= rhs,
        Cmp::Gt => lhs > rhs,
    }
}

/// §2.2's `avg(S, k)`, evaluated without division.
///
/// Returns the pair `(lhs, rhs)` such that the condition `x ⋛ avg(S, k)` is
/// `holds(lhs, rhs, cmp)`:
///
/// - `N > 0` (rule 1): `x ⋛ f × sum / N` is exactly `N × x ⋛ f × sum`, because
///   `N > 0`. `lhs = x × N`, `rhs = f × sum`.
/// - `N = 0` (rule 2): the "window" is candle `k` itself, `lhs = x`,
///   `rhs = f × range_S(k)`.
///
/// `None` when a needed bar is missing from the ring or an operation overflowed
/// — the caller reads that as "the condition does not hold".
fn cmp_avg(ring: &CandleRing, setting: Setting, offset: usize, x: Decimal, cmp: Cmp) -> bool {
    let Some((scaled_x, scaled_avg)) = avg_pair(ring, setting, offset, x) else {
        return false;
    };
    holds(scaled_x, scaled_avg, cmp)
}

/// The `(N × x, f × sum)` pair behind [`cmp_avg`] — shared with the two
/// "price plus or minus an average" conditions, where `x` is the distance
/// (`bot(t) − low(t−1)`, `high(t−1) − bot(t)`).
fn avg_pair(
    ring: &CandleRing,
    setting: Setting,
    offset: usize,
    x: Decimal,
) -> Option<(Decimal, Decimal)> {
    if setting.period == 0 {
        let bar = ring.at(offset)?;
        let range = range_value(setting.kind, bar)?;
        return Some((x, setting.factor().checked_mul(range)?));
    }
    let mut sum = Decimal::ZERO;
    for step in 1..=setting.period {
        let bar = ring.at(offset + step as usize)?;
        sum = sum.checked_add(range_value(setting.kind, bar)?)?;
    }
    // Rule 3's `d` divisor; `1` for every range these patterns measure.
    let divisor = Decimal::from(range_divisor(setting.kind));
    let rhs = setting.factor().checked_mul(sum)?.checked_div(divisor)?;
    let lhs = x.checked_mul(Decimal::from(setting.period))?;
    Some((lhs, rhs))
}

// ---------------------------------------------------------------------------
// The candle ring
// ---------------------------------------------------------------------------

/// The last [`RING_LEN`] bars, newest last: [`CandleRing::at`]`(0)` is bar `t`,
/// `at(1)` is `t-1`, `at(n)` is `t-n` — the §0 notation, made an index.
#[derive(Debug, Clone, Default)]
struct CandleRing {
    bars: VecDeque<Bar>,
}

impl CandleRing {
    /// Feed one bar and evict the oldest past the ring's bound.
    fn push(&mut self, candle: &Candle) {
        self.bars.push_back(Bar::of(candle));
        while self.bars.len() > RING_LEN {
            self.bars.pop_front();
        }
    }

    /// The bar `offset` places back from the newest (`0` = today), or `None`
    /// when the ring does not reach that far.
    fn at(&self, offset: usize) -> Option<Bar> {
        debug_assert!(
            offset <= MAX_WINDOW_OFFSET,
            "a candle-pattern evaluator asked for offset {offset}, past the ring's \
             deepest window ({MAX_WINDOW_OFFSET}); widen RING_LEN with the pattern that needs it"
        );
        let len = self.bars.len();
        if offset >= len {
            return None;
        }
        self.bars.get(len - 1 - offset).copied()
    }
}

// ---------------------------------------------------------------------------
// The patterns
// ---------------------------------------------------------------------------

/// §5's lookback per pattern — TA-Lib's own `Function(name).lookback` with the
/// default settings. Used EXACTLY as listed: for the hammer family it is one bar
/// more than the conditions need (they need 10), and for engulfing it is 2 while
/// the conditions need only 1.
const fn lookback(pattern: CandlePatternName) -> u64 {
    match pattern {
        CandlePatternName::Doji
        | CandlePatternName::DragonflyDoji
        | CandlePatternName::GravestoneDoji
        | CandlePatternName::Marubozu
        | CandlePatternName::SpinningTop => 10,
        CandlePatternName::Hammer
        | CandlePatternName::InvertedHammer
        | CandlePatternName::HangingMan
        | CandlePatternName::ShootingStar
        | CandlePatternName::Harami
        | CandlePatternName::Piercing
        | CandlePatternName::DarkCloudCover => 11,
        CandlePatternName::Engulfing => 2,
        // r5.s1.w2: the three-bar patterns. ThreeBlackCrows' L is 13 — one bar
        // more than its conditions need (definitions §2.3, §5), so it is used as
        // listed and not derived from the widest window.
        CandlePatternName::MorningStar
        | CandlePatternName::EveningStar
        | CandlePatternName::ThreeWhiteSoldiers => 12,
        CandlePatternName::ThreeBlackCrows => 13,
    }
}

/// The `&PlusMinus;100` of a colour-signed output: `sign × magnitude`.
fn signed(sign: i64, magnitude: i64) -> Decimal {
    Decimal::from(sign * magnitude)
}

/// A named candlestick pattern as an [`Indicator`].
///
/// See the module docs for the warm-up contract, the ring and the §2 averaging
/// rule. Construct with [`CandlePatternIndicator::new`]; the adapter is
/// infallible (the variant carries no parameter), so the engine's factory has
/// nothing to refuse.
#[derive(Debug, Clone)]
pub struct CandlePatternIndicator {
    /// Which pattern every bar is evaluated for.
    pattern: CandlePatternName,
    /// The shared candle ring — w2's three-bar patterns read the same one.
    ring: CandleRing,
    /// Bars fed so far; the current 0-based bar index is `seen - 1`.
    seen: u64,
}

impl CandlePatternIndicator {
    /// Build the indicator for `pattern`. Infallible: the variant takes no
    /// parameter, so no input can be degenerate (the `Anatomy` precedent — the
    /// engine's `build_structure_indicator` arm needs no `Option`).
    #[must_use]
    pub fn new(pattern: CandlePatternName) -> Self {
        Self {
            pattern,
            ring: CandleRing::default(),
            seen: 0,
        }
    }

    /// §5's lookback for this indicator's pattern.
    #[must_use]
    fn lookback(&self) -> u64 {
        lookback(self.pattern)
    }

    /// The pattern's value on the current bar — one function of the ring per
    /// pattern. Called only from `i >= L`, where the ring holds everything the
    /// pattern's windows reach; a missing bar or an overflow yields `0` (which
    /// is also unreachable there, and is the honest answer if it were).
    fn evaluate(&self) -> Decimal {
        match self.pattern {
            CandlePatternName::Doji => self.doji(),
            CandlePatternName::DragonflyDoji => self.dragonfly_doji(),
            CandlePatternName::GravestoneDoji => self.gravestone_doji(),
            CandlePatternName::Hammer => self.hammer(),
            CandlePatternName::InvertedHammer => self.inverted_hammer(),
            CandlePatternName::HangingMan => self.hanging_man(),
            CandlePatternName::ShootingStar => self.shooting_star(),
            CandlePatternName::Marubozu => self.marubozu(),
            CandlePatternName::SpinningTop => self.spinning_top(),
            CandlePatternName::Engulfing => self.engulfing(),
            CandlePatternName::Harami => self.harami(),
            CandlePatternName::Piercing => self.piercing(),
            CandlePatternName::DarkCloudCover => self.dark_cloud_cover(),
            CandlePatternName::MorningStar => self.morning_star(),
            CandlePatternName::EveningStar => self.evening_star(),
            CandlePatternName::ThreeWhiteSoldiers => self.three_white_soldiers(),
            CandlePatternName::ThreeBlackCrows => self.three_black_crows(),
        }
    }

    /// §3.14 Morning star: a long black candle, a small star gapping down under
    /// its body, then a white candle closing more than 30% back into the first
    /// body. `+100`, else 0.
    ///
    /// The star's own colour is **not** checked, and neither is any gap between
    /// the star and `t`.
    fn morning_star(&self) -> Decimal {
        let (Some(t), Some(star), Some(first)) =
            (self.ring.at(0), self.ring.at(1), self.ring.at(2))
        else {
            return Decimal::ZERO;
        };
        let (Some(body_t), Some(body_star), Some(body_first)) =
            (t.body(), star.body(), first.body())
        else {
            return Decimal::ZERO;
        };
        let Some(penetration) = three_tenths(body_first) else {
            return Decimal::ZERO;
        };
        let Some(threshold) = first.close.checked_add(penetration) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_LONG, 2, body_first, Cmp::Gt)
            && first.colour() == -1
            && cmp_avg(&self.ring, BODY_SHORT, 1, body_star, Cmp::Le)
            && star.top() < first.bot()
            && cmp_avg(&self.ring, BODY_SHORT, 0, body_t, Cmp::Gt)
            && t.colour() == 1
            && t.close > threshold
        {
            Decimal::from(100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.15 Evening star: the morning star's mirror — a long white candle, a
    /// small star gapping up over its body, then a black candle closing more
    /// than 30% back into the first body. `-100`, else 0.
    fn evening_star(&self) -> Decimal {
        let (Some(t), Some(star), Some(first)) =
            (self.ring.at(0), self.ring.at(1), self.ring.at(2))
        else {
            return Decimal::ZERO;
        };
        let (Some(body_t), Some(body_star), Some(body_first)) =
            (t.body(), star.body(), first.body())
        else {
            return Decimal::ZERO;
        };
        let Some(penetration) = three_tenths(body_first) else {
            return Decimal::ZERO;
        };
        let Some(threshold) = first.close.checked_sub(penetration) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_LONG, 2, body_first, Cmp::Gt)
            && first.colour() == 1
            && cmp_avg(&self.ring, BODY_SHORT, 1, body_star, Cmp::Le)
            && star.bot() > first.top()
            && cmp_avg(&self.ring, BODY_SHORT, 0, body_t, Cmp::Gt)
            && t.colour() == -1
            && t.close < threshold
        {
            Decimal::from(-100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.16 Three white soldiers: three rising white candles with almost no
    /// upper shadows, the second and third opening higher but no further than
    /// "near" above the prior close, and their bodies not far short of the one
    /// before. `+100`, else 0.
    ///
    /// Quirk, kept exactly: the "not short" body test applies to **the third
    /// candle only**, not to all three as textbooks describe.
    fn three_white_soldiers(&self) -> Decimal {
        let (Some(t), Some(second), Some(first)) =
            (self.ring.at(0), self.ring.at(1), self.ring.at(2))
        else {
            return Decimal::ZERO;
        };
        let (Some(upper_t), Some(upper_second), Some(upper_first)) =
            (t.upper(), second.upper(), first.upper())
        else {
            return Decimal::ZERO;
        };
        let (Some(body_t), Some(body_second), Some(body_first)) =
            (t.body(), second.body(), first.body())
        else {
            return Decimal::ZERO;
        };
        // `open(t-1) <= close(t-2) + avg` is `open(t-1) - close(t-2) <= avg`.
        let Some(open_gap_second) = second.open.checked_sub(first.close) else {
            return Decimal::ZERO;
        };
        let Some(open_gap_t) = t.open.checked_sub(second.close) else {
            return Decimal::ZERO;
        };
        // `body(t-1) > body(t-2) - avg` is `body(t-2) - body(t-1) < avg`.
        let Some(body_gap_second) = body_first.checked_sub(body_second) else {
            return Decimal::ZERO;
        };
        let Some(body_gap_t) = body_second.checked_sub(body_t) else {
            return Decimal::ZERO;
        };
        if first.colour() == 1
            && second.colour() == 1
            && t.colour() == 1
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 2, upper_first, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 1, upper_second, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, upper_t, Cmp::Lt)
            && t.close > second.close
            && second.close > first.close
            && second.open > first.open
            && cmp_avg(&self.ring, NEAR, 2, open_gap_second, Cmp::Le)
            && t.open > second.open
            && cmp_avg(&self.ring, NEAR, 1, open_gap_t, Cmp::Le)
            && cmp_avg(&self.ring, FAR, 2, body_gap_second, Cmp::Lt)
            && cmp_avg(&self.ring, FAR, 1, body_gap_t, Cmp::Lt)
            && cmp_avg(&self.ring, BODY_SHORT, 0, body_t, Cmp::Gt)
        {
            Decimal::from(100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.17 Three black crows: a white bar, then three falling black candles
    /// each opening **strictly** inside the prior body, closing below the white
    /// bar's high, with almost no lower shadows. `-100`, else 0.
    ///
    /// **Four** bars are read, and no body-length condition applies.
    fn three_black_crows(&self) -> Decimal {
        let (Some(t), Some(second), Some(first), Some(prior)) = (
            self.ring.at(0),
            self.ring.at(1),
            self.ring.at(2),
            self.ring.at(3),
        ) else {
            return Decimal::ZERO;
        };
        let (Some(lower_t), Some(lower_second), Some(lower_first)) =
            (t.lower(), second.lower(), first.lower())
        else {
            return Decimal::ZERO;
        };
        if prior.colour() == 1
            && first.colour() == -1
            && second.colour() == -1
            && t.colour() == -1
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 2, lower_first, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 1, lower_second, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, lower_t, Cmp::Lt)
            && second.open < first.open
            && second.open > first.close
            && t.open < second.open
            && t.open > second.close
            && prior.high > first.close
            && first.close > second.close
            && second.close > t.close
        {
            Decimal::from(-100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.1 Doji: `body(t) <= avg(BodyDoji, t)`. `+100`, else 0.
    fn doji(&self) -> Decimal {
        let Some(t) = self.ring.at(0) else {
            return Decimal::ZERO;
        };
        let Some(body) = t.body() else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_DOJI, 0, body, Cmp::Le) {
            Decimal::from(100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.2 Dragonfly doji: a doji with almost no upper shadow and a long lower
    /// one. `+100`, else 0. Always positive: it reads the shape, not trend.
    fn dragonfly_doji(&self) -> Decimal {
        let Some(t) = self.ring.at(0) else {
            return Decimal::ZERO;
        };
        let (Some(body), Some(upper), Some(lower)) = (t.body(), t.upper(), t.lower()) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_DOJI, 0, body, Cmp::Le)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, upper, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, lower, Cmp::Gt)
        {
            Decimal::from(100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.3 Gravestone doji: the dragonfly's mirror — a long upper shadow and
    /// almost no lower one. `+100`, else 0. Always positive, even though a
    /// gravestone is read as bearish.
    fn gravestone_doji(&self) -> Decimal {
        let Some(t) = self.ring.at(0) else {
            return Decimal::ZERO;
        };
        let (Some(body), Some(upper), Some(lower)) = (t.body(), t.upper(), t.lower()) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_DOJI, 0, body, Cmp::Le)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, lower, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, upper, Cmp::Gt)
        {
            Decimal::from(100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.4 Hammer: a small body, a lower shadow longer than the body, almost no
    /// upper shadow, and the body bottom at or under the prior bar's low plus
    /// `Near`. `+100`, else 0. Trend is not checked.
    fn hammer(&self) -> Decimal {
        let (Some(t), Some(prev)) = (self.ring.at(0), self.ring.at(1)) else {
            return Decimal::ZERO;
        };
        let (Some(body), Some(lower), Some(upper)) = (t.body(), t.lower(), t.upper()) else {
            return Decimal::ZERO;
        };
        let Some(delta) = t.bot().checked_sub(prev.low) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_SHORT, 0, body, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_LONG, 0, lower, Cmp::Gt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, upper, Cmp::Lt)
            && cmp_avg(&self.ring, NEAR, 1, delta, Cmp::Le)
        {
            Decimal::from(100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.5 Hanging man: the hammer's shape, with the body bottom at or above
    /// the prior bar's high minus `Near`. `-100`, else 0. Trend is not checked.
    fn hanging_man(&self) -> Decimal {
        let (Some(t), Some(prev)) = (self.ring.at(0), self.ring.at(1)) else {
            return Decimal::ZERO;
        };
        let (Some(body), Some(lower), Some(upper)) = (t.body(), t.lower(), t.upper()) else {
            return Decimal::ZERO;
        };
        // `bot(t) >= high(t-1) - avg` is `high(t-1) - bot(t) <= avg`.
        let Some(delta) = prev.high.checked_sub(t.bot()) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_SHORT, 0, body, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_LONG, 0, lower, Cmp::Gt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, upper, Cmp::Lt)
            && cmp_avg(&self.ring, NEAR, 1, delta, Cmp::Le)
        {
            Decimal::from(-100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.6 Inverted hammer: a small body, an upper shadow longer than the body,
    /// almost no lower shadow, and a real-body gap down. `+100`, else 0.
    fn inverted_hammer(&self) -> Decimal {
        let (Some(t), Some(prev)) = (self.ring.at(0), self.ring.at(1)) else {
            return Decimal::ZERO;
        };
        let (Some(body), Some(upper), Some(lower)) = (t.body(), t.upper(), t.lower()) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_SHORT, 0, body, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_LONG, 0, upper, Cmp::Gt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, lower, Cmp::Lt)
            && t.top() < prev.bot()
        {
            Decimal::from(100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.7 Shooting star: the inverted hammer's shape with a real-body gap up.
    /// `-100`, else 0.
    fn shooting_star(&self) -> Decimal {
        let (Some(t), Some(prev)) = (self.ring.at(0), self.ring.at(1)) else {
            return Decimal::ZERO;
        };
        let (Some(body), Some(upper), Some(lower)) = (t.body(), t.upper(), t.lower()) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_SHORT, 0, body, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_LONG, 0, upper, Cmp::Gt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, lower, Cmp::Lt)
            && t.bot() > prev.top()
        {
            Decimal::from(-100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.8 Marubozu: a long body with no shadows. `colour(t) × 100`, else 0.
    /// The first condition makes the body strictly positive, so a zero-body bar
    /// never qualifies.
    fn marubozu(&self) -> Decimal {
        let Some(t) = self.ring.at(0) else {
            return Decimal::ZERO;
        };
        let (Some(body), Some(upper), Some(lower)) = (t.body(), t.upper(), t.lower()) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_LONG, 0, body, Cmp::Gt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, upper, Cmp::Lt)
            && cmp_avg(&self.ring, SHADOW_VERY_SHORT, 0, lower, Cmp::Lt)
        {
            signed(t.colour(), 100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.9 Spinning top: a small body with both shadows longer than it.
    /// `colour(t) × 100`, else 0. A spinning top with `open = close` counts as
    /// white and gives `+100`.
    fn spinning_top(&self) -> Decimal {
        let Some(t) = self.ring.at(0) else {
            return Decimal::ZERO;
        };
        let (Some(body), Some(upper), Some(lower)) = (t.body(), t.upper(), t.lower()) else {
            return Decimal::ZERO;
        };
        if cmp_avg(&self.ring, BODY_SHORT, 0, body, Cmp::Lt) && upper > body && lower > body {
            signed(t.colour(), 100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.10 Engulfing: bar t's body covers bar t-1's body, with at least one
    /// end strictly beyond it, and the two colours oppose. **No averages.**
    /// `±100`, `±80` when a body end coincides, else 0 — the sign is `colour(t)`.
    ///
    /// Quirk: a prior bar with `open = close` counts as white, so it can be
    /// engulfed by a black bar but never by a white one.
    fn engulfing(&self) -> Decimal {
        let (Some(t), Some(prev)) = (self.ring.at(0), self.ring.at(1)) else {
            return Decimal::ZERO;
        };
        let bullish = t.colour() == 1
            && prev.colour() == -1
            && ((t.close >= prev.open && t.open < prev.close)
                || (t.close > prev.open && t.open <= prev.close));
        let bearish = t.colour() == -1
            && prev.colour() == 1
            && ((t.open >= prev.close && t.close < prev.open)
                || (t.open > prev.close && t.close <= prev.open));
        if !bullish && !bearish {
            return Decimal::ZERO;
        }
        // §3.10 strength: no end of the two bodies may coincide for the full
        // form; one exact match (the common case on continuous crypto bars,
        // where `open(t) = close(t-1)`) drops it to `±80`.
        let full = t.open != prev.close && t.close != prev.open;
        signed(t.colour(), if full { 100 } else { 80 })
    }

    /// §3.11 Harami: a long body followed by a short body inside it. The sign is
    /// the OPPOSITE of the first candle's colour. `±100` when strictly inside,
    /// `±80` when one end touches, else 0.
    fn harami(&self) -> Decimal {
        let (Some(t), Some(prev)) = (self.ring.at(0), self.ring.at(1)) else {
            return Decimal::ZERO;
        };
        let (Some(body_t), Some(body_prev)) = (t.body(), prev.body()) else {
            return Decimal::ZERO;
        };
        // The first body is long (window t-11..t-2, i.e. relative to candle t-1),
        // the second is short (`<=`, window t-10..t-1).
        if !cmp_avg(&self.ring, BODY_LONG, 1, body_prev, Cmp::Gt)
            || !cmp_avg(&self.ring, BODY_SHORT, 0, body_t, Cmp::Le)
        {
            return Decimal::ZERO;
        }
        let sign = -prev.colour();
        if t.top() < prev.top() && t.bot() > prev.bot() {
            signed(sign, 100)
        } else if t.top() <= prev.top() && t.bot() >= prev.bot() {
            signed(sign, 80)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.12 Piercing: a white bar opening below the prior low and closing
    /// strictly above the midpoint of a prior long black body, but still inside
    /// it. Penetration is fixed at 0.5. `+100`, else 0.
    fn piercing(&self) -> Decimal {
        let (Some(t), Some(prev)) = (self.ring.at(0), self.ring.at(1)) else {
            return Decimal::ZERO;
        };
        let (Some(body_t), Some(body_prev)) = (t.body(), prev.body()) else {
            return Decimal::ZERO;
        };
        let Some(midpoint) = half(body_prev) else {
            return Decimal::ZERO;
        };
        let Some(threshold) = prev.close.checked_add(midpoint) else {
            return Decimal::ZERO;
        };
        if prev.colour() == -1
            && cmp_avg(&self.ring, BODY_LONG, 1, body_prev, Cmp::Gt)
            && t.colour() == 1
            && cmp_avg(&self.ring, BODY_LONG, 0, body_t, Cmp::Gt)
            && t.open < prev.low
            && t.close < prev.open
            && t.close > threshold
        {
            Decimal::from(100)
        } else {
            Decimal::ZERO
        }
    }

    /// §3.13 Dark cloud cover: a black bar opening above the prior high and
    /// closing inside a prior long white body, below its midpoint. Penetration
    /// is fixed at 0.5. `-100`, else 0.
    ///
    /// Quirk: unlike piercing, the second candle has **no** long-body
    /// requirement.
    fn dark_cloud_cover(&self) -> Decimal {
        let (Some(t), Some(prev)) = (self.ring.at(0), self.ring.at(1)) else {
            return Decimal::ZERO;
        };
        let Some(body_prev) = prev.body() else {
            return Decimal::ZERO;
        };
        let Some(midpoint) = half(body_prev) else {
            return Decimal::ZERO;
        };
        let Some(threshold) = prev.close.checked_sub(midpoint) else {
            return Decimal::ZERO;
        };
        if prev.colour() == 1
            && cmp_avg(&self.ring, BODY_LONG, 1, body_prev, Cmp::Gt)
            && t.colour() == -1
            && t.open > prev.high
            && t.close > prev.open
            && t.close < threshold
        {
            Decimal::from(-100)
        } else {
            Decimal::ZERO
        }
    }
}

/// Half of a value — the fixed `0.5` penetrations, as an exact operation. `x/2`
/// is exact in decimal for any `Decimal` (the scale can gain one digit, which
/// the checked division handles); the `None` arm is the overflow contract.
fn half(value: Decimal) -> Option<Decimal> {
    value.checked_div(Decimal::TWO)
}

/// The two stars' fixed penetration `p = 0.3` (definitions §3.14, §3.15), as an
/// exact operation: `3 × value / 10`. Both factors are exact decimals and the
/// divisor is a power of ten, so no rounding enters the stars' comparison.
fn three_tenths(value: Decimal) -> Option<Decimal> {
    value
        .checked_mul(Decimal::new(3, 0))?
        .checked_div(Decimal::new(10, 0))
}

impl Indicator for CandlePatternIndicator {
    fn next(&mut self, candle: &Candle) -> Option<Decimal> {
        self.ring.push(candle);
        self.seen = self.seen.saturating_add(1);
        // §5 / §2.3: the 0-based index is `seen - 1`; below the lookback the
        // engine yields nothing. TA-Lib's Python wrapper pads those rows with 0,
        // which is NOT "no pattern" — the distinction the parity test pins.
        if self.seen <= self.lookback() {
            return None;
        }
        Some(self.evaluate())
    }

    fn is_ready(&self) -> bool {
        // Ready iff the NEXT call returns Some: after `seen` bars the next call
        // is index `seen`, which is ready once `seen >= L`.
        self.seen >= self.lookback()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{CandlePatternIndicator, MAX_WINDOW_OFFSET, RING_LEN, lookback};
    use crate::domain::{Candle, CandlePatternName, Indicator};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    const ALL: [CandlePatternName; 17] = [
        CandlePatternName::Doji,
        CandlePatternName::DragonflyDoji,
        CandlePatternName::GravestoneDoji,
        CandlePatternName::Hammer,
        CandlePatternName::InvertedHammer,
        CandlePatternName::HangingMan,
        CandlePatternName::ShootingStar,
        CandlePatternName::Marubozu,
        CandlePatternName::SpinningTop,
        CandlePatternName::Engulfing,
        CandlePatternName::Harami,
        CandlePatternName::Piercing,
        CandlePatternName::DarkCloudCover,
        CandlePatternName::MorningStar,
        CandlePatternName::EveningStar,
        CandlePatternName::ThreeWhiteSoldiers,
        CandlePatternName::ThreeBlackCrows,
    ];

    fn d(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn candle(idx: i64, open: &str, high: &str, low: &str, close: &str) -> Candle {
        Candle {
            open_time: idx * 900_000,
            close_time: idx * 900_000 + 899_999,
            open: d(open),
            high: d(high),
            low: d(low),
            close: d(close),
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    /// A flat bar — every primitive zero. Cheap filler for the warm-up probes.
    fn flat(idx: i64) -> Candle {
        candle(idx, "100", "100", "100", "100")
    }

    /// A steady bar with a real body and both shadows, so no pattern is
    /// trivially true and none panics.
    fn ordinary(idx: i64) -> Candle {
        candle(idx, "100", "102", "99", "101")
    }

    /// §5 is used EXACTLY as listed: the ring can hold the deepest window, and
    /// the first ready bar is index L (not L-1 and not L+1).
    #[test]
    fn warm_up_is_none_before_the_lookback_and_some_from_l_onwards() {
        for pattern in ALL {
            let l = lookback(pattern);
            let mut indicator = CandlePatternIndicator::new(pattern);
            for index in 0..l {
                let value = indicator.next(&ordinary(i64::try_from(index).unwrap()));
                assert_eq!(
                    value, None,
                    "{pattern:?}: bar index {index} < L = {l} must be None"
                );
            }
            let value = indicator.next(&ordinary(i64::try_from(l).unwrap()));
            assert!(
                value.is_some(),
                "{pattern:?}: bar index {l} (== L) must be Some"
            );
        }
    }

    /// `is_ready` means "the next call returns Some", which the engine's warm
    /// gate reads — so it flips exactly one bar before the first `Some`.
    #[test]
    fn is_ready_flips_exactly_one_bar_before_the_first_value() {
        for pattern in ALL {
            let l = lookback(pattern);
            let mut indicator = CandlePatternIndicator::new(pattern);
            for index in 0..l {
                assert!(
                    !indicator.is_ready(),
                    "{pattern:?}: not ready at bar index {index}"
                );
                let index = i64::try_from(index).unwrap();
                indicator.next(&ordinary(index));
            }
            assert!(
                indicator.is_ready(),
                "{pattern:?}: ready once L = {l} bars are fed"
            );
        }
    }

    /// A once-ready indicator never returns `None` again — the property that
    /// keeps the variant out of the engine's `is_level_or_pattern` carve-out.
    #[test]
    fn once_ready_the_value_is_never_none() {
        for pattern in ALL {
            let mut indicator = CandlePatternIndicator::new(pattern);
            for index in 0_i64..40 {
                let value = indicator.next(&ordinary(index));
                if u64::try_from(index).unwrap() >= lookback(pattern) {
                    assert!(value.is_some(), "{pattern:?}: None after warm-up");
                }
            }
        }
    }

    /// The ring covers every offset an evaluator of this family may ask for:
    /// the deepest is the hammer family's `Near` window at candle `t-1`
    /// (`t-6..t-2`), and w2's three-bar windows reach `t-12`.
    ///
    /// A compile-time check, not a runtime one: `avg(S, t-1)` with `N = 10`
    /// reaches `t-11` (offset 11) and w2's three-bar patterns reach `t-12`
    /// (offset 12), so the ring must reach offset 12. Narrowing the constant
    /// below that is a build error, not a test failure.
    const _: () = assert!(
        12 <= MAX_WINDOW_OFFSET,
        "the ring must reach offset 12 (w2's three-bar windows)"
    );

    /// The ring evicts, so a long series cannot grow the indicator's memory.
    #[test]
    fn the_ring_is_bounded() {
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::Doji);
        for index in 0_i64..5_000 {
            indicator.next(&ordinary(index));
        }
        assert_eq!(indicator.ring.bars.len(), RING_LEN);
    }

    /// A degenerate bar (zero range, zero body) yields 0, never a panic: every
    /// primitive is a checked operation.
    #[test]
    fn a_degenerate_bar_never_panics() {
        for pattern in ALL {
            let mut indicator = CandlePatternIndicator::new(pattern);
            for index in 0_i64..20 {
                indicator.next(&flat(index));
            }
        }
    }

    /// The definitions' worked shapes, on hand-built bars, so the evaluators are
    /// pinned independently of the CSV parity suite. The shapes are deliberately
    /// small: each asserts one condition's direction.
    ///
    /// Engulfing's lookback is 2, so its first two bars read `None` — the value
    /// is visible from bar 2 on. Each case therefore feeds a leading bar and
    /// asserts on the bar AFTER the pair it built.
    #[test]
    fn hand_built_shapes_match_the_definitions() {
        // Engulfing, full bullish form: a black bar then a white one covering it
        // with no coinciding end. Feed bar 0 as filler, the pair as bars 1-2,
        // and read bar 2 (index 2 == L).
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::Engulfing);
        indicator.next(&candle(0, "100", "101", "99", "100")); // filler
        indicator.next(&candle(1, "105", "106", "99", "100")); // black 100..105
        let value = indicator.next(&candle(2, "99", "108", "98", "107")); // white 99..107
        assert_eq!(value, Some(d("100")), "full bullish engulfing is +100");

        // The partial form: `open(t) = close(t-1)`, the continuous-crypto case.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::Engulfing);
        indicator.next(&candle(0, "100", "101", "99", "100"));
        indicator.next(&candle(1, "105", "106", "99", "100"));
        let value = indicator.next(&candle(2, "100", "108", "99", "107"));
        assert_eq!(value, Some(d("80")), "a coinciding body end is +80");

        // Marubozu: a long white body with no shadows, against small prior bars.
        // L = 10, so the first ready bar is index 10.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::Marubozu);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "100.05", "99.95", "100.02"));
        }
        let value = indicator.next(&candle(10, "100", "110", "100", "110"));
        assert_eq!(value, Some(d("100")), "a long white marubozu is +100");

        // Doji: a bar with no body at all, against narrow prior ranges.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::Doji);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "100.5", "99.5", "100.2"));
        }
        let value = indicator.next(&candle(10, "100", "101", "99", "100"));
        assert_eq!(value, Some(d("100")), "a zero-body doji is +100");
    }

    /// The three-bar patterns' worked shapes, on hand-built bars, so their
    /// evaluators are pinned independently of the TA-Lib parity suite. Each
    /// shape is derived from definitions §3.14–§3.17 on paper first (the windows
    /// are written out in the comments), then asserted here.
    ///
    /// `L` is 12 for the stars and the soldiers and 13 for the crows, so each
    /// case feeds filler bars and reads the bar at index `L`.
    #[test]
    fn hand_built_three_bar_shapes_match_the_definitions() {
        // Morning star, at index 12 (L = 12). t-2 = 10, t-1 = 11, t = 12.
        // avg(BodyLong, t-2) averages bars 0..9 = 1.0 each; avg(BodyShort, t-1)
        // averages bars 1..10 = (9x1 + 50)/10 = 5.9; avg(BodyShort, t) averages
        // bars 2..11 = (8x1 + 50 + 1)/10 = 5.9. Penetration 0.3 x 50 = 15, so the
        // close must beat 100 + 15 = 115.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::MorningStar);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "100.5", "98.5", "99")); // body 1
        }
        indicator.next(&candle(10, "150", "151", "99", "100")); // long black, body 50
        indicator.next(&candle(11, "95", "96", "93", "94")); // star: top 95 < bot(10) 100
        let value = indicator.next(&candle(12, "100", "121", "99", "120")); // white, body 20
        assert_eq!(value, Some(d("100")), "a morning star is +100");

        // Evening star, the mirror, at index 12.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::EveningStar);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "101.5", "99.5", "99")); // body 1
        }
        indicator.next(&candle(10, "100", "151", "99", "150")); // long white, body 50
        indicator.next(&candle(11, "156", "158", "155", "157")); // star: bot 156 > top(10) 150
        let value = indicator.next(&candle(12, "150", "151", "129", "130")); // black, body 20
        assert_eq!(value, Some(d("-100")), "an evening star is -100");

        // Three white soldiers, at index 12. Fillers have hl = 10, so
        // avg(ShadowVeryShort, k) is 1.0 and avg(Near, k)=avg(Far, k) = 2 / 6.
        // avg(BodyShort, t) averages bars 2..11 = (8x1 + 10 + 10)/10 = 2.8.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::ThreeWhiteSoldiers);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "105", "95", "101"));
        }
        indicator.next(&candle(10, "100", "110", "100", "110")); // white, body 10, no shadows
        indicator.next(&candle(11, "105", "115", "105", "115")); // white, body 10
        let value = indicator.next(&candle(12, "115", "125", "115", "125")); // white, body 10
        assert_eq!(value, Some(d("100")), "three white soldiers is +100");

        // Three black crows, at index 13 (L = 13). t-3 = 10 (white), t-2 = 11,
        // t-1 = 12, t = 13. Each crow opens strictly inside the prior body, so
        // each open sits above the prior close and below the prior open.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::ThreeBlackCrows);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "105", "95", "101"));
        }
        indicator.next(&candle(10, "100", "115", "95", "110")); // white bar, high 115
        indicator.next(&candle(11, "108", "110", "105", "105")); // black, body 3
        indicator.next(&candle(12, "107", "110", "104", "104")); // opens inside 11's body
        let value = indicator.next(&candle(13, "106", "108", "103", "103")); // opens inside 12's
        assert_eq!(value, Some(d("-100")), "three black crows is -100");
    }

    /// The quirks the definitions name, each asserted as a **negative**: a shape
    /// that meets every condition except the quirky one must read 0. These fail
    /// if an evaluator "helpfully" relaxes a strict test or widens a window.
    #[test]
    fn the_three_bar_quirks_are_kept_exactly() {
        // Three black crows: `open(t-1) > close(t-2)` is STRICT. Set the second
        // crow's open exactly at the first crow's close and the pattern must not
        // fire — the case that keeps crows off continuous crypto bars.
        //
        // Every OTHER condition holds for these bars, and the third crow is
        // well-formed (open 104.5 strictly inside 104..105), so the only thing
        // that can decide this bar is the second crow's strict open. Relaxing
        // that test to `>=` fires the pattern, which is what makes this a
        // negative worth asserting.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::ThreeBlackCrows);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "105", "95", "101"));
        }
        indicator.next(&candle(10, "100", "115", "95", "110")); // white bar, high 115
        indicator.next(&candle(11, "108", "110", "105", "105")); // black, body 3
        indicator.next(&candle(12, "105", "110", "104", "104")); // open == close(11): NOT strict
        let value = indicator.next(&candle(13, "104.5", "105", "103.5", "103.5")); // well-formed
        assert_eq!(
            value,
            Some(d("0")),
            "an open exactly at the prior close is not inside the body"
        );

        // Three white soldiers, the two halves of the same quirk. The "not
        // short" test is on the THIRD candle only, so:
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::ThreeWhiteSoldiers);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "105", "95", "101"));
        }
        // (a) a SHORT first candle does not stop the pattern: nothing checks it.
        indicator.next(&candle(10, "100", "100.5", "95", "100.5")); // body 0.5, upper 0
        indicator.next(&candle(11, "100.2", "100.8", "100.2", "100.8")); // body 0.6
        let value = indicator.next(&candle(12, "100.5", "102", "100.5", "102")); // body 1.5
        assert_eq!(
            value,
            Some(d("100")),
            "the first two candles are not required to have long bodies"
        );

        // (b) a SHORT third candle does stop it: `body(t) > avg(BodyShort, t)`.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::ThreeWhiteSoldiers);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "105", "95", "101"));
        }
        indicator.next(&candle(10, "100", "110", "100", "110"));
        indicator.next(&candle(11, "105", "115", "105", "115"));
        let value = indicator.next(&candle(12, "115", "116", "115", "115.5")); // short body
        assert_eq!(
            value,
            Some(d("0")),
            "the third candle's body must not be short"
        );

        // The stars do not check the star's colour. A white star in the morning
        // star still fires: only the first and third candles' colours matter.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::MorningStar);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "100.5", "98.5", "99"));
        }
        indicator.next(&candle(10, "150", "151", "99", "100"));
        indicator.next(&candle(11, "94", "96", "93", "95")); // WHITE star, top 96 < 100
        let value = indicator.next(&candle(12, "100", "121", "99", "120"));
        assert_eq!(
            value,
            Some(d("100")),
            "the star's colour is not part of the pattern"
        );

        // The star's gap is a REAL-BODY gap and it is STRICT: `top(t-1) <
        // bot(t-2)`. A star whose top merely reaches the first candle's body
        // bottom does not fire. This is the star's counterpart to the crows'
        // strict open — on continuous crypto bars the two bodies touch, and the
        // strictness is exactly why the gap patterns are rare there.
        //
        // Every condition below holds except the gap, so widening the gap test
        // (to `<=`, or by a tolerance) is what flips this bar.
        let mut indicator = CandlePatternIndicator::new(CandlePatternName::MorningStar);
        for index in 0_i64..10 {
            indicator.next(&candle(index, "100", "100.5", "98.5", "99"));
        }
        indicator.next(&candle(10, "150", "151", "99", "100")); // body bottom 100
        indicator.next(&candle(11, "99.5", "100", "99", "100")); // top 100 == bot(t-2): no gap
        let value = indicator.next(&candle(12, "100", "121", "99", "120"));
        assert_eq!(
            value,
            Some(d("0")),
            "a star that does not gap down under the first body is not a morning star"
        );
    }
}
