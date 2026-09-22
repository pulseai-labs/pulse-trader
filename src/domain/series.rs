//! `CandleSeries` — an ordered run of candles for one (pair, timeframe, version),
//! plus structural validation that reports gaps without rejecting them (audit C2).

use serde::{Deserialize, Serialize};

use crate::domain::backtest::CandleWindow;
use crate::domain::candle::Candle;
use crate::domain::error::{DataError, ValidationError};
use crate::domain::pair::Pair;
use crate::domain::timeframe::Timeframe;
use crate::domain::version::DataVersion;

/// A reported spacing discontinuity between two adjacent candles.
///
/// `validate` returns these through `Ok` — a gap is information, not corruption
/// (audit C2). The pipeline (WI-02/05) decides whether a gap is acceptable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    /// `open_time` (epoch ms) the next candle was expected at, given the
    /// preceding candle plus one timeframe duration.
    pub expected: i64,
    /// `open_time` (epoch ms) actually found at the discontinuity.
    pub found: i64,
}

/// An immutable, ordered run of candles keyed by (pair, timeframe, version).
///
/// FR-5: the versioned snapshot shape. Construction does not validate; call
/// [`CandleSeries::validate`] explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandleSeries {
    /// The trading pair these candles belong to.
    pub pair: Pair,
    /// The candle interval.
    pub timeframe: Timeframe,
    /// The snapshot version tag.
    pub version: DataVersion,
    /// The candles, expected to be sorted ascending by `open_time`.
    pub candles: Vec<Candle>,
}

/// A [`CandleSeries`] as a repository holds it, plus an opaque locator for the
/// snapshot it came from or went to (r1.s3.w1, #112).
///
/// `storage_location` is a **display** locator, not an instruction: it is not a
/// `PathBuf`, the caller never joins to it or opens it, and its shape is the
/// adapter's business. The Parquet adapter returns the snapshot's absolute path,
/// which is what the debug CLI's locked `path` field reports (ADR-0017); an
/// in-memory double may return anything at all. It is `None` only for the
/// existing zero-candle first-run outcome, where no snapshot exists to point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCandleSeries {
    /// The series itself.
    pub series: CandleSeries,
    /// An opaque display locator for the persisted snapshot, or `None` when the
    /// zero-candle outcome persisted nothing.
    pub storage_location: Option<String>,
}

impl CandleSeries {
    /// Validate structural soundness and report spacing gaps (audit C2).
    ///
    /// Returns `Err(DataError::Validation(..))` only on *structural corruption*:
    /// candles not strictly increasing by `open_time` (`Unsorted`) or a repeated
    /// `open_time` (`Duplicate`). For a structurally-sound series it returns
    /// `Ok(gaps)`: every adjacent pair whose spacing exceeds one timeframe
    /// duration produces a [`Gap`]. A clean contiguous series yields `Ok(vec![])`;
    /// a sorted-but-gapped series yields `Ok(non-empty)` — gaps are reported, not
    /// rejected.
    ///
    /// # Errors
    ///
    /// [`DataError::Validation`] with [`ValidationError::Unsorted`] if `open_time`
    /// ever decreases, or [`ValidationError::Duplicate`] on a repeated `open_time`.
    pub fn validate(&self) -> Result<Vec<Gap>, DataError> {
        let step = self.timeframe.duration_ms();
        let mut gaps = Vec::new();

        for window in self.candles.windows(2) {
            let prev = &window[0];
            let next = &window[1];

            if next.open_time == prev.open_time {
                return Err(ValidationError::Duplicate(next.open_time).into());
            }
            if next.open_time < prev.open_time {
                return Err(ValidationError::Unsorted {
                    earlier: prev.open_time,
                    later: next.open_time,
                }
                .into());
            }

            let expected = prev.open_time + step;
            if next.open_time != expected {
                gaps.push(Gap {
                    expected,
                    found: next.open_time,
                });
            }
        }

        Ok(gaps)
    }

