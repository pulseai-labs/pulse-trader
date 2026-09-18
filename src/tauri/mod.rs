//! `Tauri` ring (outer): the desktop entry point, the command bus and the typed event
//! channel (ADR-0020).
//!
//! **This module's name collides with the `tauri` crate on purpose.** ADR-0020's
//! registered touch surface is `src/tauri/**`, so the ring keeps its name; the extern
//! crate is reached as `tauri::` from inside these files (the crate root, where
//! `mod tauri` is in scope, uses `crate::tauri::` for the ring). Renaming the
//! *dependency* is not an option — `#[tauri::command]` and `generate_handler!` expand to
//! hard-coded `::tauri::` paths.
//!
//! Layout, matching the spec's step 4:
//!
//! | File | Holds |
//! |---|---|
//! | `mod.rs` | the app builder, managed-state wiring, [`run_desktop`] |
//! | `commands.rs` | the bus: the registration list, managed state, the commands |
//! | `events.rs` | the typed per-invocation channel and its payloads |
//! | `error.rs` | the one serializable error shape |
//!
//! **Where the boundary sits.** This ring depends inward on the domain and the adapters
//! and is depended on by nothing — it is the outermost ring, and `run_desktop` is its
//! only entry point. The transport-free command *cores* (`shell_info_core`,
//! `demo_stream_core`) are what carry behaviour; the `#[tauri::command]` wrappers do
//! nothing but adapt the transport, which is why the bus contract is testable without an
//! app handle.

// r1.s3.w3: the Backtest Lab wire contract (DTOs + the pure projection).
pub(crate) mod backtest;
// r1.s4.w3: the coach rail's wire contract (DTOs + the named recoveries) and the
// two command cores. A separate module so `commands.rs` stays the registration
// surface while the projection logic lives beside its own unit tests.
pub(crate) mod coach;
pub(crate) mod commands;
pub(crate) mod error;
pub(crate) mod events;
// r1.s1.w3: the Strategy Library's ring-owned wire DTOs + pure projections.
// A separate module so `commands.rs` stays the registration surface while the
// projection logic lives next to its own unit tests.
pub(crate) mod library;

pub use backtest::{
    BacktestRunDto, BacktestRunRequest, CompareChildRunDto, CompareChildRunRequest, EquityPointDto,
    HistogramBinDto, HistogramDto, RegimeCellDto, TradeRowDto, backtest_run_dto,
};
pub use coach::{
    AcceptFailureDto, AcceptedCoachDto, CoachActionDto, CoachCostDto, CoachDecisionDto,
    CoachDecisionRequestDto, CoachFailureDto, CoachSessionDto, CoachTurnDeps, CoachTurnRequestDto,
    MutationDto, ProposalDto, ReadBackDto, ReadBackOk, SummaryDto, coach_decide_core,
    coach_turn_core,
};
pub use commands::{
    BUS_COMMANDS, ComposeDeps, ComposeDslSummary, ComposeResult, ComposeStrategySummary,
    DesktopState, OperationGuard, OperationKey, ShellInfo, StreamOutcome, compare_child_run_core,
    compose_strategy_core, demo_stream_core, library_overview_core, run_backtest_version_core,
    shell_info_core,
};
pub use error::{BusError, BusErrorCode};
pub use events::{BusEvent, BusEventPayload, EventSink, RunId};
pub use library::{
    DslSummary, LibraryOverview, LibraryRunSummary, LibraryStrategy, LibraryVersion, VersionStats,
};

