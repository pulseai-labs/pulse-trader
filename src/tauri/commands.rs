//! The command bus (ADR-0020, bus contract clauses 2–4).
//!
//! # The contract this file pins
//!
//! **Clause 2 — async and cancellation.** Every `#[tauri::command]` here is an
//! `async fn`. A synchronous command occupies the IPC thread for its whole duration and
//! the window stops repainting, so "commands are async" is not a style preference — it
//! is the property that keeps a slow query from freezing the app.
//!
//! A streaming command stops on either of **two** cancellation signals, rather than
//! running to completion emitting into nothing:
//!
//!   1. **A failed send.** The far end is genuinely gone — the webview closed, or the
//!      channel was torn down — and the next send errors.
//!   2. **An explicit cancel command.** Unmounting a SCREEN does not do (1): a
//!      JavaScript `Channel`'s callback stays registered with Tauri for the life of the
//!      webview, so every send keeps succeeding and an SPA navigation leaves the run
//!      streaming into a channel nobody reads — billable model calls and a persist the
//!      user walked away from. [`compose_cancel`] is the signal that covers it, tripping
//!      the run's latch in [`DesktopState`]'s in-flight registry.
//!
//! **Clause 3 — managed state ownership.** [`DesktopState`] holds the things that are
//! expensive, shared and long-lived: the `SQLite` pool (opened and migrated **once**, at
//! startup) and the repositories built over it. A command constructs per call only what
//! is cheap and request-scoped. Opening a pool per command would serialize every request
//! behind a fresh connection and defeat WAL.
//!
//! **Clause 4 — one registration point, append-only.** [`BUS_COMMANDS`] is the single
//! list, one entry per line, and `generate_handler!` in `super` wires exactly those. Two
//! work items each adding one screen therefore conflict **textually** — adjacent lines
//! in one file, resolved by keeping both — and never **semantically**. This is what
//! keeps `r1.s1.w3` and `r1.s1.w4` parallel in round 3; the DAG dropped that edge on
//! this property, so weakening it re-creates a dependency the plan was authored without.
//!
//! `tests/tauri_bus_contract.rs` (AC-3) gates all four clauses.

// The `#[tauri::command]` macro expands to a wrapper whose generated signature takes its
// arguments by value and whose body is generated code we do not own. Two pedantic lints
// fire on that expansion rather than on anything written here. Scoped to this module so
// the crate-wide pedantic posture is untouched everywhere else.
#![allow(clippy::needless_pass_by_value, clippy::used_underscore_binding)]

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use chrono::SecondsFormat;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::adapters::backtest::BacktestConfig;
use crate::adapters::broker::BinanceAdapter;
use crate::adapters::store::CandleStore;
use crate::application::backtest::{BacktestRequest, run_version_backtest};
use crate::domain::strategy::VersionId;
use crate::domain::{Pair, Timeframe};

use super::backtest::{BacktestRunDto, BacktestRunRequest, backtest_run_dto};
use super::coach::{
    CoachDecisionDto, CoachDecisionRequestDto, CoachSessionDto, CoachTurnDeps, CoachTurnRequestDto,
    coach_decide_core, coach_turn_core,
};
use super::error::{BusError, BusErrorCode};
use super::events::{BusEvent, BusEventPayload, EventSink, RunId};
use super::library::{
    LibraryOverview, LibraryStrategy, LibraryVersion, dsl_summary, format_expectancy,
    recent_run_summary, version_stats,
};
use crate::adapters::clock::SystemClock;
use crate::adapters::db::{
    Db, SqliteBacktestRunRepo, SqliteLlmCallRepo, SqliteStrategyRepo, default_db_path,
    open_migrated,
};
use crate::adapters::llm::coach_transport::{coach_config, coach_provider};
use crate::adapters::llm::openai_compat::OpenAiCompatProvider;
use crate::adapters::secrets::{llm_credential_status, resolve_llm_api_key};
use crate::agent::ComposerEvent;
use crate::agent::config::{
    load_coach_prompt_from, load_composer_prompt, load_llm_transport, load_price_table,
    prompt_override_dir,
};
use crate::application::coach::CoachTurnRegistry;
use crate::cli::compose::{COMPOSE_CANCELLED, ComposeWiring, compose_config, run_compose_with};
use crate::domain::CoachingSessionId;
use crate::domain::Redactor;
use crate::domain::strategy::{CreatedBy, Strategy, StrategyVersion};
use crate::domain::{
    BacktestRunRepository, Clock, Comparator, Condition, CredentialStatus, DataError, Direction,
    EngineFingerprint, ExitRule, IndicatorSpec, LlmCallRepository, LlmConfig, LlmError,
    LlmProvider, LlmResponse, Message, PriceField, StrategyDsl, StrategyRepository, SweepableValue,
    ToolDefinition, ValueSource,
};

// ---------------------------------------------------------------------------
// Clause 4 — the ONE registration point
// ---------------------------------------------------------------------------

/// **The** command registration list. One entry per line, append-only.
///
/// Adding a screen means adding **one line here** and one `#[tauri::command] async fn`
/// below, and one line to `ui/src/routes.ts`. Nothing else. Do not introduce a second
/// list, do not group entries onto one line, and do not reorder — every one of those
/// turns a clean textual merge conflict into a silent semantic one.
///
/// `tests/tauri_bus_contract.rs::command_registration_is_one_append_only_list` enforces
/// the shape; `super::run_desktop`'s `generate_handler!` is the code that consumes it.
///
/// **`#[rustfmt::skip]` is deliberate and load-bearing, not a style preference.**
/// rustfmt collapses a short array onto one line, and one line is precisely what breaks
/// this contract: two work items each appending a command would then edit the SAME line
/// and produce a conflict a merge tool resolves by picking ONE side — silently dropping
/// the other item's command. One entry per line makes that conflict a two-added-lines
/// diff that is resolved by keeping both. Do not remove this attribute.
#[rustfmt::skip]
pub const BUS_COMMANDS: &[&str] = &[
    "shell_info",
    "bus_selftest_failure",
    "start_demo_stream",
    "credential_status",
    "library_overview",
    "compose_strategy",
    "compose_cancel",
    "run_backtest_version",
    "coach_turn",
    "coach_decide",
];

// ---------------------------------------------------------------------------
// Clause 3 — managed state
// ---------------------------------------------------------------------------

