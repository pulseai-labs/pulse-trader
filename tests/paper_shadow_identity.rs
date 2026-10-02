//! r3.s4.w3 — AC-1: shadow identity (ledger line d58).
//!
//! A live paper session and a plain `run_backtest` over the SAME bars, read
//! back from the session's own `paper_bar` rows through content-addressed
//! snapshots, must agree field for field — closed trades, the open position
//! and the funding total. The session runs bar by bar through the real runtime
//! (a scripted `ClosedBarSource` + a hand-advanced clock); the shadow reads the
//! materialised snapshots, never the runtime's memory, so the two sides are
//! independent representations of one history.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::collections::BTreeMap;

use pulse::{
    BacktestConfig, BinanceAdapter, CandleSeries, CandleSeriesRepository, EpochStart,
    ExchangeAdapter, Pair, PaperEvent, PaperSessionRepository, PaperSessionState, PaperSide,
    ShadowResult, StrategyRepository, SymbolFilters, Timeframe, compare, compile, run_backtest,
    validate,
};
use rust_decimal::Decimal;
use support::paper::{PaperWorld, create_version, promote_session};

const PAIR: &str = "BTCUSDT";
const M15_MS: i64 = 900_000;

/// Project an engine trade's side onto the paper log's vocabulary.
fn paper_side(direction: pulse::Direction) -> PaperSide {
    match direction {
        pulse::Direction::Long => PaperSide::Long,
        pulse::Direction::Short => PaperSide::Short,
    }
}

/// Every fixture bar for the served timeframes, scripted for the source.
fn script_fixture(world: &PaperWorld) {
    world
        .source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    world
        .source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
}

/// The session's replayed state, straight from the log.
async fn replay(world: &PaperWorld, id: &pulse::PaperSessionId) -> PaperSessionState {
    let session = world.paper().get_session(id).await.unwrap().unwrap();
    let log = world.paper().events(id).await.unwrap();
    PaperSessionState::replay(&session, &log).unwrap()
}

/// Drive the runtime until the session has closed at least one trade, emitted
/// at least one funding payment, and is flat again — the window the two
/// identity assertions need. Returns the number of bars consumed.
async fn run_until_trading_flat(
    world: &PaperWorld,
    runtime: &mut support::paper::TestRuntime,
    id: &pulse::PaperSessionId,
) -> u32 {
    let step = M15_MS;
    // The first live bar is the one the runtime recorded as not-lead-in.
    let first_live = world.paper().count_from_ms(id).await.unwrap().unwrap();
    let mut bars = 0_u32;
    let mut open = first_live;
    loop {
        world.clock.set(open + step);
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "wake failures: {failures:?}");
        bars += 1;
        let state = replay(world, id).await;
        let events = world.paper().events(id).await.unwrap();
        let funding_events = events
            .iter()
            .filter(|event| matches!(event, PaperEvent::Funding { .. }))
            .count();
        if !state.closed_trades.is_empty() && funding_events >= 1 && state.open_position.is_none() {
            return bars;
        }
        assert!(
            bars < 6_000,
            "no closed-trade + funding + flat window within {bars} bars"
        );
        open += step;
    }
}

/// The fixture strategy's signal-exit threshold — the constant its exit rule
/// compares a close against.
fn exit_threshold() -> Decimal {
    for rule in &pulse::fixture_strategy_dsl().exits {
        if let pulse::ExitRule::SignalExit { condition } = rule
            && let pulse::Condition::Compare {
                rhs: pulse::ValueSource::Constant { value },
                ..
            } = condition
        {
            return *value;
        }
    }
    panic!("the fixture strategy carries a signal exit");
}

/// The fixture strategy's stop distance, as a fraction of the entry price.
fn stop_distance_pct() -> Decimal {
    for rule in &pulse::fixture_strategy_dsl().exits {
        if let pulse::ExitRule::StopLoss { distance_pct } = rule
            && let pulse::SweepableValue::Fixed(value) = distance_pct
        {
            return *value;
        }
    }
    panic!("the fixture strategy carries a stop loss");
}

