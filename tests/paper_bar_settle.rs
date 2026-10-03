//! #306 — the runtime records and steps only SETTLED bars.
//!
//! Binance can still update a kline after the runtime's poll grace: a bar read
//! at close + 5 s may differ from the bar the exchange serves a few seconds
//! later. Recording that provisional read made every later re-fetch a "changed
//! re-fetch" `data_event`, which held the session for good (observed live on
//! 2026-10-03, 15m bar `1791035100000`). These suites drive the runtime by its
//! own `next_wake_ms()` over a source that serves a provisional copy of a bar
//! until shortly after its close and the final copy afterwards; the session
//! must record the final copy, write no `data_event`, and keep advancing. A
//! true revision of an already settled bar still takes the `data_event` path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use pulse::{
    BinanceAdapter, Candle, ClosedBarSource, Comparator, Condition, DataError, LiveEnv, Pair,
    PaperEvent, PaperRuntime, PaperSession, PaperSessionId, PaperSessionRepository, PriceField,
    Series, SettlePolicy, StrategyDsl, Timeframe, ValueSource,
};
use rust_decimal::Decimal;
use support::paper::{
    PaperWorld, TestRuntime, bar_of, create_version, m15_bar, promote_session, recorded_bars,
};

const M15_MS: i64 = 900_000;
/// 2025-02-01T00:00:00Z — the fixture grid's origin (the world's clock starts
/// at 00:15:00).
const BASE: i64 = 1_738_368_000_000;
/// The production poll grace (`DEFAULT_POLL_GRACE_MS`).
const GRACE_MS: i64 = 5_000;
/// How long after its close the source keeps serving the provisional copy of
/// a bar — longer than the grace, as observed live, and longer than the settle
/// time, so the first counting read IS provisional and only the confirming
/// read can catch it.
const PROVISIONAL_FOR_MS: i64 = 35_000;

/// The final copy of a bar.
fn final_bar(open_time: i64, open: i64, close: i64) -> Candle {
    m15_bar(open_time, open, close)
}

/// The provisional copy of the same bar: the last trades not yet folded in.
fn provisional_of(bar: &Candle) -> Candle {
    let mut provisional = bar.clone();
    provisional.close -= Decimal::new(1, 1);
    provisional.volume -= Decimal::new(235, 3);
    provisional
}

/// The source's view at `now`: every bar final, except one whose close was
/// less than `PROVISIONAL_FOR_MS` ago, which is served provisional.
fn script_at(world: &PaperWorld, finals: &[Candle], now: i64) {
    script_with(world, finals, now, PROVISIONAL_FOR_MS);
}

/// [`script_at`] with a chosen provisional window.
fn script_with(world: &PaperWorld, finals: &[Candle], now: i64, provisional_for_ms: i64) {
    let served: Vec<Candle> = finals
        .iter()
        .map(|bar| {
            if now < bar.close_time + 1 + provisional_for_ms {
                provisional_of(bar)
            } else {
                bar.clone()
            }
        })
        .collect();
    world.source.script(Timeframe::M15, served);
}

/// Drive the runtime by its own wake schedule through every wake due by
/// `until`, re-scripting the source before every wake.
async fn drive(world: &PaperWorld, runtime: &mut TestRuntime, finals: &[Candle], until: i64) {
    drive_with(world, runtime, finals, until, PROVISIONAL_FOR_MS).await;
}

/// [`drive`] with a chosen provisional window.
async fn drive_with(
    world: &PaperWorld,
    runtime: &mut TestRuntime,
    finals: &[Candle],
    until: i64,
    provisional_for_ms: i64,
) {
    for _ in 0..1_000 {
        let now = world.clock.now();
        if now > until {
            return;
        }
        let next = runtime.next_wake_ms().expect("a wake is always scheduled");
        assert!(next > now, "the next wake {next} lies after now {now}");
        if next > until {
            return;
        }
        world.clock.set(next);
        script_with(world, finals, next, provisional_for_ms);
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
    }
    panic!("the wake schedule never reached {until}");
}

fn data_events(events: &[PaperEvent]) -> Vec<&PaperEvent> {
    events
        .iter()
        .filter(|event| matches!(event, PaperEvent::DataEvent { .. }))
        .collect()
}

async fn session(world: &PaperWorld) -> PaperSession {
    let version = create_version(world, "settle", &pulse::fixture_strategy_dsl()).await;
    promote_session(world, &version, Timeframe::M15, None, false).await
}

async fn recorded(world: &PaperWorld, id: &PaperSessionId) -> Vec<Candle> {
    world.paper().bars(id, Timeframe::M15).await.unwrap()
}

fn world_with_grace(world: PaperWorld) -> PaperWorld {
    PaperWorld {
        grace_ms: GRACE_MS,
        ..world
    }
}

/// The series every suite scripts: two settled lead-in bars, then three live
/// bars from 00:15.
fn finals() -> Vec<Candle> {
    vec![
        final_bar(BASE - M15_MS, 60_050, 60_000),
        final_bar(BASE, 60_000, 60_050),
        final_bar(BASE + M15_MS, 60_050, 60_000),
        final_bar(BASE + 2 * M15_MS, 60_000, 60_050),
        final_bar(BASE + 3 * M15_MS, 60_050, 60_000),
    ]
}

