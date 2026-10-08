//! r4.s2.w2 AC-2 (spec §5) — the paper replay golden (`tests/paper_replay_golden.rs`).
//!
//! The Mac engine must replay what the Linux engine recorded. The committed
//! fixture under `tests/fixtures/paper-replay/` IS that recording: a window of
//! the certify fixture's synthetic BTCUSDT M15 series (`fixture_m15_candles()`,
//! `src/application/fixture.rs`) plus the event log a real engine replay over it
//! produced on draco-desk (Linux).
//!
//! This suite re-runs the replay — `EngineSession` stepped bar by bar with
//! `events_for_step`, the runtime's own path — and demands the SAME event
//! stream, then replays the committed log and pins the final state: the closed
//! trades, the open position, the funding total and a SHA-256 over them, all
//! constants computed on Linux. A darwin difference in the engine's decisions (a
//! fill price, an R, a funding amount) fails here and in CI's `macos-latest`
//! `check` job (`cargo nextest run`), which is the point: a new engine
//! fingerprint replays every session, and a replay that differs holds it.
//!
//! `regenerate_fixture` is `#[ignore]`d and is the ONLY thing that may rewrite
//! the two JSON files: regeneration is a deliberate act on a Linux host, never
//! something a test run does quietly. It searches the fixture series for the
//! smallest window that covers a closed position and a funding bar and prints
//! the constants to pin.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

use pulse::{
    BacktestConfig, BinanceAdapter, Candle, CompiledStrategy, Direction, EngineFingerprint,
    EngineSession, ExchangeAdapter, Graduation, Migrator, NonEmptyLabel, NonEmptyReason,
    OpenPositionMark, Pair, PaperClosedTrade, PaperEvent, PaperPosition, PaperSession,
    PaperSessionId, PaperSessionState, PaperSide, SessionTimeframes, StepView, StrategyDsl,
    Timeframe, Trade, VersionId, compile, events_for_step, fixture_m15_candles,
    fixture_strategy_dsl, validate,
};
use rust_decimal::Decimal;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// The pinned fixture contract (computed on draco-desk, Linux)
// ---------------------------------------------------------------------------

/// The window's first bar index in `fixture_m15_candles()` — a multiple of 32,
/// so the window starts on an 8-hour funding boundary.
const WINDOW_START: usize = 0;

/// How many bars the committed window covers (96 = 24 hours of M15 bars).
const WINDOW_LEN: usize = 96;

/// The pinned closed trades, one line each:
/// `side qty entry_price exit_price entry_fill exit_fill exit_reason realized_r`.
const PINNED_TRADES: &[&str] = &[
    "long 0.334 59814.9809 59509.95440490045 2025-01-01T01:00:00.000Z 2025-01-01T01:15:00.000Z stoploss -1.0199",
    "long 0.335 59537.9532 60187.9806 2025-01-01T01:30:00.000Z 2025-01-01T02:15:00.000Z signal 2.1835732169576833890890290128",
    "long 0.334 59737.9732 60012.9981 2025-01-01T02:45:00.000Z 2025-01-01T03:00:00.000Z signal 0.9207707770038639342387330945",
    "long 0.334 59746.9741 60044.9949 2025-01-01T04:00:00.000Z 2025-01-01T05:00:00.000Z signal 0.9976096848057783063527563649",
    "long 0.335 59636.9631 59977.0017 2025-01-01T11:45:00.000Z 2025-01-01T12:15:00.000Z signal 1.140361890761670927539232795",
    "long 0.334 59806.9801 60014.9979 2025-01-01T14:00:00.000Z 2025-01-01T16:00:00.000Z signal 0.6956305088542666610916206418",
    "long 0.334 59716.9711 59974.002 2025-01-01T17:30:00.000Z 2025-01-01T19:00:00.000Z signal 0.8608303310279579802733832895",
    "long 0.334 59756.9751 60159.9834 2025-01-01T20:15:00.000Z 2025-01-01T20:45:00.000Z signal 1.3488242981696709075891627587",
    "long 0.334 59740.9735 60048.9945 2025-01-01T22:45:00.000Z 2025-01-01T23:30:00.000Z signal 1.0311884187826299817494604436",
];

