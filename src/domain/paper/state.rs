//! The paper session's replay state machine (r3.s4.w2, ADR-0027).
//!
//! State IS the log replayed: [`PaperSessionState::replay`] folds a session's
//! event log into the status, epochs, last processed bar, closed trades, open
//! position, funding total and data-event count, and [`apply`] is the ONE
//! transition replay and the live path both use. `apply` validates as it
//! transitions — a `seq` gap or repeat, a non-increasing `bar_processed`
//! `open_time`, any event after `stop`, and an `engine_upgraded` whose `old`
//! is not the current epoch are [`ReplayError`]s, never silent state.
//!
//! The bar-time law is on the event's MAXIMUM `open_time`: one consumed bar
//! can record rows on several timeframes (an H4 row's `open_time` is its
//! group's first M15 bar, which trails the M15 row), so the sequence that
//! must strictly advance is the newest bar an event processed.
//!
//! Pure: no store, no clock.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::backtest::ExitReason;
use crate::domain::fingerprint::EngineFingerprint;
use crate::domain::paper::event::{BarRef, PaperEvent, PaperSide};
use crate::domain::paper::session::PaperSession;

/// Whether the session runs or has been stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaperSessionStatus {
    /// Live: bars are consumed and orders fill.
    Running,
    /// Stopped: the log is read-only and nothing transitions.
    Stopped,
}

/// The open position a `fill` without an exit reason opened.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperPosition {
    /// The position's side.
    pub side: PaperSide,
    /// The filled quantity.
    pub qty: Decimal,
    /// The entry fill price.
    pub entry_price: Decimal,
    /// The entry fill instant: the `fill` event's `fill_time_ms` (the
    /// engine's fill time) when it has one, else its `at`.
    pub entry_fill_time: String,
}

/// A closed trade: an exit `fill` paired with the position it closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperClosedTrade {
    /// The side that closed.
    pub side: PaperSide,
    /// The closed quantity.
    pub qty: Decimal,
    /// The open position's entry price, when the log recorded one.
    pub entry_price: Option<Decimal>,
    /// The open position's entry instant, when the log recorded one.
    pub entry_fill_time: Option<String>,
    /// The exit fill price.
    pub exit_price: Decimal,
    /// The exit fill instant: the closing `fill` event's `fill_time_ms` (the
    /// engine's fill time) when it has one, else its `at`.
    pub exit_fill_time: String,
    /// Why the position closed.
    pub exit_reason: ExitReason,
    /// The trade's realized R-multiple (r3.s4.w4, spec §2), as the exit
    /// `fill` recorded it. `None` for a w3-era fill written before the field
    /// existed; the OOS comparison counts only `Some` values.
    pub realized_r: Option<Decimal>,
}

/// Why a log (or one event) cannot be applied to a session's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The event's `seq` is not exactly `last + 1` — a gap or a repeat.
    SeqOrder {
        /// The seq the log promised next.
        expected: i64,
        /// The seq the event carries.
        found: i64,
    },
    /// A `bar_processed` whose newest `open_time` does not advance past the
    /// last processed bar — the same bar (or an older one) consumed twice.
    BarOpenTimeReversed {
        /// The newest `open_time` processed so far.
        last: i64,
        /// The offending event's newest `open_time`.
        found: i64,
    },
    /// Any event after the session's `stop` — the log is read-only (A4).
    EventAfterStop,
    /// An `engine_upgraded` whose `old` is not the epoch the session runs in.
    EpochMismatch {
        /// The fingerprint the session actually runs under.
        current: EngineFingerprint,
        /// The `old` the event claims.
        found_old: EngineFingerprint,
    },
}

impl core::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SeqOrder { expected, found } => {
                write!(
                    f,
                    "event seq {found} where {expected} was due (gap or repeat)"
                )
            }
            Self::BarOpenTimeReversed { last, found } => {
                write!(
                    f,
                    "bar_processed at open_time {found} does not advance past {last}"
                )
            }
            Self::EventAfterStop => {
                write!(f, "event after stop: a stopped session's log is read-only")
            }
            Self::EpochMismatch { current, found_old } => write!(
                f,
                "engine_upgraded claims old {} but the session runs {}",
                found_old.as_str(),
                current.as_str()
            ),
        }
    }
}

impl std::error::Error for ReplayError {}

/// The replayed state of one paper session.
#[derive(Debug, Clone, PartialEq)]
pub struct PaperSessionState {
    /// Running or stopped.
    pub status: PaperSessionStatus,
    /// The engine fingerprints in order — the session starts in its row's
    /// fingerprint and every accepted `engine_upgraded` opens an epoch (E3).
    pub epochs: Vec<EngineFingerprint>,
    /// The newest consumed bar's `open_time` (`None` until the first bar).
    pub last_bar_open_time: Option<i64>,
    /// The trades the exit `fill`s closed, in log order.
    pub closed_trades: Vec<PaperClosedTrade>,
    /// The position an entry `fill` opened, if one stands.
    pub open_position: Option<PaperPosition>,
    /// The funding the `funding` events accrued.
    pub funding_total: Decimal,
    /// How many `data_event`s the log recorded.
    pub data_event_count: u64,
    /// The last applied event's `seq` (the next must be exactly +1).
    last_seq: i64,
}

