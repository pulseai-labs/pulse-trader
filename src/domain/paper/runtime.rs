//! The live runtime's pure half (r3.s4.w3, ADR-0027).
//!
//! Three things live here and nothing else:
//!
//! - **boundary arithmetic** — given a cadence's `duration_ms`, the instant
//!   `now` and the polling grace, which bar is the newest fully closed one and
//!   when the next wake is due. Generic over the duration, so no cadence is
//!   special-cased (A10).
//! - **[`events_for_step`]** — the paper log's signal/fill/funding events for
//!   one engine step, derived from the step's before/after state. This module
//!   is the ONLY place `PaperEvent::Order` is constructed: the domain ring may
//!   name it, and the application ring's order-capability guard
//!   (`tests/tauri_backtest.rs`) stays green because the runtime never does.
//! - **[`compare`]** — the shadow-identity verdict over one epoch's trades and
//!   the open position, and the [`ShadowResult`] payload the `shadow_checked`
//!   event carries.
//!
//! Pure: no store, no clock, no I/O.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::Direction;
use crate::domain::backtest::{OpenPositionMark, Trade, funding_payment};
use crate::domain::candle::Candle;
use crate::domain::paper::event::{PaperEvent, PaperSide};

/// Where a bar cadence sits relative to `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarBoundaries {
    /// The `open_time` of the newest bar fully closed at `now` (its
    /// `close_time < now`).
    pub last_closed_open_ms: i64,
    /// The first instant strictly after `now` at which a bar of this cadence is
    /// closed, plus the polling grace — the instant a wake should target.
    pub next_poll_ms: i64,
}

/// The first bar of this cadence that is NOT closed at `now` — the bar whose
/// `close_time >= now`. Every earlier bar is closed and may be consumed; this
/// one is the first live bar a session waits for.
///
/// `duration_ms` is the cadence, never a named timeframe: the whole point is
/// that the runtime is timeframe-agnostic (A10).
#[must_use]
pub fn first_open_bar_ms(duration_ms: i64, now_ms: i64) -> i64 {
    (now_ms - duration_ms).div_euclid(duration_ms) * duration_ms + duration_ms
}

/// The newest fully closed bar and the next wake instant for one cadence.
///
/// `next_poll_ms` is `last_closed_open_ms + duration_ms + grace_ms` advanced
/// past `now` — i.e. the smallest instant of the form `bar open + duration +
/// grace` that lies strictly after `now`. The grace keeps a poll off the exact
/// boundary millisecond (the exchange's own clock may lag the local one).
#[must_use]
pub fn boundaries(duration_ms: i64, now_ms: i64, grace_ms: i64) -> BarBoundaries {
    let first_open = first_open_bar_ms(duration_ms, now_ms);
    let next_poll = (now_ms - duration_ms - grace_ms).div_euclid(duration_ms) * duration_ms
        + duration_ms
        + duration_ms
        + grace_ms;
    BarBoundaries {
        last_closed_open_ms: first_open - duration_ms,
        next_poll_ms: next_poll,
    }
}

/// The next UTC midnight strictly after `ms`.
#[must_use]
pub fn next_utc_midnight_after(ms: i64) -> i64 {
    const DAY_MS: i64 = 86_400_000;
    (ms.div_euclid(DAY_MS) + 1) * DAY_MS
}

/// Whether a daily shadow check is due: `now` is at or past the first UTC
/// midnight after the last check.
#[must_use]
pub fn daily_shadow_due(now_ms: i64, last_check_ms: i64) -> bool {
    now_ms >= next_utc_midnight_after(last_check_ms)
}

/// The engine state one step can move: the closed trades so far and the open
/// position mark, both read from an `EngineSession` around a `step`.
///
/// `before` carries the trade list truncated at the step's start (the same
/// slice's prefix), so the events are the step's *delta* — never a replay.
#[derive(Debug, Clone)]
pub struct StepView<'a> {
    /// The trades closed at this point, in chronological order.
    pub closed_trades: &'a [Trade],
    /// The position open at this point, when one stands.
    pub open_position: Option<OpenPositionMark>,
}

impl StepView<'_> {
    /// The view of a session that has stepped nothing yet.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            closed_trades: &[],
            open_position: None,
        }
    }
}

