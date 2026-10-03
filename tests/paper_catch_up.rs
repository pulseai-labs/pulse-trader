//! r3.s4.w3 — AC-2: catch-up equals an uninterrupted session (ledger line d59).
//!
//! Two identical sessions — each in its OWN database, because one server
//! process owns one runtime over one database — run the same scripted bars.
//! One runs straight through; the other's runtime is dropped for eight bars
//! (the simulated outage) and a FRESH runtime boots at the first session's last
//! bar, catching up through the same `boot()` the server hook spawns. Their
//! logs, replayed states and recorded bars must then agree bar for bar.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::collections::BTreeMap;

use pulse::{
    BacktestConfig, BinanceAdapter, CandleSeries, CandleSeriesRepository, ExchangeAdapter, Pair,
    PaperEvent, PaperSessionId, PaperSessionRepository, PaperSessionState, PaperSide, SettlePolicy,
    ShadowResult, StrategyRepository, SymbolFilters, Timeframe, compile, run_backtest, validate,
};
use rust_decimal::Decimal;
use support::paper::{PaperWorld, TestRuntime, create_version, promote_session};

const PAIR: &str = "BTCUSDT";
const M15_MS: i64 = 900_000;

/// The semantic shape of one log event — everything except `seq` and `at`,
/// which are per-session bookkeeping the two logs legitimately differ on.
#[derive(Debug, PartialEq)]
enum Shape {
    Bar(Vec<(Timeframe, i64)>),
    Signal(PaperSide, Decimal),
    Fill(PaperSide, Decimal, Decimal, Option<pulse::ExitReason>),
    Funding(Decimal, Decimal),
    Data(String),
    Upgrade,
    Stop,
}

fn shape(event: &PaperEvent) -> Option<Shape> {
    match event {
        PaperEvent::BarProcessed { bars, .. } => Some(Shape::Bar(
            bars.iter()
                .map(|bar| (bar.timeframe, bar.open_time))
                .collect(),
        )),
        PaperEvent::Order { side, qty, .. } => Some(Shape::Signal(*side, *qty)),
        PaperEvent::Fill {
            side,
            qty,
            price,
            exit_reason,
            ..
        } => Some(Shape::Fill(*side, *qty, *price, *exit_reason)),
        PaperEvent::Funding { rate, amount, .. } => Some(Shape::Funding(*rate, *amount)),
        PaperEvent::DataEvent { summary, .. } => Some(Shape::Data(summary.clone())),
        PaperEvent::EngineUpgraded { .. } => Some(Shape::Upgrade),
        PaperEvent::Stop { .. } => Some(Shape::Stop),
        // The shadow checks are excluded: the reboot adds its own, and clause
        // (iv) asserts the post-catch-up one separately.
        PaperEvent::ShadowChecked { .. } => None,
    }
}

fn shapes(events: &[PaperEvent]) -> Vec<Shape> {
    events.iter().filter_map(shape).collect()
}

/// A closed trade's economic identity — the `at`-derived fill-time strings are
/// the bookkeeping the log comparison already ignores.
#[derive(Debug, PartialEq)]
struct TradeShape {
    side: PaperSide,
    qty: Decimal,
    entry_price: Option<Decimal>,
    exit_price: Decimal,
    exit_reason: pulse::ExitReason,
}

/// The open position's economic identity.
#[derive(Debug, PartialEq)]
struct PositionShape {
    side: PaperSide,
    qty: Decimal,
    entry_price: Decimal,
}

/// The replayed state minus the `seq` counter (the extra `shadow_checked`
/// shifts it, and `seq` is not state) and minus the `at`-derived fill-time
/// strings (the spec's ignored `at`).
#[derive(Debug, PartialEq)]
struct StateShape {
    status: pulse::PaperSessionStatus,
    epochs: Vec<pulse::EngineFingerprint>,
    last_bar_open_time: Option<i64>,
    closed_trades: Vec<TradeShape>,
    open_position: Option<PositionShape>,
    funding_total: Decimal,
    data_event_count: u64,
}

async fn state_shape(world: &PaperWorld, id: &PaperSessionId) -> StateShape {
    let session = world.paper().get_session(id).await.unwrap().unwrap();
    let log = world.paper().events(id).await.unwrap();
    let state = PaperSessionState::replay(&session, &log).unwrap();
    StateShape {
        status: state.status,
        epochs: state.epochs,
        last_bar_open_time: state.last_bar_open_time,
        closed_trades: state
            .closed_trades
            .into_iter()
            .map(|trade| TradeShape {
                side: trade.side,
                qty: trade.qty,
                entry_price: trade.entry_price,
                exit_price: trade.exit_price,
                exit_reason: trade.exit_reason,
            })
            .collect(),
        open_position: state.open_position.map(|position| PositionShape {
            side: position.side,
            qty: position.qty,
            entry_price: position.entry_price,
        }),
        funding_total: state.funding_total,
        data_event_count: state.data_event_count,
    }
}

