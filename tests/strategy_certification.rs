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
    CandleWindow, CreatedBy, Db, FakeClock, FoldScheme, MIGRATOR, NewVersion,
    SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyRepository, StrategyVersion, VersionId,
    WalkForwardRunId, WalkForwardRunRepository,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use support::mcp::{seeded_walk_forward_draft, seeded_walk_forward_draft_k};
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
/// minted at insert is the pointer rule's whole order, so the pointer follows
/// save order — even inside one millisecond, and even if a clock went back.
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

/// F5: the persistence boundary refuses an INCOHERENT draft before a single
/// row lands — the disposition's attack verbatim: `k=6`, zero folds, and a
/// recorded `pass=true` would otherwise advance the pointer and read
/// "certified". One refusal per broken invariant, and each refusal persists
/// nothing (no run row, no pointer move).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_incoherent_draft_is_refused_and_persists_nothing() {
    let world = world().await;
    let runs =
        SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(1_756_512_000_000));

    // The attack: scheme k=6, zero folds, recorded pass=true.
    let mut draft = seeded_walk_forward_draft(true);
    draft.scheme = FoldScheme::rolling_oos(6).unwrap();
    draft.folds.clear();
    let err = runs
        .save_walk_forward_run(&world.version_id, &draft)
        .await
        .expect_err("k=6 with zero folds must refuse");
    assert!(
        err.to_string().contains("walk-forward draft refused"),
        "the refusal names the boundary: {err}"
    );

    // A full fold set whose recorded verdict lies — folds don't hold but the
    // run claims pass.
    let mut lying = seeded_walk_forward_draft(false);
    lying.verdict.pass = true;
    let err = runs
        .save_walk_forward_run(&world.version_id, &lying)
        .await
        .expect_err("a verdict the folds contradict must refuse");
    assert!(
        err.to_string().contains("walk-forward draft refused"),
        "the refusal names the boundary: {err}"
    );

    // Fail closed: nothing persisted, the pointer never moved, the version
    // still reads uncertified.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(world.pool())
        .await
        .expect("count walk-forward runs");
    assert_eq!(count, 0, "a refused draft wrote no run row");
    let version = world.get().await;
    assert!(
        version.latest_walk_forward_run_id.is_none() && !version.certified,
        "a refused draft cannot advance certification"
    );
}

/// The gate DERIVES the verdicts from the trades, and refuses any disagreement
/// (R1). The stored verdict is not evidence about itself: each fold's verdict is
/// recomputed from its run's `realized_r` series (`FoldVerdict::from_rs`) and the
/// run verdict from those folds and their pooled series (`RunVerdict::assess`) —
/// the identical derivation the app path and the read apply — so a draft that
/// stores a number its own trades contradict is refused before a row is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_that_disagrees_with_its_own_trades_is_refused() {
    let world = world().await;
    let runs =
        SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(1_756_512_000_000));

    // The pooled bound is moved while `holds` keeps claiming the derived truth:
    // the pooled verdict no longer describes the pooled trades.
    let mut smuggled_pooled = seeded_walk_forward_draft(true);
    smuggled_pooled.verdict.pooled.lower_bound = -0.5;
    let err = runs
        .save_walk_forward_run(&world.version_id, &smuggled_pooled)
        .await
        .expect_err("a pooled bound the pooled trades do not support must refuse");
    assert!(
        err.to_string()
            .contains("verdict records folds_holding=2 folds_required=2 pooled(n=40")
            && err.to_string().contains("but the folds' trades derive"),
        "the refusal shows both the record and the derivation: {err}"
    );

    // One FOLD's flag is fabricated over a series that derives the other truth.
    let mut smuggled_fold = seeded_walk_forward_draft(false);
    smuggled_fold.folds[0].verdict.holds = true;
    let err = runs
        .save_walk_forward_run(&world.version_id, &smuggled_fold)
        .await
        .expect_err("a fold `holds` its own trades contradict must refuse");
    assert!(
        err.to_string().contains("fold 0 records")
            && err.to_string().contains("20 trade(s) derive")
            && err.to_string().contains("holds=false"),
        "the refusal names the fold and the derived flag: {err}"
    );

    // The pooled count is not the trades it claims to summarise.
    let mut short_pooled = seeded_walk_forward_draft(true);
    short_pooled.verdict.pooled.n = 3;
    let err = runs
        .save_walk_forward_run(&world.version_id, &short_pooled)
        .await
        .expect_err("a pooled count the trades do not support must refuse");
    assert!(
        err.to_string().contains("pooled(n=3,"),
        "the refusal names the recorded count: {err}"
    );

    // All three refused before a single row: fail-closed, and the version is
    // still uncertified.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(world.pool())
        .await
        .expect("count walk-forward runs");
    assert_eq!(count, 0, "no refused draft wrote a run row");
    let version = world.get().await;
    assert!(
        version.latest_walk_forward_run_id.is_none() && !version.certified,
        "no refused draft advanced certification"
    );
}