/// The paper log's events for one stepped bar: the signal and fill of an entry
/// or exit the step produced, and the funding payment the bar's stamp accrued
/// on a position already held.
///
/// `before` is the engine read before the step and `after` the same read after
/// it; `bar` is the primary candle that was stepped; `at` is the injected
/// clock's RFC3339 instant (the events carry it as the row's instant).
///
/// The funding rule mirrors the engine's own accrual window exactly: the
/// `(entry_fill_time, exit_fill_time]` half-open window means a stamped bar at
/// `t` accrues to a position iff `entry_fill_time < t`, whether that position
/// is still open or closes on this very bar — so the sum of the emitted
/// payments over a trade's life equals the trade's `funding_total`.
#[must_use]
pub fn events_for_step(
    before: &StepView<'_>,
    after: &StepView<'_>,
    bar: &Candle,
    at: &str,
) -> Vec<PaperEvent> {
    let mut events = Vec::new();
    // An entry: a position that did not stand before and stands after.
    if before.open_position.is_none()
        && let Some(position) = after.open_position.as_ref()
    {
        let side = side_of(position.direction);
        events.push(order(side, position.qty, at));
        events.push(fill(
            side,
            position.qty,
            position.entry_price,
            None,
            None,
            at,
        ));
    }
    // Exits: every trade the step closed (the slice's new tail). One bar can
    // open and stop out a position, so both arms may fire.
    for trade in &after.closed_trades[before.closed_trades.len().min(after.closed_trades.len())..] {
        let side = side_of(trade.direction);
        events.push(order(side, trade.qty, at));
        events.push(fill(
            side,
            trade.qty,
            trade.exit_price,
            Some(trade.exit_reason),
            Some(trade.realized_r),
            at,
        ));
    }
    // Funding: the bar's own stamp, accrued on the position held across it.
    if let Some(rate) = bar.funding_rate
        && let Some(position) = before.open_position.as_ref()
        && position.entry_fill_time < bar.open_time
    {
        let notional = position.qty * position.entry_price;
        events.push(PaperEvent::Funding {
            seq: 0,
            at: at.to_owned(),
            rate,
            amount: funding_payment(rate, notional, position.direction),
        });
    }
    events
}

/// The paper log's side vocabulary for an engine direction.
fn side_of(direction: Direction) -> PaperSide {
    match direction {
        Direction::Long => PaperSide::Long,
        Direction::Short => PaperSide::Short,
    }
}

/// A signal event for `side`/`qty`.
fn order(side: PaperSide, qty: Decimal, at: &str) -> PaperEvent {
    PaperEvent::Order {
        seq: 0,
        at: at.to_owned(),
        side,
        qty,
    }
}

/// A fill event. `realized_r` rides only an exit fill (the closed trade's
/// R-multiple, spec §2); an entry fill carries `None`.
fn fill(
    side: PaperSide,
    qty: Decimal,
    price: Decimal,
    exit_reason: Option<crate::domain::backtest::ExitReason>,
    realized_r: Option<Decimal>,
    at: &str,
) -> PaperEvent {
    PaperEvent::Fill {
        seq: 0,
        at: at.to_owned(),
        side,
        qty,
        price,
        exit_reason,
        realized_r,
    }
}

/// Which slice of a session's log the live epoch covers (E3: shadow identity is
/// judged per epoch, against the live epoch's own build).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpochStart {
    /// No `engine_upgraded` has ever been recorded: the whole log is one epoch.
    WholeLog,
    /// The epoch opened at this primary bar (`open_time`): only trades entered
    /// at or after it belong to it.
    Bar(i64),
    /// The epoch opened after the newest recorded bar: it holds no trades yet.
    Empty,
}

impl EpochStart {
    /// The subset of `trades` this epoch owns.
    #[must_use]
    pub fn trades<'a>(&self, trades: &'a [Trade]) -> &'a [Trade] {
        match self {
            Self::WholeLog => trades,
            Self::Bar(from) => {
                let at = trades.partition_point(|trade| trade.entry_fill_time < *from);
                &trades[at..]
            }
            Self::Empty => &trades[..0],
        }
    }
}

/// The typed payload of a `shadow_checked` event (the `result` field), and the
/// return of `PaperRuntime::shadow_check`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ShadowResult {
    /// The live epoch and the shadow agree.
    Identical {
        /// How many closed trades the check compared.
        closed_trades: u64,
        /// Whether an open position was compared (and agreed).
        open_position: bool,
    },
    /// They disagree; the first divergence is named.
    Drift {
        /// What diverged first, in words.
        first_divergence: String,
        /// The live side of the divergence.
        live: serde_json::Value,
        /// The shadow side of the divergence.
        shadow: serde_json::Value,
    },
}

impl ShadowResult {
    /// Whether the check found identity.
    #[must_use]
    pub fn is_identical(&self) -> bool {
        matches!(self, Self::Identical { .. })
    }
}