async fn bar_flags(world: &PaperWorld, id: &PaperSessionId) -> Vec<(String, i64, i64)> {
    sqlx::query_as(
        "SELECT timeframe, open_time, lead_in FROM paper_bar \
         WHERE session_id = ?1 ORDER BY timeframe, open_time",
    )
    .bind(id.as_str())
    .fetch_all(world.db.pool())
    .await
    .unwrap()
}

/// Run the wakes the production schedule gives the bar opening at `bar_open`
/// over a steady source: its first counting read `settle_ms` past its close,
/// then the confirming re-poll that consumes it (#306).
async fn wake_at_bar(world: &PaperWorld, runtime: &mut TestRuntime, bar_open: i64) {
    let policy = SettlePolicy::DEFAULT;
    world.clock.set(bar_open + M15_MS + policy.settle_ms);
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "wake failures: {failures:?}");
    assert_eq!(
        runtime.next_wake_ms(),
        Some(world.clock.now() + policy.repoll_ms)
    );
    world.clock.advance(policy.repoll_ms);
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "wake failures: {failures:?}");
}

fn counts(events: &[PaperEvent]) -> (usize, usize) {
    let trades = events
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
    let funding = events
        .iter()
        .filter(|event| matches!(event, PaperEvent::Funding { .. }))
        .count();
    (trades, funding)
}