// ---------------------------------------------------------------------------
// The reproduction: a live bar finalized after the poll
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_live_bar_finalized_after_the_poll_is_recorded_final_and_the_session_advances() {
    let world = world_with_grace(PaperWorld::new().await);
    // Promotion a minute after the 00:00 bar's close: the lead-in is final.
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_at(&world, &finals, world.clock.now());
    let session = session(&world).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    drive(&world, &mut runtime, &finals, BASE + 4 * M15_MS + 120_000).await;

    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        data_events(&events).is_empty(),
        "a bar finalized after the poll is no data disagreement: {:?}",
        data_events(&events)
    );
    assert!(
        !world.log.contains("differs from the recorded bar"),
        "{:?}",
        world.log.lines()
    );
    let bars = recorded(&world, &session.id).await;
    assert_eq!(
        bars.iter().map(|bar| bar.open_time).collect::<Vec<_>>(),
        vec![
            BASE - M15_MS,
            BASE,
            BASE + M15_MS,
            BASE + 2 * M15_MS,
            BASE + 3 * M15_MS
        ],
        "the session kept advancing past the late-finalized bar"
    );
    assert_eq!(bars, finals, "every recorded bar is the FINAL copy");
}

// ---------------------------------------------------------------------------
// The same defect at first start: a lead-in bar finalized after promotion
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_lead_in_bar_finalized_after_promotion_is_never_recorded_provisional() {
    let world = world_with_grace(PaperWorld::new().await);
    // Promotion two seconds after the 00:00 bar's close: the exchange still
    // serves its provisional copy.
    world.clock.set(BASE + M15_MS + 2_000);
    let finals = finals();
    script_at(&world, &finals, world.clock.now());
    let session = session(&world).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    drive(&world, &mut runtime, &finals, BASE + 4 * M15_MS + 120_000).await;

    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
    let bars = recorded(&world, &session.id).await;
    assert_eq!(
        bars.last().map(|bar| bar.open_time),
        Some(BASE + 3 * M15_MS),
        "the session kept advancing"
    );
    for bar in &bars {
        let expected = finals
            .iter()
            .find(|candidate| candidate.open_time == bar.open_time)
            .expect("a scripted bar");
        assert_eq!(bar, expected, "bar {} is the FINAL copy", bar.open_time);
    }
}

// ---------------------------------------------------------------------------
// The safety net stays: a revision of a SETTLED bar is a data event
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_revision_of_a_settled_bar_is_still_one_data_event_and_never_a_replacement() {
    let world = world_with_grace(PaperWorld::new().await);
    // Promotion a minute after the 00:00 bar's close: the lead-in is final.
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_at(&world, &finals, world.clock.now());
    let session = session(&world).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    drive(&world, &mut runtime, &finals, BASE + 2 * M15_MS + 120_000).await;
    let before = recorded(&world, &session.id).await;
    assert_eq!(
        before.last().map(|bar| bar.open_time),
        Some(BASE + M15_MS),
        "the 00:15 bar settled and landed"
    );

    // Long after it settled, the exchange's copy of the 00:15 bar changes.
    let mut revised = finals.clone();
    revised[2].close = Decimal::from(59_000);
    drive(&world, &mut runtime, &revised, BASE + 4 * M15_MS + 120_000).await;

    let events = world.paper().events(&session.id).await.unwrap();
    assert_eq!(
        data_events(&events).len(),
        1,
        "exactly one data event for the revision: {:?}",
        data_events(&events)
    );
    assert!(world.log.contains("differs from the recorded bar"));
    let after = recorded(&world, &session.id).await;
    assert_eq!(
        after, before,
        "the recorded bar is never replaced, and the session holds"
    );
}

// ---------------------------------------------------------------------------
// The latency stays bounded
// ---------------------------------------------------------------------------

/// Wake at each instant the runtime schedules until the 00:15 bar lands;
/// return the wake instants and the instant it landed.
async fn wakes_until_landed(
    world: &PaperWorld,
    runtime: &mut TestRuntime,
    id: &PaperSessionId,
    finals: &[Candle],
    provisional_for_ms: i64,
) -> (Vec<i64>, i64) {
    let mut wakes = Vec::new();
    for _ in 0..20 {
        let next = runtime.next_wake_ms().expect("a wake is always scheduled");
        world.clock.set(next);
        script_with(world, finals, next, provisional_for_ms);
        assert!(runtime.wake().await.is_empty());
        wakes.push(next);
        if recorded(world, id).await.last().map(|bar| bar.open_time) == Some(BASE + M15_MS) {
            return (wakes, next);
        }
    }
    panic!("the 00:15 bar never landed: wakes {wakes:?}");
}

#[tokio::test]
async fn a_steady_bar_lands_one_repoll_after_the_settle_time() {
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_at(&world, &finals, world.clock.now());
    let session = session(&world).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    // The lead-in lands at its confirming read; the 00:15 bar is still open.
    drive(&world, &mut runtime, &finals, BASE + M15_MS + 120_000).await;

    let close = BASE + 2 * M15_MS;
    let policy = SettlePolicy::DEFAULT;
    let (wakes, landed) = wakes_until_landed(&world, &mut runtime, &session.id, &finals, 0).await;
    assert_eq!(
        wakes,
        vec![
            close + policy.settle_ms,
            close + policy.settle_ms + policy.repoll_ms
        ],
        "the first poll waits out the settle time; one re-poll confirms"
    );
    assert_eq!(
        landed,
        close + 40_000,
        "a steady bar lands 40 s after its close"
    );
}