/// R1's pin: a draft whose folds carry ZERO trades cannot be certified by the
/// numbers it claims. The verdict is left exactly as the fixture derived it for
/// twenty trades — `n = 20`, a positive bound, `holds = true`, pooled totals to
/// match and `pass = true` — and the only change is that the trades are GONE.
/// Before the gate derived the verdicts from the trades, every check passed: the
/// counts agreed with each other, so a certification no trade supports advanced
/// the pointer and the persisted DTO reported a certified run whose folds show no
/// trades at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_whose_folds_hold_no_trades_is_refused() {
    let world = world().await;
    let runs =
        SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(1_756_512_000_000));

    let mut trade_less = seeded_walk_forward_draft(true);
    assert!(
        trade_less.verdict.pass,
        "the fixture certifies on its numbers"
    );
    for fold in &mut trade_less.folds {
        fold.result.trades.clear();
    }

    let err = runs
        .save_walk_forward_run(&world.version_id, &trade_less)
        .await
        .expect_err("a fold claiming a verdict no trade supports must refuse");
    assert!(
        err.to_string().contains("fold 0 records n=20")
            && err.to_string().contains("0 trade(s) derive")
            && err.to_string().contains("holds=false"),
        "the refusal shows what the trades derive: {err}"
    );

    // Nothing persisted, and the version is still uncertified.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(world.pool())
        .await
        .expect("count walk-forward runs");
    assert_eq!(count, 0, "a refused draft wrote no run row");
    let version = world.get().await;
    assert!(
        version.latest_walk_forward_run_id.is_none() && !version.certified,
        "a refused draft cannot advance certification"
    );
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

/// R5: the gate revalidates the SCHEME, not only the folds. `FoldScheme::RollingOos`
/// is a public variant, so a repository caller can hand it a `k` the domain
/// refuses to construct — `rolling_oos(13)` cannot return it — and the row such
/// a draft writes is one the READ rejects (`decode_scheme_and_rule` re-derives
/// the scheme through that same constructor) while `get_version` still derives
/// `certified = true` from the stored `pass`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_whose_scheme_the_domain_cannot_construct_is_refused() {
    let world = world().await;
    let runs =
        SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(1_756_512_000_000));

    // A FULLY COHERENT k = 13 draft — thirteen folds, honest trades and a derived
    // passing verdict — whose only defect is the scheme. Before the gate
    // revalidated the scheme this draft saved and advanced the pointer.
    let out_of_range = seeded_walk_forward_draft_k(true, 13);
    assert!(
        out_of_range.verdict.pass && out_of_range.folds.len() == 13,
        "the fixture's only defect is the scheme"
    );

    let err = runs
        .save_walk_forward_run(&world.version_id, &out_of_range)
        .await
        .expect_err("a scheme the domain cannot construct must refuse");
    assert!(
        err.to_string().contains("walk-forward draft refused")
            && err.to_string().contains("not constructible"),
        "the refusal names the scheme: {err}"
    );

    // And the legal range still reconstructs — the refusal is the bound, not a
    // blanket one.
    for legal in pulse::K_MIN..=pulse::K_MAX {
        assert!(
            FoldScheme::rolling_oos(i64::from(legal)).is_ok(),
            "k={legal} is legal"
        );
    }

    // Fail closed: no row, no pointer move, the version still uncertified.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(world.pool())
        .await
        .expect("count walk-forward runs");
    assert_eq!(count, 0, "a refused draft wrote no run row");
    let version = world.get().await;
    assert!(
        version.latest_walk_forward_run_id.is_none() && !version.certified,
        "a refused draft cannot advance certification"
    );
}

