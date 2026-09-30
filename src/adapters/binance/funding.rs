//! Funding-rate ingest + alignment (FR-5, spec §3 audit C1).
//!
//! Funding is 8-hourly and fires at 00:00 / 08:00 / 16:00 UTC, which lands
//! exactly on every H4 open and every 96th M15 open. The alignment rule is
//! **stamp-per-event, sparse**: each funding event is stamped onto the single
//! candle whose half-open `[open_time, close_time)` interval contains the
//! event's timestamp; every other candle keeps `funding_rate = None`. There is
//! **no forward-fill** — the data layer faithfully records *where* funding
//! occurred; applying the cost is the backtester's later choice.
//!
//! The **on-boundary case** (an event exactly on a candle's `open_time`) is the
//! norm: `[open_time, close_time)` is closed on the left, so the event stamps
//! the candle that opens at it. A defensive off-boundary timestamp maps to its
//! containing candle.

use rust_decimal::Decimal;

use crate::domain::{Candle, DataError};

/// The USD-M futures funding CSV column count (`calc_time`,
/// `funding_interval_hours`, `last_funding_rate`).
const FUNDING_COLUMNS: usize = 3;

/// One funding event: a rate effective at a UTC-epoch-millisecond timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingEvent {
    /// Funding calculation time, UTC epoch milliseconds.
    pub calc_time: i64,
    /// The funding rate at `calc_time`.
    pub rate: Decimal,
}

/// Parse a USD-M futures funding-rate CSV body into [`FundingEvent`]s, detecting
/// header-row presence (schema: `calc_time,funding_interval_hours,last_funding_rate`).
///
/// # Errors
///
/// [`DataError::Parse`] if a row lacks the pinned 3 columns or a field fails to
/// parse.
pub(crate) fn parse_funding(csv_body: &str) -> Result<Vec<FundingEvent>, DataError> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(false)
        .from_reader(csv_body.as_bytes());

    let mut events = Vec::new();
    for record in reader.records() {
        let record =
            record.map_err(|e| DataError::Parse(format!("malformed funding CSV row: {e}")))?;

        if record.len() != FUNDING_COLUMNS {
            return Err(DataError::Parse(format!(
                "expected {FUNDING_COLUMNS} funding columns, found {}",
                record.len()
            )));
        }

        // Header detection: first cell is the literal column name on a header.
        let first = record.get(0).unwrap_or_default();
        if first.eq_ignore_ascii_case("calc_time") {
            continue;
        }

        let calc_time = first
            .trim()
            .parse::<i64>()
            .map_err(|e| DataError::Parse(format!("calc_time: {e}")))?;
        let rate = record
            .get(2)
            .unwrap_or_default()
            .trim()
            .parse::<Decimal>()
            .map_err(|e| DataError::Parse(format!("last_funding_rate: {e}")))?;

        events.push(FundingEvent { calc_time, rate });
    }

    Ok(events)
}

/// Stamp each funding event onto the single candle whose half-open
/// `[open_time, close_time)` interval contains the event timestamp (AC-4).
///
/// Sparse: candles with no event keep `funding_rate = None`. On-boundary events
/// (ts == a candle's `open_time`) stamp that candle (left-closed). Events that
/// fall outside every candle's interval are ignored (defensive — bulk months
/// are aligned to the candle range). `candles` is expected to be sorted
/// ascending by `open_time` (the post-[`super::normalize`] state).
///
/// If two events map to the same candle, the LAST one wins — the in-place write
/// above overwrites, and nothing accumulates. That collision cannot happen at
/// M15 (8-hourly events, 15-minute candles) nor at H4, but it is NORMAL for a
/// `1d` candle: all three 8-hourly events of the day land in one interval, so
/// the day keeps the 16:00 UTC rate. That is a property of this function, not a
/// decision about D1 funding — r3.s2.w4 adds the daily series as a *signal*
/// series (its operands are read for conditions) and leaves funding on D1 out of
/// scope, so nothing here changes behaviour.
pub(crate) fn stamp_funding(candles: &mut [Candle], events: &[FundingEvent]) {
    for event in events {
        // Find the candle whose [open_time, close_time) contains the event.
        // Candles are sorted, so a linear scan is O(n) per event; bulk months
        // hold ~3 funding events vs. thousands of candles — cheap enough, and
        // keeps the boundary rule obvious.
        if let Some(candle) = candles
            .iter_mut()
            .find(|c| c.open_time <= event.calc_time && event.calc_time < c.close_time)
        {
            candle.funding_rate = Some(event.rate);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{FundingEvent, parse_funding, stamp_funding};
    use crate::domain::Candle;
    use rust_decimal::Decimal;
    use std::str::FromStr;

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

    // ---- AC-4: parse ------------------------------------------------------

    #[test]
    fn parses_funding_with_and_without_header_identically() {
        let headerless = "1700000000000,8,0.00010000\n1700028800000,8,-0.00005000";
        let header = "calc_time,funding_interval_hours,last_funding_rate\n\
                      1700000000000,8,0.00010000\n1700028800000,8,-0.00005000";
        let a = parse_funding(headerless).expect("headerless");
        let b = parse_funding(header).expect("header");
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].calc_time, 1_700_000_000_000);
        assert_eq!(a[0].rate, Decimal::from_str("0.00010000").unwrap());
        assert_eq!(a[1].rate, Decimal::from_str("-0.00005000").unwrap());
    }

    #[test]
    fn wrong_funding_column_count_is_parse_error() {
        assert!(parse_funding("1700000000000,0.0001").is_err());
    }

    // ---- AC-4: half-open stamping incl. on-boundary -----------------------

    #[test]
    fn stamps_only_the_containing_candle_sparse() {
        // Three H4 candles; one funding event inside the middle candle.
        let mut candles = vec![
            candle(0, 14_400_000),
            candle(14_400_000, 28_800_000),
            candle(28_800_000, 43_200_000),
        ];
        let events = vec![FundingEvent {
            calc_time: 20_000_000, // inside [14_400_000, 28_800_000)
            rate: Decimal::from_str("0.0001").unwrap(),
        }];
        stamp_funding(&mut candles, &events);
        assert_eq!(candles[0].funding_rate, None);
        assert_eq!(
            candles[1].funding_rate,
            Some(Decimal::from_str("0.0001").unwrap())
        );
        assert_eq!(candles[2].funding_rate, None, "no forward-fill");
    }

    #[test]
    fn on_boundary_event_stamps_the_candle_it_opens() {
        // Event exactly on candle[1].open_time: half-open is left-closed, so it
        // stamps candle[1], NOT candle[0] (whose interval ends at that ts).
        let mut candles = vec![candle(0, 14_400_000), candle(14_400_000, 28_800_000)];
        let events = vec![FundingEvent {
            calc_time: 14_400_000, // exactly candle[1].open_time
            rate: Decimal::from_str("0.0002").unwrap(),
        }];
        stamp_funding(&mut candles, &events);
        assert_eq!(
            candles[0].funding_rate, None,
            "the candle that CLOSES at the boundary must NOT be stamped"
        );
        assert_eq!(
            candles[1].funding_rate,
            Some(Decimal::from_str("0.0002").unwrap()),
            "the candle that OPENS at the boundary IS stamped (left-closed)"
        );
    }

    #[test]
    fn event_outside_all_candle_intervals_is_ignored() {
        let mut candles = vec![candle(0, 14_400_000)];
        let events = vec![FundingEvent {
            calc_time: 99_000_000,
            rate: Decimal::from_str("0.0003").unwrap(),
        }];
        stamp_funding(&mut candles, &events);
        assert_eq!(candles[0].funding_rate, None);
    }
}