#[tokio::test]
async fn a_bar_that_changes_after_the_settle_time_is_confirmed_again_before_it_lands() {
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_at(&world, &finals, world.clock.now());
    let session = session(&world).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    // The lead-in lands at its confirming read; the 00:15 bar is still open.
    drive(&world, &mut runtime, &finals, BASE + M15_MS + 120_000).await;

    // Provisional for 35 s: the 30 s read is provisional, the 40 s read final.
    let close = BASE + 2 * M15_MS;
    let (wakes, landed) =
        wakes_until_landed(&world, &mut runtime, &session.id, &finals, 35_000).await;
    assert_eq!(
        wakes,
        vec![close + 30_000, close + 40_000, close + 50_000],
        "a read that differs from the one before restarts the confirmation"
    );
    assert_eq!(landed, close + 50_000);
    let bars = recorded(&world, &session.id).await;
    assert_eq!(bars.last(), Some(&finals[2]), "the FINAL copy landed");
    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
}

// ---------------------------------------------------------------------------
// A restart or a promotion between the settle time and the final copy
// ---------------------------------------------------------------------------

/// The window in which a restart lands: the first counting read (30 s) has
/// already seen the provisional copy, the final copy arrives at 37 s.
const LATE_FINAL_MS: i64 = 37_000;

#[tokio::test]
async fn a_restart_between_the_settle_time_and_the_final_copy_records_the_final_copy() {
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_with(&world, &finals, world.clock.now(), LATE_FINAL_MS);
    let session = session(&world).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    let close = BASE + 2 * M15_MS;
    // Run to the first counting read of the 00:15 bar (provisional), then die.
    drive_with(&world, &mut runtime, &finals, close + 30_000, LATE_FINAL_MS).await;
    assert_eq!(
        world.clock.now(),
        close + 30_000,
        "the provisional copy was read"
    );
    drop(runtime);

    // A fresh runtime boots at 32 s: its catch-up reads the provisional copy
    // too, and must not confirm it with a second read at the same instant.
    world.clock.set(close + 32_000);
    script_with(&world, &finals, world.clock.now(), LATE_FINAL_MS);
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "reboot");
    drive_with(
        &world,
        &mut runtime,
        &finals,
        BASE + 4 * M15_MS + 120_000,
        LATE_FINAL_MS,
    )
    .await;

    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
    let bars = recorded(&world, &session.id).await;
    assert_eq!(bars, finals, "every recorded bar is the FINAL copy");
}

#[tokio::test]
async fn a_first_start_between_the_settle_time_and_the_final_copy_records_the_final_copy() {
    let world = world_with_grace(PaperWorld::new().await);
    // Promotion 32 s after the 00:00 bar's close: past the settle time, and
    // the exchange still serves its provisional copy until 37 s.
    world.clock.set(BASE + M15_MS + 32_000);
    let finals = finals();
    script_with(&world, &finals, world.clock.now(), LATE_FINAL_MS);
    let session = session(&world).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    drive_with(
        &world,
        &mut runtime,
        &finals,
        BASE + 4 * M15_MS + 120_000,
        LATE_FINAL_MS,
    )
    .await;

    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
    let bars = recorded(&world, &session.id).await;
    assert_eq!(
        bars.last().map(|bar| bar.open_time),
        Some(BASE + 3 * M15_MS)
    );
    for bar in &bars {
        let expected = finals
            .iter()
            .find(|candidate| candidate.open_time == bar.open_time)
            .expect("a scripted bar");
        assert_eq!(bar, expected, "bar {} is the FINAL copy", bar.open_time);
    }
}

// ---------------------------------------------------------------------------
// A primary bar waits for the higher bar that closes with it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_primary_bar_waits_for_the_higher_bar_that_closes_with_it() {
    const H_MS: i64 = 3_600_000;
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + 8 * H_MS + 60_000);
    let m15: Vec<Candle> = (0..22)
        .map(|i| m15_bar(BASE + 7 * H_MS + i * M15_MS, 60_100, 60_050))
        .collect();
    world.source.script(Timeframe::M15, m15);
    // The H4 bar opening 08:00 closes with the M15 bar opening 11:45; the
    // exchange serves a provisional copy of it until 35 s past that close, so
    // its first counting read is provisional while the M15 bar is steady.
    let h4_early = support::paper::bar_of(Timeframe::H4, BASE + 4 * H_MS, 60_100, 60_050);
    let h4_final = support::paper::bar_of(Timeframe::H4, BASE + 8 * H_MS, 60_050, 60_000);
    let h4_close = h4_final.close_time + 1;
    let script_h4 = |now: i64| {
        let mut h4 = h4_final.clone();
        if now < h4_close + 35_000 {
            h4.close -= Decimal::new(1, 1);
        }
        world
            .source
            .script(Timeframe::H4, vec![h4_early.clone(), h4]);
    };
    script_h4(world.clock.now());
    let version = create_version(&world, "settle-h4", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    for _ in 0..200 {
        let next = runtime.next_wake_ms().expect("a wake is always scheduled");
        if next > h4_close + 16 * 60_000 {
            break;
        }
        world.clock.set(next);
        script_h4(next);
        assert!(runtime.wake().await.is_empty());
    }

    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
    let last_primary = BASE + 11 * H_MS + 45 * 60_000;
    let batch = events
        .iter()
        .find_map(|event| match event {
            PaperEvent::BarProcessed { bars, .. }
                if bars.iter().any(|bar| {
                    bar.timeframe == Timeframe::M15 && bar.open_time == last_primary
                }) =>
            {
                Some(bars.clone())
            }
            _ => None,
        })
        .expect("the 11:45 bar was consumed");
    assert!(
        batch
            .iter()
            .any(|bar| bar.timeframe == Timeframe::H4 && bar.open_time == BASE + 8 * H_MS),
        "the settled H4 bar rides the batch of the M15 bar closing with it: {batch:?}"
    );
    let h4 = world
        .paper()
        .bars(&session.id, Timeframe::H4)
        .await
        .unwrap();
    assert_eq!(h4.last(), Some(&h4_final), "the FINAL H4 copy landed");
}