/// R6: a fold's recorded window IS the window its run was given. `fold.window`
/// is what `wf-v1` judged; `fold.inputs.window` is what the ordinary
/// `backtest_run` persists as its provenance — the interval a reader of that run
/// sees. The schema only requires a fold run to be windowed, so without this the
/// stored record could name a different interval than the certified verdict.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_whose_fold_run_window_differs_from_its_fold_is_refused() {
    let world = world().await;
    let runs =
        SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(1_756_512_000_000));

    // A VALID nonempty window, just not the one this fold was judged over — the
    // shape the schema cannot see: the run stays windowed either way.
    let mut mismatched = seeded_walk_forward_draft(true);
    mismatched.folds[0].inputs.window =
        Some(CandleWindow::new(1_735_702_200_000, 1_736_000_000_000).expect("a valid window"));

    let err = runs
        .save_walk_forward_run(&world.version_id, &mismatched)
        .await
        .expect_err("a fold whose run was given another window must refuse");
    assert!(
        err.to_string().contains("fold 0 was run over"),
        "the refusal names the fold and the window it was given: {err}"
    );

    // Fail closed: nothing persisted, the version still uncertified.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(world.pool())
        .await
        .expect("count walk-forward runs");
    assert_eq!(count, 0, "a refused draft wrote no run row");
    let version = world.get().await;
    assert!(
        version.latest_walk_forward_run_id.is_none() && !version.certified,
        "a refused draft cannot advance certification"
    );
}

/// R9: every fold is the SAME EXPERIMENT. The gate checked each fold's fingerprint
/// and its trade-derived verdict, but never required the provenance inputs to
/// agree — so a caller of the public repository port could combine different
/// pairs, snapshot versions, timeframes, cost settings or starting equities into
/// one passing run and advance certification, a run that is not one walk-forward.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_draft_whose_folds_are_different_experiments_is_refused() {
    let world = world().await;
    let runs =
        SqliteBacktestRunRepo::with_deps(world.pool().clone(), FakeClock::at(1_756_512_000_000));

    // A different TAKER FEE on one fold — a valid value, a different experiment.
    let mut mixed = seeded_walk_forward_draft(true);
    mixed.folds[1].inputs.taker_fee_bps = Decimal::new(7, 0);
    let err = runs
        .save_walk_forward_run(&world.version_id, &mixed)
        .await
        .expect_err("folds that disagree on an input must refuse");
    assert!(
        err.to_string()
            .contains("fold 1 disagrees with fold 0 on the taker fee"),
        "the refusal names the fold and the field: {err}"
    );

    // And a different STARTING EQUITY — the other named field, and not part of
    // `inputs` at all.
    let mut revalued = seeded_walk_forward_draft(true);
    revalued.folds[1].starting_equity = Decimal::new(5_000, 0);
    let err = runs
        .save_walk_forward_run(&world.version_id, &revalued)
        .await
        .expect_err("folds with different starting equities must refuse");
    assert!(
        err.to_string()
            .contains("fold 1 disagrees with fold 0 on the starting equity"),
        "the refusal names the fold and the field: {err}"
    );

    // Fail closed: nothing persisted, the version still uncertified.
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM walk_forward_run")
        .fetch_one(world.pool())
        .await
        .expect("count walk-forward runs");
    assert_eq!(count, 0, "a refused draft wrote no run row");
    let version = world.get().await;
    assert!(
        version.latest_walk_forward_run_id.is_none() && !version.certified,
        "a refused draft cannot advance certification"
    );
}
