//! r2.s3.w5 — AC-2: the walk-forward commands over real desktop state.
//!
//! The Tauri half of d26: `run_walk_forward_version` runs the seeded version
//! over `rolling-oos/v1` folds through the REAL command core (migrated temp db,
//! copied candle fixture, real repositories — the `tests/tauri_backtest.rs`
//! shape), `get_walk_forward_run` reads the same DTO back, and
//! `get_backtest_run` projects a fold's ordinary persisted run through the same
//! `backtest_run_dto` `run_backtest_version` uses — carrying its `walkForward`
//! membership.
//!
//! What each test pins:
//!
//! 1. **Two cold runs agree** on every field except identity — the run id and
//!    each fold's `backtest_run_id` are minted fresh; everything else is the
//!    deterministic projection (w3's a10 oracle on the desktop wire).
//! 2. **`get_walk_forward_run` returns the same DTO** as the run call — the
//!    read-back projection is the run projection, byte for byte.
//! 3. **`get_backtest_run` on a fold id** returns a `BacktestRunDto` whose
//!    `walkForward` names the parent run and fold index, and whose KPI fields
//!    equal the persisted row's values — the same `backtest_run_dto` projection
//!    `run_backtest_version` answers with (a fold opens as an ordinary run).
//! 4. **Refusals map to the pinned `BusError` shape** — `k` out of range and a
//!    bad `from` are `validation` errors naming their field; unknown ids are
//!    `not_found`.
//! 5. **`BUS_COMMANDS` lists the three new names** — `collect_commands!` parity
//!    is the existing `tauri_bus_contract` test's; this suite asserts the list
//!    itself so a half-registration fails here too.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use pulse::{
    BUS_COMMANDS, BacktestRunId, BacktestRunRepository, BusErrorCode, CreatedBy, DesktopState,
    GetBacktestRunRequest, GetWalkForwardRunRequest, NewVersion, SqliteBacktestRunRepo, StrategyId,
    StrategyRepository, Timeframe, VersionId, WalkForwardRunRequest, get_backtest_run_core,
    get_walk_forward_run_core, run_walk_forward_version_core,
};
use tempfile::TempDir;

/// The committed candle fixture every run executes over.
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The golden strategy — RSI(14) on M15; its entry gate's first fully-warm bar
/// is the pinned oracle `tests/walk_forward.rs` measured.
const GOLDEN_STRATEGY: &str = "tests/fixtures/strategies/rsi-oversold-long.json";

/// The oracle's first fully-warm bar (RSI(14) on the M15 fixture).
const ORACLE_FIRST_WARM_MS: i64 = 1_735_702_200_000;

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// RFC 3339 (seconds) rendering of an epoch-ms bound — the wire's span/window
/// timestamp shape.
fn rfc3339(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("a real candle ms")
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// RFC 3339 → epoch ms, for comparing wire bounds with the persisted rows'.
fn parse_rfc3339_ms(raw: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(raw)
        .expect("an RFC 3339 bound")
        .timestamp_millis()
}

/// A temp environment: a db path and a WRITABLE copy of the candle fixture.
struct Env {
    _tmp: TempDir,
    db_path: PathBuf,
    store_root: PathBuf,
}

fn env() -> Env {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let store_root = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_root);
    Env {
        _tmp: tmp,
        db_path,
        store_root,
    }
}

impl Env {
    async fn cold_state(&self) -> DesktopState {
        DesktopState::open_with_store(
            &self.db_path,
            pulse::CandleStore::with_base_dir(self.store_root.clone()),
        )
        .await
        .expect("open desktop state over the temp db + fixture store")
    }
}

