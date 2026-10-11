//! The paper read's money view (r4.s4.w1, ADR-0027 addendum).
//!
//! [`PaperPnl::derive`] folds a session's log into every money figure a read
//! shows: the orders and fills in log order, each closed trade's net USDT
//! (gross at the slipped fill prices + the trade's funding − its two fees),
//! the open position marked at the last processed primary bar with its costs
//! and its live stops, and the running totals. Pure: no store, no clock.
//!
//! The engine's own cost functions ([`realized_pnl`], [`realized_r`],
//! [`taker_fee`]) do the arithmetic, so a derived figure equals the engine's
//! `Trade.realized_pnl` by construction; the pairing of fills into trades is
//! [`PaperSessionState::replay`]'s, untouched.
//!
//! **Funding is attributed by step** (the spine's money rules): a `Funding`
//! event accrues to the position open *before* its step — the step boundary is
//! the `bar_processed` event — so a bar that closes the position still accrues
//! its funding to that trade (the runtime writes the bar's `Funding` last),
//! and the open position's figure holds the funding accrued so far.
//!
//! Every figure is exact `Decimal`; serialization is the crate's usual
//! decimal-as-text (NFR-2). No event gains a money field, and nothing here
//! reads the store or the clock.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::Direction;
use crate::domain::backtest::{ExitReason, OpenPositionStops, realized_pnl, realized_r, taker_fee};
use crate::domain::paper::event::{PaperEvent, PaperSide};
use crate::domain::paper::session::PaperSession;
use crate::domain::paper::state::{
    PaperClosedTrade, PaperPosition, PaperSessionState, ReplayError,
};

/// The mark a read marks an open position against: the close and close time of
/// the session's last processed primary bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MarkBar {
    /// The bar's close — the mark price.
    pub price: Decimal,
    /// The bar's close time (epoch ms) — the mark instant.
    pub close_time_ms: i64,
}

/// One order or fill, in log order (#344: the entry fills included).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PaperPnlEntry {
    /// A signalled order, as the log recorded it.
    Order {
        /// The event's RFC3339 instant.
        at: String,
        /// The side signalled.
        side: PaperSide,
        /// The quantity signalled.
        qty: Decimal,
    },
    /// A fill, as the log recorded it.
    Fill {
        /// The event's RFC3339 instant.
        at: String,
        /// The side that filled.
        side: PaperSide,
        /// The quantity that filled.
        qty: Decimal,
        /// The (slipped) fill price.
        price: Decimal,
        /// Why the position closed, when this fill closed one.
        exit_reason: Option<ExitReason>,
        /// The fill's engine instant when the event carries one (w3-era
        /// events do not), else `at`.
        fill_time: String,
    },
}

/// One closed trade with its money figures — the replayed trade's own fields
/// (flattened on the wire) plus the read's derivation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperPnlTrade {
    /// The replayed trade, as the log pairs it — every existing field kept.
    #[serde(flatten)]
    pub trade: PaperClosedTrade,
    /// The trade's net USDT: `realized_pnl(entry, exit, qty, dir)` + its
    /// funding − its two fees at the session's rate. `None` when the log
    /// recorded no entry fill (the figure is then not derivable; the counts
    /// say so).
    pub realized_usdt: Option<Decimal>,
    /// The trade's funding: Σ the `Funding` events of the steps the position
    /// was open *before* (the money rules' trade-funding window).
    pub funding_usdt: Decimal,
    /// `taker_fee(qty × entry_price)` at the session's rate; `None` without an
    /// entry fill.
    pub entry_fee_usdt: Option<Decimal>,
    /// `taker_fee(qty × exit_price)` at the session's rate.
    pub exit_fee_usdt: Decimal,
}