/// Compare the live epoch's closed trades and open position against the
/// shadow's, both scoped to `epoch`.
///
/// A count mismatch is itself the first divergence (reported before any field
/// comparison, because the lists are no longer the same trades). Otherwise the
/// first differing trade, then the open position, is named.
#[must_use]
pub fn compare(
    live: &[Trade],
    live_open: Option<&OpenPositionMark>,
    shadow: &[Trade],
    shadow_open: Option<&OpenPositionMark>,
    epoch: EpochStart,
) -> ShadowResult {
    let live = epoch.trades(live);
    let shadow = epoch.trades(shadow);
    let common = live.len().min(shadow.len());
    for index in 0..common {
        if live[index] != shadow[index] {
            return drift(index, live, shadow);
        }
    }
    if live.len() != shadow.len() {
        return ShadowResult::Drift {
            first_divergence: format!(
                "closed-trade count differs (live {}, shadow {})",
                live.len(),
                shadow.len()
            ),
            live: serde_json::json!(live.len()),
            shadow: serde_json::json!(shadow.len()),
        };
    }
    match (live_open, shadow_open) {
        (Some(live_mark), Some(shadow_mark)) if live_mark == shadow_mark => {
            ShadowResult::Identical {
                closed_trades: u64::try_from(live.len()).unwrap_or(u64::MAX),
                open_position: true,
            }
        }
        (None, None) => ShadowResult::Identical {
            closed_trades: u64::try_from(live.len()).unwrap_or(u64::MAX),
            open_position: false,
        },
        (live_mark, shadow_mark) => ShadowResult::Drift {
            first_divergence: "open position differs".to_owned(),
            live: serde_json::to_value(live_mark).unwrap_or(serde_json::Value::Null),
            shadow: serde_json::to_value(shadow_mark).unwrap_or(serde_json::Value::Null),
        },
    }
}

/// A drift on trade `index` of the two (already epoch-scoped) lists.
fn drift(index: usize, live: &[Trade], shadow: &[Trade]) -> ShadowResult {
    ShadowResult::Drift {
        first_divergence: format!("closed-trade {index} differs"),
        live: serde_json::to_value(&live[index]).unwrap_or(serde_json::Value::Null),
        shadow: serde_json::to_value(&shadow[index]).unwrap_or(serde_json::Value::Null),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        BarBoundaries, boundaries, daily_shadow_due, first_open_bar_ms, next_utc_midnight_after,
    };

    const FIFTEEN_MINUTES: i64 = 900_000;

    #[test]
    fn first_open_bar_is_the_first_bar_not_closed_at_now() {
        // Exactly on a boundary: that bar is still forming.
        assert_eq!(
            first_open_bar_ms(FIFTEEN_MINUTES, 1_735_689_600_000),
            1_735_689_600_000
        );
        // Mid-bar: the forming bar is the one that opened at the boundary.
        assert_eq!(
            first_open_bar_ms(FIFTEEN_MINUTES, 1_735_689_600_001),
            1_735_689_600_000
        );
        assert_eq!(
            first_open_bar_ms(FIFTEEN_MINUTES, 1_735_689_899_999),
            1_735_689_600_000
        );
    }

    #[test]
    fn boundaries_report_the_last_closed_bar_and_the_next_poll() {
        // 00:15:00.000, five seconds of grace: the last closed bar opened at
        // 00:00, and the next poll is 00:15:05 (the 00:15 bar is still forming).
        let now = 1_735_689_600_000 + FIFTEEN_MINUTES;
        let b: BarBoundaries = boundaries(FIFTEEN_MINUTES, now, 5_000);
        assert_eq!(b.last_closed_open_ms, 1_735_689_600_000);
        assert_eq!(b.next_poll_ms, now + 5_000);
        assert!(b.next_poll_ms > now);
        // Later in the same bar the target moves to the next close.
        let later = boundaries(FIFTEEN_MINUTES, now + 6_000, 5_000);
        assert_eq!(later.next_poll_ms, now + FIFTEEN_MINUTES + 5_000);
        // The grace keeps the target strictly in the future at every instant.
        for offset in 0..FIFTEEN_MINUTES {
            let probe = boundaries(FIFTEEN_MINUTES, now + offset, 5_000);
            assert!(probe.next_poll_ms > now + offset, "offset {offset}");
        }
    }

    #[test]
    fn daily_cadence_lands_on_the_next_utc_midnight() {
        assert_eq!(
            next_utc_midnight_after(1_735_689_600_000),
            1_735_689_600_000 + 86_400_000
        );
        assert_eq!(
            next_utc_midnight_after(1_735_689_600_001),
            1_735_689_600_000 + 86_400_000
        );
        assert_eq!(
            next_utc_midnight_after(1_735_689_599_999),
            1_735_689_600_000
        );
        // Checked at the previous midnight: the next one has not arrived yet.
        assert!(!daily_shadow_due(
            1_735_689_599_999,
            1_735_689_600_000 - 86_400_000
        ));
        assert!(daily_shadow_due(
            1_735_689_600_000,
            1_735_689_600_000 - 86_400_000
        ));
    }
}
