//! r3.s4.w3 — AC-3: the polling loop's rules.
//!
//! Only closed bars are consumed; a wake with nothing new writes nothing; a
//! higher-timeframe bar that closes with its primary bar rides that bar's
//! batch; a re-fetched bar that changed is a `data_event` and never a
//! replacement; a gap holds the session and resumes when the source supplies
//! the missing bar; two sessions sharing a timeframe share one fetch; the loop
//! is timeframe-agnostic (run over two configurations, the second a real daily
//! series); the first start records lead-in with no events; a failed append
//! leaves no partial rows and the bar lands exactly once afterwards. Every
//! case runs without the #306 settle gate (see [`ungated_world`]).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use pulse::{
    Candle, ClosedBarSource, DataError, DataVersion, LiveEnv, PageSource, Pair, PaperEvent,
    PaperRuntime, PaperSession, PaperSessionId, PaperSessionRepository, SqliteStrategyRepo,
    StrategyDsl, SystemClock, Timeframe, VersionId, boundaries, compile, first_open_bar_ms,
    validate,
};
use rust_decimal::Decimal;
use support::paper::{
    PaperWorld, ScriptedBars, SteppedClock, bar_of, create_version, m15_bar, promote_session,
};

const PAIR: &str = "BTCUSDT";
const M15_MS: i64 = 900_000;
const D1_MS: i64 = 86_400_000;
/// 2025-02-01T00:00:00Z — 8-hour aligned, so funding stamps land on it.
const BASE: i64 = 1_738_368_000_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn h4_bar(open_time: i64, open: i64, close: i64) -> Candle {
    let mut bar = bar_of(Timeframe::H4, open_time, open, close);
    bar.funding_rate = (open_time % 28_800_000 == 0).then(|| Decimal::new(1, 5));
    bar
}

/// A world without the settle gate: these rules sit behind it, and each case
/// wakes exactly at a bar's close over a source that never revises a bar
/// (the gate itself is `tests/paper_bar_settle.rs`).
async fn ungated_world() -> PaperWorld {
    PaperWorld::ungated().await
}

async fn bar_count(world: &PaperWorld, id: &PaperSessionId) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM paper_bar WHERE session_id = ?1")
        .bind(id.as_str())
        .fetch_one(world.db.pool())
        .await
        .unwrap()
}

/// The fixture strategy (a price compare — no indicators, so the warm-up probe
/// settles on two bars).
async fn price_session(
    world: &PaperWorld,
    primary: Timeframe,
    htf: Option<Timeframe>,
    uses_d1: bool,
) -> (VersionId, PaperSession) {
    let version = create_version(world, "poll", &pulse::fixture_strategy_dsl()).await;
    let session = promote_session(world, &version, primary, htf, uses_d1).await;
    (version, session)
}

// ---------------------------------------------------------------------------
// (i) only closed bars are consumed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_closed_bars_are_consumed() {
    let world = ungated_world().await;
    // The bar opening 00:15 closes at 00:29:59.999: NOT closed at 00:15:00.
    world.source.script(
        Timeframe::M15,
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
            m15_bar(BASE + M15_MS, 60_050, 60_000),
        ],
    );
    // The source does NOT cut off: the runtime must.
    let world = PaperWorld {
        source: world.source.clone().without_cutoff(),
        ..world
    };
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    let now = world.clock.now();
    assert_eq!(now, BASE + M15_MS, "the world starts at 00:15:00");
    assert_eq!(
        first_open_bar_ms(M15_MS, now),
        BASE + M15_MS,
        "the first bar not closed at 00:15:00 opens at 00:15"
    );
    let probe = boundaries(M15_MS, now, 0);
    assert_eq!(probe.last_closed_open_ms, BASE);
    assert_eq!(probe.next_poll_ms, BASE + 2 * M15_MS);
    assert_eq!(runtime.next_wake_ms(), Some(BASE + 2 * M15_MS));

    // A wake at 00:15:00 must not consume the 00:15 bar (close >= now), even
    // though the source hands it over.
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "{failures:?}");
    let bars = world
        .paper()
        .bars(&session.id, Timeframe::M15)
        .await
        .unwrap();
    assert!(
        bars.iter().all(|bar| bar.open_time < BASE + M15_MS),
        "the still-forming bar is not consumed"
    );

    // Once its close is past, the same bar is consumed.
    world.clock.set(BASE + 2 * M15_MS);
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "{failures:?}");
    let bars = world
        .paper()
        .bars(&session.id, Timeframe::M15)
        .await
        .unwrap();
    assert_eq!(
        bars.last().map(|bar| bar.open_time),
        Some(BASE + M15_MS),
        "the bar lands once it is closed"
    );
}

