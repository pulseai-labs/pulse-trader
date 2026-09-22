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

use crate::adapters::broker::BinanceAdapter;
use crate::adapters::store::CandleStore;
use crate::application::backtest::{resolve_default_request, run_version_backtest};
use crate::domain::strategy::VersionId;

use super::backtest::{
    BacktestRunDto, BacktestRunRequest, CompareChildRunDto, CompareChildRunRequest,
    backtest_run_dto,
};
use super::coach::{
    CoachDecisionDto, CoachDecisionRequestDto, CoachSessionDto, CoachTurnDeps, CoachTurnRequestDto,
    coach_decide_core, coach_turn_core, summary_dto,
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
use crate::domain::dsl::render;
use crate::domain::strategy::{CreatedBy, Strategy, StrategyVersion};
use crate::domain::{
    BacktestInputs, BacktestRunId, BacktestRunRepository, CandleWindow, Clock, CredentialStatus,
    DataError, EngineFingerprint, LlmCallRepository, LlmConfig, LlmError, LlmProvider, LlmResponse,
    Message, SnapshotSelection, StrategyDsl, StrategyRepository, ToolDefinition,
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
    "compare_child_run",
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
        wire.push(library_strategy(strategy, &versions, &strategies_repo, &runs_repo).await?);
    }
    Ok(LibraryOverview { strategies: wire })
}

/// Project one strategy + its parent-ordered versions into the wire shape.
///
/// `version_tree` guarantees parent-before-child, so a single forward pass can
/// track the expectancies seen so far and compute each child's delta vs its
/// (already-projected) parent without a second read.
///
/// Provenance rides along (r2.s1.w4 C1): every version carries its `created_by`
/// label, and an `external_agent` version additionally carries its
/// `agent_submission` row's name and hypothesis — one extra read per agent
/// version, none for the rest. A missing submission row is reported, not
/// invented: the label still reads `external_agent` and both optional fields
/// stay `None`.
async fn library_strategy(
    strategy: &Strategy,
    versions: &[StrategyVersion],
    strategies: &SqliteStrategyRepo<SystemClock>,
    runs: &SqliteBacktestRunRepo<SystemClock>,
) -> Result<LibraryStrategy, BusError> {
    let mut expectancies: HashMap<&str, Decimal> = HashMap::new();
    let mut wire_versions = Vec::with_capacity(versions.len());

    for version in versions {
        let latest = runs.latest_run_for_version(&version.id).await?;
        let recent = runs.list_runs_for_version(&version.id).await?;
        let stats = latest.as_ref().map(|run| version_stats(&run.summary));

        let (agent_name, hypothesis) = match version.created_by {
            CreatedBy::ExternalAgent => match strategies.get_agent_submission(&version.id).await? {
                Some(submission) => (
                    Some(submission.agent_name.as_str().to_owned()),
                    Some(submission.hypothesis.as_str().to_owned()),
                ),
                None => (None, None),
            },
            _ => (None, None),
        };

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
            created_by: created_by_label(version.created_by),
            agent_name,
            hypothesis,
            dsl: dsl_summary(&version.dsl),
            stats,
            delta_vs_parent,
            recent_runs: recent
                .iter()
                .rev()
                .take(RECENT_RUN_LIMIT)
                .map(recent_run_summary)
                .collect(),
            certified: version.certified,
            latest_walk_forward_run_id: version
                .latest_walk_forward_run_id
                .as_ref()
                .map(|run| run.as_str().to_owned()),
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
/// an error, which the core maps to `cancelled: true` (never a `BusError`).
/// No orphaned compose runs emitting into nothing.
///
/// The refusal is [`LlmError::Local`], not [`LlmError::Provider`] — the typed
/// "this process faulted, not the provider" marker. The call never left the
/// process, so nothing was billed, and the redacting + cost-logging decorator
/// writes `llm_call` rows only for transport faults (PR #169, round 2): a
/// `Provider`-shaped refusal would persist a phantom zero-token round-trip in
/// the accounting ledger.
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
                // `Local`, not `Provider`: this refusal never reached a
                // provider, so it must not mint an `llm_call` ledger row.
                return Err(LlmError::Local(COMPOSE_CANCELLED.to_owned()));
            }
            inner.chat(messages, tools, config).await
        }
    }
}

