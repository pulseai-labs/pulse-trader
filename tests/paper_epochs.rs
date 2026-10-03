//! r3.s4.w3 — AC-4 (epochs): a build change opens exactly one epoch, and the
//! shadow check judges the live epoch only.
//!
//! A session whose recorded live epoch is a foreign fingerprint gets exactly
//! one `engine_upgraded { old, new = current }` at boot; a second boot adds
//! none; the check that follows compares only the live epoch's trades; and an
//! earlier epoch's `shadow_checked` verdict is left untouched.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use pulse::{
    EngineFingerprint, NonEmptyLabel, PaperEvent, PaperSessionId, PaperSessionRepository,
    ShadowResult, StopActor, Timeframe,
};
use support::paper::{PaperWorld, create_version, promote_session};

const M15_MS: i64 = 900_000;

const FOREIGN: &str = "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn upgrades(events: &[PaperEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            PaperEvent::EngineUpgraded { old, new, .. } => {
                Some((old.as_str().to_owned(), new.as_str().to_owned()))
            }
            _ => None,
        })
        .collect()
}

fn shadow_payloads(events: &[PaperEvent]) -> Vec<serde_json::Value> {
    events
        .iter()
        .filter_map(|event| match event {
            PaperEvent::ShadowChecked { result, .. } => Some(result.clone()),
            _ => None,
        })
        .collect()
}

fn closed_trades(events: &[PaperEvent]) -> usize {
    events
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

/// Run the session until it has closed at least one trade (the earlier epoch's
/// material), then return.
async fn run_until_one_trade(
    world: &PaperWorld,
    runtime: &mut support::paper::TestRuntime,
    id: &PaperSessionId,
) {
    let first_live = world.paper().count_from_ms(id).await.unwrap().unwrap();
    let mut open = first_live;
    for _ in 0..6_000 {
        world.clock.set(open + M15_MS);
        let failures = runtime.wake().await;
        assert!(failures.is_empty(), "{failures:?}");
        if closed_trades(&world.paper().events(id).await.unwrap()) >= 1 {
            return;
        }
        open += M15_MS;
    }
    panic!("no closed trade within the fixture window");
}

#[tokio::test]
async fn a_foreign_epoch_upgrades_exactly_once_and_scopes_the_next_check() {
    let world = PaperWorld::new().await;
    world
        .source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    world
        .source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version = create_version(&world, "epochs", &pulse::fixture_strategy_dsl()).await;
    let session =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;

    // The first server runs the session and closes at least one trade.
    let mut first_runtime = world.runtime();
    assert!(first_runtime.boot().await.is_empty(), "first boot");
    run_until_one_trade(&world, &mut first_runtime, &session.id).await;
    let before = world.paper().events(&session.id).await.unwrap();
    let trades_before = closed_trades(&before);
    assert!(trades_before >= 1, "the earlier epoch has trades");
    let earlier_verdict = shadow_payloads(&before)
        .first()
        .cloned()
        .expect("the first boot shadow-checked");
    assert!(upgrades(&before).is_empty(), "no upgrade yet");
    drop(first_runtime);

    // The log now records an upgrade to ANOTHER build (the session ran under a
    // different engine until now).
    let foreign = EngineFingerprint::from_stored(FOREIGN);
    world
        .paper()
        .append_bar(
            &session.id,
            &[],
            &[PaperEvent::EngineUpgraded {
                seq: 0,
                at: "2025-02-02T00:00:00.000Z".to_owned(),
                old: EngineFingerprint::current(),
                new: foreign.clone(),
            }],
        )
        .await
        .unwrap();

    // A second boot: the live epoch is foreign, so THIS build opens one epoch.
    let mut second_runtime = world.runtime();
    assert!(second_runtime.boot().await.is_empty(), "second boot");
    let after = world.paper().events(&session.id).await.unwrap();
    let seen = upgrades(&after);
    assert_eq!(seen.len(), 2, "exactly one new upgrade: {seen:?}");
    assert_eq!(seen[1].0, FOREIGN, "the old epoch is named");
    assert_eq!(
        seen[1].1,
        EngineFingerprint::current().as_str(),
        "the new epoch is this build"
    );

    // A third boot adds none.
    drop(second_runtime);
    let mut third_runtime = world.runtime();
    assert!(third_runtime.boot().await.is_empty(), "third boot");
    let after_third = world.paper().events(&session.id).await.unwrap();
    assert_eq!(
        upgrades(&after_third).len(),
        2,
        "a second boot on the same build opens no new epoch"
    );

    // The check after the upgrade judges the live epoch only: its verdict names
    // fewer trades than the log holds, because the earlier epoch's trades are
    // not the live epoch's.
    let verdict = third_runtime.shadow_check(&session.id).await.unwrap();
    let ShadowResult::Identical {
        closed_trades: judged,
        ..
    } = verdict
    else {
        panic!("the live epoch is identical, got {verdict:?}");
    };
    let total = closed_trades(&after_third);
    assert!(
        total > usize::try_from(judged).unwrap(),
        "the live epoch holds {judged} of {total} trades"
    );

    // The earlier epoch's verdict is untouched.
    let later = world.paper().events(&session.id).await.unwrap();
    assert_eq!(
        shadow_payloads(&later).first().cloned(),
        Some(earlier_verdict),
        "an earlier epoch's shadow_checked is unchanged"
    );

    // The stop path still works after the epochs (a stop actor, then nothing).
    let label = NonEmptyLabel::try_new("operator-token").unwrap();
    third_runtime
        .stop(&session.id, StopActor::Token { label })
        .await
        .unwrap();
}