/// Drive the runtime until the session holds an OPEN position whose newest
/// recorded bar leaves no pending exit and whose next bar (built like the
/// fixture's own) could neither stop the position out nor signal its exit —
/// the instant the open-position identity must be checked at. Returns the
/// number of bars consumed.
async fn run_until_open(
    world: &PaperWorld,
    runtime: &mut support::paper::TestRuntime,
    id: &pulse::PaperSessionId,
) -> u32 {
    let step = M15_MS;
    let mut bars = 0_u32;
    let mut open = world.paper().count_from_ms(id).await.unwrap().unwrap();
    loop {
        world.clock.set(open + step);
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "wake failures: {failures:?}");
        bars += 1;
        let state = replay(world, id).await;
        let recorded = world.paper().bars(id, Timeframe::M15).await.unwrap();
        if let (Some(position), Some(last)) = (state.open_position.as_ref(), recorded.last()) {
            let stop_price = position.entry_price * (Decimal::ONE - stop_distance_pct());
            let next_low = last.close - Decimal::from(20);
            let next_close = last.close + Decimal::from(10);
            if last.close < exit_threshold()
                && next_low > stop_price
                && next_close < exit_threshold()
            {
                return bars;
            }
        }
        assert!(
            bars < 6_000,
            "no open position with a clean tail within {bars} bars"
        );
        open += step;
    }
}

/// The live session's open-position mark, reconstructed from the session's own
/// log and recorded bars — the inputs the runtime's live engine holds: the
/// entry `fill` event (side, qty, price), the primary bar its batch consumed,
/// the signal bar before it, and the newest recorded primary bar for the mark.
fn live_open_mark(log: &[PaperEvent], primary_bars: &[pulse::Candle]) -> pulse::OpenPositionMark {
    let entry = log
        .iter()
        .rposition(|event| {
            matches!(
                event,
                PaperEvent::Fill {
                    exit_reason: None,
                    ..
                }
            )
        })
        .expect("an entry fill opened the position");
    assert!(
        !log[entry..].iter().any(|event| matches!(
            event,
            PaperEvent::Fill {
                exit_reason: Some(_),
                ..
            }
        )),
        "the position is still open: no exit fill follows its entry"
    );
    let fill_bar_open = log[..entry]
        .iter()
        .rev()
        .find_map(|event| match event {
            PaperEvent::BarProcessed { bars, .. } => bars
                .iter()
                .find(|bar| bar.timeframe == Timeframe::M15)
                .map(|bar| bar.open_time),
            _ => None,
        })
        .expect("the entry fill rides its bar's batch");
    let PaperEvent::Fill {
        side, qty, price, ..
    } = &log[entry]
    else {
        unreachable!("the index came from a fill");
    };
    let last = primary_bars.last().expect("the session recorded bars");
    pulse::OpenPositionMark {
        direction: match side {
            PaperSide::Long => pulse::Direction::Long,
            PaperSide::Short => pulse::Direction::Short,
        },
        qty: *qty,
        entry_price: *price,
        // The pending entry's signal bar is the bar before the fill bar, and
        // the signal instant is that bar's close.
        entry_signal_time: fill_bar_open - 1,
        entry_fill_time: fill_bar_open,
        mark_time: last.close_time,
        mark_price: last.close,
    }
}

/// The shadow: the session's own rows, materialised to content-addressed
/// snapshots and read back BY VERSION, then run through the plain engine —
/// never the runtime's memory.
async fn shadow_over_materialised(
    world: &PaperWorld,
    id: &pulse::PaperSessionId,
    version: &pulse::VersionId,
) -> pulse::BacktestResult {
    let selections = world.paper().materialise(id).await.unwrap();
    assert!(!selections.is_empty(), "the session has recorded rows");
    let mut series: BTreeMap<Timeframe, CandleSeries> = BTreeMap::new();
    for selection in &selections {
        let stored = world
            .store
            .load_version(
                &Pair::new(PAIR),
                selection.timeframe,
                &selection.data_version,
            )
            .expect("the pinned snapshot reads back");
        series.insert(selection.timeframe, stored.series);
    }
    let primary = series.remove(&Timeframe::M15).unwrap();
    let htf = series.remove(&Timeframe::H4);
    let version_row = world
        .strategies()
        .get_version(version)
        .await
        .unwrap()
        .unwrap();
    let compiled = compile(&validate(&version_row.dsl).unwrap()).unwrap();
    let filters: SymbolFilters = BinanceAdapter::new()
        .symbol_filters(&Pair::new(PAIR))
        .unwrap();
    let count_from = world.paper().count_from_ms(id).await.unwrap();
    run_backtest(
        &compiled,
        &primary,
        htf.as_ref(),
        None,
        &BacktestConfig::default(),
        &filters,
        pulse::SeriesEnd::WindowEdge,
        count_from,
    )
    .expect("the shadow runs over the materialised snapshots")
}