/// What Tauri's managed state owns, shared by every command for the app's lifetime.
///
/// Currently the migrated `SQLite` pool. Repositories are handed out over it by
/// [`DesktopState::strategy_repo`] — cheap wrappers around a cloned pool handle, not new
/// connections. Round 3 adds the backtest-run and LLM-call repos on the same pattern.
///
/// It also owns the **in-flight compose registry**: the cancellation latch of every
/// compose run currently streaming, keyed by its run id. A latch has to outlive the
/// command that created it because the thing that cancels the run — the
/// [`compose_cancel`] command the Designer fires on unmount — arrives on a DIFFERENT
/// invocation and can name the run only by id.
pub struct DesktopState {
    db: Db,
    /// The candle store every backtest reads through (r1.s3.w3).
    ///
    /// Injected here rather than resolved inside the command, for the same reason
    /// `db` is: this struct IS the desktop composition root (ADR-0015), and the
    /// application ring stays generic over `CandleSeriesRepository`. It is also what
    /// lets `tests/tauri_backtest.rs` point the real command at the committed
    /// fixture instead of the user's Application Support directory.
    candles: CandleStore,
    /// Every compose run currently streaming → its cancellation latch (the same
    /// `Arc<AtomicBool>` [`RefusingProvider`] reads before each model turn).
    ///
    /// A `std::sync::Mutex`, deliberately never held across an `.await`: every accessor
    /// below locks, performs one map operation, and drops the guard before it returns.
    compose_runs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    /// The process-local coach-turn single-flight registry (r1.s4.w1), ONE per
    /// process.
    ///
    /// It lives here for the reason `compose_runs` does: it must outlive the command
    /// that created it. A registry minted per turn is not wrong, it is blind — it can
    /// never say "in flight", and telling a LIVE claim from one an earlier process
    /// abandoned is the whole reason it exists.
    coach_registry: CoachTurnRegistry,
    /// Every operation currently running, by key (r1.s4.w3, `#141`).
    ///
    /// The single-flight latch behind "navigating away and back reattaches the same
    /// operation, and a second overlapping invocation is refused". The UI refuses an
    /// overlap before the bus is called; this is what refuses it if reached — from a
    /// second window, a double-click that beats a re-render, or a screen whose state
    /// was rebuilt by a remount.
    ///
    /// A `std::sync::Mutex` around a plain set, never held across an `.await`:
    /// [`DesktopState::begin_operation`] locks, performs one set operation and drops
    /// the guard before returning the RAII guard that releases the key.
    operations: Mutex<HashSet<OperationKey>>,
}

/// What the `#141` latch is keyed on: one running operation per version, and one
/// per coaching session.
///
/// A typed key rather than a formatted string, so a version id and a session id
/// that happen to share text cannot collide, and so the exhaustive `match` in
/// [`OperationKey::describe`] names every kind rather than defaulting one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OperationKey {
    /// A backtest of one strategy version.
    Backtest(VersionId),
    /// A coach turn or decision for one coaching session.
    Coach(CoachingSessionId),
}

impl OperationKey {
    /// How the refusal names this key — the text the rail shows.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Backtest(version) => format!("a backtest of version `{}`", version.as_str()),
            Self::Coach(session) => format!("a coach operation for session `{}`", session.as_str()),
        }
    }
}

/// One held operation key, released on drop.
///
/// RAII rather than a manual release at each return, because "every exit path"
/// includes the ones nobody writes: a `?`, an unwinding panic, and the future being
/// dropped when the webview navigates away mid-call. A guard releases on all three.
///
/// Dropping it releases the KEY, never the work: a durable result already written
/// by the operation stays written, because this guard owns no result.
pub struct OperationGuard<'a> {
    state: &'a DesktopState,
    key: OperationKey,
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        self.state.held_operations().remove(&self.key);
    }
}

impl DesktopState {
    /// Open (and migrate) the database at `path` and take ownership of the pool.
    ///
    /// Uses `open_migrated` — migrate-then-open — so a migration failure **refuses to
    /// start** rather than running the shell against a half-migrated database. That is
    /// the same startup discipline the CLI uses (MASTER-SPEC §7.4).
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the migration or the pool open fails.
    pub async fn open(path: &Path) -> Result<Self, BusError> {
        let candles = CandleStore::with_default_base_dir()?;
        Self::open_with_store(path, candles).await
    }

    /// Open the database at `path` against an explicit candle store.
    ///
    /// The production constructors resolve the platform store; this one takes it, so
    /// a test can drive the REAL command over the committed Parquet fixture in a
    /// temp directory. Same composition root, injected dependency.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the migration or the pool open fails.
    pub async fn open_with_store(path: &Path, candles: CandleStore) -> Result<Self, BusError> {
        let db = open_migrated(path).await?;
        Ok(Self {
            db,
            candles,
            compose_runs: Mutex::new(HashMap::new()),
            coach_registry: CoachTurnRegistry::new(),
            operations: Mutex::new(HashSet::new()),
        })
    }

    /// Open the default `~/Library/Application Support/PulseTrader/pulse.db`.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] if the path cannot be resolved or the open fails.
    pub async fn open_default() -> Result<Self, BusError> {
        let path = default_db_path()?;
        Self::open(&path).await
    }

    /// The candle store this desktop session reads snapshots through.
    #[must_use]
    pub fn candles(&self) -> CandleStore {
        self.candles.clone()
    }

    /// The process-local coach-turn registry (r1.s4.w1) every turn claims through.
    #[must_use]
    pub fn coach_registry(&self) -> &CoachTurnRegistry {
        &self.coach_registry
    }