/// Seed a strategy plus one real, compilable version from the golden DSL.
async fn seed_version(env: &Env) -> VersionId {
    let state = env.cold_state().await;
    let repo = state.strategy_repo();
    let strat = repo
        .create_strategy("Walk-forward demo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let dsl = std::fs::read_to_string(manifest(GOLDEN_STRATEGY)).expect("read golden strategy");
    repo.create_version(NewVersion {
        strategy_id: StrategyId::new(strat.id.as_str().to_owned()),
        parent_version_id: None,
        dsl_json: dsl,
        created_by: CreatedBy::Human,
        creating_llm_call_ids: vec![],
    })
    .await
    .expect("create version")
    .id
}

/// One `run_walk_forward_version` request — `k: 2`, no bounds.
fn wf_request(version_id: &VersionId) -> WalkForwardRunRequest {
    WalkForwardRunRequest {
        version_id: version_id.as_str().to_owned(),
        from: None,
        to: None,
        k: Some(2),
    }
}

// ---------------------------------------------------------------------------
// 1 + 2. cold determinism, and the read-back DTO is the run DTO
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_walk_forwards_agree_on_every_field_except_identity() {
    let env = env();
    let version_id = seed_version(&env).await;

    // Two invocations with the state dropped in between — no shared cache, no
    // shared pool, no shared store handle.
    let first = {
        let state = env.cold_state().await;
        run_walk_forward_version_core(&state, wf_request(&version_id))
            .await
            .expect("first cold walk-forward succeeds")
    };
    let second = {
        let state = env.cold_state().await;
        run_walk_forward_version_core(&state, wf_request(&version_id))
            .await
            .expect("second cold walk-forward succeeds")
    };

    assert_ne!(
        first.walk_forward_run_id, second.walk_forward_run_id,
        "every invocation mints a fresh walk-forward run id — there is no cached path"
    );
    assert_eq!(first.scheme, "rolling-oos/v1", "the scheme names itself");
    assert_eq!(first.rule, "wf-v1", "the verdict rule names itself");
    assert_eq!(first.k, 2);
    assert_eq!(first.folds.len(), 2, "one row per fold");
    assert!(
        first.from_defaulted,
        "no `from` was given — the span opened at the first fully-warm bar"
    );

    // The folds are contiguous and their union is exactly the counted span.
    assert_eq!(
        first.folds[0].window_from, first.span_from,
        "the first fold opens the counted span"
    );
    assert_eq!(
        first.folds[1].window_from, first.folds[0].window_to,
        "the folds are contiguous — no gap, no overlap"
    );
    assert_eq!(
        first.folds[1].window_to, first.span_to,
        "the last fold closes the counted span"
    );

    // Normalize ONLY identity: the run id and each fold's `backtest_run_id`.
    let mut a = first.clone();
    let mut b = second.clone();
    a.walk_forward_run_id = String::new();
    b.walk_forward_run_id = String::new();
    for (fa, fb) in a.folds.iter_mut().zip(b.folds.iter_mut()) {
        fa.backtest_run_id = String::new();
        fb.backtest_run_id = String::new();
    }
    assert_eq!(
        a, b,
        "two cold walk-forwards over the same version and the same snapshots \
         differ only in the minted ids"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_walk_forward_run_returns_the_same_dto() {
    let env = env();
    let version_id = seed_version(&env).await;

    let state = env.cold_state().await;
    let ran = run_walk_forward_version_core(&state, wf_request(&version_id))
        .await
        .expect("walk-forward succeeds");

    let fetched = get_walk_forward_run_core(
        &state,
        GetWalkForwardRunRequest {
            walk_forward_run_id: ran.walk_forward_run_id.clone(),
        },
    )
    .await
    .expect("get_walk_forward_run reads the persisted run");

    assert_eq!(
        fetched, ran,
        "the read-back DTO is the run call's DTO — one projection, byte for byte"
    );
}

// ---------------------------------------------------------------------------
// 3. a fold run opens as an ordinary run — the same projection, plus membership
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_backtest_run_on_a_fold_id_returns_the_ordinary_run_dto() {
    let env = env();
    let version_id = seed_version(&env).await;

    let state = env.cold_state().await;
    let wf = run_walk_forward_version_core(&state, wf_request(&version_id))
        .await
        .expect("walk-forward succeeds");
    let runs = SqliteBacktestRunRepo::new(state.db().pool().clone());

    for (index, fold) in wf.folds.iter().enumerate() {
        let dto = get_backtest_run_core(
            &state,
            GetBacktestRunRequest {
                run_id: fold.backtest_run_id.clone(),
            },
        )
        .await
        .expect("the fold run reads back");

        // The membership names the parent run and this fold's index.
        let membership = dto
            .walk_forward
            .as_ref()
            .expect("a fold run carries its walk-forward membership");
        assert_eq!(membership.walk_forward_run_id, wf.walk_forward_run_id);
        assert_eq!(membership.fold_index, u32::try_from(index).unwrap());
        assert_eq!(dto.run_id, fold.backtest_run_id);
        assert_eq!(dto.strategy_version_id, wf.version_id);

        // The KPIs are the fold row's own summary values — the same numbers
        // the walk-forward table renders — AND the persisted row's, through
        // the same `backtest_run_dto` projection `run_backtest_version` uses.
        assert_eq!(dto.expectancy, fold.expectancy, "fold expectancy agrees");
        assert_eq!(dto.win_rate, fold.win_rate, "fold win rate agrees");
        assert_eq!(dto.trade_count, fold.trades, "fold trade count agrees");
        assert_eq!(dto.trade_count as usize, fold.n as usize, "`trades` is `n`");

        let persisted = runs
            .get_run(&BacktestRunId::new(fold.backtest_run_id.clone()))
            .await
            .expect("get_run reads the fold")
            .expect("the fold run row exists");
        assert_eq!(dto.result_content_hash, persisted.result_content_hash);
        assert_eq!(
            dto.net_pnl,
            persisted.net_pnl.normalize().to_string(),
            "net_pnl is the persisted row's own decimal"
        );

        // The fold run is a windowed run: its reloaded primary opens on the
        // fold window's `from`, and its recorded lead-in is the snapshot's
        // first candle (r2.s3.w2 — warmed on full history, counted on [from,to)).
        let inputs = persisted
            .inputs
            .as_ref()
            .expect("a fresh run carries inputs");
        let window = inputs.window.as_ref().expect("a fold run is windowed");
        assert_eq!(window.from_ms, parse_rfc3339_ms(&fold.window_from));
        assert_eq!(window.to_ms, parse_rfc3339_ms(&fold.window_to));
        // The reloaded primary is the fold's own `[from, to)` slice: its first
        // counted candle opens at-or-after the window's `from` (exactly at it
        // for fold 0 — the span's `from` is the first fully-warm bar's open —
        // strictly inside for later folds, whose `from` is the scheme's mid-bar
        // step) and strictly before `to`.
        let first_open: i64 = dto.first_open_time_ms.parse().unwrap();
        assert!(
            first_open >= window.from_ms && first_open < window.to_ms,
            "fold {index}'s reloaded primary opens at {first_open}, outside [{}, {})",
            window.from_ms,
            window.to_ms
        );
        assert_eq!(
            inputs.lead_in_from_ms,
            Some(parse_rfc3339_ms(
                dto.lead_in_from.as_deref().expect("lead_in_from recorded")
            )),
            "the fold counted only its window but warmed on the full snapshot"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. the refusal surface maps to the pinned BusError shape
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_forward_refusals_map_to_the_pinned_bus_error_shape() {
    let env = env();
    let version_id = seed_version(&env).await;
    let state = env.cold_state().await;

    // `k` outside 2..=12 — validation, and the message names `k`. The wire
    // value is i64: -1 and 256 refuse as BusErrors naming `k`, not as argument
    // decode failures (F4).
    for k in [13_i64, 1, -1, 256] {
        let err = run_walk_forward_version_core(
            &state,
            WalkForwardRunRequest {
                k: Some(k),
                ..wf_request(&version_id)
            },
        )
        .await
        .expect_err("k out of range refuses");
        assert_eq!(err.code, BusErrorCode::Validation, "k={k}: {err:?}");
        assert!(
            err.message.contains("`k`") || err.message.contains(" k "),
            "the refusal names the `k` field: {}",
            err.message
        );
        assert_eq!(err.run_id, None, "a request refusal names no run");
    }

    // A malformed `from` — validation naming `from`.
    let err = run_walk_forward_version_core(
        &state,
        WalkForwardRunRequest {
            from: Some("not-a-timestamp".to_owned()),
            ..wf_request(&version_id)
        },
    )
    .await
    .expect_err("a malformed `from` refuses");
    assert_eq!(err.code, BusErrorCode::Validation, "{err:?}");
    assert!(
        err.message.contains("from"),
        "the refusal names the `from` field: {}",
        err.message
    );

    // An explicit `from` before the first fully-warm bar — validation naming
    // `from` AND the earliest allowed bound.
    let too_early = ORACLE_FIRST_WARM_MS - Timeframe::M15.duration_ms();
    let err = run_walk_forward_version_core(
        &state,
        WalkForwardRunRequest {
            from: Some(rfc3339(too_early)),
            ..wf_request(&version_id)
        },
    )
    .await
    .expect_err("a pre-warm `from` refuses");
    assert_eq!(err.code, BusErrorCode::Validation, "{err:?}");
    assert!(
        err.message.contains(&rfc3339(ORACLE_FIRST_WARM_MS)),
        "the refusal names the earliest allowed `from`: {}",
        err.message
    );

    // An unknown version — the shared resolve seam's refusal. Assert the code
    // is one of the pinned families rather than asserting a mapping the spec
    // leaves to the shared resolver.
    let version_err = run_walk_forward_version_core(
        &state,
        WalkForwardRunRequest {
            version_id: "ver-unknown".to_owned(),
            ..wf_request(&version_id)
        },
    )
    .await
    .expect_err("an unknown version refuses");
    assert!(
        [
            BusErrorCode::Data,
            BusErrorCode::Validation,
            BusErrorCode::NotFound
        ]
        .contains(&version_err.code),
        "an unknown version is a named refusal, not an internal fault: {version_err:?}"
    );
}

/// The read commands' half of the refusal surface: an unknown
/// `walk_forward_run_id` or `run_id` is `not_found` naming the asked-for id —
/// refused, never an empty read. Neither core touches the strategy table, so
/// no seeded version is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn walk_forward_reads_refuse_unknown_ids_as_not_found() {
    let env = env();
    let state = env.cold_state().await;

    // An unknown walk-forward run id — not-found, naming the asked-for id.
    let err = get_walk_forward_run_core(
        &state,
        GetWalkForwardRunRequest {
            walk_forward_run_id: "wf-unknown".to_owned(),
        },
    )
    .await
    .expect_err("an unknown walk-forward run id refuses");
    assert_eq!(err.code, BusErrorCode::NotFound, "{err:?}");
    assert!(
        err.message.contains("wf-unknown"),
        "the refusal names the asked-for id: {}",
        err.message
    );

    // An unknown backtest run id on `get_backtest_run` — not-found naming it.
    let err = get_backtest_run_core(
        &state,
        GetBacktestRunRequest {
            run_id: "run-unknown".to_owned(),
        },
    )
    .await
    .expect_err("an unknown run id refuses");
    assert_eq!(err.code, BusErrorCode::NotFound, "{err:?}");
    assert!(
        err.message.contains("run-unknown"),
        "the refusal names the asked-for id: {}",
        err.message
    );
}

// ---------------------------------------------------------------------------
// 5. registration
// ---------------------------------------------------------------------------

#[test]
fn bus_commands_lists_the_three_new_names() {
    for name in [
        "run_walk_forward_version",
        "get_walk_forward_run",
        "get_backtest_run",
    ] {
        assert!(
            BUS_COMMANDS.contains(&name),
            "BUS_COMMANDS carries `{name}`: {BUS_COMMANDS:?}"
        );
    }
}