/// A world with the fixture script and one session promoted.
async fn world_with_session() -> (PaperWorld, pulse::VersionId, PaperSessionId) {
    let world = PaperWorld::new().await;
    world
        .source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    world
        .source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version = create_version(&world, "catch-up", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    (world, version, session.id)
}

#[tokio::test]
async fn catch_up_after_an_outage_equals_an_uninterrupted_session() {
    // Two identical worlds: one runtime per database, the server's own shape.
    let (world_a, _version_a, session_a) = world_with_session().await;
    let (world_b, version_b, session_b) = world_with_session().await;
    assert_eq!(
        world_a.clock.now(),
        world_b.clock.now(),
        "both worlds start at the same instant"
    );

    let mut runtime_a = world_a.runtime();
    let mut runtime_b = world_b.runtime();
    assert!(runtime_a.boot().await.is_empty(), "A boots");
    assert!(runtime_b.boot().await.is_empty(), "B boots");
    // The lead-in lands at its confirming read, one re-poll later (#306).
    let repoll = SettlePolicy::DEFAULT.repoll_ms;
    for (world, runtime) in [(&world_a, &mut runtime_a), (&world_b, &mut runtime_b)] {
        assert_eq!(runtime.next_wake_ms(), Some(world.clock.now() + repoll));
        world.clock.advance(repoll);
        assert!(runtime.wake().await.is_empty(), "the lead-in confirms");
    }

    let first_live = world_a
        .paper()
        .count_from_ms(&session_a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        first_live,
        world_b
            .paper()
            .count_from_ms(&session_b)
            .await
            .unwrap()
            .unwrap(),
        "both sessions count from the same bar"
    );

    // Phase 1: both run in lockstep until A's log holds a closed trade AND a
    // funding payment — the window d59 must cover.
    let mut bar = first_live;
    let mut bars_run = 0_u32;
    loop {
        wake_at_bar(&world_a, &mut runtime_a, bar).await;
        wake_at_bar(&world_b, &mut runtime_b, bar).await;
        bars_run += 1;
        let log = world_a.paper().events(&session_a).await.unwrap();
        let (trades, funding) = counts(&log);
        let state = state_shape(&world_a, &session_a).await;
        if trades >= 1 && funding >= 1 && state.open_position.is_none() {
            break;
        }
        assert!(bars_run < 6_000, "no trading window within {bars_run} bars");
        bar += M15_MS;
    }
    assert!(
        bars_run > 0,
        "the uninterrupted session really ran live bars ({bars_run})"
    );

    // Phase 2: the outage. B's runtime is dropped; A keeps running for eight
    // more bars (two hours of live bars B never sees).
    drop(runtime_b);
    let b_bars_before = world_b
        .paper()
        .bars(&session_b, Timeframe::M15)
        .await
        .unwrap()
        .len();
    for _ in 0..8 {
        bar += M15_MS;
        wake_at_bar(&world_a, &mut runtime_a, bar).await;
    }
    let a_log_before_boot = world_a.paper().events(&session_a).await.unwrap();
    let (a_trades, a_funding) = counts(&a_log_before_boot);
    assert!(a_trades >= 1, "A closed at least one trade");
    assert!(a_funding >= 1, "A logged at least one funding payment");
    let a_bars = world_a
        .paper()
        .bars(&session_a, Timeframe::M15)
        .await
        .unwrap();
    assert!(
        a_bars.len() > b_bars_before,
        "B really missed bars during the outage ({} vs {})",
        b_bars_before,
        a_bars.len()
    );

    // Phase 3: a FRESH runtime boots at bar N's time — the same `boot()` the
    // server hook spawns — and catches up.
    world_b.clock.set(world_a.clock.now());
    let mut runtime_b2 = world_b.runtime();
    let failures = runtime_b2.boot().await;
    assert!(failures.is_empty(), "B's reboot boots: {failures:?}");
    // Catch-up confirms the backlog at the first re-poll after the boot, never
    // with a second read at the boot's own instant (#306).
    assert_eq!(
        runtime_b2.next_wake_ms(),
        Some(world_b.clock.now() + repoll)
    );
    world_b.clock.advance(repoll);
    let failures = runtime_b2.wake().await;
    assert!(failures.is_empty(), "B's catch-up re-poll: {failures:?}");
    runtime_b2
        .shadow_check(&session_b)
        .await
        .expect("B shadow-checks after catch-up");

    // (iii) the log equals A's, ignoring `seq`, `at` and the boot's extra
    // `ShadowChecked`.
    let a_log = world_a.paper().events(&session_a).await.unwrap();
    let b_log = world_b.paper().events(&session_b).await.unwrap();
    assert_eq!(
        shapes(&a_log),
        shapes(&b_log),
        "B's caught-up log is A's log, event for event"
    );
    assert!(
        shapes(&a_log).len() > 4,
        "the comparison is not vacuous: {} events",
        shapes(&a_log).len()
    );

    // Its replayed state equals A's.
    assert_eq!(
        state_shape(&world_a, &session_a).await,
        state_shape(&world_b, &session_b).await,
        "the replayed states agree"
    );

    // Its `paper_bar` rows equal A's (values and lead-in flags).
    for timeframe in [Timeframe::M15, Timeframe::H4] {
        assert_eq!(
            world_a.paper().bars(&session_a, timeframe).await.unwrap(),
            world_b.paper().bars(&session_b, timeframe).await.unwrap(),
            "{timeframe:?} rows agree"
        );
    }
    assert_eq!(
        bar_flags(&world_a, &session_a).await,
        bar_flags(&world_b, &session_b).await,
        "the lead-in flags agree"
    );

    // (iv) B has a ShadowChecked after catch-up, and it is Identical.
    let b_shadow = b_log
        .iter()
        .rev()
        .find_map(|event| match event {
            PaperEvent::ShadowChecked { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("B's boot shadow-checked");
    let verdict: ShadowResult = serde_json::from_value(b_shadow).unwrap();
    assert!(
        matches!(verdict, ShadowResult::Identical { .. }),
        "B's post-catch-up shadow is identical, got {verdict:?}"
    );

    // The identity itself is the same check A would run: an independent shadow
    // over B's own materialised snapshots agrees with B's log.
    let selections = world_b.paper().materialise(&session_b).await.unwrap();
    let mut series: BTreeMap<Timeframe, CandleSeries> = BTreeMap::new();
    for selection in &selections {
        series.insert(
            selection.timeframe,
            world_b
                .store
                .load_version(
                    &Pair::new(PAIR),
                    selection.timeframe,
                    &selection.data_version,
                )
                .unwrap()
                .series,
        );
    }
    let version_row = world_b
        .strategies()
        .get_version(&version_b)
        .await
        .unwrap()
        .unwrap();
    let compiled = compile(&validate(&version_row.dsl).unwrap()).unwrap();
    let filters: SymbolFilters = BinanceAdapter::new()
        .symbol_filters(&Pair::new(PAIR))
        .unwrap();
    let primary = series.remove(&Timeframe::M15).unwrap();
    let htf = series.remove(&Timeframe::H4);
    let count_from = world_b.paper().count_from_ms(&session_b).await.unwrap();
    let shadow = run_backtest(
        &compiled,
        &primary,
        htf.as_ref(),
        None,
        &BacktestConfig::default(),
        &filters,
        pulse::SeriesEnd::WindowEdge,
        count_from,
    )
    .unwrap();
    let b_state = state_shape(&world_b, &session_b).await;
    assert_eq!(b_state.closed_trades.len(), shadow.trades.len());
    assert_eq!(b_state.funding_total, shadow.funding_total);
    assert!(b_state.open_position.is_none() && shadow.open_position.is_none());
}