// ---------------------------------------------------------------------------
// (ii) repeated wakes with no new bar append nothing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn repeated_wakes_with_no_new_bar_append_nothing() {
    let world = ungated_world().await;
    world.source.script(
        Timeframe::M15,
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
        ],
    );
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let mut runtime = world.runtime();
    runtime.boot().await;
    world.clock.set(BASE + M15_MS);
    runtime.wake().await;

    let bars = bar_count(&world, &session.id).await;
    let events = world.paper().events(&session.id).await.unwrap().len();
    for _ in 0..3 {
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
    }
    assert_eq!(bar_count(&world, &session.id).await, bars, "no new rows");
    assert_eq!(
        world.paper().events(&session.id).await.unwrap().len(),
        events,
        "no new events"
    );
}

// ---------------------------------------------------------------------------
// (iii) a higher bar closing at the same boundary rides its primary bar
// ---------------------------------------------------------------------------

#[tokio::test]
async fn higher_bar_closing_at_the_same_boundary_rides_its_primary_bar() {
    let world = ungated_world().await;
    world.clock.set(BASE + 8 * 3_600_000); // 08:00:00 exactly
    let m15: Vec<Candle> = (0..21)
        .map(|i| m15_bar(BASE + 7 * 3_600_000 + i * M15_MS, 60_100, 60_050))
        .collect();
    // The H4 bar opening 04:00 closes at 07:59:59.999 (before the first live
    // bar); the one opening 08:00 closes at 11:59:59.999 — the same instant the
    // M15 bar opening 11:45 closes.
    world.source.script(Timeframe::M15, m15);
    world.source.script(
        Timeframe::H4,
        vec![
            h4_bar(BASE + 4 * 3_600_000, 60_100, 60_050),
            h4_bar(BASE + 8 * 3_600_000, 60_050, 60_000),
        ],
    );
    let (_version, session) =
        price_session(&world, Timeframe::M15, Some(Timeframe::H4), false).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    // Run to the 11:45 bar's close.
    world.clock.set(BASE + 12 * 3_600_000);
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "{failures:?}");

    let events = world.paper().events(&session.id).await.unwrap();
    let last_primary = BASE + 11 * 3_600_000 + 45 * 60_000;
    let batches: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, PaperEvent::BarProcessed { .. }))
        .map(|(index, _)| index)
        .collect();
    let mut found: Option<(usize, &Vec<pulse::BarRef>)> = None;
    for (order, index) in batches.iter().enumerate() {
        if let PaperEvent::BarProcessed { bars, .. } = &events[*index]
            && bars
                .iter()
                .any(|bar| bar.timeframe == Timeframe::M15 && bar.open_time == last_primary)
        {
            found = Some((order, bars));
            break;
        }
    }
    let (order, batch) = found.expect("the 11:45 bar was consumed");
    assert!(
        batch
            .iter()
            .any(|bar| bar.timeframe == Timeframe::H4 && bar.open_time == BASE + 8 * 3_600_000),
        "the H4 bar closing at 11:59:59.999 is in the SAME batch as the M15 bar closing then"
    );
    for index in batches.iter().skip(order + 1) {
        if let PaperEvent::BarProcessed { bars, .. } = &events[*index] {
            assert!(
                !bars.iter().any(|bar| bar.timeframe == Timeframe::H4),
                "no later batch carries a higher-timeframe bar"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// (iv) a re-fetched bar that changed is a data event, never a replacement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_changed_refetch_is_one_data_event_and_never_replaces_a_bar() {
    let world = ungated_world().await;
    let script = |last_close: i64| {
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
            m15_bar(BASE + M15_MS, 60_050, last_close),
        ]
    };
    world.source.script(Timeframe::M15, script(60_000));
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let mut runtime = world.runtime();
    runtime.boot().await;
    world.clock.set(BASE + 2 * M15_MS);
    runtime.wake().await;
    let recorded = world
        .paper()
        .bars(&session.id, Timeframe::M15)
        .await
        .unwrap();
    assert_eq!(recorded.len(), 3, "two lead-in + one live bar");
    let events_before = world.paper().events(&session.id).await.unwrap();
    let data_events_before = events_before
        .iter()
        .filter(|event| matches!(event, PaperEvent::DataEvent { .. }))
        .count();

    // The exchange's copy of the newest recorded bar now differs.
    world.source.script(Timeframe::M15, script(59_000));
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "{failures:?}");

    let events = world.paper().events(&session.id).await.unwrap();
    let data_events: Vec<&PaperEvent> = events
        .iter()
        .filter(|event| matches!(event, PaperEvent::DataEvent { .. }))
        .collect();
    assert_eq!(
        data_events.len(),
        data_events_before + 1,
        "exactly one data event for the changed re-fetch"
    );
    assert_eq!(
        world
            .paper()
            .bars(&session.id, Timeframe::M15)
            .await
            .unwrap(),
        recorded,
        "the recorded bar is never replaced"
    );

    // And it is not re-reported on the next wake.
    runtime.wake().await;
    let again = world.paper().events(&session.id).await.unwrap();
    assert_eq!(
        again
            .iter()
            .filter(|event| matches!(event, PaperEvent::DataEvent { .. }))
            .count(),
        data_events_before + 1,
        "at most one data event per distinct bar and refusal"
    );
}

