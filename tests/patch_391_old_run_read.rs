//! #391 — opening a run saved before migration `0006` read "a freshly saved run
//! read back with no input provenance".
//!
//! `get_backtest_run_core` (`src/tauri/walk_forward.rs`) is the app's persisted
//! run read, also served over HTTP. It reads a stored run through the shared
//! `read_back`, which answers a run with no recorded inputs
//! (`PersistedRun.inputs == None`) as `FreshInputsMissing`. That wording is right
//! on the SAVE path — a row written seconds ago must carry its inputs — and wrong
//! for a run a trader opens later from the list.
//!
//! 1. **An old run is refused with its reason.** A run whose inputs are NULL (the
//!    pre-`0006` shape) opens as `data` with one message that names the run id and
//!    the remedy, and never the "freshly saved" text.
//! 2. **A run with inputs still opens.** The new arm must not catch a run that
//!    carries them.
//!
//! The save-path half is pinned where it always was:
//! `tests/tauri_backtest.rs::every_post_save_failure_stage_names_the_persisted_run`
//! (its `GetRunLegacyInputs` case still maps to `FreshInputsMissing`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use pulse::{
    BusErrorCode, CandleStore, CreatedBy, DesktopState, NewVersion, StrategyId, StrategyRepository,
    VersionId, get_backtest_run_core, run_backtest_version_core,
};
use tempfile::TempDir;

mod coach_support;

/// The committed candle fixture every arm runs over.
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The golden strategy — RSI(14) on M15.
const GOLDEN_STRATEGY: &str = "tests/fixtures/strategies/rsi-oversold-long.json";

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

/// A desktop state over a temp db and a WRITABLE copy of the candle fixture.
async fn open_state() -> (TempDir, DesktopState) {
    let tmp = TempDir::new().unwrap();
    let store_root = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_root);
    let state = DesktopState::open_with_store(
        &tmp.path().join("pulse.db"),
        CandleStore::with_base_dir(store_root),
    )
    .await
    .expect("open desktop state over the temp db + fixture store");
    (tmp, state)
}

async fn seed_version(state: &DesktopState) -> VersionId {
    let repo = state.strategy_repo();
    let strategy = repo
        .create_strategy("patch 391", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create the strategy");
    let dsl = std::fs::read_to_string(manifest(GOLDEN_STRATEGY)).expect("read the golden strategy");
    repo.create_version(NewVersion {
        strategy_id: StrategyId::new(strategy.id.as_str().to_owned()),
        parent_version_id: None,
        dsl_json: dsl,
        created_by: CreatedBy::Human,
        creating_llm_call_ids: vec![],
    })
    .await
    .expect("create the version")
    .id
}

/// Save one real run through the app's own command and return its id.
async fn saved_run_id(state: &DesktopState) -> String {
    let version = seed_version(state).await;
    run_backtest_version_core(
        state,
        pulse::BacktestRunRequest {
            version_id: version.as_str().to_owned(),
        },
    )
    .await
    .expect("the app backtest runs")
    .run_id
}

/// Blank the run's `0006` provenance columns and the `0015` symbol-filter group —
/// the pre-`0006` legacy shape (the same surgery `tests/coach_decision.rs` does).
/// `backtest_run` is UPDATE-immutable, so the helper lifts that trigger around it.
async fn make_run_legacy(state: &DesktopState, run_id: &str) {
    coach_support::with_run_immutability_lifted(
        state.db().pool(),
        &[&format!(
            "UPDATE backtest_run SET pair = NULL, primary_timeframe = NULL, \
             primary_data_version = NULL, htf_timeframe = NULL, htf_data_version = NULL, \
             taker_fee_bps = NULL, slippage_bps = NULL, funding_config = NULL, \
             lot_step = NULL, min_qty = NULL, min_notional = NULL, max_leverage = NULL \
             WHERE id = '{run_id}'"
        )],
    )
    .await;
}

fn request(run_id: &str) -> pulse::GetBacktestRunRequest {
    pulse::GetBacktestRunRequest {
        run_id: run_id.to_owned(),
    }
}

/// The reported defect. A run with no recorded inputs, opened later, is refused
/// as `data` with the one message that names the run and the remedy.
///
/// On the base commit the refusal is `SavedButReadBackFailed`'s text — "was saved,
/// but reading back its run failed: a freshly saved run read back with no input
/// provenance" — so the message assertion fails.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_a_run_saved_before_inputs_were_recorded_says_so() {
    let (_tmp, state) = open_state().await;
    let run_id = saved_run_id(&state).await;
    make_run_legacy(&state, &run_id).await;

    let err = get_backtest_run_core(&state, request(&run_id))
        .await
        .expect_err("a run with no recorded inputs does not open");

    assert_eq!(
        err.code,
        BusErrorCode::Data,
        "the refusal is `data`: {err:?}"
    );
    assert_eq!(
        err.message,
        format!(
            "backtest run {run_id} predates recorded inputs, so it cannot be opened; \
             re-run the version to get a run that opens"
        ),
        "the refusal names the run and the remedy"
    );
    assert!(
        !err.message.contains("freshly saved"),
        "an old run is not a freshly saved one: {}",
        err.message
    );
    assert_eq!(
        err.child_run_id.as_deref(),
        Some(run_id.as_str()),
        "the asked-for id rides `child_run_id`, as the `not_found` refusal carries it"
    );
}

/// The new arm must not catch a run that carries its inputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_with_recorded_inputs_still_opens() {
    let (_tmp, state) = open_state().await;
    let run_id = saved_run_id(&state).await;

    let dto = get_backtest_run_core(&state, request(&run_id))
        .await
        .expect("a run with its inputs opens");

    assert_eq!(dto.run_id, run_id, "the open answers the run asked for");
}