/// The open position marked at the last processed primary bar, with its costs
/// so far and — when a running engine answered — its live stops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperPnlOpenPosition {
    /// The replayed position, as the log pairs it — every existing field kept.
    #[serde(flatten)]
    pub position: PaperPosition,
    /// The mark price: the last processed primary bar's close. `None` before
    /// any bar was processed (a hand-seeded log; the runtime records the bar
    /// with every fill it writes).
    pub mark_price: Option<Decimal>,
    /// The mark instant (RFC3339, from the bar's close time).
    pub mark_time: Option<String>,
    /// The position's funding so far.
    pub funding_usdt: Decimal,
    /// `taker_fee(qty × entry_price)` at the session's rate.
    pub entry_fee_usdt: Decimal,
    /// The unrealized P&L in USDT, net of the costs paid so far (grill Q1):
    /// `realized_pnl(entry, mark, qty, dir)` + funding so far − the entry fee.
    /// The exit fee and exit slippage are not charged until the exit. `None`
    /// without a mark.
    pub unrealized_usdt: Option<Decimal>,
    /// The unrealized R at the mark: the engine's
    /// `realized_r(entry, mark, initial_stop, dir)`. `None` without a mark or
    /// without the live initial stop.
    pub unrealized_r: Option<Decimal>,
    /// The live initial stop (the R denominator), when a running engine
    /// answered the read.
    pub initial_stop: Option<Decimal>,
    /// The live current (possibly trailed) stop.
    pub stop_price: Option<Decimal>,
    /// The live take-profit, when one is declared.
    pub take_profit_price: Option<Decimal>,
}

/// One session's money view — what the summary reads its totals from and the
/// trades read its entries and per-trade figures from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperPnl {
    /// Every `Order` and `Fill` event, in log order.
    pub entries: Vec<PaperPnlEntry>,
    /// The closed trades with their money figures, in log order (the same
    /// order and length as the replayed state's `closed_trades`).
    pub closed_trades: Vec<PaperPnlTrade>,
    /// The open position, when one stands.
    pub open_position: Option<PaperPnlOpenPosition>,
    /// Σ realized USDT over the trades that carry one (0 when none do — the
    /// count below says how many did).
    pub realized_usdt_total: Decimal,
    /// How many closed trades carry a USDT figure ("n of m", m =
    /// `closed_trade_count`).
    pub realized_usdt_count: u64,
    /// Σ realized R over the trades that carry one (0 when none do).
    pub realized_r_total: Decimal,
    /// How many closed trades carry a realized R.
    pub realized_r_count: u64,
    /// How many closed trades the log holds.
    pub closed_trade_count: u64,
    /// The open position's unrealized USDT — `None` when no position stands
    /// (and in the unmarkable hand-seeded case: a position but no bar).
    pub unrealized_usdt: Option<Decimal>,
    /// The session's running total: `realized_usdt_total` + the unrealized
    /// figure when a position stands, else the realized total alone. Always
    /// present.
    pub session_pnl_usdt: Decimal,
}

/// A sum with the count of the terms that carried one.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SumCount {
    /// Σ over the terms that exist (0 when none does).
    total: Decimal,
    /// How many terms the sum counted.
    count: u64,
}

/// One pass over the log: the entries in order, and the funding attribution by
/// step. Position ids are minted at entry fills and closed ids are indexed
/// into the replay's `closed_trades`, so a `Funding` event credits exactly the
/// position that was open before its step — the still-open one
/// ([`Self::open_funding`]) or the trade its exit fill closed
/// ([`Self::trade_funding`]).
struct LogWalk {
    /// Every order and fill, in log order.
    entries: Vec<PaperPnlEntry>,
    /// Per closed trade (aligned with the replay's `closed_trades`).
    trade_funding: Vec<Decimal>,
    /// The still-open position's funding so far (0 when flat).
    open_funding: Decimal,
}

/// The engine direction for a paper side.
fn direction_of(side: PaperSide) -> Direction {
    match side {
        PaperSide::Long => Direction::Long,
        PaperSide::Short => Direction::Short,
    }
}

/// One fill's fee at `fee_bps`.
fn fill_fee(qty: Decimal, price: Decimal, fee_bps: Decimal) -> Decimal {
    taker_fee(qty * price, fee_bps)
}