/// Render a [`StrategyDsl`] into the summary card's line vocabulary — the DSL's
/// own values, compactly, with no field invented and none echoed beyond what
/// the document carries.
///
/// `pub` so `tests/dsl_render.rs` (a separate crate) can prove this adapter and
/// [`crate::tauri::library::dsl_summary`] emit identical lines — the b10
/// consolidation claim. The vocabulary lives in the domain ring's
/// [`render`](crate::domain::dsl::render); this is the thin DTO adapter over it.
#[must_use]
pub fn summarize_dsl(dsl: &StrategyDsl) -> ComposeDslSummary {
    let rendered = render::strategy(dsl);
    ComposeDslSummary {
        direction: rendered.direction,
        entry: rendered.entry,
        filters: rendered.filters,
        exits: rendered.exits,
        risk: rendered.risk,
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
        CreatedBy::ExternalAgent => "external_agent",
    }
    .to_owned()
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
/// **The request carries only a version id.** The shared application resolver
/// ([`resolve_default_request`], r2.s1.w3) decides what the version runs with:
/// the version's parent's latest persisted run (then its own, then the r1
/// BTCUSDT M15+H4 / default-cost configuration) supplies the pair, timeframes,
/// cost model and the exact snapshot pins — the desktop and MCP surfaces run a
/// version identically, and a field the user cannot vary would be a control
/// that does not exist. The desktop sends no window, so the run covers the
/// whole pinned snapshot.
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
    // One resolver for every surface (r2.s1.w3): parent's latest run → own
    // latest run → app defaults. `window: None` — the desktop has no date-range
    // surface, so a pinned snapshot runs in full.
    let app_request = resolve_default_request(
        &strategies,
        &runs,
        &VersionId::new(request.version_id),
        None,
    )
    .await?;
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

/// A one-line, human-readable description of how two recorded
/// [`BacktestInputs`] differ — each differing field named with both values,
/// so the "inputs differ" badge's hover explains itself (G6). Window bounds
/// render RFC 3339; a snapshot names `timeframe@data_version`.
fn describe_input_differences(child: &BacktestInputs, parent: &BacktestInputs) -> String {
    fn ms_rfc3339(ms: i64) -> String {
        chrono::DateTime::from_timestamp_millis(ms).map_or_else(
            || format!("{ms}ms"),
            |dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true),
        )
    }
    let selection = |sel: &SnapshotSelection| {
        format!(
            "{}@{}",
            sel.timeframe.binance_interval(),
            sel.data_version.as_str()
        )
    };
    let opt_selection =
        |sel: &Option<SnapshotSelection>| sel.as_ref().map_or_else(|| "none".to_owned(), selection);
    let window = |w: &Option<CandleWindow>| match w {
        None => "whole snapshot".to_owned(),
        Some(w) => format!("[{}, {})", ms_rfc3339(w.from_ms), ms_rfc3339(w.to_ms)),
    };

    let mut diffs = Vec::new();
    if child.pair != parent.pair {
        diffs.push(format!(
            "pair (child {}, parent {})",
            child.pair, parent.pair
        ));
    }
    if child.primary != parent.primary {
        diffs.push(format!(
            "primary snapshot (child {}, parent {})",
            selection(&child.primary),
            selection(&parent.primary)
        ));
    }
    if child.htf != parent.htf {
        diffs.push(format!(
            "htf snapshot (child {}, parent {})",
            opt_selection(&child.htf),
            opt_selection(&parent.htf)
        ));
    }
    if child.taker_fee_bps != parent.taker_fee_bps {
        diffs.push(format!(
            "taker fee bps (child {}, parent {})",
            child.taker_fee_bps, parent.taker_fee_bps
        ));
    }
    if child.slippage_bps != parent.slippage_bps {
        diffs.push(format!(
            "slippage bps (child {}, parent {})",
            child.slippage_bps, parent.slippage_bps
        ));
    }
    if child.funding != parent.funding {
        diffs.push(format!(
            "funding (child {:?}, parent {:?})",
            child.funding, parent.funding
        ));
    }
    if child.window != parent.window {
        diffs.push(format!(
            "window (child {}, parent {})",
            window(&child.window),
            window(&parent.window)
        ));
    }
    // r2.s3.w2: a lead-in difference is a real provenance difference — two runs
    // over the same counted window warmed on different history. A legacy
    // (pre-0012) parent reports `none`, which is honest, not "same".
    if child.lead_in_from_ms != parent.lead_in_from_ms {
        let lead_in = |v: Option<i64>| v.map_or_else(|| "none".to_owned(), ms_rfc3339);
        diffs.push(format!(
            "lead-in from (child {}, parent {})",
            lead_in(child.lead_in_from_ms),
            lead_in(parent.lead_in_from_ms)
        ));
    }
    format!("recorded inputs differ: {}", diffs.join(", "))
}

