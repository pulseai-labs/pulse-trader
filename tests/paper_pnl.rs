//! r4.s4.w1 AC-1 — the paper P&L read model (`tests/paper_pnl.rs`).
//!
//! The read side derives every USDT figure from the log at the session's
//! `taker_fee_bps` (ADR-0027 addendum). This suite drives the REAL engine over
//! the certify fixture's M15 series — the runtime's own step shape:
//! `events_for_step` per bar, `PaperSessionState::replay` for the pairing —
//! and demands, with exact `Decimal` equality and no tolerance, that each
//! derived closed-trade USDT equals the engine's `Trade.realized_pnl`.
//!
//! Covered: a long and a short run over the golden window; a trade that closes
//! on a funding bar (its step's `Funding` event accrues to the trade that
//! closed in that same step); a trade that opens and closes in ONE step
//! (crafted bars: the entry fills at a bar's open and its 0.5% stop is hit
//! intra-bar); a log whose first closed trade has no entry fill (USDT `None`,
//! counted 1 of 2); the open position marked at the last processed bar with
//! its money rule and the engine's live stops; and the running totals.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    BacktestConfig, BarRef, BinanceAdapter, Candle, Direction, EngineSession, ExchangeAdapter,
    Graduation, MarkBar, Migrator, NonEmptyLabel, NonEmptyReason, OpenPositionMark,
    OpenPositionStops, Pair, PaperEvent, PaperPnl, PaperPnlEntry, PaperSession, PaperSessionId,
    PaperSessionState, PaperSide, StepView, StrategyDsl, Timeframe, Trade, VersionId, compile,
    events_for_step, fixture_m15_candles, fixture_strategy_dsl, realized_pnl, realized_r,
    taker_fee, validate,
};
use rust_decimal::Decimal;

/// The golden window's length: the same 96 bars (24 hours of M15)
/// `tests/paper_replay_golden.rs` records, whose ninth trade closes on the
/// 16:00 funding bar.
const WINDOW_LEN: usize = 96;

/// How many closed trades the long window produces (the golden's pinned
/// count).
const WINDOW_TRADES: usize = 9;

/// The window trade that closes on a funding bar, in log order.
const FUNDING_TRADE: usize = 5;

/// The prefix length that ends with [`FUNDING_TRADE`] still open (bar 60 =
/// 15:00; the trade entered at 14:00).
const OPEN_PREFIX_LEN: usize = 61;

/// The fixture session's taker fee, bps (A1's default).
const TAKER_FEE_BPS: i64 = 4;

/// One M15 bar in milliseconds.
const M15_MS: i64 = 900_000;

/// The fixture series' first bar open — 8-hour aligned, so the funding
/// cadence starts on it.
const START_MS: i64 = 1_735_689_600_000;

// ---------------------------------------------------------------------------
// The session and the engine drive
// ---------------------------------------------------------------------------