// ---------------------------------------------------------------------------
// (iv) a gap holds the session and resumes when the source supplies the bar
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_gap_holds_the_session_until_the_missing_bar_arrives() {
    let world = ungated_world().await;
    let without_gap_bar = || {
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
            m15_bar(BASE + 2 * M15_MS, 60_050, 60_000),
        ]
    };
    world.source.script(Timeframe::M15, without_gap_bar());
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let mut runtime = world.runtime();
    runtime.boot().await;
    world.clock.set(BASE + M15_MS);
    runtime.wake().await;
    let before = bar_count(&world, &session.id).await;

    // The 00:15 bar is missing from the feed: the 00:30 bar cannot step.
    world.clock.set(BASE + 3 * M15_MS);
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        bar_count(&world, &session.id).await,
        before,
        "the session holds: the gap bar is not consumed"
    );
    let events = world.paper().events(&session.id).await.unwrap();
    let gaps = events
        .iter()
        .filter(|event| {
            matches!(event, PaperEvent::DataEvent { summary, .. } if summary.contains("refused"))
        })
        .count();
    assert_eq!(gaps, 1, "one data event names the refusal");
    runtime.wake().await;
    let events = world.paper().events(&session.id).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, PaperEvent::DataEvent { .. }))
            .count(),
        gaps,
        "the same refusal is not re-reported every wake"
    );

    // The source supplies the missing bar: the session resumes and consumes
    // each bar exactly once.
    let mut with_gap_bar = without_gap_bar();
    with_gap_bar.push(m15_bar(BASE + M15_MS, 60_000, 60_050));
    with_gap_bar.sort_by_key(|bar| bar.open_time);
    world.source.script(Timeframe::M15, with_gap_bar);
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        bar_count(&world, &session.id).await,
        before + 2,
        "the gap bar and the held bar both land, once each"
    );
    let events = world.paper().events(&session.id).await.unwrap();
    let consumed: Vec<i64> = events
        .iter()
        .filter_map(|event| match event {
            PaperEvent::BarProcessed { bars, .. } => bars
                .iter()
                .find(|bar| bar.timeframe == Timeframe::M15)
                .map(|bar| bar.open_time),
            _ => None,
        })
        .collect();
    for open_time in [BASE + M15_MS, BASE + 2 * M15_MS] {
        assert_eq!(
            consumed.iter().filter(|seen| **seen == open_time).count(),
            1,
            "bar {open_time} consumed exactly once"
        );
    }
}

// ---------------------------------------------------------------------------
// (v) two sessions sharing a timeframe share one fetch per wake
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_sessions_sharing_a_timeframe_share_one_fetch_per_wake() {
    let world = ungated_world().await;
    world.source.script(
        Timeframe::M15,
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
            m15_bar(BASE + M15_MS, 60_050, 60_000),
        ],
    );
    let (_v1, first) = price_session(&world, Timeframe::M15, None, false).await;
    let (_v2, second) = price_session(&world, Timeframe::M15, None, false).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    let before = world.source.calls().len();
    world.clock.set(BASE + 2 * M15_MS);
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "{failures:?}");
    let calls: Vec<(Timeframe, i64)> = world.source.calls()[before..].to_vec();
    assert_eq!(
        calls.iter().filter(|(tf, _)| *tf == Timeframe::M15).count(),
        1,
        "one fetch serves both sessions, got {calls:?}"
    );
    for session in [&first, &second] {
        assert_eq!(
            world
                .paper()
                .bars(&session.id, Timeframe::M15)
                .await
                .unwrap()
                .len(),
            3,
            "each session consumed the bar"
        );
    }
}

// ---------------------------------------------------------------------------
// (vi) timeframe-agnostic: M15+H4 and H4+D1 (real daily bars)
// ---------------------------------------------------------------------------

/// The fixture's own H4 series aggregated to the daily grid — real fixture
/// data, six H4 bars per day, contiguous.
fn fixture_d1_candles() -> Vec<Candle> {
    let h4 = pulse::fixture_h4_candles();
    let mut days: BTreeMap<i64, Vec<Candle>> = BTreeMap::new();
    for candle in h4 {
        let day = candle.open_time - candle.open_time.rem_euclid(D1_MS);
        days.entry(day).or_default().push(candle);
    }
    days.into_iter()
        .map(|(day, bars)| Candle {
            open_time: day,
            close_time: day + D1_MS - 1,
            open: bars.first().unwrap().open,
            high: bars.iter().map(|bar| bar.high).max().unwrap(),
            low: bars.iter().map(|bar| bar.low).min().unwrap(),
            close: bars.last().unwrap().close,
            volume: bars.iter().map(|bar| bar.volume).sum(),
            funding_rate: None,
        })
        .collect()
}