// ---------------------------------------------------------------------------
// (i) the live session equals its shadow, field for field
// ---------------------------------------------------------------------------

#[tokio::test]
async fn live_session_equals_its_shadow_field_for_field() {
    let world = PaperWorld::new().await;
    script_fixture(&world);
    let version = create_version(&world, "shadow-identity", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;

    let mut runtime = world.runtime();
    let failures = runtime.boot().await;
    assert!(failures.is_empty(), "boot failures: {failures:?}");

    // The first start already proves the lead-in shape: the shipped bars are
    // flagged, no event rides them.
    let first_live = world
        .paper()
        .count_from_ms(&session.id)
        .await
        .unwrap()
        .unwrap();
    let lead_in = world
        .paper()
        .bars(&session.id, Timeframe::M15)
        .await
        .unwrap();
    assert!(
        lead_in.iter().all(|bar| bar.open_time < first_live),
        "every recorded M15 bar before the first live bar is lead-in"
    );
    assert!(
        !lead_in.is_empty(),
        "the fixture supplies warm-up history before the first live bar"
    );

    let bars = run_until_trading_flat(&world, &mut runtime, &session.id).await;
    assert!(bars > 0);

    let log = world.paper().events(&session.id).await.unwrap();
    let closed_trades = log
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
    let funding_events = log
        .iter()
        .filter(|event| matches!(event, PaperEvent::Funding { .. }))
        .count();
    assert!(closed_trades >= 1, "the window closes at least one trade");
    assert!(
        funding_events >= 1,
        "the window carries at least one funding payment in the log"
    );

    // The runtime's own check: identical, and the payload says so.
    let checked = runtime.shadow_check(&session.id).await.unwrap();
    assert!(
        matches!(checked, ShadowResult::Identical { .. }),
        "the shadow check is identical, got {checked:?}"
    );

    // The independent shadow: the session's own materialised snapshots, pinned
    // by content version, run through the engine — never the runtime's memory.
    let selections = world.paper().materialise(&session.id).await.unwrap();
    assert!(!selections.is_empty(), "the session has recorded rows");
    let mut series: BTreeMap<Timeframe, CandleSeries> = BTreeMap::new();
    for selection in &selections {
        let stored = world
            .store
            .load_version(
                &Pair::new(PAIR),
                selection.timeframe,
                &selection.data_version,
            )
            .expect("the pinned snapshot reads back");
        series.insert(selection.timeframe, stored.series);
    }
    let primary = series.remove(&Timeframe::M15).unwrap();
    let htf = series.remove(&Timeframe::H4);
    let version_row = world
        .strategies()
        .get_version(&version)
        .await
        .unwrap()
        .unwrap();
    let compiled = compile(&validate(&version_row.dsl).unwrap()).unwrap();
    let filters: SymbolFilters = BinanceAdapter::new()
        .symbol_filters(&Pair::new(PAIR))
        .unwrap();
    let config = BacktestConfig::default();
    let count_from = world.paper().count_from_ms(&session.id).await.unwrap();
    let shadow = run_backtest(
        &compiled,
        &primary,
        htf.as_ref(),
        None,
        &config,
        &filters,
        pulse::SeriesEnd::WindowEdge,
        count_from,
    )
    .expect("the shadow runs over the materialised snapshots");

    let state = replay(&world, &session.id).await;
    assert!(
        state.open_position.is_none(),
        "the chosen window ends flat, so both sides see only closed trades"
    );
    assert_eq!(
        state.closed_trades.len(),
        shadow.trades.len(),
        "the log's closed trades and the shadow's trades are the same list"
    );
    for (paper, engine) in state.closed_trades.iter().zip(shadow.trades.iter()) {
        assert_eq!(paper.side, paper_side(engine.direction), "side");
        assert_eq!(paper.qty, engine.qty, "qty");
        assert_eq!(paper.entry_price, Some(engine.entry_price), "entry price");
        assert_eq!(paper.exit_price, engine.exit_price, "exit price");
        assert_eq!(paper.exit_reason, engine.exit_reason, "exit reason");
    }
    assert_eq!(
        state.funding_total, shadow.funding_total,
        "the log's funding total equals the shadow's, payment for payment"
    );
    assert!(
        state.funding_total != Decimal::ZERO,
        "the funding identity is not vacuous"
    );

    // ---- the same identity while a position is OPEN -----------------------
    // AC-1 (i) also requires the shadow check to run with an open position,
    // with BOTH marks `Some` and compared field for field — a flat `None`/`None`
    // window would prove nothing about the open-position arm.
    let open_bars = run_until_open(&world, &mut runtime, &session.id).await;
    assert!(open_bars > 0, "the run continued to an open position");

    let log = world.paper().events(&session.id).await.unwrap();
    let closed_trades_open = log
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
    let funding_events_open = log
        .iter()
        .filter(|event| matches!(event, PaperEvent::Funding { .. }))
        .count();
    assert!(
        closed_trades_open >= 1,
        "the open window still holds a closed trade"
    );
    assert!(
        funding_events_open >= 1,
        "the open window still holds a funding payment"
    );
    let state = replay(&world, &session.id).await;
    assert!(
        state.open_position.is_some(),
        "the live session holds an open position at the check instant"
    );

    let checked = runtime.shadow_check(&session.id).await.unwrap();
    let ShadowResult::Identical {
        closed_trades: judged,
        open_position: true,
    } = checked
    else {
        panic!(
            "the open-position check must be identical with a mark on both sides, got {checked:?}"
        );
    };
    assert_eq!(
        usize::try_from(judged).unwrap(),
        closed_trades_open,
        "the check compared the live epoch's closed trades"
    );

    let shadow = shadow_over_materialised(&world, &session.id, &version).await;
    let shadow_mark = shadow
        .open_position
        .as_ref()
        .expect("the shadow ends with the position open");
    let recorded = world
        .paper()
        .bars(&session.id, Timeframe::M15)
        .await
        .unwrap();
    let live_mark = live_open_mark(&log, &recorded);
    assert_eq!(live_mark.direction, shadow_mark.direction, "direction");
    assert_eq!(live_mark.qty, shadow_mark.qty, "qty");
    assert_eq!(
        live_mark.entry_price, shadow_mark.entry_price,
        "entry price"
    );
    assert_eq!(
        live_mark.entry_signal_time, shadow_mark.entry_signal_time,
        "entry signal time"
    );
    assert_eq!(
        live_mark.entry_fill_time, shadow_mark.entry_fill_time,
        "entry fill time"
    );
    assert_eq!(live_mark.mark_time, shadow_mark.mark_time, "mark time");
    assert_eq!(live_mark.mark_price, shadow_mark.mark_price, "mark price");
}

// ---------------------------------------------------------------------------
// (i) an open-position divergence is a drift on the runtime's own path
// ---------------------------------------------------------------------------

/// The runtime's shadow check must report a `Drift` when the OPEN POSITION
/// differs — through `PaperRuntime::shadow_check`, over the session's own
/// materialised snapshots, with the appended `ShadowChecked.result` decoding to
/// the same verdict. The divergence is forged the only way the runtime's own
/// data can diverge: the recorded rows gain one more bar than the live engine
/// has consumed, and that bar neither stops the position out nor signals its
/// exit — so the closed trades stay equal and the marks are what differs.
#[tokio::test]
async fn an_open_position_divergence_is_a_drift_on_the_runtime_path() {
    let world = PaperWorld::new().await;
    script_fixture(&world);
    let version = create_version(&world, "shadow-open-drift", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    // The next bar this position would see, built like the fixture's own: it
    // must not stop the position out and must not signal its exit.
    let next_bar = |last: &pulse::Candle| pulse::Candle {
        open_time: last.open_time + M15_MS,
        close_time: last.open_time + 2 * M15_MS - 1,
        open: last.close + Decimal::from(5),
        high: last.close + Decimal::from(30),
        low: last.close - Decimal::from(20),
        close: last.close + Decimal::from(10),
        volume: Decimal::from(100),
        funding_rate: ((last.open_time + M15_MS) % 28_800_000 == 0).then(|| Decimal::new(1, 5)),
    };

    let mut open = world
        .paper()
        .count_from_ms(&session.id)
        .await
        .unwrap()
        .unwrap();
    let mut extra = None;
    for _ in 0..6_000 {
        world.clock.set(open + M15_MS);
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
        let state = replay(&world, &session.id).await;
        let recorded = world
            .paper()
            .bars(&session.id, Timeframe::M15)
            .await
            .unwrap();
        if let (Some(position), Some(last)) = (state.open_position.as_ref(), recorded.last()) {
            let stop_price = position.entry_price * (Decimal::ONE - stop_distance_pct());
            let candidate = next_bar(last);
            if last.close < exit_threshold()
                && candidate.low > stop_price
                && candidate.close < exit_threshold()
            {
                extra = Some(candidate);
                break;
            }
        }
        open += M15_MS;
    }
    let extra = extra.expect("an open position with a safe next bar was reached");

    // The live engine has not consumed this bar; the recorded rows have it. The
    // shadow reads the rows, so its mark is the extra bar's and the live one's
    // is not.
    world
        .paper()
        .append_bar(&session.id, &[(Timeframe::M15, extra.clone(), false)], &[])
        .await
        .unwrap();
    world.clock.set(extra.close_time + 1);

    let verdict = runtime.shadow_check(&session.id).await.unwrap();
    let ShadowResult::Drift {
        first_divergence,
        live,
        shadow,
    } = &verdict
    else {
        panic!("an open-position divergence is a drift, got {verdict:?}");
    };
    assert!(
        first_divergence.contains("open position"),
        "the open position diverged first, got {first_divergence}"
    );
    assert_ne!(
        live["mark_time"], shadow["mark_time"],
        "the two marks differ in their mark time: live {live}, shadow {shadow}"
    );
    assert_ne!(
        live["mark_price"], shadow["mark_price"],
        "the two marks differ in their mark price"
    );

    // The appended event carries the same verdict.
    let logged = world.paper().events(&session.id).await.unwrap();
    let payload = logged
        .iter()
        .rev()
        .find_map(|event| match event {
            PaperEvent::ShadowChecked { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("the drift is appended");
    let decoded: ShadowResult =
        serde_json::from_value(payload).expect("the payload decodes to the typed result");
    assert_eq!(decoded, verdict, "the appended result is the verdict");
}

// ---------------------------------------------------------------------------
// (ii) `compare` — identical, and a drift names its first divergence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn compare_names_the_first_divergence_of_a_drifted_shadow() {
    let world = PaperWorld::new().await;
    script_fixture(&world);
    let version = create_version(&world, "shadow-compare", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    let mut runtime = world.runtime();
    runtime.boot().await;
    run_until_trading_flat(&world, &mut runtime, &session.id).await;

    // A real trade list from a real run over the recorded snapshots.
    let selections = world.paper().materialise(&session.id).await.unwrap();
    let primary = world
        .store
        .load_version(
            &Pair::new(PAIR),
            Timeframe::M15,
            &selections
                .iter()
                .find(|s| s.timeframe == Timeframe::M15)
                .unwrap()
                .data_version,
        )
        .unwrap()
        .series;
    let version_row = world
        .strategies()
        .get_version(&version)
        .await
        .unwrap()
        .unwrap();
    let compiled = compile(&validate(&version_row.dsl).unwrap()).unwrap();
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new(PAIR))
        .unwrap();
    let count_from = world.paper().count_from_ms(&session.id).await.unwrap();
    let shadow = run_backtest(
        &compiled,
        &primary,
        None,
        None,
        &BacktestConfig::default(),
        &filters,
        pulse::SeriesEnd::WindowEdge,
        count_from,
    )
    .unwrap();
    assert!(
        shadow.trades.len() >= 2,
        "the fixture trades more than once"
    );

    // The same list against itself is identical.
    let same = compare(
        &shadow.trades,
        shadow.open_position.as_ref(),
        &shadow.trades,
        shadow.open_position.as_ref(),
        EpochStart::WholeLog,
    );
    assert!(matches!(same, ShadowResult::Identical { .. }), "{same:?}");

    // One field moved on the shadow side: drift, naming the first divergence.
    let mut drifted = shadow.trades.clone();
    drifted[0].exit_price += Decimal::ONE;
    let drift = compare(
        &shadow.trades,
        shadow.open_position.as_ref(),
        &drifted,
        shadow.open_position.as_ref(),
        EpochStart::WholeLog,
    );
    let ShadowResult::Drift {
        first_divergence, ..
    } = drift
    else {
        panic!("a differing trade is a drift");
    };
    assert!(
        first_divergence.contains('0'),
        "the divergence names the first mismatching trade: {first_divergence}"
    );

    // A shorter shadow list is a drift too — the count itself diverges.
    let truncated = compare(
        &shadow.trades,
        shadow.open_position.as_ref(),
        &shadow.trades[..1],
        None,
        EpochStart::WholeLog,
    );
    assert!(matches!(truncated, ShadowResult::Drift { .. }));

    // Epoch scoping: only trades entered at/after the epoch's first bar are
    // judged; an epoch with no bars yet judges nothing (and still compares the
    // open position).
    let first_entry = shadow.trades[0].entry_fill_time;
    let windowed = compare(
        &shadow.trades,
        None,
        &shadow.trades,
        None,
        EpochStart::Bar(first_entry + 1),
    );
    assert!(
        matches!(windowed, ShadowResult::Identical { .. }),
        "{windowed:?}"
    );
    let empty = compare(&shadow.trades, None, &[], None, EpochStart::Empty);
    assert!(matches!(empty, ShadowResult::Identical { .. }), "{empty:?}");
}

// ---------------------------------------------------------------------------
// (iii) repeat shadow checks agree; HEAD stays absent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeat_shadow_checks_agree_and_never_touch_head() {
    let world = PaperWorld::new().await;
    script_fixture(&world);
    let version = create_version(&world, "shadow-repeat", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    let mut runtime = world.runtime();
    runtime.boot().await;
    run_until_trading_flat(&world, &mut runtime, &session.id).await;

    let head = |tf: &str| std::fs::read(world.store_base().join(format!("{PAIR}/{tf}/HEAD"))).ok();
    assert!(head("15m").is_none(), "HEAD absent before the checks");
    assert!(head("4h").is_none(), "HEAD absent before the checks");

    let first = runtime.shadow_check(&session.id).await.unwrap();
    let second = runtime.shadow_check(&session.id).await.unwrap();
    assert_eq!(first, second, "the same rows give the same verdict");

    let checks: Vec<_> = world
        .paper()
        .events(&session.id)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|event| match event {
            PaperEvent::ShadowChecked {
                data_versions,
                result,
                ..
            } => Some((data_versions, result)),
            _ => None,
        })
        .collect();
    assert!(checks.len() >= 2, "both checks are in the log");
    let last_two = &checks[checks.len() - 2..];
    assert_eq!(
        last_two[0].0, last_two[1].0,
        "two checks over the same rows carry the same data versions"
    );
    assert!(!last_two[0].0.is_empty());

    assert!(head("15m").is_none(), "HEAD still absent after the checks");
    assert!(head("4h").is_none(), "HEAD still absent after the checks");
}

// ---------------------------------------------------------------------------
// (iv) the payload round-trips and replay accepts the log
// ---------------------------------------------------------------------------

#[tokio::test]
async fn shadow_payload_round_trips_and_replay_accepts_the_log() {
    let world = PaperWorld::new().await;
    script_fixture(&world);
    let version = create_version(&world, "shadow-roundtrip", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    let mut runtime = world.runtime();
    runtime.boot().await;
    run_until_trading_flat(&world, &mut runtime, &session.id).await;

    let returned = runtime.shadow_check(&session.id).await.unwrap();
    let log = world.paper().events(&session.id).await.unwrap();
    let last = log
        .iter()
        .rev()
        .find_map(|event| match event {
            PaperEvent::ShadowChecked { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("the check is in the log");
    let decoded: ShadowResult =
        serde_json::from_value(last).expect("the payload decodes to the typed result");
    assert_eq!(
        decoded, returned,
        "the payload is the result, round-tripped"
    );

    let row = world
        .paper()
        .get_session(&session.id)
        .await
        .unwrap()
        .unwrap();
    let state = PaperSessionState::replay(&row, &log).expect("replay accepts the whole log");
    assert!(
        state.funding_total != Decimal::ZERO,
        "the log's funding total came from the emitted payments"
    );
    assert!(state.last_bar_open_time.is_some());
}
