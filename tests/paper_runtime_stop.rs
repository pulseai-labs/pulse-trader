//! r3.s4.w3 — AC-4 (stop): `stop` and `stop_all` close the log with an actor,
//! and a stopped session is never polled again.
//!
//! The final shadow check runs BEFORE the `stop` event (the schema refuses
//! anything after it), the stopping actor is recorded (a token label, or
//! `stop_all` plus its issuer), and later wakes — and later boots — leave a
//! stopped session's rows and log exactly as they were.
//!
//! The r3.s4 close fix round adds the kill switch's fail-safe proofs: a failed
//! final shadow check never vetoes a stop, a refused `Stop` append still
//! detaches the session in memory, and the serve loop ends when its control
//! handle is dropped. The two fault-injecting doubles below are test-only:
//! the suite's `ScriptedBars` and the real store cannot fail on demand.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::time::Duration;

use pulse::{
    BinanceAdapter, Candle, CandleSeriesRepository, CandleStore, CertifiedDataVersion, Clock,
    ClosedBarSource, DataError, DataVersion, LiveEnv, NonEmptyLabel, Pair, PaperEvent,
    PaperRuntime, PaperRuntimeError, PaperSession, PaperSessionDraft, PaperSessionId,
    PaperSessionRepository, SessionEnv, SnapshotSelection, SqlitePaperSessionRepo,
    SqliteStrategyRepo, StopActor, StoredCandleSeries, SystemClock, Timeframe,
};
use support::paper::{PaperWorld, ScriptedBars, SteppedClock, create_version, promote_session};

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
async fn run_bars<R, B, S, C, E>(
    world: &PaperWorld,
    runtime: &mut PaperRuntime<R, B, S, C, E>,
    id: &PaperSessionId,
    bars: usize,
) where
    R: PaperSessionRepository,
    B: ClosedBarSource,
    S: CandleSeriesRepository,
    C: Clock,
    E: SessionEnv,
{
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

// ---------------------------------------------------------------------------
// The r3.s4 close fix round — the fail-safe kill switch (F1) and the serve
// loop's dropped control handle (F2)
// ---------------------------------------------------------------------------

/// The real `CandleStore`, with `load_version` refused for ONE timeframe: the
/// final shadow check dies at its snapshot read (the runtime's only `series`
/// caller), and every other path — attach, catch-up, the bar writes — stays
/// the real store's.
struct RefusingSeries {
    inner: CandleStore,
    refused: Timeframe,
}

impl CandleSeriesRepository for RefusingSeries {
    fn load_head(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
    ) -> Result<Option<StoredCandleSeries>, DataError> {
        self.inner.load_head(pair, timeframe)
    }

    fn load_version(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        version: &DataVersion,
    ) -> Result<StoredCandleSeries, DataError> {
        if timeframe == self.refused {
            return Err(DataError::Db(format!(
                "injected: the {timeframe:?} snapshot is unreadable"
            )));
        }
        self.inner.load_version(pair, timeframe, version)
    }

    fn commit(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        candles: Vec<Candle>,
    ) -> Result<StoredCandleSeries, DataError> {
        self.inner.commit(pair, timeframe, candles)
    }
}

/// The real paper-session repository, with any append that carries a `Stop`
/// refused — the kill switch's own log write failing while every other write
/// still lands (the `coach_decision.rs` `ReadBackFailingRuns` precedent).
struct StopRefusingRepo<R> {
    inner: R,
}

impl<R: PaperSessionRepository + Send + Sync> PaperSessionRepository for StopRefusingRepo<R> {
    async fn insert_session(&self, draft: &PaperSessionDraft) -> Result<PaperSession, DataError> {
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
        versions: &[CertifiedDataVersion],
    ) -> Result<bool, DataError> {
        self.inner.all_versions_are_fixtures(versions).await
    }

    async fn append_bar(
        &self,
        session_id: &PaperSessionId,
        bars: &[(Timeframe, Candle, bool)],
        events: &[PaperEvent],
    ) -> Result<Vec<PaperEvent>, DataError> {
        if events
            .iter()
            .any(|event| matches!(event, PaperEvent::Stop { .. }))
        {
            return Err(DataError::Db(
                "injected: the stop append is refused".to_owned(),
            ));
        }
        self.inner.append_bar(session_id, bars, events).await
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

    async fn events(&self, session_id: &PaperSessionId) -> Result<Vec<PaperEvent>, DataError> {
        self.inner.events(session_id).await
    }

    async fn bars(
        &self,
        session_id: &PaperSessionId,
        timeframe: Timeframe,
    ) -> Result<Vec<Candle>, DataError> {
        self.inner.bars(session_id, timeframe).await
    }

    async fn count_from_ms(&self, session_id: &PaperSessionId) -> Result<Option<i64>, DataError> {
        self.inner.count_from_ms(session_id).await
    }

    async fn materialise(
        &self,
        session_id: &PaperSessionId,
    ) -> Result<Vec<SnapshotSelection>, DataError> {
        self.inner.materialise(session_id).await
    }
}

/// The runtime over the H4-refusing series (tests (a) and (b)).
type FaultRuntime = PaperRuntime<
    SqlitePaperSessionRepo<SteppedClock>,
    ScriptedBars,
    RefusingSeries,
    SteppedClock,
    LiveEnv<SqliteStrategyRepo<SystemClock>, BinanceAdapter>,
>;

/// The runtime over the `Stop`-refusing repository (test (c)).
type StopFaultRuntime = PaperRuntime<
    StopRefusingRepo<SqlitePaperSessionRepo<SteppedClock>>,
    ScriptedBars,
    CandleStore,
    SteppedClock,
    LiveEnv<SqliteStrategyRepo<SystemClock>, BinanceAdapter>,
>;

/// The world's runtime, with the `H4` snapshot load failing: the sessions this
/// world promotes (M15 primary + H4 htf) fail their final shadow check and
/// nothing else.
fn series_fault_runtime(world: &PaperWorld) -> FaultRuntime {
    PaperRuntime::new(
        world.paper(),
        world.source.clone(),
        RefusingSeries {
            inner: world.store.clone(),
            refused: Timeframe::H4,
        },
        world.clock.clone(),
        LiveEnv::new(world.strategies(), BinanceAdapter::new()),
        world.grace_ms,
        world.log.clone(),
    )
}

/// The world's runtime, with an append that carries a `Stop` refused.
fn stop_fault_runtime(world: &PaperWorld) -> StopFaultRuntime {
    PaperRuntime::new(
        StopRefusingRepo {
            inner: world.paper(),
        },
        world.source.clone(),
        world.store.clone(),
        world.clock.clone(),
        LiveEnv::new(world.strategies(), BinanceAdapter::new()),
        world.grace_ms,
        world.log.clone(),
    )
}

/// F1 (a): a failed final shadow check is logged and does NOT veto the stop.
#[tokio::test]
async fn a_failed_final_shadow_check_does_not_block_the_stop() {
    let (world, ids) = fixture_world_with_sessions(1).await;
    let id = &ids[0];
    let mut runtime = series_fault_runtime(&world);
    assert!(runtime.boot().await.is_empty(), "boot");
    run_bars(&world, &mut runtime, id, 40).await;

    let label = NonEmptyLabel::try_new("desk-token-9").unwrap();
    runtime
        .stop(
            id,
            StopActor::Token {
                label: label.clone(),
            },
        )
        .await
        .expect("a failed final check does not veto the stop");

    let events = world.paper().events(id).await.unwrap();
    assert_eq!(
        stops(&events),
        vec![StopActor::Token { label }],
        "the stop lands despite the refused check"
    );
    let kinds: Vec<&str> = events.iter().map(PaperEvent::kind).collect();
    assert_eq!(
        *kinds.last().unwrap(),
        "stop",
        "the refused check writes nothing: {kinds:?}"
    );
    assert!(
        runtime.attached_ids().is_empty(),
        "the session stops trading"
    );

    let lines = world.log.lines();
    let refusals: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("final shadow check failed"))
        .collect();
    assert_eq!(refusals.len(), 1, "one diagnostic line: {lines:?}");
    assert!(
        refusals[0].contains(id.as_str()),
        "the line names the session: {refusals:?}"
    );
}

/// F1 (b): `stop_all` sweeps on when one session's final check fails — the
/// failing session (M15 + H4, refused at the H4 load) and the healthy one
/// (M15 only) both stop, and neither is a failure.
#[tokio::test]
async fn stop_all_stops_every_session_when_one_final_check_fails() {
    let world = PaperWorld::new().await;
    world
        .source
        .script(Timeframe::M15, pulse::fixture_m15_candles());
    world
        .source
        .script(Timeframe::H4, pulse::fixture_h4_candles());
    let version = create_version(&world, "stop-all-fault", &pulse::fixture_strategy_dsl()).await;
    let failing =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    let healthy = promote_session(&world, &version, Timeframe::M15, None, false).await;

    let mut runtime = series_fault_runtime(&world);
    assert!(runtime.boot().await.is_empty(), "boot");
    for id in [&failing.id, &healthy.id] {
        run_bars(&world, &mut runtime, id, 40).await;
    }

    let issuer = NonEmptyLabel::try_new("kill-switch-fault").unwrap();
    let failures = runtime.stop_all(issuer.clone()).await;
    assert!(
        failures.is_empty(),
        "a failed check is not a stop failure: {failures:?}"
    );
    assert!(
        runtime.attached_ids().is_empty(),
        "both sessions stop trading"
    );

    let failing_events = world.paper().events(&failing.id).await.unwrap();
    let failing_kinds: Vec<&str> = failing_events.iter().map(PaperEvent::kind).collect();
    assert_eq!(
        &failing_kinds[failing_kinds.len() - 1..],
        ["stop"],
        "the refused check writes nothing: {failing_kinds:?}"
    );
    assert_eq!(
        stops(&failing_events),
        vec![StopActor::StopAll {
            issuer: issuer.clone()
        }]
    );

    let healthy_events = world.paper().events(&healthy.id).await.unwrap();
    let healthy_kinds: Vec<&str> = healthy_events.iter().map(PaperEvent::kind).collect();
    assert_eq!(
        &healthy_kinds[healthy_kinds.len() - 2..],
        ["shadow_checked", "stop"],
        "the healthy session's own final check still runs: {healthy_kinds:?}"
    );
    assert_eq!(stops(&healthy_events), vec![StopActor::StopAll { issuer }]);
}

/// F1 (c): a refused `Stop` append comes back as an error, and the session is
/// still gone from the runtime — the in-memory halt wins over the log write.
#[tokio::test]
async fn a_refused_stop_append_still_detaches_the_session() {
    let (world, ids) = fixture_world_with_sessions(1).await;
    let id = &ids[0];
    let mut runtime = stop_fault_runtime(&world);
    assert!(runtime.boot().await.is_empty(), "boot");
    run_bars(&world, &mut runtime, id, 40).await;

    let issuer = NonEmptyLabel::try_new("kill-switch-refused").unwrap();
    let error = runtime
        .stop(id, StopActor::StopAll { issuer })
        .await
        .expect_err("the refused Stop append comes back");
    assert!(
        matches!(error, PaperRuntimeError::Data(_)),
        "the append's own refusal: {error:?}"
    );
    assert!(
        !runtime.attached_ids().contains(id),
        "the halt wins over the log write"
    );

    // The refusal is real: the log ends without a stop (the known restart
    // limit the report records).
    let events = world.paper().events(id).await.unwrap();
    assert!(stops(&events).is_empty(), "the append was refused");
    let kinds: Vec<&str> = events.iter().map(PaperEvent::kind).collect();
    assert_eq!(
        *kinds.last().unwrap(),
        "shadow_checked",
        "the healthy final check landed before the refused stop: {kinds:?}"
    );
}

/// Round 1 (iL): a refused `Stop` append keeps the session halted — the next
/// wake's discovery does not re-attach it, and a second stop retries the
/// append.
#[tokio::test]
async fn a_refused_stop_append_is_not_reattached_by_the_next_wake() {
    let (world, ids) = fixture_world_with_sessions(1).await;
    let id = &ids[0];
    let mut runtime = stop_fault_runtime(&world);
    assert!(runtime.boot().await.is_empty(), "boot");
    run_bars(&world, &mut runtime, id, 40).await;

    let issuer = NonEmptyLabel::try_new("kill-switch-halted").unwrap();
    runtime
        .stop(id, StopActor::StopAll { issuer })
        .await
        .expect_err("the refused Stop append comes back");
    let bars_before = world.paper().bars(id, Timeframe::M15).await.unwrap().len();

    // Later wakes past new closed bars: the session is not picked up again.
    let first_live = world.paper().count_from_ms(id).await.unwrap().unwrap();
    for step in 41..44 {
        world.clock.set(first_live + step * M15_MS);
        runtime.wake().await;
        assert!(
            !runtime.attached_ids().contains(id),
            "a halted session is never re-attached"
        );
    }
    assert_eq!(
        world.paper().bars(id, Timeframe::M15).await.unwrap().len(),
        bars_before,
        "a halted session consumes no bar"
    );
}

/// Round 1 (iP): stop-all sweeps persisted running sessions the runtime has
/// not attached, through the direct `Stop` path.
#[tokio::test]
async fn stop_all_stops_a_running_session_that_is_not_attached() {
    let (world, ids) = fixture_world_with_sessions(1).await;
    let attached = &ids[0];
    let mut runtime = world.runtime();
    assert!(runtime.boot().await.is_empty(), "boot");

    // Promoted after the boot and never woken: persisted, running, unattached.
    let version = create_version(
        &world,
        "stop-all-unattached",
        &pulse::fixture_strategy_dsl(),
    )
    .await;
    let unattached =
        promote_session(&world, &version, Timeframe::M15, Some(Timeframe::H4), false).await;
    assert!(!runtime.attached_ids().contains(&unattached.id));

    let issuer = NonEmptyLabel::try_new("kill-switch-unattached").unwrap();
    let reply = runtime.stop_all_reply(issuer.clone()).await;
    assert!(reply.failures.is_empty(), "{:?}", reply.failures);
    assert!(reply.stopped.contains(attached), "{:?}", reply.stopped);
    assert!(
        reply.stopped.contains(&unattached.id),
        "{:?}",
        reply.stopped
    );
    for id in [attached, &unattached.id] {
        let events = world.paper().events(id).await.unwrap();
        assert_eq!(
            stops(&events),
            vec![StopActor::StopAll {
                issuer: issuer.clone()
            }],
            "session {id} carries the stop_all"
        );
    }

    // A later wake does not start it.
    runtime.wake().await;
    assert!(runtime.attached_ids().is_empty());
}

/// F2: the serve loop ends when its control handle is dropped instead of
/// re-firing the `recv()` arm (a busy-spin once the sender is gone).
///
/// The loop gets its own thread and current-thread runtime — the `PaperHost`
/// idiom — because the runtime's future is deliberately `!Send` (the w3
/// engine's indicators), so it cannot go through `tokio::spawn`; and a bounded
/// completion channel makes a re-introduced spin a clean failure (a spinning
/// task would also hang the runtime's own drop).
#[test]
fn a_dropped_control_sender_ends_the_serve_loop() {
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let serving = std::thread::Builder::new()
        .name("paper-loop-dropped-control".to_owned())
        .spawn(move || {
            let Ok(thread_rt) = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            else {
                return;
            };
            thread_rt.block_on(async move {
                let world = PaperWorld::new().await;
                let (commands_tx, commands_rx) = tokio::sync::mpsc::channel(8);
                let (_tick_tx, tick_rx) = tokio::sync::mpsc::unbounded_channel();
                let (_stop_tx, stop_rx) = tokio::sync::watch::channel(false);
                drop(commands_tx);
                pulse::run_paper_runtime(
                    world.runtime(),
                    stop_rx,
                    commands_rx,
                    pulse::WakeTrigger::Tick(tick_rx),
                )
                .await;
            });
            let _ = done_tx.send(());
        })
        .expect("spawn the serve loop's thread");

    done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the loop ends when its control handle is dropped");
    serving.join().expect("the loop's thread ends");
}