/// A D1 operand in the entry: `close(primary) < primary_threshold AND
/// close(d1) > d1_threshold`, so the compiled strategy needs the daily series.
fn daily_operand_dsl(primary_threshold: i64, d1_threshold: i64) -> StrategyDsl {
    use pulse::{Comparator, Condition, PriceField, Series, ValueSource};
    let mut dsl = pulse::fixture_strategy_dsl();
    dsl.entry = Condition::And {
        conditions: vec![
            Condition::Compare {
                lhs: ValueSource::Price {
                    series: Series::Primary,
                    field: PriceField::Close,
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant {
                    value: Decimal::from(primary_threshold),
                },
            },
            Condition::Compare {
                lhs: ValueSource::Price {
                    series: Series::D1,
                    field: PriceField::Close,
                },
                op: Comparator::Gt,
                rhs: ValueSource::Constant {
                    value: Decimal::from(d1_threshold),
                },
            },
        ],
    };
    dsl
}

async fn agnostic_case(
    primary: Timeframe,
    htf: Option<Timeframe>,
    uses_d1: bool,
    dsl: StrategyDsl,
    bars: i64,
) {
    let world = ungated_world().await;
    if primary == Timeframe::M15 {
        world
            .source
            .script(Timeframe::M15, pulse::fixture_m15_candles());
        world
            .source
            .script(Timeframe::H4, pulse::fixture_h4_candles());
    } else {
        world
            .source
            .script(Timeframe::H4, pulse::fixture_h4_candles());
        world.source.script(Timeframe::D1, fixture_d1_candles());
    }
    let version = create_version(&world, "agnostic", &dsl).await;
    let session = promote_session(&world, &version, primary, htf, uses_d1).await;
    assert_eq!(session.primary_timeframe, primary);
    assert_eq!(session.uses_d1, uses_d1);

    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    let step = primary.duration_ms();
    let first_live = world
        .paper()
        .count_from_ms(&session.id)
        .await
        .unwrap()
        .unwrap();
    let mut open = first_live;
    for _ in 0..bars {
        world.clock.set(open + step);
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
        open += step;
    }
    let verdict = runtime.shadow_check(&session.id).await.unwrap();
    assert!(
        verdict.is_identical(),
        "{primary:?}/{htf:?}/{uses_d1} shadow must be identical, got {verdict:?}"
    );
    // Every timeframe the session declares is recorded and materialised.
    for timeframe in session.timeframes() {
        assert!(
            !world
                .paper()
                .bars(&session.id, timeframe)
                .await
                .unwrap()
                .is_empty(),
            "{timeframe:?} has recorded rows"
        );
    }
    let selections = world.paper().materialise(&session.id).await.unwrap();
    let materialised: Vec<Timeframe> = selections.iter().map(|s| s.timeframe).collect();
    assert_eq!(materialised, session.timeframes());
}

#[tokio::test]
async fn the_runtime_is_timeframe_agnostic_over_two_configurations() {
    // Configuration 1: M15 primary with H4.
    agnostic_case(
        Timeframe::M15,
        Some(Timeframe::H4),
        false,
        pulse::fixture_strategy_dsl(),
        400,
    )
    .await;

    // Configuration 2: H4 primary with the real daily series, consumed by a
    // strategy that actually reads it.
    let dsl = daily_operand_dsl(60_100, 40_000);
    let compiled = compile(&validate(&dsl).unwrap()).unwrap();
    assert!(
        compiled.needs_d1(),
        "the configuration exercises the daily engine"
    );
    agnostic_case(Timeframe::H4, None, true, dsl, 200).await;
}

// ---------------------------------------------------------------------------
// (vii) the first start records lead-in with no events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn first_start_records_lead_in_with_no_events() {
    let world = ungated_world().await;
    world.source.script(
        Timeframe::M15,
        vec![
            m15_bar(BASE - 4 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - 3 * M15_MS, 60_050, 60_000),
            m15_bar(BASE - 2 * M15_MS, 60_000, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
            m15_bar(BASE + M15_MS, 60_050, 60_000),
        ],
    );
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let mut runtime = world.runtime();
    runtime.boot().await;

    let count_from = world.paper().count_from_ms(&session.id).await.unwrap();
    assert_eq!(
        count_from,
        Some(BASE + M15_MS),
        "count_from_ms is the first non-lead-in open_time (the first bar not closed at boot)"
    );
    let flags: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT open_time, lead_in FROM paper_bar WHERE session_id = ?1 ORDER BY open_time",
    )
    .bind(session.id.as_str())
    .fetch_all(world.db.pool())
    .await
    .unwrap();
    assert!(!flags.is_empty(), "the first start recorded lead-in");
    assert!(
        flags
            .iter()
            .all(|(open_time, lead_in)| *open_time < BASE + M15_MS && *lead_in == 1),
        "every shipped bar is lead-in: {flags:?}"
    );
    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, PaperEvent::BarProcessed { .. })),
        "the lead-in append carries no events"
    );

    // The first counted bar lands with lead_in = 0 and IS the boundary.
    world.clock.set(BASE + 2 * M15_MS);
    runtime.wake().await;
    let live: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT open_time, lead_in FROM paper_bar \
         WHERE session_id = ?1 AND lead_in = 0 ORDER BY open_time",
    )
    .bind(session.id.as_str())
    .fetch_all(world.db.pool())
    .await
    .unwrap();
    assert_eq!(live, vec![(BASE + M15_MS, 0)]);
}

