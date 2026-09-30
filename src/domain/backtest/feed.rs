//! MTF-aligned, no-look-ahead candle feed (FR-5, BACKLOG-4).
//!
//! The deterministic iteration substrate for the backtester: given a
//! primary-timeframe [`CandleSeries`] (e.g. BTCUSDT M15), an optional
//! higher-timeframe series (e.g. H4) and — since r3.s2.w4 — an optional fixed
//! daily series, [`align`] produces one [`AlignedBar`] per
//! primary candle, each paired with the most-recent HTF bar and the
//! most-recent daily bar that have **already
//! closed** at the primary bar's `close_time` — the no-look-ahead guarantee.
//!
//! Pure logic over already-loaded, already-`validate()`-d series (zero I/O, no
//! `f64`). It does not touch the indicator engine, the cost model, or candle
//! fetching, and it does not gap-fill or resample — it reports what exists.
//!
//! **C6 (substrate-only):** the VS-1.2.1 DSL is single-TF (#15), so nothing
//! reads [`AlignedBar::htf`] this slice; 1.03 accepts it as a documented
//! pass-through. Correctness is validated by this module's unit tests alone.

use crate::domain::candle::Candle;
use crate::domain::series::CandleSeries;

/// One primary candle paired with its no-look-ahead HTF context.
///
/// Borrows from the input series for the feed's lifetime — no clones, so the
/// stream is allocation-cheap (one [`AlignedBar`] per primary candle, each a
/// tuple of references plus an index).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignedBar<'a> {
    /// The primary-timeframe candle this step iterates.
    pub primary: &'a Candle,
    /// Position of `primary` within the primary series (0-based).
    pub index: usize,
    /// The most-recent HTF candle whose `close_time` is `<= primary.close_time`,
    /// or `None` when no HTF series was supplied or none has closed yet.
    pub htf: Option<&'a Candle>,
    /// The most-recent daily candle whose `close_time` is
    /// `<= primary.close_time`, or `None` when no D1 series was supplied or
    /// none has closed yet (r3.s2.w4). Same closed-bar rule as [`AlignedBar::htf`],
    /// advanced by its own forward-only pointer.
    pub d1: Option<&'a Candle>,
}