/// The pinned open position at the window's end (empty = none, else
/// `side qty entry_price entry_fill`).
const PINNED_OPEN: &str = "";

/// The pinned funding total (Decimal-as-TEXT).
const PINNED_FUNDING_TOTAL: &str = "-0.199755313534";

/// How many funding events the window's log carries (one accrual per stamped
/// bar held across).
const PINNED_FUNDING_EVENTS: usize = 1;

/// The pinned SHA-256 over the replayed final state.
const PINNED_STATE_DIGEST: &str =
    "b931e0142b6e791a68d2d2059bfd229e6b6df0b46beace102f61791a0549ab9a";

/// The session row the fixture was recorded for. No `engine_upgraded` event
/// rides the log, so this fingerprint never has to match the running build's.
const SESSION_FINGERPRINT: &str =
    "1111111111111111111111111111111111111111111111111111111111111111";
const SESSION_PROMOTER: &str = "operator-token";

// ---------------------------------------------------------------------------
// The session and the engine replay
// ---------------------------------------------------------------------------

fn session() -> PaperSession {
    PaperSession {
        id: PaperSessionId::new("sess-replay-golden".to_owned()),
        seq: 1,
        strategy_version_id: VersionId::new("ver-replay-golden"),
        created_at: "2025-01-01T00:00:00.000Z".to_owned(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        uses_d1: false,
        starting_equity: Decimal::from(10_000),
        taker_fee_bps: Decimal::from(4),
        slippage_bps: Decimal::from(1),
        engine_fingerprint: EngineFingerprint::from_stored(SESSION_FINGERPRINT.to_owned()),
        graduation: Graduation::Override {
            reason: NonEmptyReason::try_new("paper replay golden").unwrap(),
            at: "2025-01-01T00:00:00.000Z".to_owned(),
        },
        fixture: false,
        min_trades: 1,
        promoted_by: NonEmptyLabel::try_new(SESSION_PROMOTER).unwrap(),
    }
}

fn compiled_fixture() -> CompiledStrategy {
    let dsl: StrategyDsl = fixture_strategy_dsl();
    let json = serde_json::to_string(&dsl).unwrap();
    let loaded = Migrator::v1().load(&json).unwrap();
    let validated = validate(&loaded.dsl).unwrap();
    compile(&validated).unwrap()
}

/// An instant, rendered the way the runtime stamps events: RFC3339 with
/// milliseconds (UTC).
fn millis_text(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// The bar's poll instant: its own close (a deterministic stand-in for the live
/// clock, and the one the fixture was recorded with).
fn at_of(bar: &Candle) -> String {
    millis_text(bar.close_time)
}

/// What one full engine replay produced.
struct Rebuilt {
    /// The event stream, exactly as the repository would have logged it (seqs
    /// minted `MAX(seq)+1`, one per event, in order).
    events: Vec<PaperEvent>,
    /// The closed trades the engine held at the end.
    closed: Vec<Trade>,
    /// The open position mark at the window's last bar, when one stands.
    open: Option<OpenPositionMark>,
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

/// Replay the window through the real engine — the runtime's own step shape:
/// one primary bar at a time, the log's `bar_processed` first and the step's
/// delta (`events_for_step`) after it.
fn replay(bars: &[Candle]) -> Rebuilt {
    let pair = Pair::new("BTCUSDT");
    let mut engine = EngineSession::new(
        &compiled_fixture(),
        &pair,
        SessionTimeframes {
            primary: Timeframe::M15,
            htf: None,
            d1: None,
        },
        BacktestConfig::default(),
        BinanceAdapter::new().symbol_filters(&pair).unwrap(),
        Some(bars[0].open_time),
    )
    .expect("the engine builds over the fixture strategy");

    let mut events: Vec<PaperEvent> = Vec::new();
    let mut previous: Option<&Candle> = None;
    let mut seq: i64 = 0;
    for bar in bars {
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
        let at = at_of(bar);
        seq += 1;
        events.push(PaperEvent::BarProcessed {
            seq,
            at: at.clone(),
            bars: vec![pulse::BarRef {
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
    let open = engine.open_position_mark(bars.last().expect("a non-empty window"));
    Rebuilt {
        events,
        closed: engine.closed_trades().to_vec(),
        open,
    }
}

// ---------------------------------------------------------------------------
// Canonical renderings (the pinned constants' shape)
// ---------------------------------------------------------------------------

fn side_text(direction: Direction) -> &'static str {
    match direction {
        Direction::Long => "long",
        Direction::Short => "short",
    }
}

fn paper_side_text(side: PaperSide) -> &'static str {
    match side {
        PaperSide::Long => "long",
        PaperSide::Short => "short",
    }
}

/// One engine trade as the pinned line shape.
fn rebuilt_line(trade: &Trade) -> String {
    format!(
        "{} {} {} {} {} {} {} {}",
        side_text(trade.direction),
        trade.qty,
        trade.entry_price,
        trade.exit_price,
        millis_text(trade.entry_fill_time),
        millis_text(trade.exit_fill_time),
        format!("{:?}", trade.exit_reason).to_lowercase(),
        trade.realized_r,
    )
}

/// One replayed trade as the same line shape.
fn replayed_line(trade: &PaperClosedTrade) -> String {
    format!(
        "{} {} {} {} {} {} {} {}",
        paper_side_text(trade.side),
        trade.qty,
        trade
            .entry_price
            .map_or_else(String::new, |price| price.to_string()),
        trade.exit_price,
        trade.entry_fill_time.clone().unwrap_or_default(),
        trade.exit_fill_time,
        format!("{:?}", trade.exit_reason).to_lowercase(),
        trade.realized_r.map_or_else(String::new, |r| r.to_string()),
    )
}

/// The engine's open-position mark as the pinned line shape.
fn rebuilt_open_line(open: &OpenPositionMark) -> String {
    format!(
        "{} {} {} {}",
        side_text(open.direction),
        open.qty,
        open.entry_price,
        millis_text(open.entry_fill_time),
    )
}

/// The replayed open position as the same line shape.
fn replayed_open_line(position: &PaperPosition) -> String {
    format!(
        "{} {} {} {}",
        paper_side_text(position.side),
        position.qty,
        position.entry_price,
        position.entry_fill_time,
    )
}

/// SHA-256 over the replayed final state, canonically.
fn state_digest(state: &PaperSessionState) -> String {
    let payload = serde_json::json!({
        "status": format!("{:?}", state.status),
        "last_bar_open_time": state.last_bar_open_time,
        "closed_trades": state.closed_trades,
        "open_position": state.open_position,
        "funding_total": state.funding_total.to_string(),
        "data_event_count": state.data_event_count,
    });
    hex::encode(Sha256::digest(payload.to_string().as_bytes()))
}

// ---------------------------------------------------------------------------
// The fixture files
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/paper-replay")
        .join(name)
}

fn load_bars() -> Vec<Candle> {
    serde_json::from_str(&fs::read_to_string(fixture("bars.json")).unwrap()).unwrap()
}

fn load_events() -> Vec<PaperEvent> {
    serde_json::from_str(&fs::read_to_string(fixture("events.json")).unwrap()).unwrap()
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// The recorded window is the certify fixture's own series, the engine
/// reproduces the recorded log exactly, and the replayed final state equals the
/// pinned Linux constants.
#[test]
fn the_engine_replays_the_recorded_log_and_the_final_state_is_pinned() {
    let bars = load_bars();
    let events = load_events();
    assert_eq!(bars.len(), WINDOW_LEN, "the window length is pinned");
    let series = fixture_m15_candles();
    assert_eq!(
        bars,
        series[WINDOW_START..WINDOW_START + WINDOW_LEN].to_vec(),
        "the recorded bars ARE the certify fixture's synthetic series at the pinned window"
    );

    // The engine replays what was recorded — the darwin-sensitive half.
    let rebuilt = replay(&bars);
    assert_eq!(
        rebuilt.events, events,
        "the engine reproduces the Linux-recorded log exactly"
    );

    // The committed log replays, and its final state matches the engine's and
    // the pinned constants.
    let state = PaperSessionState::replay(&session(), &events).expect("the committed log replays");
    let replayed_trades: Vec<String> = state.closed_trades.iter().map(replayed_line).collect();
    let rebuilt_trades: Vec<String> = rebuilt.closed.iter().map(rebuilt_line).collect();
    assert_eq!(
        replayed_trades, rebuilt_trades,
        "the log's trades and the engine's rebuilt trades agree (the runtime's hold check)"
    );
    assert_eq!(
        replayed_trades, PINNED_TRADES,
        "the rebuilt trades equal the pinned Linux constants"
    );
    let replayed_open = state
        .open_position
        .as_ref()
        .map_or_else(String::new, replayed_open_line);
    assert_eq!(
        rebuilt
            .open
            .as_ref()
            .map_or_else(String::new, rebuilt_open_line),
        replayed_open,
        "the open position agrees between the engine and the log"
    );
    assert_eq!(replayed_open, PINNED_OPEN, "the open position is pinned");
    assert_eq!(
        state.funding_total.to_string(),
        PINNED_FUNDING_TOTAL,
        "the funding total is pinned (the fixture carries a funding bar)"
    );
    assert_eq!(
        state_digest(&state),
        PINNED_STATE_DIGEST,
        "the replayed final state digest is pinned"
    );

    // What the spec's fixture must cover: at least one opened and one closed
    // position, and a funding bar accrual.
    let entries = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                PaperEvent::Fill {
                    exit_reason: None,
                    ..
                }
            )
        })
        .count();
    let exits = events
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
        .count();
    assert!(entries >= 1, "at least one position opened ({entries})");
    assert!(exits >= 1, "at least one position closed ({exits})");
    assert_eq!(entries, exits, "every opened position in the window closed");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, PaperEvent::Funding { .. }))
            .count(),
        PINNED_FUNDING_EVENTS,
        "the window's funding bar accrual count is pinned"
    );
    assert!(
        !state.closed_trades.is_empty(),
        "the replayed state carries the closed trades"
    );
}