// ---------------------------------------------------------------------------
// A failed confirming poll keeps the re-poll
// ---------------------------------------------------------------------------

/// The scripted source with injected faults: every read fails while `fail` is
/// set, the next `empty_reads` reads return no bars, and a read with
/// `slow_ms` set advances the clock by that much before it returns (a slow
/// REST request).
#[derive(Clone)]
struct FaultyBars {
    inner: support::paper::ScriptedBars,
    clock: support::paper::SteppedClock,
    fail: Arc<AtomicBool>,
    empty_reads: Arc<AtomicUsize>,
    slow_ms: Arc<AtomicI64>,
}

impl ClosedBarSource for FaultyBars {
    fn closed_since(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        since_ms: i64,
    ) -> impl Future<Output = Result<Vec<Candle>, DataError>> + Send {
        let fail = self.fail.load(Ordering::SeqCst);
        let empty = self
            .empty_reads
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        let read = self.inner.closed_since(pair, timeframe, since_ms);
        let (clock, slow_ms) = (self.clock.clone(), self.slow_ms.load(Ordering::SeqCst));
        async move {
            let bars = read.await;
            clock.advance(slow_ms);
            if fail {
                Err(DataError::Io("scripted outage".to_owned()))
            } else if empty {
                Ok(Vec::new())
            } else {
                bars
            }
        }
    }
}

/// A runtime reading through [`FaultyBars`].
type FaultyRuntime = PaperRuntime<
    pulse::SqlitePaperSessionRepo<support::paper::SteppedClock>,
    FaultyBars,
    pulse::CandleStore,
    support::paper::SteppedClock,
    LiveEnv<pulse::SqliteStrategyRepo<pulse::SystemClock>, BinanceAdapter>,
>;

/// A runtime over `world` reading through a [`FaultyBars`] it returns.
fn faulty_runtime(world: &PaperWorld) -> (FaultyBars, FaultyRuntime) {
    let source = FaultyBars {
        inner: world.source.clone(),
        clock: world.clock.clone(),
        fail: Arc::new(AtomicBool::new(false)),
        empty_reads: Arc::new(AtomicUsize::new(0)),
        slow_ms: Arc::new(AtomicI64::new(0)),
    };
    let runtime = PaperRuntime::new(
        world.paper(),
        source.clone(),
        world.store.clone(),
        world.clock.clone(),
        LiveEnv::new(world.strategies(), BinanceAdapter::new()),
        GRACE_MS,
        world.log.clone(),
    );
    (source, runtime)
}

/// Wake at every scheduled instant (never earlier than now) through `until`,
/// re-scripting the source before each wake.
macro_rules! wake_through {
    ($world:expr, $runtime:expr, $finals:expr, $provisional_for:expr, $until:expr) => {
        for _ in 0..200 {
            let next = $runtime.next_wake_ms().unwrap().max($world.clock.now());
            if next > $until {
                break;
            }
            $world.clock.set(next);
            script_with(&$world, &$finals, next, $provisional_for);
            let _ = $runtime.wake().await;
        }
    };
}

#[tokio::test]
async fn a_failed_confirming_poll_keeps_the_repoll() {
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_with(&world, &finals, world.clock.now(), 0);
    let session = session(&world).await;
    let (source, mut runtime) = faulty_runtime(&world);
    assert!(runtime.boot().await.is_empty(), "boot");
    let close = BASE + 2 * M15_MS;
    // The lead-in confirms; then the first counting read of the 00:15 bar.
    wake_through!(world, runtime, finals, 0, close + 30_000);
    assert_eq!(world.clock.now(), close + 30_000);
    assert_eq!(runtime.next_wake_ms(), Some(close + 40_000));

    // The confirming poll fails: the re-poll is kept, not lost to the next
    // bar boundary.
    source.fail.store(true, Ordering::SeqCst);
    world.clock.set(close + 40_000);
    assert!(!runtime.wake().await.is_empty(), "the outage is reported");
    assert_eq!(runtime.next_wake_ms(), Some(close + 50_000));

    source.fail.store(false, Ordering::SeqCst);
    world.clock.set(close + 50_000);
    script_with(&world, &finals, close + 50_000, 0);
    assert!(runtime.wake().await.is_empty());
    let bars = recorded(&world, &session.id).await;
    assert_eq!(
        bars.last(),
        Some(&finals[2]),
        "the bar lands at the kept re-poll"
    );
}