    /// The held-operation set, with a poisoned lock RECOVERED rather than
    /// propagated (`CoachTurnRegistry::lock`'s discipline).
    ///
    /// A panicking operation must not make every LATER operation unrunnable: the
    /// guard's `Drop` runs during that same unwind, and a poisoned lock there would
    /// leave the key held forever — the latch would have turned one fault into a
    /// permanently jammed screen.
    fn held_operations(&self) -> std::sync::MutexGuard<'_, HashSet<OperationKey>> {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Take single-flight ownership of `key`, or refuse with
    /// [`BusErrorCode::Busy`] when this process is already running it (`#141`).
    ///
    /// The returned guard releases the key on EVERY exit path — return, `?`,
    /// panic-unwind, and the future being dropped by a navigation — because it
    /// releases in `Drop` rather than at a call site someone can forget. It releases
    /// the KEY and nothing else: a durable result the operation already wrote stays
    /// written.
    ///
    /// # Errors
    ///
    /// Returns a [`BusError`] with [`BusErrorCode::Busy`], naming the key, when the
    /// operation is already in flight.
    pub fn begin_operation(&self, key: OperationKey) -> Result<OperationGuard<'_>, BusError> {
        if !self.held_operations().insert(key.clone()) {
            return Err(BusError::new(
                BusErrorCode::Busy,
                format!(
                    "{} is already running; its result will appear here when it finishes",
                    key.describe()
                ),
            ));
        }
        Ok(OperationGuard { state: self, key })
    }

    /// Is `key`'s operation running in this process right now?
    ///
    /// Exists so "the latch was released" is an assertion rather than a claim —
    /// including on the paths (a `BusError`, an unwinding panic) where the release
    /// is the guard's `Drop` and nothing else observable happens.
    #[must_use]
    pub fn operation_in_flight(&self, key: &OperationKey) -> bool {
        self.held_operations().contains(key)
    }

    /// A strategy repository over the shared pool.
    #[must_use]
    pub fn strategy_repo(&self) -> SqliteStrategyRepo<SystemClock> {
        SqliteStrategyRepo::new(self.db.pool().clone())
    }

    /// A backtest-run repository over the shared pool (r1.s1.w3) — the Library's
    /// per-version run reads. Same cheap-wrapper pattern as
    /// [`DesktopState::strategy_repo`]: a cloned pool handle, not a connection.
    #[must_use]
    pub fn backtest_run_repo(&self) -> SqliteBacktestRunRepo<SystemClock> {
        SqliteBacktestRunRepo::new(self.db.pool().clone())
    }

    /// An `LlmCall` ledger repository over the shared pool (r1.s1.w4) — the
    /// same cheap-wrapper-around-the-pool pattern as [`DesktopState::strategy_repo`],
    /// for the compose run's redacted, credential-labelled audit rows.
    #[must_use]
    pub fn llm_call_repo(&self) -> SqliteLlmCallRepo<SystemClock> {
        SqliteLlmCallRepo::with_deps(self.db.pool().clone(), SystemClock)
    }

    /// The owned database handle.
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Register a compose run as in-flight and hand back its cancellation latch.
    ///
    /// The latch is what [`RefusingProvider`] reads before every model turn, so the
    /// registry is the ONLY way a later, separate command can reach into a streaming
    /// run and stop it. Registration happens before the run's first event, so the id
    /// the frontend learns from that event is always already resolvable here.
    #[must_use]
    pub fn register_compose_run(&self, run_id: &RunId) -> Arc<AtomicBool> {
        let latch = Arc::new(AtomicBool::new(false));
        if let Ok(mut runs) = self.compose_runs.lock() {
            runs.insert(run_id.as_str().to_owned(), Arc::clone(&latch));
        }
        latch
    }

    /// Drop a finished run from the registry.
    ///
    /// Called on EVERY exit path of the compose command — success, cancellation and
    /// error alike — so the map holds only runs that are genuinely streaming and a
    /// long session cannot accumulate dead latches.
    pub fn finish_compose_run(&self, run_id: &RunId) {
        if let Ok(mut runs) = self.compose_runs.lock() {
            runs.remove(run_id.as_str());
        }
    }

    /// Trip the cancellation latch of an in-flight compose run.
    ///
    /// Returns whether a run by that id was actually in flight. `false` is an
    /// ordinary outcome, not an error: the run may have finished between the
    /// frontend deciding to cancel and this command arriving.
    pub fn cancel_compose_run(&self, run_id: &str) -> bool {
        let latch = match self.compose_runs.lock() {
            Ok(runs) => runs.get(run_id).map(Arc::clone),
            Err(_) => None,
        };
        match latch {
            Some(latch) => {
                latch.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Round-trip command: shell metadata
// ---------------------------------------------------------------------------

/// The metadata the placeholder page renders — the one round-trip command this work
/// item ships.
///
/// Deliberately boring: no credential and no LLM-derived data crosses this boundary, so
/// no risk gate fires on this item. `r1.s1.w4` is where that changes and it carries the
/// controls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ShellInfo {
    /// The crate version this bundle was built from.
    pub app_version: String,
    /// The build-time `engine_fingerprint` (FR-7) — proves the GUI and CLI share one core.
    pub engine_fingerprint: String,
    /// The compiled target triple.
    pub target_triple: String,
    /// How many strategies the database holds — a real read through managed state.
    pub strategy_count: u32,
}

/// The transport-free core of the `shell_info` command.
///
/// Split from the `#[tauri::command]` wrapper so it is drivable from a test without an
/// app handle. The wrapper does nothing but unwrap the managed state and call this.
///
/// # Errors
///
/// Returns a [`BusError`] if the strategy read fails.
pub async fn shell_info_core(state: &DesktopState) -> Result<ShellInfo, BusError> {
    let strategies = state.strategy_repo().list_strategies(true).await?;
    Ok(ShellInfo {
        app_version: env!("CARGO_PKG_VERSION").to_owned(),
        engine_fingerprint: EngineFingerprint::current().as_str().to_owned(),
        target_triple: EngineFingerprint::target().to_owned(),
        strategy_count: u32::try_from(strategies.len()).unwrap_or(u32::MAX),
    })
}

// ---------------------------------------------------------------------------
// The Strategy Library's read (r1.s1.w3, ledger line d2)
// ---------------------------------------------------------------------------

/// How many of a version's runs the details pane's "Recent backtests" list
/// carries. The catalog read is best-effort per row; the cap keeps one
/// long-running version from flooding the pane.
const RECENT_RUN_LIMIT: usize = 5;

/// The transport-free core of the `library_overview` command — the whole
/// Strategy Library payload in one read.
///
/// Every strategy (archived included — the record exists, and the Library hides
/// nothing that is persisted), each with its `version_tree`-ordered versions,
/// each version with its DSL summary, its latest run's stats (`None` when no
/// run exists — the screen renders an em dash there, grill A1), its expectancy
/// delta vs the parent when both carry a run, and its recent run catalog.
///
/// `latest_run_for_version` is fail-closed by design (#39): one corrupt run row
/// is a `BusError` naming the row, not a silently missing KPI. The recent-runs
/// list reads `list_runs_for_version`, the one best-effort read in the port — a
/// bad row costs its row there, not the screen.
///
/// # Errors
///
/// Returns a [`BusError`] if any repository read fails.
pub async fn library_overview_core(state: &DesktopState) -> Result<LibraryOverview, BusError> {
    let strategies_repo = state.strategy_repo();
    let runs_repo = state.backtest_run_repo();
    let strategies = strategies_repo.list_strategies(true).await?;

    let mut wire = Vec::with_capacity(strategies.len());
    for strategy in &strategies {
        let versions = strategies_repo.version_tree(&strategy.id).await?;
        wire.push(library_strategy(strategy, &versions, &runs_repo).await?);
    }
    Ok(LibraryOverview { strategies: wire })
}

/// Project one strategy + its parent-ordered versions into the wire shape.
///
/// `version_tree` guarantees parent-before-child, so a single forward pass can
/// track the expectancies seen so far and compute each child's delta vs its
/// (already-projected) parent without a second read.
async fn library_strategy(
    strategy: &Strategy,
    versions: &[StrategyVersion],
    runs: &SqliteBacktestRunRepo<SystemClock>,
) -> Result<LibraryStrategy, BusError> {
    let mut expectancies: HashMap<&str, Decimal> = HashMap::new();
    let mut wire_versions = Vec::with_capacity(versions.len());

    for version in versions {
        let latest = runs.latest_run_for_version(&version.id).await?;
        let recent = runs.list_runs_for_version(&version.id).await?;
        let stats = latest.as_ref().map(|run| version_stats(&run.summary));

        let delta_vs_parent = match (
            latest.as_ref(),
            version
                .parent_version_id
                .as_ref()
                .and_then(|parent| expectancies.get(parent.as_str())),
        ) {
            (Some(run), Some(parent)) => Some(format_expectancy(run.summary.expectancy - *parent)),
            _ => None,
        };
        if let Some(run) = &latest {
            expectancies.insert(version.id.as_str(), run.summary.expectancy);
        }

        wire_versions.push(LibraryVersion {
            id: version.id.as_str().to_owned(),
            parent_id: version
                .parent_version_id
                .as_ref()
                .map(|parent| parent.as_str().to_owned()),
            created_at: version
                .created_at
                .to_rfc3339_opts(SecondsFormat::Millis, true),
            dsl: dsl_summary(&version.dsl),
            stats,
            delta_vs_parent,
            recent_runs: recent
                .iter()
                .rev()
                .take(RECENT_RUN_LIMIT)
                .map(recent_run_summary)
                .collect(),
        });
    }

    Ok(LibraryStrategy {
        id: strategy.id.as_str().to_owned(),
        name: strategy.name.clone(),
        created_at: strategy
            .created_at
            .to_rfc3339_opts(SecondsFormat::Millis, true),
        pinned_version_id: strategy
            .pinned_version_id
            .as_ref()
            .map(|pinned| pinned.as_str().to_owned()),
        versions: wire_versions,
    })
}

// ---------------------------------------------------------------------------
// Clause 2 — the streaming core, and what cancellation means
// ---------------------------------------------------------------------------

/// How a streaming run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct StreamOutcome {
    /// The run this outcome describes.
    pub run_id: RunId,
    /// How many events actually reached the far end.
    pub emitted: u32,
    /// True when the far end went away mid-run (the screen unmounted).
    pub cancelled: bool,
}

/// Emit `steps` events for `run_id` into `sink`, stopping early if the far end dies.
///
/// The demo stream for this work item: `Started`, then `Progress`, then `Finished`,
/// with `seq` monotonic from 0. `r1.s1.w4` replaces the body with the real compose
/// stream; **the shape of this function is the part that is pinned** — a run id, a
/// sink, a `StreamOutcome`, and cancellation-by-failed-send.
///
/// **Cancellation is a normal return, not an error.** When a screen unmounts its channel
/// drops, and the next `send_event` fails. That is not a fault to report: there is no
/// screen left to report it to, and treating it as an error would put a spurious failure
/// in the log for every user who navigated away mid-run. The loop stops at the first
/// failed send and returns `cancelled: true`, so the caller can distinguish "the user
/// left" from "the run finished".
///
/// The `yield_now` between steps is what makes "a slow command does not block the
/// window" true in practice — it hands control back to the runtime between events
/// instead of monopolising the executor.
///
/// # Errors
///
/// Returns a [`BusError`] only for a genuine failure. A dead sink is cancellation.
pub async fn demo_stream_core<S>(
    run_id: &RunId,
    steps: u32,
    sink: &S,
) -> Result<StreamOutcome, BusError>
where
    S: EventSink + ?Sized,
{
    // A run always opens with `Started` and closes with `Finished`, so fewer than two
    // events is not expressible. A request for fewer is raised rather than rejected --
    // an unterminated one-event stream would leave a screen spinning forever.
    let steps = steps.max(2);

    let mut emitted = 0_u32;
    let mut cancelled = false;

    for seq in 0..steps {
        let payload = if seq == 0 {
            BusEventPayload::Started
        } else if seq + 1 == steps {
            BusEventPayload::Finished {
                message: format!("run complete after {steps} step(s)"),
            }
        } else {
            BusEventPayload::Progress {
                message: format!("step {seq} of {steps}"),
            }
        };

        if sink
            .send_event(BusEvent::new(run_id, seq, payload))
            .is_err()
        {
            cancelled = true;
            break;
        }
        emitted += 1;

        // Cooperative yield: the window stays responsive between events.
        tokio::task::yield_now().await;
    }

    Ok(StreamOutcome {
        run_id: run_id.clone(),
        emitted,
        cancelled,
    })
}

// ---------------------------------------------------------------------------
// The compose stream (r1.s1.w4) — the real streaming core behind the Designer
// ---------------------------------------------------------------------------

/// The compact DSL summary a finalized run returns — the finalize summary
/// card's data, rendered from the fields the persisted version actually
/// carries (the `w3` "real fields" discipline: render what the DSL carries,
/// omit what it does not).
///
/// Lines, not structures, on purpose: the ring renders the DSL's own values
/// into mono summary lines and the screen never parses strategy JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ComposeDslSummary {
    /// The trade side, `"long"` / `"short"` — the DSL's own `Direction` value.
    pub direction: String,
    /// The required entry trigger, e.g. `rsi(14) < 30`.
    pub entry: String,
    /// The gating conditions conjoined with the entry, one line each.
    pub filters: Vec<String>,
    /// The exit rules, one line each (e.g. `stop_loss 5%`, `take_profit 2R`).
    pub exits: Vec<String>,
    /// The risk / sizing inputs, one line each.
    pub risk: Vec<String>,
}