/// `compare_child_run`'s transport-free core (r2.s1.w4 C3) — a child's run
/// beside its parent's LATEST run, for ANY child version.
///
/// Until this command the only before/after comparison on the bus was the
/// coach accept's, which exists only for the child the coach just minted. An
/// external agent's child has no coach session, so the comparison is resolved
/// from the persisted rows themselves: the run the caller names, its version's
/// parent, and the parent's latest run.
///
/// The verdict on comparability is honest rather than invented: `inputs_differ`
/// is `BacktestInputs` equality over the two persisted tuples (window
/// included), and when either side predates recorded inputs the flag reads
/// `true` with `inputs_note` saying which side — "cannot be proven equal" is a
/// difference, not an equality.
///
/// # Errors
///
/// Returns a [`BusError`]. The three refusals are [`BusErrorCode::NotFound`]:
/// an unknown run id (carrying the asked-for id in `child_run_id`), a version
/// with no parent, and a parent with no run of its own.
pub async fn compare_child_run_core(
    state: &DesktopState,
    request: CompareChildRunRequest,
) -> Result<CompareChildRunDto, BusError> {
    let runs = state.backtest_run_repo();
    let strategies = state.strategy_repo();

    // 1. The asked-for run. An id that names nothing is `not_found` carrying
    //    the asked-for id — the refusal's field, not just its prose.
    let child_run = runs
        .get_run(&BacktestRunId::new(&request.child_run_id))
        .await?
        .ok_or_else(|| {
            BusError::with_child_run_id(
                BusErrorCode::NotFound,
                format!("no backtest run {}", request.child_run_id),
                request.child_run_id.clone(),
            )
        })?;

    // 2. Its version must name a parent — a root has nothing to compare with.
    let child_version = strategies
        .get_version(&child_run.strategy_version_id)
        .await?
        .ok_or_else(|| {
            BusError::new(
                BusErrorCode::Data,
                format!(
                    "run {} belongs to version {} which no longer exists",
                    child_run.id.as_str(),
                    child_run.strategy_version_id.as_str()
                ),
            )
        })?;
    let parent_version_id = match &child_version.parent_version_id {
        Some(parent) => parent.clone(),
        None => {
            return Err(BusError::new(
                BusErrorCode::NotFound,
                format!(
                    "version {} has no parent — nothing to compare against",
                    child_version.id.as_str()
                ),
            ));
        }
    };

    // 3. The parent's LATEST run — the "before" half.
    let parent_run = runs
        .latest_run_for_version(&parent_version_id)
        .await?
        .ok_or_else(|| {
            BusError::new(
                BusErrorCode::NotFound,
                format!(
                    "parent version {} has no run to compare against",
                    parent_version_id.as_str()
                ),
            )
        })?;

    // 4. `BacktestInputs` equality over the PERSISTED tuples — window included.
    //    A side with no recorded inputs cannot be proven equal, so it reads as
    //    a difference whose note says which side lacks the provenance; when
    //    BOTH sides carry inputs that differ, the note names the differing
    //    fields so the badge's hover explains itself (G6).
    let (inputs_differ, inputs_note) = match (&child_run.inputs, &parent_run.inputs) {
        (Some(child), Some(parent)) if child == parent => (false, None),
        (Some(child), Some(parent)) => (true, Some(describe_input_differences(child, parent))),
        (None, Some(_)) => (
            true,
            Some("child run predates recorded inputs — inputs cannot be compared".to_owned()),
        ),
        (Some(_), None) => (
            true,
            Some("parent run predates recorded inputs — inputs cannot be compared".to_owned()),
        ),
        (None, None) => (
            true,
            Some("neither run carries recorded inputs — inputs cannot be compared".to_owned()),
        ),
    };

    Ok(CompareChildRunDto {
        child_version_id: child_run.strategy_version_id.as_str().to_owned(),
        parent_version_id: parent_version_id.as_str().to_owned(),
        child_run_id: child_run.id.as_str().to_owned(),
        parent_run_id: parent_run.id.as_str().to_owned(),
        // The same `SummaryDto` projection the coach accept uses — one cell
        // shape, so the compare table is one component.
        before: summary_dto(&parent_run.summary)?,
        after: summary_dto(&child_run.summary)?,
        inputs_differ,
        inputs_note,
    })
}