#[tokio::test]
async fn an_empty_confirming_poll_keeps_the_repoll() {
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_with(&world, &finals, world.clock.now(), 0);
    let session = session(&world).await;
    let (source, mut runtime) = faulty_runtime(&world);
    assert!(runtime.boot().await.is_empty(), "boot");
    let close = BASE + 2 * M15_MS;
    wake_through!(world, runtime, finals, 0, close + 30_000);
    assert_eq!(runtime.next_wake_ms(), Some(close + 40_000));

    // The confirming poll succeeds with no bars: no evidence either way.
    source.empty_reads.store(1, Ordering::SeqCst);
    world.clock.set(close + 40_000);
    assert!(runtime.wake().await.is_empty());
    assert_eq!(runtime.next_wake_ms(), Some(close + 50_000));

    world.clock.set(close + 50_000);
    assert!(runtime.wake().await.is_empty());
    let bars = recorded(&world, &session.id).await;
    assert_eq!(
        bars.last(),
        Some(&finals[2]),
        "the bar lands at the kept re-poll"
    );
}

#[tokio::test]
async fn a_slow_counting_read_is_timed_when_it_returns() {
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    // Provisional until 45 s past the close.
    let window = 45_000;
    script_with(&world, &finals, world.clock.now(), window);
    let session = session(&world).await;
    let (source, mut runtime) = faulty_runtime(&world);
    assert!(runtime.boot().await.is_empty(), "boot");
    let close = BASE + 2 * M15_MS;
    wake_through!(world, runtime, finals, window, close + 29_999);

    // The first counting read starts at 30 s and returns at 42 s.
    source.slow_ms.store(12_000, Ordering::SeqCst);
    world.clock.set(close + 30_000);
    script_with(&world, &finals, close + 30_000, window);
    assert!(runtime.wake().await.is_empty());
    source.slow_ms.store(0, Ordering::SeqCst);
    assert_eq!(world.clock.now(), close + 42_000);

    // A read right away (42 s) must not confirm the provisional copy; later
    // bars are steady.
    wake_through!(world, runtime, finals, window, close + 120_000);
    wake_through!(world, runtime, finals, 0, BASE + 4 * M15_MS + 120_000);
    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
    let bars = recorded(&world, &session.id).await;
    assert_eq!(bars, finals, "every recorded bar is the FINAL copy");
}

#[tokio::test]
async fn an_empty_lead_in_confirmation_starts_the_first_start_over() {
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_with(&world, &finals, world.clock.now(), 0);
    let session = session(&world).await;
    let (source, mut runtime) = faulty_runtime(&world);
    assert!(
        runtime.boot().await.is_empty(),
        "boot: the probe is pending"
    );
    assert!(recorded(&world, &session.id).await.is_empty());

    // The confirming read returns nothing: the session must not attach with
    // a cut (cold) lead-in.
    source.empty_reads.store(1, Ordering::SeqCst);
    wake_through!(world, runtime, finals, 0, BASE + 2 * M15_MS);
    let flags: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT open_time, lead_in FROM paper_bar WHERE session_id = ?1 ORDER BY open_time",
    )
    .bind(session.id.as_str())
    .fetch_all(world.db.pool())
    .await
    .unwrap();
    assert_eq!(
        flags,
        vec![(BASE - M15_MS, 1), (BASE, 1)],
        "the whole lead-in landed, at a later confirming read"
    );
}

// ---------------------------------------------------------------------------
// R2-4: a successful read that omits the due higher bar must not step the
// primary without it
// ---------------------------------------------------------------------------

const H_MS: i64 = 3_600_000;
const D1_MS: i64 = 86_400_000;

/// The fixture entry with a condition on `series`: the compiled strategy then
/// really READS that series (`Series::Htf` / `Series::D1`), so a step that
/// omits a due bar of it would read the stale one — an HTF-insensitive
/// fixture would be no determinism evidence.
fn series_operand_dsl(series: Series, primary_below: i64, series_above: i64) -> StrategyDsl {
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
                    value: Decimal::from(primary_below),
                },
            },
            Condition::Compare {
                lhs: ValueSource::Price {
                    series,
                    field: PriceField::Close,
                },
                op: Comparator::Gt,
                rhs: ValueSource::Constant {
                    value: Decimal::from(series_above),
                },
            },
        ],
    };
    dsl
}

/// Contiguous M15 bars on the fixture grid, opening every 15 minutes from
/// `from` through `to` (inclusive), flat at `60_050`.
fn m15_run(from: i64, to: i64) -> Vec<Candle> {
    let mut bars = Vec::new();
    let mut open = from;
    while open <= to {
        bars.push(m15_bar(open, 60_100, 60_050));
        open += M15_MS;
    }
    bars
}