/// What a finalized compose run reports about the strategy it persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ComposeStrategySummary {
    /// The persisted strategy's repo-minted id.
    pub strategy_id: String,
    /// The strategy's name (the DSL's own `name`).
    pub strategy_name: String,
    /// The persisted initial version's repo-minted id.
    pub version_id: String,
    /// Who authored the version — the `strategy_version.created_by` label
    /// (`"composer_llm"` for this run; the pinned serialization strings).
    pub created_by: String,
    /// How many `LlmCall`s produced this version (its provenance count).
    pub llm_call_count: u32,
    /// The compact DSL summary rendered above.
    pub dsl: ComposeDslSummary,
}

/// The outcome of one compose run — [`StreamOutcome`]'s shape (a run id, what
/// actually crossed, cancellation) extended with the finalize payload.
///
/// `strategy` is `None` exactly when the run did not finalize: a cancelled run
/// (the screen went away) carries no summary because nothing persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ComposeResult {
    /// The run this outcome describes.
    pub run_id: RunId,
    /// How many events actually reached the far end.
    pub emitted: u32,
    /// True when the far end went away mid-run (the screen unmounted).
    pub cancelled: bool,
    /// The persisted strategy summary — present iff the run finalized.
    pub strategy: Option<ComposeStrategySummary>,
}

/// The injectable deps of one compose run: the CLI's [`ComposeWiring`] bundle
/// plus the strategy repository the finalized version persists through.
///
/// The core wraps `wiring.provider` in its cancellation guard before handing
/// the wiring to [`run_compose_with`], so the live arm and every test double
/// get identical cancellation behaviour without either knowing about it.
pub struct ComposeDeps<P, R, S, C> {
    /// The LLM-side wiring (provider, ledger repo, redactor, prices, clock,
    /// prompt, credential-source label, chat config) — `run_compose_with`'s input.
    pub wiring: ComposeWiring<P, R, C>,
    /// The repository the finalized `StrategyVersion` persists through.
    pub strategy_repo: S,
}

/// A provider wrapper that ends the run when the far end goes away.
///
/// `run_compose_with`'s `on_event` callback returns `()` — it **cannot abort
/// the compose loop** — so cancellation is delivered at the next seam the loop
/// must pass through: the provider. When a `send_event` fails the shared latch
/// trips, every subsequent `chat()` refuses, and the composer ends the run with
/// a provider error, which the core maps to `cancelled: true` (never a
/// `BusError`). No orphaned compose runs emitting into nothing.
struct RefusingProvider<P> {
    /// The wrapped (live or faked) provider.
    inner: P,
    /// Set by the event sink's failure; read before every `chat()`.
    cancelled: Arc<AtomicBool>,
}