impl PaperSessionState {
    /// The state a freshly promoted session replays to, before any event: the
    /// first epoch is the row's fingerprint at start.
    #[must_use]
    pub fn initial(session: &PaperSession) -> Self {
        Self {
            status: PaperSessionStatus::Running,
            epochs: vec![session.engine_fingerprint.clone()],
            last_bar_open_time: None,
            closed_trades: Vec::new(),
            open_position: None,
            funding_total: Decimal::ZERO,
            data_event_count: 0,
            last_seq: 0,
        }
    }

    /// The one transition. Validates (`seq` continuity, bar-time order, the
    /// stop wall, the epoch chain) and then folds the event's payload in.
    ///
    /// # Errors
    ///
    /// [`ReplayError`] for each refusal the module docs list.
    pub fn apply(&mut self, event: &PaperEvent) -> Result<(), ReplayError> {
        if self.status == PaperSessionStatus::Stopped {
            return Err(ReplayError::EventAfterStop);
        }
        let expected = self.last_seq + 1;
        if event.seq() != expected {
            return Err(ReplayError::SeqOrder {
                expected,
                found: event.seq(),
            });
        }
        match event {
            PaperEvent::BarProcessed { bars, .. } => {
                Self::fold_bars(self, bars)?;
            }
            // An `Order` is a signal, not a fill (positions move on
            // `fill`s); a `ShadowChecked` records itself and transitions
            // nothing. Both fold to "no state change".
            PaperEvent::Order { .. } | PaperEvent::ShadowChecked { .. } => {}
            PaperEvent::Fill {
                side,
                qty,
                price,
                exit_reason,
                at,
                realized_r,
                fill_time_ms,
                ..
            } => {
                // The engine's fill time when the event carries it; a
                // pre-#303 event has none and reads as its row instant.
                let fill_time = fill_time_ms
                    .and_then(chrono::DateTime::from_timestamp_millis)
                    .map_or_else(
                        || at.clone(),
                        |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                    );
                match exit_reason {
                    Some(exit_reason) => {
                        let entry = self.open_position.take();
                        self.closed_trades.push(PaperClosedTrade {
                            side: *side,
                            qty: *qty,
                            entry_price: entry.as_ref().map(|p| p.entry_price),
                            entry_fill_time: entry.as_ref().map(|p| p.entry_fill_time.clone()),
                            exit_price: *price,
                            exit_fill_time: fill_time,
                            exit_reason: *exit_reason,
                            realized_r: *realized_r,
                        });
                    }
                    None => {
                        self.open_position = Some(PaperPosition {
                            side: *side,
                            qty: *qty,
                            entry_price: *price,
                            entry_fill_time: fill_time,
                        });
                    }
                }
            }
            PaperEvent::Funding { amount, .. } => {
                self.funding_total += *amount;
            }
            PaperEvent::Stop { .. } => {
                self.status = PaperSessionStatus::Stopped;
            }
            PaperEvent::DataEvent { .. } => {
                self.data_event_count += 1;
            }
            PaperEvent::EngineUpgraded { old, new, .. } => {
                // `epochs` is never empty: `initial` seeds one epoch and
                // nothing pops, so the fall-through arm is unreachable — but
                // the refusal stays honest (an empty epoch list has nothing
                // the upgrade can claim as its `old`).
                let Some(current) = self.epochs.last() else {
                    return Err(ReplayError::EpochMismatch {
                        current: EngineFingerprint::from_stored(String::new()),
                        found_old: old.clone(),
                    });
                };
                if current != old {
                    return Err(ReplayError::EpochMismatch {
                        current: current.clone(),
                        found_old: old.clone(),
                    });
                }
                self.epochs.push(new.clone());
            }
        }
        self.last_seq = event.seq();
        Ok(())
    }

    /// The bar-time half of [`apply`]: every row the event names must sit at
    /// or below (in `open_time`) the bar it consumed, and the event's NEWEST
    /// `open_time` must strictly advance past the last processed bar.
    fn fold_bars(state: &mut Self, bars: &[BarRef]) -> Result<(), ReplayError> {
        let newest = bars
            .iter()
            .map(|bar| bar.open_time)
            .max()
            .unwrap_or(i64::MIN);
        if let Some(last) = state.last_bar_open_time
            && newest <= last
        {
            return Err(ReplayError::BarOpenTimeReversed {
                last,
                found: newest,
            });
        }
        state.last_bar_open_time = Some(newest);
        Ok(())
    }

    /// Replay a whole log, in order, from the fresh-session state.
    ///
    /// # Errors
    ///
    /// The first [`ReplayError`] `apply` refuses.
    pub fn replay(session: &PaperSession, log: &[PaperEvent]) -> Result<Self, ReplayError> {
        let mut state = Self::initial(session);
        for event in log {
            state.apply(event)?;
        }
        Ok(state)
    }
}