/// Every consumed primary bar's batch: its `open_time` and the `(timeframe,
/// open_time)` of every bar that rode with it, in order. The primary is first
/// by construction.
fn batches(events: &[PaperEvent]) -> Vec<(i64, Vec<(Timeframe, i64)>)> {
    events
        .iter()
        .filter_map(|event| match event {
            PaperEvent::BarProcessed { bars, .. } => {
                let primary = bars.first()?;
                Some((
                    primary.open_time,
                    bars.iter()
                        .map(|bar| (bar.timeframe, bar.open_time))
                        .collect(),
                ))
            }
            _ => None,
        })
        .collect()
}

/// Drive the runtime by its own wake schedule through every wake due by
/// `until`, re-scripting the source before each wake. Returns the wake
/// instants. Bounded, so a deadlock fails fast instead of hanging the test.
async fn drive_scripted(
    world: &PaperWorld,
    runtime: &mut TestRuntime,
    script: &impl Fn(i64),
    until: i64,
) -> Vec<i64> {
    let mut wakes = Vec::new();
    for _ in 0..2_000 {
        let now = world.clock.now();
        let next = runtime.next_wake_ms().expect("a wake is always scheduled");
        assert!(next > now, "the next wake {next} lies after now {now}");
        if next > until {
            return wakes;
        }
        world.clock.set(next);
        script(next);
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
        wakes.push(next);
    }
    panic!("the wake schedule never reached {until}");
}

/// The R2-4 higher-timeframe scenario: an HTF-reading M15 session booted at
/// 08:01, whose 04:00 H4 bar the probe records as lead-in while the 08:00 H4
/// bar — the one closing with the 11:45 M15 bar — is served only from
/// `serve_at`, two minutes past the primary's own confirming read.
struct DelayedHigherBar {
    world: PaperWorld,
    session: PaperSession,
    early: Candle,
    late: Candle,
    m15: Vec<Candle>,
    /// The M15 bar closing with the delayed H4 bar.
    primary: i64,
    /// The instant the source first serves the delayed H4 bar.
    serve_at: i64,
}

impl DelayedHigherBar {
    async fn htf() -> Self {
        let world = world_with_grace(PaperWorld::new().await);
        world.clock.set(BASE + 8 * H_MS + 60_000);
        let m15 = m15_run(BASE, BASE + 12 * H_MS + 30 * 60_000);
        // 60_050 lies above the HTF threshold, so a stale 04:00 copy would
        // satisfy the entry; the 08:00 copy at 59_900 would not.
        let early = bar_of(Timeframe::H4, BASE + 4 * H_MS, 60_100, 60_050);
        let late = bar_of(Timeframe::H4, BASE + 8 * H_MS, 59_950, 59_900);
        let primary = BASE + 11 * H_MS + 45 * 60_000;
        let serve_at = primary + M15_MS + 2 * 60_000;
        let dsl = series_operand_dsl(Series::Htf, 60_100, 60_000);
        let version = create_version(&world, "r2-4-htf", &dsl).await;
        let session =
            promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
        let scenario = Self {
            world,
            session,
            early,
            late,
            m15,
            primary,
            serve_at,
        };
        scenario.script(scenario.world.clock.now());
        scenario
    }

    /// The source's script at `now`: the M15 series is steady, and the delayed
    /// H4 bar exists only from [`Self::serve_at`] on — the successful reads
    /// before that close without it.
    fn script(&self, now: i64) {
        self.world.source.script(Timeframe::M15, self.m15.clone());
        let mut h4 = vec![self.early.clone()];
        if now >= self.serve_at {
            h4.push(self.late.clone());
        }
        self.world.source.script(Timeframe::H4, h4);
    }

    /// The shared-close primary bar's own confirming read: `settle_ms` past its
    /// close plus one `repoll_ms`.
    fn own_confirm(&self) -> i64 {
        self.primary + M15_MS + SettlePolicy::DEFAULT.settle_ms + SettlePolicy::DEFAULT.repoll_ms
    }

    async fn drive(&self, runtime: &mut TestRuntime, until: i64) -> Vec<i64> {
        drive_scripted(&self.world, runtime, &|now| self.script(now), until).await
    }
}