/// A session row the fixture log replays under. Its fingerprint never has to
/// match the running build's (the log carries no `engine_upgraded`).
fn session() -> PaperSession {
    PaperSession {
        id: PaperSessionId::new("sess-pnl".to_owned()),
        seq: 1,
        strategy_version_id: VersionId::new("ver-pnl"),
        created_at: "2025-01-01T00:00:00.000Z".to_owned(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        uses_d1: false,
        starting_equity: Decimal::from(10_000),
        taker_fee_bps: Decimal::from(TAKER_FEE_BPS),
        slippage_bps: Decimal::from(1),
        engine_fingerprint: pulse::EngineFingerprint::from_stored(
            "1111111111111111111111111111111111111111111111111111111111111111".to_owned(),
        ),
        graduation: Graduation::Override {
            reason: NonEmptyReason::try_new("paper pnl suite").unwrap(),
            at: "2025-01-01T00:00:00.000Z".to_owned(),
        },
        fixture: false,
        min_trades: 1,
        promoted_by: NonEmptyLabel::try_new("operator-token").unwrap(),
    }
}

/// An instant, rendered the way the runtime stamps events.
fn millis_text(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// Stamp an event with the next per-session sequence.
fn stamped(event: PaperEvent, seq: i64) -> PaperEvent {
    match event {
        PaperEvent::BarProcessed { at, bars, .. } => PaperEvent::BarProcessed { seq, at, bars },
        PaperEvent::Order { at, side, qty, .. } => PaperEvent::Order { seq, at, side, qty },
        PaperEvent::Fill {
            at,
            side,
            qty,
            price,
            exit_reason,
            realized_r,
            fill_time_ms,
            ..
        } => PaperEvent::Fill {
            seq,
            at,
            side,
            qty,
            price,
            exit_reason,
            realized_r,
            fill_time_ms,
        },
        PaperEvent::Funding {
            at, rate, amount, ..
        } => PaperEvent::Funding {
            seq,
            at,
            rate,
            amount,
        },
        other => other,
    }
}

/// What one driven window produced: the log the runtime would have written,
/// and the engine that produced it (its trades and live stops).
struct Driven {
    events: Vec<PaperEvent>,
    engine: EngineSession,
}

/// Drive `window` through the real engine, one primary bar at a time — the
/// runtime's own step shape (`tests/paper_replay_golden.rs`'s `replay`).
fn drive(window: &[Candle], dsl: &StrategyDsl) -> Driven {
    let pair = Pair::new("BTCUSDT");
    let json = serde_json::to_string(dsl).unwrap();
    let loaded = Migrator::v1().load(&json).unwrap();
    let validated = validate(&loaded.dsl).unwrap();
    let compiled = compile(&validated).unwrap();
    let mut engine = EngineSession::new(
        &compiled,
        &pair,
        pulse::SessionTimeframes {
            primary: Timeframe::M15,
            htf: None,
            d1: None,
        },
        BacktestConfig::default(),
        BinanceAdapter::new().symbol_filters(&pair).unwrap(),
        Some(window[0].open_time),
    )
    .expect("the engine builds over the fixture strategy");
    let mut events: Vec<PaperEvent> = Vec::new();
    let mut previous: Option<&Candle> = None;
    let mut seq: i64 = 0;
    for bar in window {
        let before_len = engine.closed_trades().len();
        let before_open = previous.and_then(|last| engine.open_position_mark(last));
        engine
            .step(bar, &[], &[])
            .expect("the fixture window steps");
        let before = StepView {
            closed_trades: &engine.closed_trades()[..before_len],
            open_position: before_open,
        };
        let after = StepView {
            closed_trades: engine.closed_trades(),
            open_position: engine.open_position_mark(bar),
        };
        let at = millis_text(bar.close_time);
        seq += 1;
        events.push(PaperEvent::BarProcessed {
            seq,
            at: at.clone(),
            bars: vec![BarRef {
                timeframe: Timeframe::M15,
                open_time: bar.open_time,
            }],
        });
        for event in events_for_step(&before, &after, bar, &at) {
            seq += 1;
            events.push(stamped(event, seq));
        }
        previous = Some(bar);
    }
    Driven { events, engine }
}

/// The fixture window every driven case shares.
fn window() -> Vec<Candle> {
    fixture_m15_candles()[..WINDOW_LEN].to_vec()
}

/// Derive the fixture window's long-run money view against the engine's own
/// trades.
fn derived_long() -> (Driven, PaperPnl) {
    let driven = drive(&window(), &fixture_strategy_dsl());
    let pnl = PaperPnl::derive(&session(), &driven.events, None, None).unwrap();
    (driven, pnl)
}

/// A minimal M15 bar (the fixture grid's shape), 20-point wicks, a funding
/// stamp on the 8-hour boundaries, mimicking the fixture generator.
fn craft_bar(open_time: i64, open: i64, close: i64) -> Candle {
    Candle {
        open_time,
        close_time: open_time + M15_MS - 1,
        open: Decimal::from(open),
        high: Decimal::from(open.max(close) + 20),
        low: Decimal::from(open.min(close) - 20),
        close: Decimal::from(close),
        volume: Decimal::from(100),
        funding_rate: (open_time % 28_800_000 == 0).then(|| Decimal::new(1, 5)),
    }
}

/// The counts of exit fills before `index` — how many trades closed by then.
fn exits_before(events: &[PaperEvent], index: usize) -> usize {
    events[..index]
        .iter()
        .filter(|event| {
            matches!(
                event,
                PaperEvent::Fill {
                    exit_reason: Some(_),
                    ..
                }
            )
        })
        .count()
}

/// The funding figures the log attributes to the trade at `index`.
fn funding_of(pnl: &PaperPnl, index: usize) -> Decimal {
    pnl.closed_trades[index].funding_usdt
}

// ---------------------------------------------------------------------------
// The long and short runs
// ---------------------------------------------------------------------------

/// Every closed trade's derived USDT equals the engine's `realized_pnl`
/// exactly, its funding equals the engine's `funding_total`, and the entries
/// carry the log's orders and fills in order (the entry fills included, #344).
#[test]
fn the_long_run_derives_each_trades_usdt_from_the_log_exactly() {
    let (driven, pnl) = derived_long();
    let engine_trades: Vec<Trade> = driven.engine.closed_trades().to_vec();
    assert_eq!(pnl.closed_trades.len(), WINDOW_TRADES);
    assert_eq!(engine_trades.len(), WINDOW_TRADES, "the pinned window");
    for (derived, engine_trade) in pnl.closed_trades.iter().zip(&engine_trades) {
        assert_eq!(
            derived.realized_usdt,
            Some(engine_trade.realized_pnl),
            "derived USDT must equal the engine's realized_pnl exactly \
             (entry {} exit {})",
            engine_trade.entry_price,
            engine_trade.exit_price,
        );
        assert_eq!(derived.funding_usdt, engine_trade.funding_total);
        assert_eq!(
            derived.entry_fee_usdt.unwrap_or_default() + derived.exit_fee_usdt,
            engine_trade.fees_total
        );
        assert_eq!(derived.trade.exit_price, engine_trade.exit_price);
    }

    // The entries: one order + one fill per leg, in log order, 4 per trade.
    assert_eq!(pnl.entries.len(), 4 * WINDOW_TRADES);
    assert!(matches!(
        pnl.entries[0],
        PaperPnlEntry::Order {
            side: PaperSide::Long,
            ..
        }
    ));
    assert!(matches!(
        pnl.entries[1],
        PaperPnlEntry::Fill {
            exit_reason: None,
            ..
        }
    ));
    assert!(matches!(
        pnl.entries[3],
        PaperPnlEntry::Fill {
            exit_reason: Some(_),
            ..
        }
    ));

    // The totals sum the trades, and the session is flat at the window's end.
    let expected: Decimal = engine_trades.iter().map(|trade| trade.realized_pnl).sum();
    assert_eq!(pnl.realized_usdt_total, expected);
    assert_eq!(
        pnl.realized_usdt_count,
        u64::try_from(WINDOW_TRADES).unwrap()
    );
    assert_eq!(
        pnl.closed_trade_count,
        u64::try_from(WINDOW_TRADES).unwrap()
    );
    let expected_r: Decimal = engine_trades.iter().map(|trade| trade.realized_r).sum();
    assert_eq!(pnl.realized_r_total, expected_r);
    assert_eq!(pnl.realized_r_count, u64::try_from(WINDOW_TRADES).unwrap());
    assert_eq!(pnl.unrealized_usdt, None, "flat at the window's end");
    assert_eq!(pnl.session_pnl_usdt, expected);
}

/// The short direction derives the same exact equality over the same window.
#[test]
fn the_short_run_derives_each_trades_usdt_from_the_log_exactly() {
    let mut dsl = fixture_strategy_dsl();
    dsl.direction = Direction::Short;
    let window = window();
    let driven = drive(&window, &dsl);
    let engineered = driven.engine.closed_trades().to_vec();
    assert!(
        engineered.len() >= 2,
        "the short run trades on the fixture series: {}",
        engineered.len()
    );
    let pnl = PaperPnl::derive(&session(), &driven.events, None, None).unwrap();
    assert_eq!(pnl.closed_trades.len(), engineered.len());
    for (derived, engine_trade) in pnl.closed_trades.iter().zip(&engineered) {
        assert_eq!(
            derived.realized_usdt,
            Some(engine_trade.realized_pnl),
            "short trade {} → {}",
            engine_trade.entry_price,
            engine_trade.exit_price,
        );
        assert_eq!(derived.funding_usdt, engine_trade.funding_total);
    }
    let expected: Decimal = engineered.iter().map(|trade| trade.realized_pnl).sum();
    assert_eq!(pnl.realized_usdt_total, expected);
    assert_eq!(
        pnl.realized_usdt_count,
        u64::try_from(engineered.len()).unwrap()
    );
}

// ---------------------------------------------------------------------------
// A trade that closes on a funding bar
// ---------------------------------------------------------------------------

/// The window's funding event rides the step of the bar that closes the trade:
/// the trade's derived funding equals the engine's `funding_total` for it, and
/// lands on no other trade.
#[test]
fn the_funding_of_the_step_accrues_to_the_trade_closing_on_that_bar() {
    let (driven, pnl) = derived_long();
    let engine_trades: Vec<Trade> = driven.engine.closed_trades().to_vec();

    let funding_index = driven
        .events
        .iter()
        .position(|event| matches!(event, PaperEvent::Funding { .. }))
        .expect("the window carries its one funding stamp");
    // The Funding event is the last event of the step, right after the exit
    // fill it accrues to ([`events_for_step`]'s order).
    assert!(matches!(
        driven.events[funding_index - 1],
        PaperEvent::Fill {
            exit_reason: Some(_),
            ..
        }
    ));
    assert_eq!(
        exits_before(&driven.events, funding_index),
        FUNDING_TRADE + 1,
        "the funding step closes trade {FUNDING_TRADE}"
    );
    assert_ne!(funding_of(&pnl, FUNDING_TRADE), Decimal::ZERO);
    assert_eq!(
        funding_of(&pnl, FUNDING_TRADE),
        engine_trades[FUNDING_TRADE].funding_total
    );
    let funded = pnl
        .closed_trades
        .iter()
        .filter(|trade| trade.funding_usdt != Decimal::ZERO)
        .count();
    assert_eq!(funded, 1, "exactly the closing trade carries the payment");
}

// ---------------------------------------------------------------------------
// A trade that opens and closes in one step
// ---------------------------------------------------------------------------

/// Bar 1's close signals the fixture entry; bar 2 fills it at its open and
/// takes the 0.5% stop out intra-bar in the SAME step — entry events before
/// exit events, one closed trade, and the derived figure still exact.
#[test]
fn a_trade_opening_and_closing_in_one_step_is_derived_exactly() {
    let window = vec![
        craft_bar(START_MS, 60_000, 60_000),
        craft_bar(START_MS + M15_MS, 60_000, 59_700),
        craft_bar(START_MS + 2 * M15_MS, 59_700, 59_000),
    ];
    let driven = drive(&window, &fixture_strategy_dsl());
    let engine_trade = driven.engine.closed_trades().first().expect("one trade");
    assert_eq!(
        driven.engine.closed_trades().len(),
        1,
        "one trade opens and closes in this step"
    );
    assert_eq!(
        engine_trade.entry_fill_time, engine_trade.exit_fill_time,
        "the entry and the stop-out filled on the same bar"
    );

    // The step's own event order: BarProcessed, entry Order, entry Fill, exit
    // Order, exit Fill — nothing interleaves.
    let step_open = window.last().unwrap().open_time;
    let start = driven
        .events
        .iter()
        .position(|event| {
            matches!(event, PaperEvent::BarProcessed { bars, .. }
            if bars.first().is_some_and(|bar| bar.open_time == step_open))
        })
        .expect("the step's bar_processed");
    let after: Vec<&PaperEvent> = driven.events[start + 1..start + 5].iter().collect();
    assert!(matches!(after[0], PaperEvent::Order { .. }));
    assert!(matches!(
        after[1],
        PaperEvent::Fill {
            exit_reason: None,
            ..
        }
    ));
    assert!(matches!(after[2], PaperEvent::Order { .. }));
    assert!(matches!(
        after[3],
        PaperEvent::Fill {
            exit_reason: Some(_),
            ..
        }
    ));

    let pnl = PaperPnl::derive(&session(), &driven.events, None, None).unwrap();
    assert_eq!(pnl.closed_trades.len(), 1);
    assert_eq!(
        pnl.closed_trades[0].realized_usdt,
        Some(engine_trade.realized_pnl)
    );
    assert_eq!(pnl.closed_trades[0].funding_usdt, Decimal::ZERO);
}

// ---------------------------------------------------------------------------
// A log whose first closed trade has no entry fill
// ---------------------------------------------------------------------------

/// Dropping the first entry fill leaves the trade without a USDT figure
/// (`None`) while the totals count what they have: 1 of 2.
#[test]
fn a_trade_without_an_entry_fill_has_no_usdt_and_the_totals_count_it() {
    let driven = drive(&window(), &fixture_strategy_dsl());
    // The log through the SECOND trade's exit fill.
    let second_exit = driven
        .events
        .iter()
        .enumerate()
        .filter(|(_, event)| {
            matches!(
                event,
                PaperEvent::Fill {
                    exit_reason: Some(_),
                    ..
                }
            )
        })
        .nth(1)
        .map(|(index, _)| index)
        .expect("two closed trades in the window");
    let mut log: Vec<PaperEvent> = driven.events[..=second_exit].to_vec();
    // Drop the FIRST entry fill (the trade's entry, leaving its signal order).
    let first_entry = log
        .iter()
        .position(|event| {
            matches!(
                event,
                PaperEvent::Fill {
                    exit_reason: None,
                    ..
                }
            )
        })
        .expect("the first entry fill");
    log.remove(first_entry);
    let mut seq = 0;
    let log: Vec<PaperEvent> = log
        .into_iter()
        .map(|event| {
            seq += 1;
            stamped(event, seq)
        })
        .collect();

    let state = PaperSessionState::replay(&session(), &log).unwrap();
    assert_eq!(state.closed_trades.len(), 2);
    assert_eq!(state.closed_trades[0].entry_price, None, "no entry fill");
    assert!(state.closed_trades[1].entry_price.is_some());

    let pnl = PaperPnl::derive(&session(), &log, None, None).unwrap();
    assert_eq!(pnl.closed_trades[0].realized_usdt, None);
    assert_eq!(pnl.closed_trades[0].entry_fee_usdt, None);
    assert!(pnl.closed_trades[1].realized_usdt.is_some());
    assert_eq!(pnl.realized_usdt_count, 1, "counted 1 of 2");
    assert_eq!(pnl.closed_trade_count, 2);
    assert_eq!(pnl.realized_r_count, 2, "both exit fills recorded R");
    assert_eq!(
        pnl.realized_usdt_total,
        pnl.closed_trades[1].realized_usdt.unwrap()
    );
    assert_eq!(pnl.session_pnl_usdt, pnl.realized_usdt_total);
}

// ---------------------------------------------------------------------------
// The marked open position
// ---------------------------------------------------------------------------

/// The open position is marked at the last processed bar, its unrealized USDT
/// is the money rule at that mark (gross + funding so far − the entry fee),
/// its R uses the engine's live initial stop, and the totals sum it in.
#[test]
fn the_open_position_is_marked_at_the_last_bar_and_the_totals_sum_it() {
    let window = fixture_m15_candles()[..OPEN_PREFIX_LEN].to_vec();
    let driven = drive(&window, &fixture_strategy_dsl());
    let last = window.last().unwrap();
    let stops: Option<OpenPositionStops> = driven.engine.open_position_stops();
    let stops = stops.expect("the prefix ends with an open position");
    let mark = MarkBar {
        price: last.close,
        close_time_ms: last.close_time,
    };
    let pnl = PaperPnl::derive(&session(), &driven.events, Some(mark), Some(stops)).unwrap();
    let position = pnl.open_position.as_ref().expect("a position stands");

    let engine_mark: OpenPositionMark = driven
        .engine
        .open_position_mark(last)
        .expect("the engine holds the position");
    assert_eq!(position.mark_price, Some(engine_mark.mark_price));
    assert_eq!(
        position.mark_time.as_deref(),
        Some(millis_text(last.close_time).as_str())
    );
    assert_eq!(
        position.position.entry_price, engine_mark.entry_price,
        "the replayed entry matches the engine's"
    );
    assert_eq!(
        position.funding_usdt,
        Decimal::ZERO,
        "no stamp before 16:00"
    );

    // The money rule at the mark, computed from the engine's own position.
    let expected_usdt = realized_pnl(
        engine_mark.entry_price,
        engine_mark.mark_price,
        engine_mark.qty,
        engine_mark.direction,
    ) + position.funding_usdt
        - taker_fee(
            engine_mark.qty * engine_mark.entry_price,
            Decimal::from(TAKER_FEE_BPS),
        );
    assert_eq!(
        position.entry_fee_usdt,
        taker_fee(
            engine_mark.qty * engine_mark.entry_price,
            Decimal::from(TAKER_FEE_BPS),
        )
    );
    assert_eq!(position.unrealized_usdt, Some(expected_usdt));
    assert_eq!(
        position.unrealized_r,
        realized_r(
            engine_mark.entry_price,
            engine_mark.mark_price,
            stops.initial_stop,
            engine_mark.direction,
        )
        .ok()
    );
    assert_eq!(position.initial_stop, Some(stops.initial_stop));
    assert_eq!(position.stop_price, Some(stops.stop_price));
    assert_eq!(position.take_profit_price, stops.take_profit_price);

    // The totals: realized sum + this unrealized figure.
    let expected_total: Decimal = pnl
        .closed_trades
        .iter()
        .filter_map(|trade| trade.realized_usdt)
        .sum();
    assert_eq!(pnl.realized_usdt_total, expected_total);
    assert_eq!(pnl.realized_usdt_count, pnl.closed_trade_count);
    assert_eq!(pnl.unrealized_usdt, Some(expected_usdt));
    assert_eq!(pnl.session_pnl_usdt, expected_total + expected_usdt);

    // Without live stops the USDT figures stay; the R is absent.
    let without_stops = PaperPnl::derive(&session(), &driven.events, Some(mark), None).unwrap();
    let position = without_stops.open_position.as_ref().expect("still marked");
    assert_eq!(position.unrealized_usdt, Some(expected_usdt));
    assert_eq!(position.unrealized_r, None);
    assert_eq!(position.initial_stop, None);
    assert_eq!(position.stop_price, None);
    assert_eq!(position.take_profit_price, None);
}