/// Rewrite the committed fixture and print the pinned values — run by hand on
/// Linux: `cargo test --test paper_replay_golden -- --ignored regenerate --nocapture`.
#[test]
#[ignore = "rewrites the committed fixture; a deliberate, manual act"]
fn regenerate_fixture() {
    let series = fixture_m15_candles();
    let (start, len) = choose_window(&series);
    let bars = series[start..start + len].to_vec();
    let rebuilt = replay(&bars);
    let state = PaperSessionState::replay(&session(), &rebuilt.events).expect("the log replays");

    fs::create_dir_all(
        fixture("bars.json")
            .parent()
            .expect("the fixture directory"),
    )
    .expect("create the fixture directory");
    fs::write(
        fixture("bars.json"),
        format!("{}\n", serde_json::to_string(&bars).unwrap()),
    )
    .unwrap();
    fs::write(
        fixture("events.json"),
        format!("{}\n", serde_json::to_string(&rebuilt.events).unwrap()),
    )
    .unwrap();

    println!("WINDOW_START = {start}");
    println!("WINDOW_LEN = {len}");
    println!("PINNED_TRADES = &[");
    for line in state.closed_trades.iter().map(replayed_line) {
        println!("    \"{line}\",");
    }
    println!("];");
    println!(
        "PINNED_OPEN = {:?}",
        state
            .open_position
            .as_ref()
            .map_or_else(String::new, replayed_open_line)
    );
    println!(
        "PINNED_FUNDING_TOTAL = {:?}",
        state.funding_total.to_string()
    );
    println!("PINNED_STATE_DIGEST = {:?}", state_digest(&state));
    println!("EVENTS = {}", rebuilt.events.len());
}

/// The smallest window (starting on a funding boundary) that covers at least one
/// closed position and a funding bar accrual.
fn choose_window(series: &[Candle]) -> (usize, usize) {
    for start in (0..2048).step_by(32) {
        for len in [64_usize, 96, 128, 160, 192, 256, 320, 384, 512] {
            if start + len > series.len() {
                continue;
            }
            let rebuilt = replay(&series[start..start + len]);
            let funding = rebuilt
                .events
                .iter()
                .filter(|event| matches!(event, PaperEvent::Funding { .. }))
                .count();
            if !rebuilt.closed.is_empty() && funding >= 1 {
                return (start, len);
            }
        }
    }
    panic!("no window in the searched prefix covers a closed trade and a funding bar");
}