/// Build the `tauri-specta` builder that owns the command registry.
///
/// **One place, one list.** `collect_commands!` here and [`BUS_COMMANDS`] in
/// `commands.rs` are the two halves of clause 4, and
/// `tests/tauri_bus_contract.rs::command_registration_is_one_append_only_list` asserts
/// they cannot drift: every name in the list must have a matching `async fn`, and the
/// count of `#[tauri::command]` functions must equal the list's length.
///
/// Adding a screen in round 3 means appending **one line** here, one line to
/// `BUS_COMMANDS`, one `async fn`, and one row in `ui/src/routes.ts`.
fn specta_builder() -> tauri_specta::Builder<tauri::Wry> {
    tauri_specta::Builder::<tauri::Wry>::new().commands(tauri_specta::collect_commands![
        commands::shell_info,
        commands::bus_selftest_failure,
        commands::start_demo_stream,
        commands::credential_status,
        commands::library_overview,
        commands::compose_strategy,
        commands::compose_cancel,
        commands::run_backtest_version,
        commands::coach_turn,
        commands::coach_decide,
        commands::compare_child_run,
    ])
}

/// Export the generated TypeScript bindings to `path`.
///
/// Called by `examples/export-bindings.rs`, which `scripts/check-bindings.sh` (AC-8)
/// drives into a temporary file and diffs against the committed `ui/src/bindings.ts`.
/// Generation lives here, next to the registry it reflects, so a command added without
/// regenerating is a **diff**, not a runtime surprise.
///
/// **No repair step.** `tauri-specta` 2.0.0-rc.21's output needed `post_process_bindings`
/// to compile (the `TAURI_CHANNEL` name collision and dead event machinery — see
/// `r1.s1.w1` report §7.2). `r1.s5.w2` bumped the trio to rc.25 and confirmed, by reading
/// the raw generator output before deleting the repair, that neither defect survives: the
/// import is a plain `Channel`, no colliding local declaration is emitted, and no dead
/// `__EventObj__`/`__makeEvents__`/`TAURI_API_EVENT` machinery appears for an empty
/// `collect_events!`. The generator's output is written to `path` directly.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] if the bindings cannot be generated or written.
pub fn export_bindings(path: &std::path::Path) -> anyhow::Result<()> {
    specta_builder()
        .export(specta_typescript::Typescript::default(), path)
        .map_err(|e| anyhow::anyhow!("export tauri-specta bindings to {}: {e}", path.display()))
}

/// The desktop entry point — what a Finder launch reaches (ADR-0020).
///
/// Startup order, and why it is this order:
///
/// 1. **Open and migrate the database first.** `DesktopState::open_default` uses
///    migrate-then-open, so a migration failure refuses to start rather than opening a
///    window onto a half-migrated database. Failing here surfaces as a non-zero exit
///    from `main`, which is the honest outcome — a window that renders an unusable app
///    is worse than no window.
/// 2. **Then build the app**, handing the state to Tauri's managed state so every
///    command shares one pool.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] if the database cannot be opened/migrated, if the tokio
/// runtime cannot be built, or if the Tauri runtime fails to start.
pub fn run_desktop() -> anyhow::Result<()> {
    // The same sync -> async bridge shape the CLI uses (audit C3): a thin sync entry
    // that builds the runtime itself. Tauri's own event loop must own the main thread,
    // so the runtime is entered only for startup work, not wrapped around the app.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("build tokio runtime: {e}"))?;

    let state = runtime
        .block_on(commands::DesktopState::open_default())
        .map_err(|e| anyhow::anyhow!("open the desktop database: {e}"))?;

    let builder = specta_builder();

    tauri::Builder::default()
        .invoke_handler(builder.invoke_handler())
        .manage(state)
        // Keep the tokio runtime alive for the app's lifetime — and NOT for the
        // reason it looks like. Tauri 2 runs async commands on `tauri::async_runtime`,
        // not on this one; a `tokio::Runtime` only becomes the command executor if
        // `tauri::async_runtime::set` is called, which nothing here does. What makes
        // this load-bearing is the `SqlitePool`: it was created inside
        // `runtime.block_on(DesktopState::open_default())` above and is bound to THIS
        // runtime, so dropping it would take the pool's connections with it mid-session.
        // Do not remove this on the reasoning that commands do not run on it.
        .manage(runtime)
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("run the desktop shell: {e}"))
}
