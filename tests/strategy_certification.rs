//! r2.s3.w4 — AC-3: `certified` is DERIVED, on every read path.
//!
//! The database stores one fact — `strategy_version.latest_walk_forward_run_id`
//! — and `certified` is `true` exactly when the run that pointer names has
//! `pass != 0`. A newer FAILING walk-forward revokes certification by simply
//! being the row the pointer now names; there is no flag to forget to flip.
//!
//! i.   A fresh version reads `certified: false`, `latest_walk_forward_run_id:
//!      None` — on `get_version`, `list_versions` and `version_tree` alike.
//! ii.  A passing walk-forward run certifies the version on the next read —
//!      same three surfaces, plus the pointer names the run.
//! iii. A later FAILING run de-certifies: the pointer advanced, `certified`
//!      reads `false` again, on all three surfaces.
//! iv.  `certified` follows the JOIN, not the pointer: a version pointing at a
//!      failing run is uncertified even though the pointer is set.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    CreatedBy, Db, FakeClock, MIGRATOR, NewVersion, SqliteBacktestRunRepo, SqliteStrategyRepo,
    StrategyRepository, StrategyVersion, VersionId, WalkForwardRunId, WalkForwardRunRepository,
};
use sqlx::SqlitePool;
use support::mcp::seeded_walk_forward_draft;
use tempfile::TempDir;

const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (cert)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

struct World {
    _tmp: TempDir,
    db: Db,
    version_id: VersionId,
}

impl World {
    fn pool(&self) -> &SqlitePool {
        self.db.pool()
    }

    fn strategies(&self) -> SqliteStrategyRepo<pulse::SystemClock> {
        SqliteStrategyRepo::new(self.pool().clone())
    }

    /// The version as `get_version` reads it.
    async fn get(&self) -> StrategyVersion {
        self.strategies()
            .get_version(&self.version_id)
            .await
            .expect("read")
            .expect("the version exists")
    }

    /// The version as `list_versions` reads it.
    async fn listed(&self) -> StrategyVersion {
        let versions = self
            .strategies()
            .list_versions(&self.get().await.strategy_id)
            .await
            .expect("list versions");
        versions
            .into_iter()
            .find(|v| v.id == self.version_id)
            .expect("the version is listed")
    }

    /// The version as `version_tree` reads it.
    async fn tree(&self) -> StrategyVersion {
        let versions = self
            .strategies()
            .version_tree(&self.get().await.strategy_id)
            .await
            .expect("version tree");
        versions
            .into_iter()
            .find(|v| v.id == self.version_id)
            .expect("the version is in its tree")
    }
}

async fn world() -> World {
    let tmp = TempDir::new().unwrap();
    let db = Db::with_path(&tmp.path().join("pulse.db")).await.unwrap();
    MIGRATOR.run(db.pool()).await.expect("run embedded set");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("RSI Oversold", None, &[])
        .await
        .expect("create strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: MINIMAL_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    World {
        _tmp: tmp,
        db,
        version_id: version.id,
    }
}

/// Persist a synthetic `k=2` draft (shared `seeded_walk_forward_draft`) against
/// the version at `at_ms` — the injected clock pins `created_at`, and `seq`
/// minted at insert makes the `(created_at, seq)` pointer order follow save
/// order even inside one millisecond.
async fn save_wf(world: &World, pass: bool, at_ms: i64) -> pulse::WalkForwardRunId {
    SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(at_ms))
        .save_walk_forward_run(&world.version_id, &seeded_walk_forward_draft(pass))
        .await
        .expect("the walk-forward run persists")
}

/// i. A fresh version is born uncertified — NULL pointer, `false`, on every
/// read surface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_version_is_uncertified_on_every_read() {
    let world = world().await;

    for (surface, version) in [
        ("get_version", world.get().await),
        ("list_versions", world.listed().await),
        ("version_tree", world.tree().await),
    ] {
        assert_eq!(
            version.latest_walk_forward_run_id, None,
            "{surface}: the pointer reads NULL"
        );
        assert!(!version.certified, "{surface}: derived certified is false");
    }
}

