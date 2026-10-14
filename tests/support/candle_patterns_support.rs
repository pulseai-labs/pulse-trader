//! Support for `tests/candle_patterns.rs` (r5.s1.w1): reading the committed
//! TA-Lib reference CSVs, streaming a pattern over a bar list, and loading the
//! fixture store's own series through the same public API recipe the other
//! fixture-store suites use.
//!
//! Everything here is plumbing. The expectations live in the suite, and their
//! source is TA-Lib's output in the CSVs — never a value recomputed from the
//! implementation under test.

#![allow(clippy::unwrap_used, clippy::expect_used)]
// Shared harness: every `tests/*.rs` consumer compiles this module separately,
// so a helper only `candle_patterns.rs` uses reads as dead code inside the
// other suites (the `mcp.rs` precedent).
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use pulse::{
    Candle, CandlePatternIndicator, CandlePatternName, CandleSeries, CandleStore, Indicator, Pair,
    Timeframe,
};
use rust_decimal::Decimal;

/// One reference CSV, parsed.
pub struct Table {
    /// The bars, in file order, parsed from the exact decimal strings.
    pub candles: Vec<Candle>,
    /// Each pattern column, by its TA-Lib name, as raw integers.
    pub columns: BTreeMap<String, Vec<i64>>,
    /// Each row's `open_time`, for the mismatch lines.
    pub open_times: Vec<i64>,
}

/// The directory the reference CSVs live in.
#[must_use]
pub fn fixture_path(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/candle-patterns")
        .join(file)
}

/// Parse one reference CSV.
///
/// The files have no quoting and a fixed layout — `open_time` then OHLC then one
/// column per pattern — so a split on commas is exact. Columns are located by
/// header name, so a reordered file cannot silently swap two patterns.
#[must_use]
pub fn read_csv(file: &str) -> Table {
    let path = fixture_path(file);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut lines = text.lines();
    let header: Vec<&str> = lines
        .next()
        .unwrap_or_else(|| panic!("{}: empty", path.display()))
        .split(',')
        .collect();
    let index_of = |name: &str| {
        header
            .iter()
            .position(|column| *column == name)
            .unwrap_or_else(|| panic!("{}: no {name} column in {header:?}", path.display()))
    };
    let (i_time, i_open, i_high, i_low, i_close) = (
        index_of("open_time"),
        index_of("open"),
        index_of("high"),
        index_of("low"),
        index_of("close"),
    );
    let pattern_columns: Vec<(String, usize)> = header
        .iter()
        .enumerate()
        .filter(|(_, name)| name.starts_with("CDL"))
        .map(|(index, name)| ((*name).to_owned(), index))
        .collect();

    let mut candles = Vec::new();
    let mut open_times = Vec::new();
    let mut columns: BTreeMap<String, Vec<i64>> = pattern_columns
        .iter()
        .map(|(name, _)| (name.clone(), Vec::new()))
        .collect();

    for (row, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        let field = |index: usize, what: &str| -> &str {
            fields.get(index).unwrap_or_else(|| {
                panic!(
                    "{}: row {row} has {} fields, missing {what} at {index}",
                    path.display(),
                    fields.len()
                )
            })
        };
        let open_time = field(i_time, "open_time")
            .parse::<i64>()
            .unwrap_or_else(|e| panic!("{}: row {row} open_time: {e}", path.display()));
        // Exact decimal strings — never `f64` (ADR-0019).
        let decimal = |index: usize, what: &str| -> Decimal {
            Decimal::from_str_exact(field(index, what))
                .unwrap_or_else(|e| panic!("{}: row {row} {what}: {e}", path.display()))
        };
        open_times.push(open_time);
        candles.push(Candle {
            open_time,
            close_time: open_time,
            open: decimal(i_open, "open"),
            high: decimal(i_high, "high"),
            low: decimal(i_low, "low"),
            close: decimal(i_close, "close"),
            volume: Decimal::from(1),
            funding_rate: None,
        });
        for (name, index) in &pattern_columns {
            let value = field(*index, name)
                .parse::<i64>()
                .unwrap_or_else(|e| panic!("{}: row {row} {name}: {e}", path.display()));
            columns
                .get_mut(name)
                .expect("the column was seeded from the header")
                .push(value);
        }
    }

    Table {
        candles,
        columns,
        open_times,
    }
}

/// Stream one pattern over a bar list, returning one value per bar.
#[must_use]
pub fn stream(candles: &[Candle], pattern: CandlePatternName) -> Vec<Option<Decimal>> {
    let mut indicator = CandlePatternIndicator::new(pattern);
    candles
        .iter()
        .map(|candle| indicator.next(candle))
        .collect()
}

/// Load one committed fixture series (no network, no DB): `with_base_dir` ->
/// `read_head` -> `read_snapshot` — the recipe the other fixture-store suites
/// use.
#[must_use]
pub fn load_fixture_series(timeframe: Timeframe) -> CandleSeries {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("BTCUSDT");
    let head = store
        .read_head(&pair, timeframe)
        .expect("read HEAD")
        .expect("HEAD present in the fixture store");
    store
        .read_snapshot(&pair, timeframe, &head)
        .expect("read snapshot")
}