/// `compare_child_run` — the Backtest Lab's child-vs-parent comparison
/// (r2.s1.w4 C3).
///
/// # Errors
///
/// Returns a [`BusError`]; see [`compare_child_run_core`].
#[tauri::command]
#[specta::specta]
pub async fn compare_child_run(
    state: tauri::State<'_, DesktopState>,
    request: CompareChildRunRequest,
) -> Result<CompareChildRunDto, BusError> {
    compare_child_run_core(&state, request).await
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

    /// r2.s3.w2: a lead-in difference is provenance the badge must name —
    /// a windowed child replaying a legacy (pre-0012) parent has `Some`/`None`
    /// lead-ins, and the hover says so rather than reading "inputs equal".
    #[test]
    fn describe_input_differences_names_a_lead_in_difference() {
        use crate::domain::{
            BacktestInputs, CandleWindow, DataVersion, FundingConfig, Pair, SnapshotSelection,
            Timeframe,
        };
        use rust_decimal::Decimal;

        let base = BacktestInputs {
            pair: Pair::new("BTCUSDT"),
            primary: SnapshotSelection {
                timeframe: Timeframe::M15,
                data_version: DataVersion::new("v-primary"),
            },
            htf: None,
            taker_fee_bps: Decimal::new(4, 0),
            slippage_bps: Decimal::new(1, 0),
            funding: FundingConfig::SnapshotRates,
            window: Some(CandleWindow::new(1_700_000_000_000, 1_700_086_400_000).unwrap()),
            lead_in_from_ms: Some(1_699_999_000_000),
        };
        // Same tuple, but the parent predates 0012 — no recorded lead-in.
        let legacy_parent = BacktestInputs {
            lead_in_from_ms: None,
            ..base.clone()
        };

        let text = super::describe_input_differences(&base, &legacy_parent);
        assert!(
            text.contains("lead-in from"),
            "the diff must name the lead-in field: {text}"
        );
        assert!(
            text.contains("none"),
            "a legacy parent's absent lead-in renders as `none`: {text}"
        );

        // And identical lead-ins produce no diff line at all.
        let same = super::describe_input_differences(&base, &base);
        assert!(
            !same.contains("lead-in"),
            "equal lead-ins must not be reported: {same}"
        );
    }
}