impl<P> LlmProvider for RefusingProvider<P>
where
    P: LlmProvider + Sync,
{
    fn chat(
        &self,
        messages: Vec<Message>,
        tools: &[ToolDefinition],
        config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> + Send {
        let tripped = Arc::clone(&self.cancelled);
        let inner = &self.inner;
        async move {
            if tripped.load(Ordering::SeqCst) {
                return Err(LlmError::Provider(COMPOSE_CANCELLED.to_owned()));
            }
            inner.chat(messages, tools, config).await
        }
    }
}

/// Render a [`StrategyDsl`] into the summary card's line vocabulary — the DSL's
/// own values, compactly, with no field invented and none echoed beyond what
/// the document carries.
fn summarize_dsl(dsl: &StrategyDsl) -> ComposeDslSummary {
    ComposeDslSummary {
        direction: match dsl.direction {
            Direction::Long => "long".to_owned(),
            Direction::Short => "short".to_owned(),
        },
        entry: render_condition(&dsl.entry),
        filters: dsl.filters.iter().map(render_condition).collect(),
        exits: dsl.exits.iter().map(render_exit).collect(),
        risk: vec![
            format!(
                "risk_per_trade {}",
                render_percent(&dsl.risk.risk_per_trade_pct)
            ),
            format!("max_leverage {}x", render_sweepable(&dsl.risk.max_leverage)),
        ],
    }
}

/// The `created_by` label — the same strings `CreatedBy` serializes to (pinned
/// by `strategy.rs`'s own test), so the card's label and the persisted column
/// can never disagree.
fn created_by_label(created_by: CreatedBy) -> String {
    match created_by {
        CreatedBy::Human => "human",
        CreatedBy::ComposerLlm => "composer_llm",
        CreatedBy::CoachLlm => "coach_llm",
        CreatedBy::AutoOptimizer => "auto_optimizer",
        CreatedBy::Migration => "migration",
    }
    .to_owned()
}

/// Render one condition, e.g. `rsi(14) < 30` / `close crosses above ema(200)`.
fn render_condition(condition: &Condition) -> String {
    match condition {
        Condition::Compare { lhs, op, rhs } => format!(
            "{} {} {}",
            render_value_source(lhs),
            render_comparator(*op),
            render_value_source(rhs)
        ),
        Condition::CrossesAbove { lhs, rhs } => {
            format!(
                "{} crosses above {}",
                render_value_source(lhs),
                render_value_source(rhs)
            )
        }
        Condition::CrossesBelow { lhs, rhs } => {
            format!(
                "{} crosses below {}",
                render_value_source(lhs),
                render_value_source(rhs)
            )
        }
        Condition::And { conditions } => render_joined(conditions, "and"),
        Condition::Or { conditions } => render_joined(conditions, "or"),
        Condition::Not { condition } => format!("not ({})", render_condition(condition)),
    }
}

/// Render a conjoined/disjoined condition list, parenthesized per term.
fn render_joined(conditions: &[Condition], joiner: &str) -> String {
    conditions
        .iter()
        .map(|c| format!("({})", render_condition(c)))
        .collect::<Vec<_>>()
        .join(&format!(" {joiner} "))
}

/// The comparator's source rendering (not its `Debug`).
fn render_comparator(op: Comparator) -> &'static str {
    match op {
        Comparator::Gt => ">",
        Comparator::Gte => ">=",
        Comparator::Lt => "<",
        Comparator::Lte => "<=",
        Comparator::Eq => "=",
    }
}

/// Render one operand, e.g. `rsi(14)`, `close`, `30`.
fn render_value_source(source: &ValueSource) -> String {
    match source {
        ValueSource::Constant { value } => value.normalize().to_string(),
        ValueSource::Price { field } => match field {
            PriceField::Open => "open",
            PriceField::High => "high",
            PriceField::Low => "low",
            PriceField::Close => "close",
            PriceField::Volume => "volume",
        }
        .to_owned(),
        ValueSource::Indicator { spec } => render_indicator(spec),
    }
}

/// Render an indicator with its parameters, e.g. `ema(200)`.
fn render_indicator(spec: &IndicatorSpec) -> String {
    match spec {
        IndicatorSpec::Rsi { period } => format!("rsi({})", render_sweepable(period)),
        IndicatorSpec::Ema { period } => format!("ema({})", render_sweepable(period)),
        IndicatorSpec::Adx { period } => format!("adx({})", render_sweepable(period)),
        IndicatorSpec::Macd { fast, slow, signal } => format!(
            "macd({},{},{})",
            render_sweepable(fast),
            render_sweepable(slow),
            render_sweepable(signal)
        ),
    }
}

/// Render one exit rule, e.g. `stop_loss 5%` / `take_profit 2R`.
fn render_exit(rule: &ExitRule) -> String {
    match rule {
        ExitRule::StopLoss { distance_pct } => {
            format!("stop_loss {}", render_percent(distance_pct))
        }
        ExitRule::TakeProfit { target_r } => format!("take_profit {}R", render_sweepable(target_r)),
        ExitRule::TrailingStop { trail_pct } => {
            format!("trailing_stop {}", render_percent(trail_pct))
        }
        ExitRule::TimeStop { max_bars } => format!("time_stop {} bars", render_sweepable(max_bars)),
        ExitRule::SignalExit { condition } => {
            format!("signal_exit {}", render_condition(condition))
        }
    }
}

/// Render a `Decimal`-fraction sweepable as a percentage (`0.05` → `5%`) — the
/// DSL stores decimal fractions; the summary speaks the human unit.
fn render_percent(value: &SweepableValue<Decimal>) -> String {
    match value {
        SweepableValue::Fixed(fraction) => {
            format!("{}%", (*fraction * Decimal::from(100)).normalize())
        }
        SweepableValue::Sweep { .. } => "sweep".to_owned(),
    }
}

/// Render a sweepable's fixed value, or name the sweep (v1 validation rejects
/// sweeps before persist; the label keeps rendering total).
fn render_sweepable<T: std::fmt::Display>(value: &SweepableValue<T>) -> String {
    match value {
        SweepableValue::Fixed(v) => v.to_string(),
        SweepableValue::Sweep { .. } => "sweep".to_owned(),
    }
}

/// A cancelled run's outcome: nothing persisted, so no summary.
///
/// The `strategy: None` is the contract, not a convenience — a caller reading
/// `cancelled: true` may conclude the database is untouched, so this shape is only
/// ever returned from a path that genuinely persisted nothing.
fn cancelled_compose(run_id: &RunId, emitted: u32) -> ComposeResult {
    ComposeResult {
        run_id: run_id.clone(),
        emitted,
        cancelled: true,
        strategy: None,
    }
}

/// Map a genuine (non-cancelled) compose failure onto the bus's one error
/// shape, recovering the error FAMILY from the anyhow chain so the frontend's
/// code stays meaningful (`llm` vs `composer` vs `data`).
///
/// **Walks the whole chain, innermost first, rather than only `root_cause()`.**
/// `root_cause()` alone is a false floor: a typed error that wraps an untyped one
/// has a plain string at its root, and every such failure would classify as
/// `Internal` — which is what happened while `run_compose_with` built its errors
/// with `anyhow!("...: {e}")` (a formatted string with no source) instead of
/// `.context(...)`. Innermost-first keeps the old preference where both apply: a
/// transport failure inside the composer is an `Llm` error, which is the family
/// the user can act on.
fn compose_failure(error: anyhow::Error) -> BusError {
    let code = error
        .chain()
        .rev()
        .find_map(|cause| {
            if cause.downcast_ref::<LlmError>().is_some() {
                Some(BusErrorCode::Llm)
            } else if cause
                .downcast_ref::<crate::agent::ComposerError>()
                .is_some()
            {
                Some(BusErrorCode::Composer)
            } else if cause.downcast_ref::<DataError>().is_some() {
                Some(BusErrorCode::Data)
            } else {
                None
            }
        })
        .unwrap_or(BusErrorCode::Internal);
    // The whole chain, not `to_string()`. An anyhow error Displays only its
    // OUTERMOST layer, so with `.context("compose run failed")` above it the
    // Designer would render exactly that and nothing about what actually went
    // wrong. Joining the chain restores the detail the old `anyhow!("...: {e}")`
    // formatting carried, without the erased source that cost the classifier
    // above its only signal.
    let message = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    BusError::new(code, message)
}