// ---------------------------------------------------------------------------
// (viii) a failed append leaves no partial rows and consumes the bar once
// ---------------------------------------------------------------------------

/// A repository that fails the next `append_bar` once, then delegates. Two
/// switches: `fail_next` fails the next append that carries bars,
/// `fail_events_next` the next events-only one (a `data_event` or a
/// `shadow_checked`).
struct FlakyRepo<P> {
    inner: P,
    fail_next: Arc<AtomicBool>,
    fail_events_next: Arc<AtomicBool>,
    /// While set, every `bars` read fails (a rebuild's first read).
    fail_bars: Arc<AtomicBool>,
    appends: Arc<AtomicUsize>,
}

impl<P> FlakyRepo<P> {
    fn new(inner: P) -> Self {
        Self {
            inner,
            fail_next: Arc::new(AtomicBool::new(false)),
            fail_events_next: Arc::new(AtomicBool::new(false)),
            fail_bars: Arc::new(AtomicBool::new(false)),
            appends: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The failure switch, so a test can arm it after boot.
    fn flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.fail_next)
    }

    /// The events-only failure switch.
    fn events_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.fail_events_next)
    }

    /// The `bars`-read failure switch.
    fn bars_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.fail_bars)
    }
}

impl<P> PaperSessionRepository for FlakyRepo<P>
where
    P: PaperSessionRepository + Send + Sync,
{
    async fn insert_session(
        &self,
        draft: &pulse::PaperSessionDraft,
    ) -> Result<PaperSession, DataError> {
        self.inner.insert_session(draft).await
    }

    async fn get_session(&self, id: &PaperSessionId) -> Result<Option<PaperSession>, DataError> {
        self.inner.get_session(id).await
    }

    async fn list_sessions(&self) -> Result<Vec<PaperSession>, DataError> {
        self.inner.list_sessions().await
    }

    async fn all_versions_are_fixtures(
        &self,
        versions: &[pulse::CertifiedDataVersion],
    ) -> Result<bool, DataError> {
        self.inner.all_versions_are_fixtures(versions).await
    }

    async fn insert_fixture_snapshot(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        data_version: &DataVersion,
    ) -> Result<(), DataError> {
        self.inner
            .insert_fixture_snapshot(pair, timeframe, data_version)
            .await
    }

    async fn append_bar(
        &self,
        session_id: &PaperSessionId,
        bars: &[(Timeframe, Candle, bool)],
        events: &[PaperEvent],
    ) -> Result<Vec<PaperEvent>, DataError> {
        self.appends.fetch_add(1, Ordering::SeqCst);
        if self.fail_next.swap(false, Ordering::SeqCst) && !bars.is_empty() {
            return Err(DataError::Db("injected append failure".to_owned()));
        }
        if bars.is_empty() && self.fail_events_next.swap(false, Ordering::SeqCst) {
            return Err(DataError::Db("injected events append failure".to_owned()));
        }
        self.inner.append_bar(session_id, bars, events).await
    }

    async fn events(&self, session_id: &PaperSessionId) -> Result<Vec<PaperEvent>, DataError> {
        self.inner.events(session_id).await
    }

    async fn bars(
        &self,
        session_id: &PaperSessionId,
        timeframe: Timeframe,
    ) -> Result<Vec<Candle>, DataError> {
        if self.fail_bars.load(Ordering::SeqCst) {
            return Err(DataError::Db("injected bars read failure".to_owned()));
        }
        self.inner.bars(session_id, timeframe).await
    }

    async fn count_from_ms(&self, session_id: &PaperSessionId) -> Result<Option<i64>, DataError> {
        self.inner.count_from_ms(session_id).await
    }

    async fn materialise(
        &self,
        session_id: &PaperSessionId,
    ) -> Result<Vec<pulse::SnapshotSelection>, DataError> {
        self.inner.materialise(session_id).await
    }
}

