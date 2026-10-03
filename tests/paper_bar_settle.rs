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

use pulse::{
    Candle, PaperEvent, PaperSession, PaperSessionId, PaperSessionRepository, SettlePolicy,
    Timeframe,
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
/// a bar — longer than the grace, as observed live.
const PROVISIONAL_FOR_MS: i64 = 12_000;

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

/// Drive the runtime by its own wake schedule until the clock passes `until`,
/// re-scripting the source before every wake.
async fn drive(world: &PaperWorld, runtime: &mut TestRuntime, finals: &[Candle], until: i64) {
    for _ in 0..1_000 {
        let now = world.clock.now();
        if now > until {
            return;
        }
        let next = runtime.next_wake_ms().expect("a wake is always scheduled");
        assert!(next > now, "the next wake {next} lies after now {now}");
        world.clock.set(next);
        script_at(world, finals, next);
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