/// The transport-free compose core (r1.s1.w4) — `demo_stream_core`'s pinned
/// shape (a run id, a sink, an outcome, cancellation-by-failed-send) with the
/// composer's real stream in the body.
///
/// Opens the channel with `Started`, then runs [`run_compose_with`] over
/// `deps`, mapping each [`ComposerEvent`] onto a [`BusEventPayload`] as it
/// arrives — `ToolCallStarted` / `ToolCallResult` per step, and the composer's
/// `Finalized` line as the closing `Finished`. The LLM credential is already
/// INSIDE `deps.wiring` (label and redactor alike) — it neither crosses this
/// seam nor appears in any event.
///
/// **`cancelled` is the run's latch, owned by the caller.** Two things trip it,
/// and both end the run as `cancelled: true` rather than as a `BusError`:
///
///   - a failed send, which means the far end is gone; and
///   - the [`compose_cancel`] command, which the Designer fires when it
///     unmounts — the latch lives in [`DesktopState`]'s registry precisely so a
///     separate invocation can reach it.
///
/// Either way [`RefusingProvider`] refuses at the run's next model turn, so
/// cancellation costs at most one further LLM call and nothing is persisted.
///
/// The latch is checked at every point the run can still be abandoned, but NOT
/// after `run_compose_with` returns `Ok`: at that moment the composer has
/// finalized and the version is already persisted. Reporting a persisted
/// strategy as `cancelled` (whose contract is "nothing persisted", see
/// [`cancelled_compose`]) would make the result lie about the database. A cancel
/// that loses the race to the last event is therefore a completed run.
///
/// # Errors
///
/// Returns a [`BusError`] only for a genuine failure (config load is the
/// wrapper's; composer/transport/persist failures arrive here). A dead sink is
/// cancellation.
pub async fn compose_strategy_core<P, R, S, C, K>(
    run_id: &RunId,
    deps: ComposeDeps<P, R, S, C>,
    nl_target: &str,
    sink: &K,
    cancelled: Arc<AtomicBool>,
) -> Result<ComposeResult, BusError>
where
    P: LlmProvider + Send + Sync,
    R: LlmCallRepository + Send + Sync,
    S: StrategyRepository + Send + Sync,
    C: Clock + Send + Sync,
    K: EventSink + Sync + ?Sized,
{
    // A cancel that arrived before the run started is honoured before anything
    // billable happens — no `Started`, no composer, no persist.
    if cancelled.load(Ordering::SeqCst) {
        return Ok(cancelled_compose(run_id, 0));
    }

    // The stream always opens with `Started`. A far end already dead cancels
    // before the composer is ever invoked — no run, no persist.
    if sink
        .send_event(BusEvent::new(run_id, 0, BusEventPayload::Started))
        .is_err()
    {
        return Ok(cancelled_compose(run_id, 0));
    }

    // Wrap the provider in the cancellation guard so a sink that dies MID-run
    // ends the compose loop at its next model turn.
    let ComposeWiring {
        provider,
        llm_repo,
        redactor,
        prices,
        clock,
        prompt,
        key_source,
        config,
    } = deps.wiring;
    let wiring = ComposeWiring {
        provider: RefusingProvider {
            inner: provider,
            cancelled: Arc::clone(&cancelled),
        },
        llm_repo,
        redactor,
        prices,
        clock,
        prompt,
        key_source,
        config,
    };

    let mut emitted = 1_u32;
    let mut seq = 1_u32;
    let events_run_id = run_id.clone();
    let mut on_event = |event: ComposerEvent| {
        // Once the far end is gone, stop emitting — the guard ends the run
        // within one model turn; this keeps the window between airtight too.
        if cancelled.load(Ordering::SeqCst) {
            return;
        }
        let payload = match event {
            ComposerEvent::ToolCallStarted {
                name,
                arguments_preview,
            } => BusEventPayload::ToolCallStarted {
                name,
                arguments_preview,
            },
            ComposerEvent::ToolCallResult { name, outcome } => {
                BusEventPayload::ToolCallResult { name, outcome }
            }
            ComposerEvent::Finalized { version_summary } => BusEventPayload::Finished {
                message: version_summary,
            },
        };
        if sink
            .send_event(BusEvent::new(&events_run_id, seq, payload))
            .is_ok()
        {
            emitted += 1;
        } else {
            cancelled.store(true, Ordering::SeqCst);
        }
        seq += 1;
    };

    match run_compose_with(
        wiring,
        &deps.strategy_repo,
        nl_target,
        &mut on_event,
        &cancelled,
    )
    .await
    {
        Ok(outcome) => Ok(ComposeResult {
            run_id: run_id.clone(),
            emitted,
            cancelled: false,
            strategy: Some(ComposeStrategySummary {
                strategy_id: outcome.strategy.id.as_str().to_owned(),
                strategy_name: outcome.strategy.name.clone(),
                version_id: outcome.version.id.as_str().to_owned(),
                created_by: created_by_label(outcome.version.created_by),
                llm_call_count: u32::try_from(outcome.llm_call_ids.len()).unwrap_or(u32::MAX),
                dsl: summarize_dsl(&outcome.version.dsl),
            }),
        }),
        Err(error) => {
            if cancelled.load(Ordering::SeqCst) {
                Ok(cancelled_compose(run_id, emitted))
            } else {
                Err(compose_failure(error))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The registered commands. One `async fn` per BUS_COMMANDS entry, same order.
// ---------------------------------------------------------------------------

/// Round-trip command: shell + core metadata for the placeholder page.
///
/// # Errors
///
/// Returns a [`BusError`] if the read through managed state fails.
#[tauri::command]
#[specta::specta]
pub async fn shell_info(state: tauri::State<'_, DesktopState>) -> Result<ShellInfo, BusError> {
    shell_info_core(&state).await
}

/// A command that fails **on purpose**, so the error path is demonstrated rather than
/// asserted only in a unit test.
///
/// The placeholder page invokes it and renders the resulting [`BusError`]. Keeping a
/// deliberate-failure command on the bus means the frontend's error rendering is
/// exercised by every developer who opens the app, not only when something breaks.
///
/// # Errors
///
/// Always. That is the point.
#[tauri::command]
#[specta::specta]
pub async fn bus_selftest_failure() -> Result<(), BusError> {
    // A real domain error, mapped through the real `From` impl -- not a synthetic
    // BusError, so this exercises the mapping the frontend actually depends on.
    Err(
        crate::domain::DataError::Parse("deliberate bus self-test failure (r1.s1.w1)".to_owned())
            .into(),
    )
}

/// Start the demo event stream on a **per-invocation** channel.
///
/// The `channel` argument is the whole correlation mechanism: Tauri mints one per
/// `invoke`, so a second run cannot reach the first run's screen.
///
/// # Errors
///
/// Returns a [`BusError`] on a genuine failure; a dropped channel is reported as
/// `cancelled` in the [`StreamOutcome`], not as an error.
#[tauri::command]
#[specta::specta]
pub async fn start_demo_stream(
    steps: u32,
    channel: tauri::ipc::Channel<BusEvent>,
) -> Result<StreamOutcome, BusError> {
    let run_id = RunId::new();
    demo_stream_core(&run_id, steps.min(64), &channel).await
}

// ---------------------------------------------------------------------------
// The no-credential banner's seam (r1.s1.w5, grill G4/A7)
// ---------------------------------------------------------------------------

/// Report which credential source would answer an LLM call, without exposing the
/// credential itself — the no-credential banner's read.
///
/// This is `llm_credential_status`'s first production caller (`src/adapters/secrets.rs`
/// r1.s1.w2), which is what makes removing its `#[allow(dead_code)]` sound rather than
/// a bare grep of convenience: `deny(warnings)` would not let the allow come off before
/// a real caller existed.
///
/// No `Result`: the read has no failure mode (an unresolvable credential reads as
/// [`CredentialStatus::None`], not an error), so wrapping it in one would claim a
/// failure mode this command does not have.
#[tauri::command]
#[specta::specta]
pub async fn credential_status() -> CredentialStatus {
    llm_credential_status()
}

// ---------------------------------------------------------------------------
// The Strategy Library's read (r1.s1.w3) — the app's first real screen
// ---------------------------------------------------------------------------

/// The Strategy Library's one read: every strategy, its version tree, per-version
/// stats where a persisted run exists, and each version's recent run catalog.
///
/// A pure read — the Library writes nothing (ADR-0010); pin/archive/rename each
/// need a write command and are out of this item's budget.
///
/// # Errors
///
/// Returns a [`BusError`] if any repository read fails — including a corrupt
/// run row surfacing from the fail-closed `latest_run_for_version` (#39).
#[tauri::command]
#[specta::specta]
pub async fn library_overview(
    state: tauri::State<'_, DesktopState>,
) -> Result<LibraryOverview, BusError> {
    library_overview_core(&state).await
}

// The compose command (r1.s1.w4) — the Designer's one bus entry
// ---------------------------------------------------------------------------

/// Compose a strategy from a natural-language target, streaming the composer's
/// tool-call steps over a **per-invocation** channel (grill A2 — the channel is
/// the correlation) until the run finalizes and a persisted, attributable
/// `StrategyVersion` exists.
///
/// **Nothing but the target crosses the boundary in, and no credential crosses
/// it in any direction, ever** (ADR-0016, the risk gate's IPC half): the key
/// resolves INSIDE the ring via [`resolve_llm_api_key`], `key.expose()` reaches
/// exactly two consumers (the provider constructor and `Redactor::from_config`),
/// and the credential-source LABEL is captured before either — the live arm's
/// key discipline (`src/cli/compose.rs`), mirrored.
///
/// An unresolvable credential is a [`BusError`] carrying the resolver's own
/// message — it names every searched location and fails closed (`w2`); the
/// screen renders it, and `w5`'s banner already states the condition globally.
///
/// # Errors
///
/// Returns a [`BusError`] on a config-load failure, an unresolvable credential,
/// or a genuine compose/persist failure. A dropped channel is reported as
/// `cancelled` in the [`ComposeResult`], not as an error.
#[tauri::command]
#[specta::specta]
pub async fn compose_strategy(
    state: tauri::State<'_, DesktopState>,
    nl_target: String,
    channel: tauri::ipc::Channel<BusEvent>,
) -> Result<ComposeResult, BusError> {
    // Config-driven overlays, loaded exactly as the CLI live arm loads them
    // (ADR-0014): prompt + transport + prices are DATA, each with an embedded
    // default, so a relocated binary is self-contained.
    let transport =
        load_llm_transport().map_err(|e| BusError::internal(format!("load llm transport: {e}")))?;
    let prices =
        load_price_table().map_err(|e| BusError::internal(format!("load price table: {e}")))?;
    let prompt = load_composer_prompt()
        .map_err(|e| BusError::internal(format!("load composer prompt: {e}")))?;

    // The credential resolves inside the ring — `w2`'s seam. This is the
    // least-privilege control: the value never appears in an argument, a
    // return value, or an event, because it never leaves this function.
    let key = resolve_llm_api_key().map_err(BusError::from)?;
    // The provenance LABEL, captured before either consumer — all that reaches
    // the persisted ledger rows (the audit-trail control).
    let key_source = key.source();
    // The key's two consumers, and its only two: the redactor (so the persisted
    // copy is scrubbed) and the provider constructor (the live transport).
    let redactor = Redactor::from_config(vec![key.expose().to_owned()]);
    let provider = match transport.base_url {
        Some(base_url) => OpenAiCompatProvider::with_base_url(key.expose().to_owned(), base_url),
        None => OpenAiCompatProvider::new(key.expose().to_owned()),
    };

    let deps = ComposeDeps {
        wiring: ComposeWiring {
            provider,
            llm_repo: state.llm_call_repo(),
            redactor,
            prices,
            clock: SystemClock,
            prompt,
            key_source: Some(key_source),
            config: compose_config(transport.model.as_deref()),
        },
        strategy_repo: state.strategy_repo(),
    };

    // Register BEFORE the run streams its first event, so the id the frontend
    // learns from that event is always already cancellable, then deregister on
    // every exit path — success, cancellation and error alike.
    let run_id = RunId::new();
    let cancelled = state.register_compose_run(&run_id);
    let outcome = compose_strategy_core(&run_id, deps, &nl_target, &channel, cancelled).await;
    state.finish_compose_run(&run_id);
    outcome
}

/// Cancel an in-flight compose run by id.
///
/// The Designer fires this from its unmount cleanup. Without it, navigating away
/// mid-compose left the run streaming into a channel nobody read: the JavaScript
/// `Channel`'s callback stays registered with Tauri for the life of the webview, so
/// every send kept SUCCEEDING, the failed-send guard never tripped, and the
/// remaining billable LLM calls ran to completion and persisted a strategy the user
/// had already walked away from.
///
/// Tripping the latch makes [`RefusingProvider`] refuse at the run's next model turn,
/// so the run stops within one LLM call and persists nothing.
///
/// Returns whether a run by that id was in flight. `false` is an ordinary outcome —
/// the run may have finished between the frontend deciding to cancel and this command
/// arriving — not an error, so the Designer's cleanup needs no failure path.
///
/// # Errors
///
/// Never. The `Result` is the bus's uniform command shape.
#[tauri::command]
#[specta::specta]
pub async fn compose_cancel(
    state: tauri::State<'_, DesktopState>,
    run_id: String,
) -> Result<bool, BusError> {
    Ok(state.cancel_compose_run(&run_id))
}

/// Run one persisted strategy version and answer from the row it just wrote
/// (r1.s3.w3) — the drivable core, split from the `#[tauri::command]` wrapper so a
/// test reaches it without a webview (the `library_overview_core` pattern).
///
/// **The request carries only a version id.** r1's Lab runs the fixed BTCUSDT
/// M15+H4 / default-cost configuration; those are product defaults, and a field the
/// user cannot vary would be a control that does not exist.
///
/// **A normal request/response command, not a `Channel`.** The r1 target is under
/// five seconds, there is no meaningful progress to report, and a percentage bar
/// over an opaque engine loop would be fiction. There is likewise no cancellation
/// path: cancelling between the commit and the read-back would produce exactly the
/// ambiguous half-state this item exists to eliminate.
///
/// **Single-flight (r1.s4.w3, `#141`).** The whole call is held under the
/// operation latch keyed on the version, released through an RAII guard on every
/// exit path. A second invocation for the SAME version while one is in flight is
/// refused with [`BusErrorCode::Busy`] and starts no second engine run — the case
/// that used to persist two runs when a remount re-enabled the Run button.
///
/// # Errors
///
/// Returns a [`BusError`]. When the run was saved but could not be read back, its
/// `run_id` field carries the persisted id.
pub async fn run_backtest_version_core(
    state: &DesktopState,
    request: BacktestRunRequest,
) -> Result<BacktestRunDto, BusError> {
    let _operation =
        state.begin_operation(OperationKey::Backtest(VersionId::new(&request.version_id)))?;
    let strategies = state.strategy_repo();
    let runs = state.backtest_run_repo();
    let candles = state.candles();
    let app_request = BacktestRequest {
        version_id: VersionId::new(request.version_id),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: BacktestConfig::default(),
    };
    let outcome = run_version_backtest(
        &strategies,
        &candles,
        &BinanceAdapter::new(),
        &runs,
        &app_request,
    )
    .await?;
    // The projection is fallible on purpose: a saved value that will not fit the wire
    // refuses, and the refusal still names the run that exists.
    Ok(backtest_run_dto(&outcome)?)
}

/// `run_backtest_version` — the Backtest Lab's one command (r1.s3.w3).
///
/// # Errors
///
/// Returns a [`BusError`]; see [`run_backtest_version_core`].
#[tauri::command]
#[specta::specta]
pub async fn run_backtest_version(
    state: tauri::State<'_, DesktopState>,
    request: BacktestRunRequest,
) -> Result<BacktestRunDto, BusError> {
    run_backtest_version_core(&state, request).await
}

/// `coach_turn` — start or reload one coach turn for a persisted run (r1.s4.w3).
///
/// This wrapper is where the credential lives, exactly as [`compose_strategy`]'s
/// is: the config overlays load, the key resolves, the redactor and the provider
/// are built from it, and the core receives everything EXCEPT the key. It therefore
/// appears in no argument, no return value, no event, no error and no DTO, because
/// it never leaves this function (ADR-0016).
///
/// The prompt and its version resolve together from the same bytes (audit C2), so
/// the ledger row's `prompt_version` is a true answer to "which prompt produced
/// this?" — including when an operator's `$PULSE_PROMPT_DIR/coach.md` overlay won.
///
/// **The transport makes ONE attempt per turn** at the coach's own request timeout
/// ([`coach_provider`]), the same constructor `pulse coach` builds through: a turn
/// records one exchange and names one ledger row, and the retrying default would put
/// three upstream attempts and their cost behind that one record.
///
/// # Errors
///
/// Returns a [`BusError`] on a config-load failure, an unresolvable credential, a
/// live duplicate (`busy`), an absent run, or an unrecordable turn. A provider
/// TRANSPORT fault is not an error — it comes back as a recorded failed session.
#[tauri::command]
#[specta::specta]
pub async fn coach_turn(
    state: tauri::State<'_, DesktopState>,
    request: CoachTurnRequestDto,
) -> Result<CoachSessionDto, BusError> {
    let transport =
        load_llm_transport().map_err(|e| BusError::internal(format!("load llm transport: {e}")))?;
    let prices =
        load_price_table().map_err(|e| BusError::internal(format!("load price table: {e}")))?;
    // The operator's overlay is honoured here for the same reason `pulse coach`
    // honours it: an overlay edit must change what the coach says AND what the
    // ledger records.
    let prompt = load_coach_prompt_from(prompt_override_dir().as_deref())
        .map_err(|e| BusError::internal(format!("load coach prompt: {e}")))?;

    // The credential resolves inside the ring and is consumed by exactly two
    // things — the redactor (so the persisted copy is scrubbed) and the provider
    // constructor — then dropped with this frame.
    let key = resolve_llm_api_key().map_err(BusError::from)?;
    let key_source = key.source();
    let redactor = Redactor::from_config(vec![key.expose().to_owned()]);
    // The SHARED coach transport (#165 review R6): one constructor, so this surface
    // and `pulse coach` cannot end up with different retry, timeout or model
    // postures — and so swapping in a retrying provider here would have to be a
    // visible edit rather than a one-word substitution.
    let provider = coach_provider(key.expose(), transport.base_url.as_deref());

    let deps = CoachTurnDeps {
        provider,
        prices,
        redactor,
        key_source: Some(key_source),
        // The COACH's knobs, shared with `pulse coach` (#164) — a coach turn asks a
        // whole backtest's worth of question and the model reasons before it calls a
        // tool, so the composer's step-sized cap cut the turn off mid-thought.
        config: coach_config(transport.model.as_deref()),
        prompt: prompt.text,
        prompt_version: Some(prompt.version),
        turn_timeout: None,
        max_dsl_bytes: None,
    };
    coach_turn_core(&state, deps, request).await
}

/// `coach_decide` — modify, reject or accept one recorded proposal (r1.s4.w3).
///
/// No credential, no provider and no config overlay: an accept re-runs the parent
/// run's exact persisted inputs through the real engine and asks the coach nothing,
/// so this wrapper is a thin adapter over the core and nothing else.
///
/// # Errors
///
/// Returns a [`BusError`]; see [`coach_decide_core`].
#[tauri::command]
#[specta::specta]
pub async fn coach_decide(
    state: tauri::State<'_, DesktopState>,
    request: CoachDecisionRequestDto,
) -> Result<CoachDecisionDto, BusError> {
    coach_decide_core(&state, request).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{BUS_COMMANDS, BusError, RunId, demo_stream_core};
    use crate::tauri::error::BusErrorCode;
    use crate::tauri::events::{BusEvent, EventSink};
    use std::cell::RefCell;

    struct Collector {
        events: RefCell<Vec<BusEvent>>,
    }

    impl EventSink for Collector {
        fn send_event(&self, event: BusEvent) -> Result<(), BusError> {
            self.events.borrow_mut().push(event);
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_stream_opens_with_started_and_closes_with_finished() {
        let sink = Collector {
            events: RefCell::new(Vec::new()),
        };
        let run_id = RunId::new();
        let outcome = demo_stream_core(&run_id, 3, &sink).await.unwrap();

        assert_eq!(outcome.emitted, 3);
        assert!(!outcome.cancelled);

        let events = sink.events.borrow();
        assert!(matches!(
            events[0].payload,
            crate::tauri::events::BusEventPayload::Started
        ));
        assert!(matches!(
            events[2].payload,
            crate::tauri::events::BusEventPayload::Finished { .. }
        ));
    }

    #[tokio::test]
    async fn a_stream_can_never_be_left_unterminated() {
        // Edge case: a request for 0 or 1 steps cannot express both `Started` and
        // `Finished`, and an unterminated stream would leave a screen spinning. The
        // core raises the count instead of emitting a run with no end.
        for requested in [0_u32, 1] {
            let sink = Collector {
                events: RefCell::new(Vec::new()),
            };
            let outcome = demo_stream_core(&RunId::new(), requested, &sink)
                .await
                .unwrap();
            assert_eq!(
                outcome.emitted, 2,
                "a {requested}-step request must still emit Started + Finished"
            );
            let events = sink.events.borrow();
            assert!(matches!(
                events[0].payload,
                crate::tauri::events::BusEventPayload::Started
            ));
            assert!(matches!(
                events[1].payload,
                crate::tauri::events::BusEventPayload::Finished { .. }
            ));
        }
    }

    #[test]
    fn the_registration_list_is_not_empty() {
        assert!(!BUS_COMMANDS.is_empty());
        assert!(BUS_COMMANDS.contains(&"shell_info"));
    }

    #[test]
    fn internal_errors_carry_the_internal_code() {
        assert_eq!(BusError::internal("x").code, BusErrorCode::Internal);
    }
}