    /// Slice the series to a half-open candle window `[from_ms, to_ms)` on
    /// `open_time` (r2.s1.w3, ruling 1: the backtester's date-window semantics).
    ///
    /// The slice keeps `pair`, `timeframe` and `version` — the version names
    /// the snapshot the candles were sliced FROM, which is the identity a run's
    /// `inputs` must record; the slice itself is not a stored snapshot. A
    /// window containing no candle yields an empty `candles` — whether that is
    /// a refusal is the caller's choice (a windowed backtest refuses an empty
    /// primary slice; an empty HTF slice is a legal all-`None` alignment).
    /// Whether the slice is the engine's whole input or only its counted span
    /// is likewise the caller's: r2.s3.w2's backtest slices `[snapshot_start,
    /// to)` so indicators warm on the full lead-in and only `[from, to)`
    /// counts; `windowed` stays the pure `[from, to)` filter either way.
    #[must_use]
    pub fn windowed(&self, window: &CandleWindow) -> CandleSeries {
        CandleSeries {
            pair: self.pair.clone(),
            timeframe: self.timeframe,
            version: self.version.clone(),
            candles: self
                .candles
                .iter()
                .filter(|c| c.open_time >= window.from_ms && c.open_time < window.to_ms)
                .cloned()
                .collect(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{CandleSeries, Gap};
    use crate::domain::backtest::CandleWindow;
    use crate::domain::candle::Candle;
    use crate::domain::error::{DataError, ValidationError};
    use crate::domain::pair::Pair;
    use crate::domain::timeframe::Timeframe;
    use crate::domain::version::DataVersion;
    use rust_decimal::Decimal;

    fn candle_at(open_time: i64) -> Candle {
        Candle {
            open_time,
            close_time: open_time + 899_999,
            open: Decimal::ONE,
            high: Decimal::ONE,
            low: Decimal::ONE,
            close: Decimal::ONE,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    fn series(open_times: &[i64]) -> CandleSeries {
        CandleSeries {
            pair: Pair::new("BTCUSDT"),
            timeframe: Timeframe::M15,
            version: DataVersion::new("v1"),
            candles: open_times.iter().copied().map(candle_at).collect(),
        }
    }

    #[test]
    fn clean_contiguous_series_reports_no_gaps() {
        let s = series(&[0, 900_000, 1_800_000, 2_700_000]);
        let gaps = s.validate().expect("clean series validates");
        assert!(gaps.is_empty(), "expected no gaps, got {gaps:?}");
    }

    #[test]
    fn empty_and_single_series_are_clean() {
        assert!(series(&[]).validate().expect("empty validates").is_empty());
        assert!(
            series(&[42])
                .validate()
                .expect("single validates")
                .is_empty()
        );
    }

    #[test]
    fn sorted_but_gapped_series_reports_gaps_not_error() {
        // Missing the 900_000 bar: jump from 0 to 1_800_000.
        let s = series(&[0, 1_800_000, 2_700_000]);
        let gaps = s.validate().expect("gapped series still validates Ok");
        assert_eq!(
            gaps,
            vec![Gap {
                expected: 900_000,
                found: 1_800_000,
            }]
        );
    }

    #[test]
    fn duplicate_open_time_is_structural_error() {
        let s = series(&[0, 900_000, 900_000]);
        let err = s.validate().expect_err("duplicate must reject");
        assert_eq!(
            err,
            DataError::Validation(ValidationError::Duplicate(900_000))
        );
    }

    #[test]
    fn unsorted_open_time_is_structural_error() {
        let s = series(&[0, 1_800_000, 900_000]);
        let err = s.validate().expect_err("unsorted must reject");
        assert_eq!(
            err,
            DataError::Validation(ValidationError::Unsorted {
                earlier: 1_800_000,
                later: 900_000,
            })
        );
    }

    #[test]
    fn series_serde_round_trips() {
        let s = series(&[0, 900_000]);
        let json = serde_json::to_string(&s).expect("serialize series");
        let back: CandleSeries = serde_json::from_str(&json).expect("deserialize series");
        assert_eq!(s, back);
    }

    #[test]
    fn windowed_keeps_only_open_times_inside_the_half_open_bounds() {
        // `from` is inclusive, `to` is exclusive: the candle AT `to` drops.
        let s = series(&[0, 900_000, 1_800_000, 2_700_000, 3_600_000]);
        let window = CandleWindow::new(900_000, 2_700_000).expect("window");
        let sliced = s.windowed(&window);
        let times: Vec<i64> = sliced.candles.iter().map(|c| c.open_time).collect();
        assert_eq!(times, vec![900_000, 1_800_000]);
    }

    #[test]
    fn windowed_boundary_candles_follow_from_inclusive_to_exclusive() {
        let s = series(&[0, 900_000, 1_800_000]);
        let window = CandleWindow::new(0, 1_800_000).expect("window");
        let times: Vec<i64> = s
            .windowed(&window)
            .candles
            .iter()
            .map(|c| c.open_time)
            .collect();
        assert_eq!(times, vec![0, 900_000], "candle at `to` is excluded");
    }

    #[test]
    fn windowed_empty_result_preserves_the_source_identity() {
        // An empty slice still names the snapshot it was sliced from — the
        // run's `inputs` record THAT version, not a slice id.
        let s = series(&[0, 900_000]);
        let window = CandleWindow::new(3_600_000, 4_500_000).expect("window");
        let sliced = s.windowed(&window);
        assert!(sliced.candles.is_empty());
        assert_eq!(sliced.pair, s.pair);
        assert_eq!(sliced.timeframe, s.timeframe);
        assert_eq!(sliced.version, s.version, "version tag names the source");
    }

    #[test]
    fn windowed_covering_everything_returns_the_whole_series() {
        let s = series(&[0, 900_000, 1_800_000]);
        let window = CandleWindow::new(-1, 10_000_000).expect("window");
        assert_eq!(s.windowed(&window).candles.len(), 3);
    }
}