#[tokio::test]
async fn a_read_that_omits_the_due_higher_bar_holds_the_primary_until_it_settles() {
    let scenario = DelayedHigherBar::htf().await;
    let world = &scenario.world;
    let repoll = SettlePolicy::DEFAULT.repoll_ms;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    // The primary bar's own confirming read is satisfied, but no read has yet
    // seen the H4 bar that closes with it: the primary must not advance.
    let wakes = scenario.drive(&mut runtime, scenario.own_confirm()).await;
    assert_eq!(
        wakes.last().copied(),
        Some(scenario.own_confirm()),
        "the driver reached the primary's own confirming read"
    );
    assert_eq!(
        recorded_bars(world, &scenario.session.id, Timeframe::H4).await,
        vec![scenario.early.clone()],
        "the probe recorded the earlier H4 bar, so the due 08:00 bar lies inside the fetch window"
    );
    assert_eq!(
        recorded(world, &scenario.session.id)
            .await
            .last()
            .map(|bar| bar.open_time),
        Some(scenario.primary - M15_MS),
        "the primary bar closing with the missing H4 bar did NOT advance"
    );
    assert_eq!(
        runtime.next_wake_ms(),
        Some(scenario.own_confirm() + repoll),
        "the missing higher bar keeps the short re-poll alive"
    );

    // The H4 bar arrives at `serve_at`, past the primary's own confirmation.
    // A first counting read is not enough: neither bar lands.
    scenario.drive(&mut runtime, scenario.serve_at).await;
    assert_eq!(world.clock.now(), scenario.serve_at);
    assert_eq!(
        recorded_bars(world, &scenario.session.id, Timeframe::H4).await,
        vec![scenario.early.clone()],
        "the H4 bar does not land on its first counting read"
    );
    assert_eq!(
        recorded(world, &scenario.session.id)
            .await
            .last()
            .map(|bar| bar.open_time),
        Some(scenario.primary - M15_MS),
        "the primary waits for the confirming read"
    );
    assert_eq!(
        runtime.next_wake_ms(),
        Some(scenario.serve_at + repoll),
        "the confirming read is the next re-poll"
    );

    // The confirming read lands the higher bar and the primary in ONE batch.
    scenario
        .drive(&mut runtime, scenario.serve_at + repoll)
        .await;
    assert_eq!(
        recorded_bars(world, &scenario.session.id, Timeframe::H4).await,
        vec![scenario.early.clone(), scenario.late.clone()],
        "the FINAL H4 copy landed once settled"
    );
    let bars = recorded(world, &scenario.session.id).await;
    assert_eq!(
        bars.last().map(|bar| bar.open_time),
        Some(scenario.primary),
        "the primary landed with it"
    );
    let events = world.paper().events(&scenario.session.id).await.unwrap();
    let batch = events
        .iter()
        .find_map(|event| match event {
            PaperEvent::BarProcessed { bars, .. }
                if bars.iter().any(|bar| {
                    bar.timeframe == Timeframe::M15 && bar.open_time == scenario.primary
                }) =>
            {
                Some(bars.clone())
            }
            _ => None,
        })
        .expect("the shared-close primary was consumed");
    assert!(
        batch
            .iter()
            .any(|bar| bar.timeframe == Timeframe::H4 && bar.open_time == scenario.late.open_time),
        "the settled H4 bar rides the same batch: {batch:?}"
    );
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
    assert!(
        !world.log.contains("step refused"),
        "{:?}",
        world.log.lines()
    );
}

#[tokio::test]
async fn after_a_delayed_higher_bar_live_and_rebuilt_history_are_identical() {
    let scenario = DelayedHigherBar::htf().await;
    let world = &scenario.world;
    let repoll = SettlePolicy::DEFAULT.repoll_ms;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    scenario
        .drive(&mut runtime, scenario.serve_at + repoll + 15 * 60_000)
        .await;

    let live_bars = (
        recorded(world, &scenario.session.id).await,
        recorded_bars(world, &scenario.session.id, Timeframe::H4).await,
    );
    let live_batches = batches(&world.paper().events(&scenario.session.id).await.unwrap());
    assert!(
        live_batches
            .iter()
            .any(|(open, bars)| *open == scenario.primary
                && bars
                    .iter()
                    .any(|(timeframe, bar)| *timeframe == Timeframe::H4
                        && *bar == scenario.late.open_time)),
        "the live run fed the H4 bar to the primary bar closing with it: {live_batches:?}"
    );

    // A fresh runtime rebuilds from the recorded bars and replays the log: the
    // rebuilt engine must reproduce the live trades, and the shadow run over
    // the recorded bars must agree with them. A different higher input by
    // primary bar would show up as drift here.
    drop(runtime);
    let mut rebooted = world.runtime();
    assert!(
        rebooted.boot().await.is_empty(),
        "the rebuild agrees with the log"
    );
    let verdict = rebooted.shadow_check(&scenario.session.id).await.unwrap();
    assert!(verdict.is_identical(), "shadow drift: {verdict:?}");

    let rebooted_bars = (
        recorded(world, &scenario.session.id).await,
        recorded_bars(world, &scenario.session.id, Timeframe::H4).await,
    );
    let rebooted_events = world.paper().events(&scenario.session.id).await.unwrap();
    assert_eq!(
        rebooted_bars, live_bars,
        "the paper history is unchanged by the restart"
    );
    assert_eq!(
        batches(&rebooted_events),
        live_batches,
        "the same higher inputs rode the same primary bars"
    );
    assert!(
        data_events(&rebooted_events).is_empty(),
        "{:?}",
        data_events(&rebooted_events)
    );
}

