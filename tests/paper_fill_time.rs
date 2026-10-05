//! #303 — a paper `fill` event carries the engine's fill time.
//!
//! `events_for_step` stamps every event's `at` with the runtime's poll
//! instant, which trails the engine's fill by about one bar. The engine's own
//! fill time rides the `fill` event as `fill_time_ms`, and replay reads the
//! position's and trade's fill times from it. An event written before the
//! field existed has none: it still reads, and replays with `at` as before.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    Candle, Direction, ExitReason, OpenPositionMark, PaperEvent, PaperSide, Regime, StepView,
    Trade, TradeSource, events_for_step,
};
use rust_decimal::Decimal;

const FIFTEEN_MINUTES: i64 = 900_000;
/// 2025-01-01T00:00:00Z.
const BAR_OPEN: i64 = 1_735_689_600_000;
/// The poll instant: one bar plus a grace after the engine's fill.
const POLL_AT: &str = "2025-01-01T00:16:00.000Z";

fn candle(open_time: i64) -> Candle {
    Candle {
        open_time,
        open: Decimal::new(100, 0),
        high: Decimal::new(101, 0),
        low: Decimal::new(98, 0),
        close: Decimal::new(99, 0),
        volume: Decimal::ONE,
        close_time: open_time + FIFTEEN_MINUTES - 1,
        funding_rate: None,
    }
}

fn trade(entry_fill_time: i64, exit_fill_time: i64) -> Trade {
    Trade {
        direction: Direction::Long,
        qty: Decimal::new(2, 0),
        entry_price: Decimal::new(100, 0),
        exit_price: Decimal::new(99, 0),
        entry_signal_time: entry_fill_time - FIFTEEN_MINUTES,
        entry_fill_time,
        exit_signal_time: exit_fill_time - FIFTEEN_MINUTES,
        exit_fill_time,
        fills: Vec::new(),
        fees_total: Decimal::ZERO,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        realized_pnl: Decimal::new(-2, 0),
        realized_r: Decimal::new(-1, 0),
        mfe_r: Decimal::ZERO,
        mae_r: Decimal::new(-1, 0),
        exit_reason: ExitReason::StopLoss,
        source: TradeSource::Backtest,
        regime: Regime::TrendingUp,
        stop_price: Some(Decimal::new(99, 0)),
    }
}

fn mark(entry_fill_time: i64) -> OpenPositionMark {
    OpenPositionMark {
        direction: Direction::Long,
        qty: Decimal::new(2, 0),
        entry_price: Decimal::new(100, 0),
        entry_signal_time: entry_fill_time - FIFTEEN_MINUTES,
        entry_fill_time,
        mark_time: entry_fill_time + FIFTEEN_MINUTES - 1,
        mark_price: Decimal::new(100, 0),
    }
}

/// `(exit_reason, fill_time_ms)` of each fill event, in order.
fn fill_times(events: &[PaperEvent]) -> Vec<(Option<ExitReason>, Option<i64>)> {
    events
        .iter()
        .filter_map(|event| match event {
            PaperEvent::Fill {
                exit_reason,
                fill_time_ms,
                ..
            } => Some((*exit_reason, *fill_time_ms)),
            _ => None,
        })
        .collect()
}

#[test]
fn an_exit_fill_carries_the_engines_exit_fill_time() {
    let held = mark(BAR_OPEN - 4 * FIFTEEN_MINUTES);
    let trades = vec![trade(BAR_OPEN - 4 * FIFTEEN_MINUTES, BAR_OPEN)];
    let events = events_for_step(
        &StepView {
            closed_trades: &[],
            open_position: Some(held),
        },
        &StepView {
            closed_trades: &trades,
            open_position: None,
        },
        &candle(BAR_OPEN),
        POLL_AT,
    );
    assert_eq!(
        fill_times(&events),
        vec![(Some(ExitReason::StopLoss), Some(BAR_OPEN))],
        "the exit fill names the engine's exit fill time, not the poll"
    );
}

#[test]
fn an_entry_fill_carries_the_engines_entry_fill_time() {
    let events = events_for_step(
        &StepView::empty(),
        &StepView {
            closed_trades: &[],
            open_position: Some(mark(BAR_OPEN)),
        },
        &candle(BAR_OPEN),
        POLL_AT,
    );
    assert_eq!(fill_times(&events), vec![(None, Some(BAR_OPEN))]);
}

#[test]
fn a_same_bar_trade_carries_both_engine_fill_times() {
    let trades = vec![trade(BAR_OPEN, BAR_OPEN + FIFTEEN_MINUTES)];
    let events = events_for_step(
        &StepView::empty(),
        &StepView {
            closed_trades: &trades,
            open_position: None,
        },
        &candle(BAR_OPEN),
        POLL_AT,
    );
    assert_eq!(
        fill_times(&events),
        vec![
            (None, Some(BAR_OPEN)),
            (Some(ExitReason::StopLoss), Some(BAR_OPEN + FIFTEEN_MINUTES)),
        ]
    );
}

#[test]
fn fill_time_ms_round_trips_and_a_legacy_fill_still_reads() {
    let event = PaperEvent::Fill {
        seq: 3,
        at: POLL_AT.to_owned(),
        side: PaperSide::Long,
        qty: Decimal::ONE,
        price: Decimal::new(100, 0),
        exit_reason: None,
        realized_r: None,
        fill_time_ms: Some(BAR_OPEN),
    };
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["fill_time_ms"], BAR_OPEN);
    assert_eq!(
        serde_json::from_value::<PaperEvent>(json.clone()).unwrap(),
        event
    );

    // A payload written before the field existed has no such key.
    let mut legacy = json;
    legacy.as_object_mut().unwrap().remove("fill_time_ms");
    let read: PaperEvent = serde_json::from_value(legacy).expect("a legacy fill reads");
    assert!(matches!(
        read,
        PaperEvent::Fill {
            fill_time_ms: None,
            ..
        }
    ));
}