#[tokio::test]
async fn a_failed_append_leaves_no_rows_and_consumes_the_bar_once() {
    let world = ungated_world().await;
    world.source.script(
        Timeframe::M15,
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
            m15_bar(BASE + M15_MS, 60_050, 60_000),
        ],
    );
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let repo = FlakyRepo::new(world.paper());
    let fail_next = repo.flag();
    let mut runtime: PaperRuntime<
        _,
        ScriptedBars,
        pulse::CandleStore,
        SteppedClock,
        LiveEnv<SqliteStrategyRepo<SystemClock>, pulse::BinanceAdapter>,
    > = PaperRuntime::new(
        repo,
        world.source.clone(),
        world.store.clone(),
        world.clock.clone(),
        LiveEnv::new(world.strategies(), pulse::BinanceAdapter::new()),
        0,
        world.log.clone(),
    )
    .without_settle_gate();
    assert!(runtime.boot().await.is_empty(), "boot");
    let bars_before = bar_count(&world, &session.id).await;
    let events_before = world.paper().events(&session.id).await.unwrap().len();

    // Arm the failure, then consume the next bar (00:15 closes at 00:29:59.999).
    world.clock.set(BASE + 2 * M15_MS);
    fail_next.store(true, Ordering::SeqCst);
    let failures = runtime.wake().await;
    assert_eq!(
        failures.len(),
        1,
        "the append failure is reported: {failures:?}"
    );
    assert_eq!(
        bar_count(&world, &session.id).await,
        bars_before,
        "a failed append leaves no partial rows"
    );
    assert_eq!(
        world.paper().events(&session.id).await.unwrap().len(),
        events_before,
        "and no events"
    );

    // The next wake rebuilds from the log and consumes the bar exactly once.
    let failures = runtime.wake().await;
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(bar_count(&world, &session.id).await, bars_before + 1);
    let events = world.paper().events(&session.id).await.unwrap();
    let consumed = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                PaperEvent::BarProcessed { bars, .. }
                    if bars.iter().any(|bar| bar.open_time == BASE + M15_MS)
            )
        })
        .count();
    assert_eq!(consumed, 1, "the bar is consumed exactly once");
}

/// The runtime over a `FlakyRepo` on the world's real repository.
type FlakyRuntime = PaperRuntime<
    FlakyRepo<pulse::SqlitePaperSessionRepo<SteppedClock>>,
    ScriptedBars,
    pulse::CandleStore,
    SteppedClock,
    LiveEnv<SqliteStrategyRepo<SystemClock>, pulse::BinanceAdapter>,
>;

fn flaky_runtime(
    world: &PaperWorld,
    repo: FlakyRepo<pulse::SqlitePaperSessionRepo<SteppedClock>>,
) -> FlakyRuntime {
    PaperRuntime::new(
        repo,
        world.source.clone(),
        world.store.clone(),
        world.clock.clone(),
        LiveEnv::new(world.strategies(), pulse::BinanceAdapter::new()),
        0,
        world.log.clone(),
    )
    .without_settle_gate()
}

fn data_event_count(events: &[PaperEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, PaperEvent::DataEvent { .. }))
        .count()
}

/// A refused `data_event` append is retried on the next wake: the refusal is
/// marked reported only once its event commits.
#[tokio::test]
async fn a_failed_data_event_append_is_retried_on_the_next_wake() {
    let world = ungated_world().await;
    let script = |last_close: i64| {
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
            m15_bar(BASE + M15_MS, 60_050, last_close),
        ]
    };
    world.source.script(Timeframe::M15, script(60_000));
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let repo = FlakyRepo::new(world.paper());
    let fail_events = repo.events_flag();
    let mut runtime = flaky_runtime(&world, repo);
    assert!(runtime.boot().await.is_empty(), "boot");
    world.clock.set(BASE + 2 * M15_MS);
    assert!(runtime.wake().await.is_empty(), "the live bar lands");
    let before = data_event_count(&world.paper().events(&session.id).await.unwrap());

    // The exchange's copy of the newest recorded bar now differs, and the
    // data_event append is refused once.
    world.source.script(Timeframe::M15, script(59_000));
    fail_events.store(true, Ordering::SeqCst);
    let failures = runtime.wake().await;
    assert_eq!(
        failures.len(),
        1,
        "the refused append is reported: {failures:?}"
    );
    assert_eq!(
        data_event_count(&world.paper().events(&session.id).await.unwrap()),
        before,
        "nothing landed"
    );

    // The next wake writes it, and the one after does not repeat it.
    assert!(runtime.wake().await.is_empty(), "the retry lands");
    assert_eq!(
        data_event_count(&world.paper().events(&session.id).await.unwrap()),
        before + 1,
        "the data event is retried after a refused append"
    );
    runtime.wake().await;
    assert_eq!(
        data_event_count(&world.paper().events(&session.id).await.unwrap()),
        before + 1,
        "at most one data event per distinct bar and refusal"
    );
}

