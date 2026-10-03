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
use std::sync::atomic::{AtomicBool, Ordering};

use pulse::{
    BinanceAdapter, Candle, ClosedBarSource, DataError, LiveEnv, Pair, PaperEvent, PaperRuntime,
    PaperSession, PaperSessionId, PaperSessionRepository, SettlePolicy, Timeframe,
};
use rust_decimal::Decimal;
use support::paper::{PaperWorld, TestRuntime, create_version, m15_bar, promote_session};

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

/// The scripted source, failing every read while `fail` is set.
#[derive(Clone)]
struct FailingBars {
    inner: support::paper::ScriptedBars,
    fail: Arc<AtomicBool>,
}

impl ClosedBarSource for FailingBars {
    fn closed_since(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        since_ms: i64,
    ) -> impl Future<Output = Result<Vec<Candle>, DataError>> + Send {
        let fail = self.fail.load(Ordering::SeqCst);
        let read = self.inner.closed_since(pair, timeframe, since_ms);
        async move {
            if fail {
                Err(DataError::Io("scripted outage".to_owned()))
            } else {
                read.await
            }
        }
    }
}

#[tokio::test]
async fn a_failed_confirming_poll_keeps_the_repoll() {
    let world = world_with_grace(PaperWorld::new().await);
    world.clock.set(BASE + M15_MS + 60_000);
    let finals = finals();
    script_with(&world, &finals, world.clock.now(), 0);
    let session = session(&world).await;
    let fail = Arc::new(AtomicBool::new(false));
    let mut runtime = PaperRuntime::new(
        world.paper(),
        FailingBars {
            inner: world.source.clone(),
            fail: fail.clone(),
        },
        world.store.clone(),
        world.clock.clone(),
        LiveEnv::new(world.strategies(), BinanceAdapter::new()),
        GRACE_MS,
        world.log.clone(),
    );
    assert!(runtime.boot().await.is_empty(), "boot");
    let close = BASE + 2 * M15_MS;
    // The lead-in confirms; then the first counting read of the 00:15 bar.
    for _ in 0..10 {
        let next = runtime.next_wake_ms().unwrap();
        world.clock.set(next);
        script_with(&world, &finals, next, 0);
        assert!(runtime.wake().await.is_empty());
        if next == close + 30_000 {
            break;
        }
    }
    assert_eq!(world.clock.now(), close + 30_000);
    assert_eq!(runtime.next_wake_ms(), Some(close + 40_000));

    // The confirming poll fails: the re-poll is kept, not lost to the next
    // bar boundary.
    fail.store(true, Ordering::SeqCst);
    world.clock.set(close + 40_000);
    assert!(!runtime.wake().await.is_empty(), "the outage is reported");
    assert_eq!(runtime.next_wake_ms(), Some(close + 50_000));

    fail.store(false, Ordering::SeqCst);
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