/// An instant in milliseconds, rendered the way replay renders fill times.
fn rfc3339_millis(ms: i64) -> Option<String> {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// The fill's engine instant: the event's `fill_time_ms` when it carries one
/// (w3-era events do not), else its row instant — the replay's own rule.
fn fill_time(at: &str, fill_time_ms: Option<i64>) -> String {
    fill_time_ms
        .and_then(rfc3339_millis)
        .unwrap_or_else(|| at.to_owned())
}

impl LogWalk {
    /// Walk `events`, attributing each `Funding` payment to the position open
    /// before its step.
    fn of(events: &[PaperEvent], closed_trades: usize) -> Self {
        let mut walk = Self {
            entries: Vec::new(),
            trade_funding: vec![Decimal::ZERO; closed_trades],
            open_funding: Decimal::ZERO,
        };
        let mut next_id = 0_usize;
        let mut current: Option<usize> = None;
        let mut before: Option<usize> = None;
        let mut closed_by_id: Vec<Option<usize>> = Vec::new();
        let mut closed_index = 0_usize;
        for event in events {
            match event {
                PaperEvent::BarProcessed { .. } => before = current,
                PaperEvent::Order { at, side, qty, .. } => {
                    walk.entries.push(PaperPnlEntry::Order {
                        at: at.clone(),
                        side: *side,
                        qty: *qty,
                    });
                }
                PaperEvent::Fill {
                    at,
                    side,
                    qty,
                    price,
                    exit_reason: None,
                    fill_time_ms,
                    ..
                } => {
                    current = Some(next_id);
                    closed_by_id.push(None);
                    next_id += 1;
                    walk.open_funding = Decimal::ZERO;
                    walk.entries.push(PaperPnlEntry::Fill {
                        at: at.clone(),
                        side: *side,
                        qty: *qty,
                        price: *price,
                        exit_reason: None,
                        fill_time: fill_time(at, *fill_time_ms),
                    });
                }
                PaperEvent::Fill {
                    at,
                    side,
                    qty,
                    price,
                    exit_reason: Some(reason),
                    fill_time_ms,
                    ..
                } => {
                    if let Some(id) = current.take() {
                        if let Some(index) = closed_by_id.get_mut(id) {
                            *index = Some(closed_index);
                        }
                        if let Some(slot) = walk.trade_funding.get_mut(closed_index) {
                            *slot += walk.open_funding;
                        }
                        walk.open_funding = Decimal::ZERO;
                    }
                    closed_index += 1;
                    walk.entries.push(PaperPnlEntry::Fill {
                        at: at.clone(),
                        side: *side,
                        qty: *qty,
                        price: *price,
                        exit_reason: Some(*reason),
                        fill_time: fill_time(at, *fill_time_ms),
                    });
                }
                PaperEvent::Funding { amount, .. } => match before {
                    // Still open before and after the step: the payment joins
                    // the position's funding so far.
                    Some(id) if current == Some(id) => walk.open_funding += *amount,
                    // It closed in this step: the payment joins that trade's
                    // funding (the runtime writes `Funding` after the fills).
                    Some(id) => {
                        if let Some(Some(index)) = closed_by_id.get(id)
                            && let Some(slot) = walk.trade_funding.get_mut(*index)
                        {
                            *slot += *amount;
                        }
                    }
                    // No position was open before the step: nothing to credit
                    // (the runtime never writes this shape).
                    None => {}
                },
                // `Stop`, `DataEvent`, `EngineUpgraded` and `ShadowChecked`
                // carry no money and no entry.
                _ => {}
            }
        }
        walk
    }
}

/// The closed trades with their money figures, in the replay's order.
fn derive_trades(
    state: &PaperSessionState,
    walk: &LogWalk,
    fee_bps: Decimal,
) -> Vec<PaperPnlTrade> {
    state
        .closed_trades
        .iter()
        .enumerate()
        .map(|(index, trade)| {
            let direction = direction_of(trade.side);
            let exit_fee = fill_fee(trade.qty, trade.exit_price, fee_bps);
            let entry_fee = trade
                .entry_price
                .map(|entry| fill_fee(trade.qty, entry, fee_bps));
            let funding = walk.trade_funding.get(index).copied().unwrap_or_default();
            let realized_usdt = trade.entry_price.map(|entry| {
                realized_pnl(entry, trade.exit_price, trade.qty, direction) + funding
                    - entry_fee.unwrap_or_default()
                    - exit_fee
            });
            PaperPnlTrade {
                trade: trade.clone(),
                realized_usdt,
                funding_usdt: funding,
                entry_fee_usdt: entry_fee,
                exit_fee_usdt: exit_fee,
            }
        })
        .collect()
}

/// The open position's projection, when one stands.
fn derive_open_position(
    position: Option<&PaperPosition>,
    walk: &LogWalk,
    mark: Option<MarkBar>,
    stops: Option<OpenPositionStops>,
    fee_bps: Decimal,
) -> Option<PaperPnlOpenPosition> {
    let position = position?;
    let direction = direction_of(position.side);
    let entry_fee = fill_fee(position.qty, position.entry_price, fee_bps);
    let unrealized_usdt = mark.map(|mark| {
        realized_pnl(position.entry_price, mark.price, position.qty, direction) + walk.open_funding
            - entry_fee
    });
    let unrealized_r = mark.and_then(|mark| {
        let stops = stops?;
        realized_r(
            position.entry_price,
            mark.price,
            stops.initial_stop,
            direction,
        )
        .ok()
    });
    Some(PaperPnlOpenPosition {
        position: position.clone(),
        mark_price: mark.map(|mark| mark.price),
        mark_time: mark.and_then(|mark| rfc3339_millis(mark.close_time_ms)),
        funding_usdt: walk.open_funding,
        entry_fee_usdt: entry_fee,
        unrealized_usdt,
        unrealized_r,
        initial_stop: stops.map(|stops| stops.initial_stop),
        stop_price: stops.map(|stops| stops.stop_price),
        take_profit_price: stops.and_then(|stops| stops.take_profit_price),
    })
}

/// Σ and count of the trades' realized USDT figures.
fn usdt_totals(trades: &[PaperPnlTrade]) -> SumCount {
    sum_count(trades.iter().map(|trade| trade.realized_usdt))
}

/// Σ and count of the trades' realized R figures.
fn r_totals(trades: &[PaperPnlTrade]) -> SumCount {
    sum_count(trades.iter().map(|trade| trade.trade.realized_r))
}

/// Σ the `Some` values, with the count of them.
fn sum_count(values: impl Iterator<Item = Option<Decimal>>) -> SumCount {
    let mut total = Decimal::ZERO;
    let mut count = 0_u64;
    for value in values.flatten() {
        total += value;
        count += 1;
    }
    SumCount { total, count }
}

impl PaperPnl {
    /// Derive one session's money view from its log (spec §3).
    ///
    /// `mark` is the last processed primary bar (the read's one-row
    /// `paper_bar` read); `stops` the live stop geometry a running engine
    /// answered with. Both only shape the open position's figures.
    ///
    /// # Errors
    ///
    /// The first [`ReplayError`] [`PaperSessionState::replay`] refuses — the
    /// read's existing refusal, never a new one.
    pub fn derive(
        session: &PaperSession,
        events: &[PaperEvent],
        mark: Option<MarkBar>,
        stops: Option<OpenPositionStops>,
    ) -> Result<Self, ReplayError> {
        let state = PaperSessionState::replay(session, events)?;
        let walk = LogWalk::of(events, state.closed_trades.len());
        let trades = derive_trades(&state, &walk, session.taker_fee_bps);
        let usdt = usdt_totals(&trades);
        let r = r_totals(&trades);
        let open_position = derive_open_position(
            state.open_position.as_ref(),
            &walk,
            mark,
            stops,
            session.taker_fee_bps,
        );
        let unrealized_usdt = open_position
            .as_ref()
            .and_then(|position| position.unrealized_usdt);
        Ok(Self {
            entries: walk.entries,
            closed_trades: trades,
            open_position,
            realized_usdt_total: usdt.total,
            realized_usdt_count: usdt.count,
            realized_r_total: r.total,
            realized_r_count: r.count,
            closed_trade_count: u64::try_from(state.closed_trades.len()).unwrap_or(u64::MAX),
            unrealized_usdt,
            session_pnl_usdt: usdt.total + unrealized_usdt.unwrap_or_default(),
        })
    }
}