/// ii. A passing walk-forward certifies the version on the next read — the
/// pointer names the run, `certified` is true, on all three surfaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_passing_walk_forward_certifies_on_every_read() {
    let world = world().await;
    let run = save_wf(&world, true, 1_756_512_000_000).await;

    for (surface, version) in [
        ("get_version", world.get().await),
        ("list_versions", world.listed().await),
        ("version_tree", world.tree().await),
    ] {
        assert_eq!(
            version
                .latest_walk_forward_run_id
                .as_ref()
                .map(pulse::WalkForwardRunId::as_str),
            Some(run.as_str()),
            "{surface}: the pointer names the passing run"
        );
        assert!(version.certified, "{surface}: derived certified is true");
    }
}

/// iii. A later FAILING run de-certifies: the pointer advances to it, and
/// `certified` reads `false` — the revocation is the newest row, not a flag.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_failing_run_decertifies_on_every_read() {
    let world = world().await;
    let certified_run = save_wf(&world, true, 1_756_512_000_000).await;
    assert!(world.get().await.certified, "setup: the pass certifies");

    let failing_run = save_wf(&world, false, 1_756_598_400_000).await;
    assert_ne!(
        failing_run.as_str(),
        certified_run.as_str(),
        "two distinct runs"
    );

    for (surface, version) in [
        ("get_version", world.get().await),
        ("list_versions", world.listed().await),
        ("version_tree", world.tree().await),
    ] {
        assert_eq!(
            version
                .latest_walk_forward_run_id
                .as_ref()
                .map(pulse::WalkForwardRunId::as_str),
            Some(failing_run.as_str()),
            "{surface}: the pointer advanced to the newer run"
        );
        assert!(
            !version.certified,
            "{surface}: a newer failing run de-certifies"
        );
    }
}

/// Two saves inside ONE millisecond — the tie case the `seq` tiebreak exists
/// for (F3): under the old `(created_at, id)` rule the second save's random
/// UUID could sort before the first's and be refused as a backward move. `seq`
/// minted `MAX(seq)+1` at insert makes the later save strictly later.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_millisecond_saves_advance_the_pointer_in_save_order() {
    let world = world().await;
    let at = 1_756_512_000_000;

    let first = save_wf(&world, true, at).await;
    let second = save_wf(&world, false, at).await;
    assert_ne!(first, second, "two distinct runs at one timestamp");

    // The pointer names the SECOND save — insertion order won, whatever the
    // two ids' lexical order happened to be.
    let version = world.get().await;
    assert_eq!(
        version
            .latest_walk_forward_run_id
            .as_ref()
            .map(WalkForwardRunId::as_str),
        Some(second.as_str()),
        "the later same-millisecond save advances the pointer"
    );
    assert!(!version.certified, "the newer run's verdict rules");

    // And the minted seqs really are the insertion sequence the trigger orders
    // by — pinned, not assumed.
    let seqs: Vec<i64> = sqlx::query_scalar("SELECT seq FROM walk_forward_run ORDER BY seq")
        .fetch_all(world.pool())
        .await
        .expect("read the seq column");
    assert_eq!(seqs, vec![1, 2], "seq mints in insertion order");
}

/// iv. `certified` follows the JOIN's `pass`, never the pointer alone: a
/// version whose pointer names a FAILING run reads uncertified — the pointer's
/// presence is not the credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pointer_at_a_failing_run_is_not_certification() {
    let world = world().await;
    let run = save_wf(&world, false, 1_756_512_000_000).await;

    let version = world.get().await;
    assert_eq!(
        version
            .latest_walk_forward_run_id
            .as_ref()
            .map(pulse::WalkForwardRunId::as_str),
        Some(run.as_str()),
        "the pointer is set"
    );
    assert!(
        !version.certified,
        "a pointer at a failing run is uncertified — certification is the joined pass"
    );
}