/// Round 2 (Codex): a shadow check after a failed bar append rebuilds the
/// engine first, so it never records a verdict over a bar that never
/// committed. The fixture strategy trades, so at some step the uncommitted bar
/// moves the engine; every verdict must still be identical.
#[tokio::test]
async fn a_shadow_check_after_a_failed_append_never_counts_the_uncommitted_bar() {
    let world = ungated_world().await;
    world
        .source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    world
        .source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version = create_version(&world, "rollback", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    let repo = FlakyRepo::new(world.paper());
    let fail_next = repo.flag();
    let mut runtime = flaky_runtime(&world, repo);
    assert!(runtime.boot().await.is_empty(), "boot");
    let first_live = world
        .paper()
        .count_from_ms(&session.id)
        .await
        .unwrap()
        .unwrap();

    let mut trades_seen = 0;
    for step in 0..60 {
        world.clock.set(first_live + (step + 1) * M15_MS);
        fail_next.store(true, Ordering::SeqCst);
        let failures = runtime.wake().await;
        assert!(!failures.is_empty(), "step {step}: the append failed");
        let verdict = runtime
            .shadow_check(&session.id)
            .await
            .expect("the check runs");
        assert!(
            verdict.is_identical(),
            "step {step}: no verdict over an uncommitted bar: {verdict:?}"
        );
        assert!(
            runtime.wake().await.is_empty(),
            "step {step}: the bar lands"
        );
        if let pulse::ShadowResult::Identical { closed_trades, .. } = verdict {
            trades_seen = closed_trades;
        }
    }
    assert!(trades_seen > 0, "the fixture traded (non-vacuous)");
}

/// Round 2 (the rebuild-first ruling): the rebuild a stop's final shadow check runs first
/// can itself fail; the stop still lands — a failed check never vetoes it.
#[tokio::test]
async fn a_stop_lands_when_the_pre_check_rebuild_fails() {
    let world = ungated_world().await;
    world.source.script(
        Timeframe::M15,
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
            m15_bar(BASE + M15_MS, 60_050, 60_000),
        ],
    );
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let repo = FlakyRepo::new(world.paper());
    let fail_next = repo.flag();
    let fail_bars = repo.bars_flag();
    let mut runtime = flaky_runtime(&world, repo);
    assert!(runtime.boot().await.is_empty(), "boot");

    // A failed bar append leaves the session needing a rebuild, and the
    // rebuild's reads then fail.
    world.clock.set(BASE + 2 * M15_MS);
    fail_next.store(true, Ordering::SeqCst);
    assert_eq!(runtime.wake().await.len(), 1, "the append failed");
    fail_bars.store(true, Ordering::SeqCst);

    let label = pulse::NonEmptyLabel::try_new("desk-token-rebuild").unwrap();
    runtime
        .stop(&session.id, pulse::StopActor::Token { label })
        .await
        .expect("a failed pre-check rebuild does not veto the stop");
    let events = world.paper().events(&session.id).await.unwrap();
    assert_eq!(events.last().map(PaperEvent::kind), Some("stop"));
    assert!(runtime.attached_ids().is_empty());
    assert!(
        world
            .log
            .lines()
            .iter()
            .any(|line| line.contains("final shadow check failed")),
        "the failed check is logged"
    );
}

// ---------------------------------------------------------------------------
// the daily cadence
// ---------------------------------------------------------------------------

/// A session whose attach-time shadow check failed is due at once: the boot's
/// daily pass retries it instead of waiting forever.
#[tokio::test]
async fn a_failed_attach_shadow_check_is_retried() {
    let world = ungated_world().await;
    world.source.script(
        Timeframe::M15,
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
        ],
    );
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let repo = FlakyRepo::new(world.paper());
    // The attach-time check's `shadow_checked` append is the first
    // events-only append of the boot.
    repo.events_flag().store(true, Ordering::SeqCst);
    let mut runtime = flaky_runtime(&world, repo);
    let failures = runtime.boot().await;
    assert_eq!(failures.len(), 1, "the attach check failed: {failures:?}");
    let checks = world
        .paper()
        .events(&session.id)
        .await
        .unwrap()
        .iter()
        .filter(|event| matches!(event, PaperEvent::ShadowChecked { .. }))
        .count();
    assert_eq!(checks, 1, "the boot's daily pass retried the failed check");
}