/// Pair each primary candle with the most-recent already-closed HTF candle and
/// the most-recent already-closed daily candle.
///
/// The no-look-ahead invariant: for a primary bar with `close_time = c`, the
/// paired HTF (resp. daily) bar is the one with the **greatest
/// `close_time <= c`**. A bar that has not closed by `c` is invisible; before
/// the first close the pairing is `None`. Single-TF mode (`htf = None`) yields
/// every `htf` as `None`; the same holds for `d1 = None`.
///
/// Each pointer advances forward-only — all series are ascending and
/// `validate()`-d upstream, so the walk is O(primary + htf + d1) with no
/// rescans and no gap-filling. Pure function of its inputs: identical inputs
/// yield an identical stream.
#[must_use]
pub fn align<'a>(
    primary: &'a CandleSeries,
    htf: Option<&'a CandleSeries>,
    d1: Option<&'a CandleSeries>,
) -> Vec<AlignedBar<'a>> {
    let htf_candles: &[Candle] = htf.map_or(&[], |s| s.candles.as_slice());
    let d1_candles: &[Candle] = d1.map_or(&[], |s| s.candles.as_slice());
    // Index of each candidate bar: the greatest j whose close_time <= the
    // current primary close_time. Advances forward-only across the whole walk.
    let mut htf_idx: Option<usize> = None;
    let mut d1_idx: Option<usize> = None;

    primary
        .candles
        .iter()
        .enumerate()
        .map(|(index, candle)| {
            let c = candle.close_time;
            // Advance while the NEXT bar of a series has also already closed
            // by `c`. Monotonic: primary close_times are ascending, so we
            // never rewind.
            let mut next = htf_idx.map_or(0, |i| i + 1);
            while next < htf_candles.len() && htf_candles[next].close_time <= c {
                htf_idx = Some(next);
                next += 1;
            }
            let mut next = d1_idx.map_or(0, |i| i + 1);
            while next < d1_candles.len() && d1_candles[next].close_time <= c {
                d1_idx = Some(next);
                next += 1;
            }
            AlignedBar {
                primary: candle,
                index,
                htf: htf_idx.map(|i| &htf_candles[i]),
                d1: d1_idx.map(|i| &d1_candles[i]),
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_wrap,
    clippy::doc_markdown
)]
mod tests {
    use super::{AlignedBar, align};
    use crate::domain::candle::Candle;
    use crate::domain::pair::Pair;
    use crate::domain::series::CandleSeries;
    use crate::domain::timeframe::Timeframe;
    use crate::domain::version::DataVersion;
    use rust_decimal::Decimal;

    /// A candle spanning `[open_time, close_time]` (epoch ms); prices irrelevant
    /// to alignment so they are all `ONE`.
    fn candle(open_time: i64, close_time: i64) -> Candle {
        Candle {
            open_time,
            close_time,
            open: Decimal::ONE,
            high: Decimal::ONE,
            low: Decimal::ONE,
            close: Decimal::ONE,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    /// M15 series: bars of nominal 15m width starting at `start`, `n` of them.
    fn m15_series(start: i64, n: usize) -> CandleSeries {
        let step = Timeframe::M15.duration_ms();
        let candles = (0..n as i64)
            .map(|i| {
                let open = start + i * step;
                candle(open, open + step - 1)
            })
            .collect();
        CandleSeries {
            pair: Pair::new("BTCUSDT"),
            timeframe: Timeframe::M15,
            version: DataVersion::new("v1"),
            candles,
        }
    }

    /// H4 series: bars of nominal 4h width starting at `start`, `n` of them.
    fn h4_series(start: i64, n: usize) -> CandleSeries {
        let step = Timeframe::H4.duration_ms();
        let candles = (0..n as i64)
            .map(|i| {
                let open = start + i * step;
                candle(open, open + step - 1)
            })
            .collect();
        CandleSeries {
            pair: Pair::new("BTCUSDT"),
            timeframe: Timeframe::H4,
            version: DataVersion::new("v1"),
            candles,
        }
    }

    /// (c) Single-TF mode: no HTF series → one item per primary, every htf None.
    #[test]
    fn single_tf_mode_yields_primary_count_with_htf_none() {
        let primary = m15_series(0, 5);
        let feed = align(&primary, None, None);
        assert_eq!(feed.len(), 5, "one AlignedBar per primary candle");
        for (i, bar) in feed.iter().enumerate() {
            assert_eq!(bar.index, i);
            assert_eq!(bar.primary, &primary.candles[i]);
            assert!(bar.htf.is_none(), "single-TF mode: htf must be None");
        }
    }

    /// Empty primary series → empty feed (no panic, no off-by-one).
    #[test]
    fn empty_primary_yields_empty_feed() {
        let primary = m15_series(0, 0);
        let htf = h4_series(0, 3);
        assert!(align(&primary, Some(&htf), None).is_empty());
        assert!(align(&primary, None, None).is_empty());
    }

    /// (b) Correct most-recent-closed pairing across an H4 boundary.
    ///
    /// H4 bar 0 closes at 14_399_999. The 16 M15 bars 0..15 (closing at
    /// 899_999 .. 14_399_999) all close at or before that — bar 15 closes
    /// exactly on the H4 close, so it pairs with H4 bar 0 (`<=`, inclusive).
    /// M15 bar 16 (closes 15_299_999) still sees only H4 bar 0; once M15 reaches
    /// the bar closing at/after the H4-bar-1 close (28_799_999) it advances.
    #[test]
    fn pairs_most_recent_closed_htf_bar() {
        let primary = m15_series(0, 64); // 64 * 15m = 16h, spans 4 H4 bars
        let htf = h4_series(0, 4);
        let feed = align(&primary, Some(&htf), None);

        for bar in &feed {
            let c = bar.primary.close_time;
            // The expected HTF bar: greatest close_time <= c, else None.
            let expected = htf.candles.iter().rev().find(|h| h.close_time <= c);
            assert_eq!(
                bar.htf, expected,
                "bar idx {} close_time {c}: wrong HTF pairing",
                bar.index
            );
        }

        // Spot-check the inclusive boundary: M15 bar 15 closes at exactly the H4
        // bar-0 close (14_399_999) and must pair with H4 bar 0.
        assert_eq!(feed[15].primary.close_time, 14_399_999);
        assert_eq!(feed[15].htf, Some(&htf.candles[0]));
        // M15 bar 14 closes at 13_499_999 (< first H4 close) → None.
        assert_eq!(feed[14].htf, None);
    }

    /// (a) No-look-ahead: an HTF bar that closes AFTER the primary bar is never
    /// paired. We assert the paired bar (when present) always closed at/before
    /// the primary, and that the bar just past it closes strictly after.
    #[test]
    fn no_look_ahead_never_pairs_a_future_htf_bar() {
        let primary = m15_series(0, 40);
        let htf = h4_series(0, 3);
        let feed = align(&primary, Some(&htf), None);

        for bar in &feed {
            let c = bar.primary.close_time;
            if let Some(h) = bar.htf {
                assert!(
                    h.close_time <= c,
                    "look-ahead leak: paired HTF close {} > primary close {c}",
                    h.close_time
                );
                // And it is genuinely the LATEST closed one: the next HTF bar,
                // if any, must close strictly after the primary bar.
                let pos = htf
                    .candles
                    .iter()
                    .position(|x| x == h)
                    .expect("paired bar is in series");
                if let Some(after) = htf.candles.get(pos + 1) {
                    assert!(
                        after.close_time > c,
                        "not the most-recent: next HTF close {} also <= {c}",
                        after.close_time
                    );
                }
            }
        }
    }

    /// (d) HTF series starting later than primary → early primary bars get None
    /// until the first HTF bar has closed.
    #[test]
    fn htf_starting_later_leaves_early_bars_unpaired() {
        // Primary starts at 0; HTF starts at 4h (its bar 0 closes at
        // 4h + 4h - 1 = 28_799_999). Every primary bar closing before that is
        // None.
        let primary = m15_series(0, 80); // 80 * 15m = 20h
        let htf = h4_series(Timeframe::H4.duration_ms(), 3); // starts at 4h
        let feed = align(&primary, Some(&htf), None);

        let first_htf_close = htf.candles[0].close_time;
        let mut saw_none = false;
        let mut saw_some = false;
        for bar in &feed {
            if bar.primary.close_time < first_htf_close {
                assert!(bar.htf.is_none(), "before first HTF close must be None");
                saw_none = true;
            } else {
                assert!(bar.htf.is_some(), "after first HTF close must be Some");
                saw_some = true;
            }
        }
        assert!(saw_none, "test must exercise the unpaired-early region");
        assert!(saw_some, "test must exercise the paired region");
    }

    /// Determinism: identical inputs yield an identical stream.
    #[test]
    fn align_is_deterministic() {
        let primary = m15_series(0, 30);
        let htf = h4_series(0, 2);
        let a: Vec<AlignedBar> = align(&primary, Some(&htf), None);
        let b: Vec<AlignedBar> = align(&primary, Some(&htf), None);
        assert_eq!(a, b);
    }

    /// D1 series: bars of nominal 1d width starting at `start`, `n` of them.
    fn d1_series(start: i64, n: usize) -> CandleSeries {
        let step = Timeframe::D1.duration_ms();
        let candles = (0..n as i64)
            .map(|i| {
                let open = start + i * step;
                candle(open, open + step - 1)
            })
            .collect();
        CandleSeries {
            pair: Pair::new("BTCUSDT"),
            timeframe: Timeframe::D1,
            version: DataVersion::new("v1"),
            candles,
        }
    }

    /// (r3.s2.w4) The daily pointer follows the same closed-bar rule as the
    /// HTF pointer, independently: day 0 closes at 86_399_999 (M15 index 95),
    /// day 1 at 172_799_999 (index 191). Every paired bar must be the greatest
    /// close_time <= the primary's, the early bars stay None, and a forming
    /// day-2 bar (never supplied here) would be invisible anyway.
    #[test]
    fn pairs_most_recent_closed_d1_bar_independently_of_htf() {
        let primary = m15_series(0, 96 * 2);
        let htf = h4_series(0, 12);
        let daily = d1_series(0, 2);
        let feed = align(&primary, Some(&htf), Some(&daily));

        for bar in &feed {
            let c = bar.primary.close_time;
            let expected = daily.candles.iter().rev().find(|d| d.close_time <= c);
            assert_eq!(
                bar.d1, expected,
                "bar idx {} close_time {c}: wrong D1 pairing",
                bar.index
            );
            // The HTF pairing is unaffected by the daily pointer.
            let expected_htf = htf.candles.iter().rev().find(|h| h.close_time <= c);
            assert_eq!(
                bar.htf, expected_htf,
                "bar idx {}: H4 pairing drifted",
                bar.index
            );
        }
        assert_eq!(feed[94].d1, None);
        assert_eq!(feed[95].d1, Some(&daily.candles[0]));
        assert_eq!(feed[190].d1, Some(&daily.candles[0]));
        assert_eq!(feed[191].d1, Some(&daily.candles[1]));
        // No D1 series at all: every bar None, HTF unchanged.
        let feed = align(&primary, Some(&htf), None);
        assert!(feed.iter().all(|bar| bar.d1.is_none()));
    }
}