#[tokio::test]
async fn a_session_with_no_recorded_higher_bar_advances_while_no_eligible_bar_is_due() {
    // A session with an H4 timeframe whose source serves NO H4 bar at all:
    // every due H4 bar of the driven window opened at or before the session's
    // fetch floor, so none can ever be delivered and none is required — the
    // primary must keep advancing rather than deadlock on a bar outside the
    // window. (The fixture strategy, so the empty series itself is not a
    // refusal: an `Htf` operand requires a higher series to exist at all.)
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + 8 * H_MS + 60_000);
    let m15 = m15_run(BASE, BASE + 16 * H_MS);
    let script = |_now: i64| {
        world.source.script(Timeframe::M15, m15.clone());
        world.source.script(Timeframe::H4, Vec::new());
    };
    script(world.clock.now());
    let version = create_version(&world, "r2-4-floor", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    // The M15 11:30 bar is the last whose due H4 bar (04:00) opened before the
    // fetch floor; from the 11:45 bar on, the due bar (08:00) lies inside the
    // window and would be waited for — so the drive stops at 11:30's own
    // confirming read.
    let last = BASE + 11 * H_MS + 30 * 60_000;
    let until = last + M15_MS + SettlePolicy::DEFAULT.settle_ms + SettlePolicy::DEFAULT.repoll_ms;
    let wakes = drive_scripted(&world, &mut runtime, &script, until).await;
    assert_eq!(
        wakes.last().copied(),
        Some(until),
        "the schedule reached the bar's own confirming read"
    );
    let bars = recorded(&world, &session.id).await;
    assert_eq!(
        bars.last().map(|bar| bar.open_time),
        Some(last),
        "the session advanced through every bar with no eligible due higher bar"
    );
    assert!(
        recorded_bars(&world, &session.id, Timeframe::H4)
            .await
            .is_empty(),
        "no H4 bar ever arrived"
    );
    let events = world.paper().events(&session.id).await.unwrap();
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
    assert!(
        runtime.next_wake_ms().is_some(),
        "the session is awake, not deadlocked"
    );
}

#[tokio::test]
async fn the_same_guard_holds_the_primary_for_a_missing_due_d1_bar() {
    let world = world_with_grace(PaperWorld::new().await);
    // Boot at 00:01 on 2025-02-01: the first live M15 bar opens 00:00 (still
    // forming), and the daily probe reaches back past the 2025-01-31 bar, so
    // that bar is recorded as lead-in and the 2025-02-01 D1 bar — closing with
    // the 23:45 M15 bar — is due and inside the fetch window.
    world.clock.set(BASE + 60_000);
    let m15 = m15_run(BASE - 32 * H_MS, BASE + D1_MS + 30 * 60_000);
    let early = bar_of(Timeframe::D1, BASE - D1_MS, 60_100, 60_050);
    let late = bar_of(Timeframe::D1, BASE, 59_950, 59_900);
    let primary = BASE + 23 * H_MS + 45 * 60_000;
    let serve_at = primary + M15_MS + 2 * 60_000;
    let script = |now: i64| {
        world.source.script(Timeframe::M15, m15.clone());
        let mut d1 = vec![early.clone()];
        if now >= serve_at {
            d1.push(late.clone());
        }
        world.source.script(Timeframe::D1, d1);
    };
    script(world.clock.now());
    let dsl = series_operand_dsl(Series::D1, 60_100, 60_000);
    let version = create_version(&world, "r2-4-d1", &dsl).await;
    let session = promote_session(&world, &version, Timeframe::M15, None, true).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    let repoll = SettlePolicy::DEFAULT.repoll_ms;
    let own_confirm =
        primary + M15_MS + SettlePolicy::DEFAULT.settle_ms + SettlePolicy::DEFAULT.repoll_ms;
    let wakes = drive_scripted(&world, &mut runtime, &script, own_confirm).await;
    assert_eq!(wakes.last().copied(), Some(own_confirm));
    assert_eq!(
        recorded_bars(&world, &session.id, Timeframe::D1).await,
        vec![early.clone()],
        "the daily lead-in landed, so the due 2025-02-01 bar lies inside the fetch window"
    );
    assert_eq!(
        recorded(&world, &session.id)
            .await
            .last()
            .map(|bar| bar.open_time),
        Some(primary - M15_MS),
        "the primary bar closing with the missing D1 bar did NOT advance"
    );
    assert_eq!(
        runtime.next_wake_ms(),
        Some(own_confirm + repoll),
        "the missing D1 bar keeps the short re-poll alive"
    );

    drive_scripted(&world, &mut runtime, &script, serve_at).await;
    assert_eq!(world.clock.now(), serve_at);
    assert_eq!(
        recorded_bars(&world, &session.id, Timeframe::D1).await,
        vec![early.clone()],
        "the D1 bar does not land on its first counting read"
    );
    drive_scripted(&world, &mut runtime, &script, serve_at + repoll).await;
    assert_eq!(
        recorded_bars(&world, &session.id, Timeframe::D1).await,
        vec![early.clone(), late.clone()],
        "the FINAL D1 copy landed once settled"
    );
    let bars = recorded(&world, &session.id).await;
    assert_eq!(
        bars.last().map(|bar| bar.open_time),
        Some(primary),
        "the primary landed with it"
    );
    let events = world.paper().events(&session.id).await.unwrap();
    let batch = events
        .iter()
        .find_map(|event| match event {
            PaperEvent::BarProcessed { bars, .. }
                if bars
                    .iter()
                    .any(|bar| bar.timeframe == Timeframe::M15 && bar.open_time == primary) =>
            {
                Some(bars.clone())
            }
            _ => None,
        })
        .expect("the shared-close primary was consumed");
    assert!(
        batch
            .iter()
            .any(|bar| bar.timeframe == Timeframe::D1 && bar.open_time == late.open_time),
        "the settled D1 bar rides the same batch: {batch:?}"
    );
    assert!(
        data_events(&events).is_empty(),
        "{:?}",
        data_events(&events)
    );
}
