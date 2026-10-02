//! r3.s4.w3 — AC-4 (stop): `stop` and `stop_all` close the log with an actor,
//! and a stopped session is never polled again.
//!
//! The final shadow check runs BEFORE the `stop` event (the schema refuses
//! anything after it), the stopping actor is recorded (a token label, or
//! `stop_all` plus its issuer), and later wakes — and later boots — leave a
//! stopped session's rows and log exactly as they were.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use pulse::{
    NonEmptyLabel, PaperEvent, PaperSessionId, PaperSessionRepository, StopActor, Timeframe,
};
use support::paper::{PaperWorld, create_version, promote_session};

const M15_MS: i64 = 900_000;

fn stops(events: &[PaperEvent]) -> Vec<StopActor> {
    events
        .iter()
        .filter_map(|event| match event {
            PaperEvent::Stop { actor, .. } => Some(actor.clone()),
            _ => None,
        })
        .collect()
}

async fn fixture_world_with_sessions(count: usize) -> (PaperWorld, Vec<PaperSessionId>) {
    let world = PaperWorld::new().await;
    world
        .source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    world
        .source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version = create_version(&world, "stop", &pulse::fixture_strategy_dsl()).await;
    let mut ids = Vec::new();
    for _ in 0..count {
        let session =
            promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
        ids.push(session.id);
    }
    (world, ids)
}

/// Advance one M15 bar and wake.
async fn run_bars(
    world: &PaperWorld,
    runtime: &mut support::paper::TestRuntime,
    id: &PaperSessionId,
    bars: usize,
) {
    let first_live = world.paper().count_from_ms(id).await.unwrap().unwrap();
    let mut open = first_live;
    for _ in 0..bars {
        world.clock.set(open + M15_MS);
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
        open += M15_MS;
    }
}

#[tokio::test]
async fn stop_appends_a_shadow_check_then_the_stop_and_nothing_more() {
    let (world, ids) = fixture_world_with_sessions(1).await;
    let id = &ids[0];
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    run_bars(&world, &mut runtime, id, 60).await;

    let label = NonEmptyLabel::try_new("desk-token-7").unwrap();
    runtime
        .stop(
            id,
            StopActor::Token {
                label: label.clone(),
            },
        )
        .await
        .expect("the stop lands");

    let events = world.paper().events(id).await.unwrap();
    let kinds: Vec<&str> = events.iter().map(PaperEvent::kind).collect();
    let tail = &kinds[kinds.len() - 2..];
    assert_eq!(
        tail,
        ["shadow_checked", "stop"],
        "the final check runs before the stop: {kinds:?}"
    );
    assert_eq!(
        stops(&events),
        vec![StopActor::Token { label }],
        "the stopping token's label is recorded"
    );

    // The session is gone from the runtime, and later wakes write nothing.
    let bars_before = world.paper().bars(id, Timeframe::M15).await.unwrap().len();
    let events_before = events.len();
    for _ in 0..3 {
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
    }
    assert_eq!(
        world.paper().bars(id, Timeframe::M15).await.unwrap().len(),
        bars_before
    );
    assert_eq!(world.paper().events(id).await.unwrap().len(), events_before);

    // The schema itself refuses anything after the stop.
    let refused = world
        .paper()
        .append_bar(
            id,
            &[],
            &[PaperEvent::DataEvent {
                seq: 0,
                at: "2025-02-03T00:00:00.000Z".to_owned(),
                summary: "after the stop".to_owned(),
            }],
        )
        .await;
    assert!(
        refused.is_err(),
        "the migration's wall refuses a post-stop write"
    );

    // A later boot does not resurrect the session.
    let mut reborn = world.runtime();
    let failures = reborn.boot().await;
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(
        world.paper().events(id).await.unwrap().len(),
        events_before,
        "a stopped session stays stopped across boots"
    );
}

#[tokio::test]
async fn stop_all_stops_every_running_session_with_its_issuer() {
    let (world, ids) = fixture_world_with_sessions(2).await;
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");
    for id in &ids {
        run_bars(&world, &mut runtime, id, 30).await;
    }

    let issuer = NonEmptyLabel::try_new("kill-switch-token").unwrap();
    let failures = runtime.stop_all(issuer.clone()).await;
    assert!(failures.is_empty(), "both stops land: {failures:?}");

    for id in &ids {
        let events = world.paper().events(id).await.unwrap();
        assert_eq!(
            stops(&events),
            vec![StopActor::StopAll {
                issuer: issuer.clone()
            }],
            "every session records the sweep and its issuer"
        );
        let kinds: Vec<&str> = events.iter().map(PaperEvent::kind).collect();
        assert_eq!(
            &kinds[kinds.len() - 2..],
            ["shadow_checked", "stop"],
            "each stop is preceded by its own final check"
        );
    }

    // A stopped session is not polled on later wakes: no new fetches, no rows.
    let calls_before = world.source.calls().len();
    let rows_before: Vec<usize> = {
        let mut counts = Vec::new();
        for id in &ids {
            counts.push(world.paper().bars(id, Timeframe::M15).await.unwrap().len());
        }
        counts
    };
    for _ in 0..3 {
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
    }
    assert_eq!(
        world.source.calls().len(),
        calls_before,
        "no fetch is made for stopped sessions"
    );
    for (id, rows) in ids.iter().zip(rows_before) {
        assert_eq!(
            world.paper().bars(id, Timeframe::M15).await.unwrap().len(),
            rows
        );
    }

    // Both are gone from the runtime: a second stop refuses.
    assert!(
        runtime
            .stop(&ids[0], StopActor::StopAll { issuer })
            .await
            .is_err()
    );
}