#[tokio::test]
async fn a_daily_shadow_check_runs_on_the_first_wake_after_midnight() {
    let world = ungated_world().await;
    world.source.script(
        Timeframe::M15,
        vec![
            m15_bar(BASE - 2 * M15_MS, 60_100, 60_050),
            m15_bar(BASE - M15_MS, 60_050, 60_000),
            m15_bar(BASE, 60_000, 60_050),
        ],
    );
    let (_version, session) = price_session(&world, Timeframe::M15, None, false).await;
    let mut runtime = world.runtime();
    runtime.boot().await;
    let checks = |events: &[PaperEvent]| {
        events
            .iter()
            .filter(|event| matches!(event, PaperEvent::ShadowChecked { .. }))
            .count()
    };
    let before = checks(&world.paper().events(&session.id).await.unwrap());
    assert_eq!(before, 1, "the boot check ran");

    // Before the next UTC midnight: no new check.
    world.clock.set(BASE + M15_MS);
    runtime.wake().await;
    assert_eq!(
        checks(&world.paper().events(&session.id).await.unwrap()),
        before,
        "no daily check before midnight"
    );

    // The first wake at or after the next midnight: one check.
    world.clock.set(BASE + D1_MS);
    runtime.wake().await;
    assert_eq!(
        checks(&world.paper().events(&session.id).await.unwrap()),
        before + 1,
        "the first wake after midnight checks"
    );
}

// ---------------------------------------------------------------------------
// the REST closed-bar source: cutoff + funding
// ---------------------------------------------------------------------------

/// A scripted `PageSource` keyed by `(endpoint, startTime)`.
struct Pages {
    bodies: Mutex<Vec<(String, Vec<u8>)>>,
}

impl Pages {
    fn new() -> Self {
        Self {
            bodies: Mutex::new(Vec::new()),
        }
    }

    fn script(&self, marker: &str, body: &str) {
        lock(&self.bodies).push((marker.to_owned(), body.as_bytes().to_vec()));
    }

    fn marker(url: &str) -> String {
        let endpoint = if url.contains("fundingRate") {
            "funding"
        } else {
            "klines"
        };
        let start = url
            .split("startTime=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .unwrap_or("?");
        format!("{endpoint}:{start}")
    }
}

impl PageSource for Pages {
    fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, DataError>> + Send {
        let marker = Self::marker(url);
        let body = lock(&self.bodies)
            .iter()
            .find(|(key, _)| *key == marker)
            .map(|(_, body)| body.clone());
        async move { body.ok_or_else(|| DataError::Io(format!("unscripted URL: {url}"))) }
    }
}

#[tokio::test]
async fn rest_closed_bars_drops_the_forming_bar_and_stamps_funding() {
    let pages = Pages::new();
    // The klines page: 00:00, 00:15, 00:30 (closed at 00:45) and 00:45 (the
    // forming bar — close_time 00:59:59.999 >= now).
    pages.script(
        &format!("klines:{BASE}"),
        &format!(
            r#"[
              [{}, "60000.0", "60050.0", "59950.0", "60010.0", "10.0", 0, "0", 1, "0", "0", "0"],
              [{}, "60010.0", "60060.0", "59960.0", "60020.0", "11.0", 0, "0", 1, "0", "0", "0"],
              [{}, "60020.0", "60070.0", "59970.0", "60030.0", "12.0", 0, "0", 1, "0", "0", "0"],
              [{}, "60030.0", "60080.0", "59980.0", "60040.0", "13.0", 0, "0", 1, "0", "0", "0"]
            ]"#,
            BASE,
            BASE + M15_MS,
            BASE + 2 * M15_MS,
            BASE + 3 * M15_MS,
        ),
    );
    // The pagination loop's follow-up page (startTime past the last open) is
    // empty.
    pages.script(&format!("klines:{}", BASE + 3 * M15_MS + 1), "[]");
    // Funding from the boundary: one event at 00:00, stamped on the 00:00 bar.
    pages.script(
        &format!("funding:{BASE}"),
        &format!(r#"[{{"fundingTime": {BASE}, "fundingRate": "0.00010000"}}]"#),
    );
    let clock = SteppedClock::at(BASE + 3 * M15_MS); // 00:45:00
    let source = pulse::RestClosedBars::new(pages, clock);

    let bars = source
        .closed_since(&Pair::new(PAIR), Timeframe::M15, BASE - 1)
        .await
        .unwrap();
    let opens: Vec<i64> = bars.iter().map(|bar| bar.open_time).collect();
    assert_eq!(
        opens,
        vec![BASE, BASE + M15_MS, BASE + 2 * M15_MS],
        "the still-forming 00:45 bar is dropped by the adapter's clock cutoff"
    );
    assert_eq!(
        bars[0].funding_rate,
        Some(Decimal::new(1, 4)),
        "the 00:00 funding event is stamped on the 00:00 bar"
    );
    assert_eq!(bars[1].funding_rate, None, "funding stays sparse");
}
