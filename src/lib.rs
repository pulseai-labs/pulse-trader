//! `PulseTrader` core library.
//!
//! Hexagonal layout via module visibility (MASTER-SPEC §7.1): `mod domain`
//! holds pure types + ports + logic with zero I/O; `mod adapters`, `mod agent`,
//! and `mod tauri` are the outer rings. `pub(crate)` on `domain` enforces the
//! dependency-inward direction *inside* the library; the binary (a separate
//! crate, audit C1) reaches only `run()`.

pub(crate) mod domain;

mod adapters;
mod agent;
// r1.s3.w3: the application ring — use cases shared by the CLI and the desktop
// command. Private like `adapters`; `lib.rs` curates what crosses the crate
// boundary, and the composition roots (`cli::mod`, `tauri::commands::DesktopState`)
// choose the implementations it is generic over.
mod application;
mod cli;
// r1.s1.w1 (ADR-0020): the argv dispatch that decides GUI vs CLI. Kept OUT of
// `cli` on purpose -- it runs BEFORE any surface is chosen, so it cannot live
// inside one of them.
mod entry;
// r2.s1.w2: the `pulse mcp` delivery ring — the ONLY module in the crate that
// may name `rmcp` (`scripts/check-mcp-boundary.sh` enforces it). Private like
// `adapters`; `cli::mcp` is its composition root.
mod mcp;
mod tauri;

// The domain layer is the library's stable public API surface (the port traits
// + value types — "Internal API surface = port traits in mod domain", tech
// context). `mod domain` stays `pub(crate)` so the dependency-inward direction
// is enforced *within* the crate; the curated re-exports below are what external
// consumers (and the integration boundary) actually see. The binary, a separate
// crate (audit C1), still reaches only `run()` — it never imports these.
pub use domain::{
    CANDLE_SCHEMA_VERSION, Candle, CandleSeries, Clock, DataError, DataVersion, Gap,
    MarketDataSource, Pair, Timeframe, ValidationError,
};

// r1.s3.w1 (#112): the candle-snapshot persistence port + the value it returns.
// Re-exported beside `CandleStore` so the contract target
// (`tests/candle_repository.rs`) can drive BOTH a zero-I/O double and the real
// Parquet adapter through the same bound — which is what proves the port, not the
// adapter, is what the use cases depend on (ADR-0015's named exception, closed).
pub use domain::{CandleSeriesRepository, StoredCandleSeries};

// VS-1.2.3 work-3.01: the build-time `EngineFingerprint` domain newtype (FR-7 /
// NFR-2). `current()` is the sha2-256 hex baked in by `build.rs`; `target()` is the
// compiled triple (the arch tag for the cross-arch story); `compare()` is the FR-7
// cross-fingerprint warning mechanism (built-but-unwired this slice — VS-1.2.4
// surfaces it). REQUIRED under `deny(warnings)` + `pub(crate) mod domain` — an
// un-re-exported public domain type is a `dead_code` build error, not a warning.
// 3.03 attaches it to `BacktestResult` and reaches it through this re-export.
pub use domain::EngineFingerprint;

// VS-1.1.3 work-3.01: the streaming `Indicator` port (FR-5). REQUIRED under
// `deny(warnings)` + `pub(crate) mod domain` — a new public domain type unused
// outside its module is a `dead_code` build error, not a warning (VS-1.1.2
// harvested gotcha). The EMA adapter + future indicators implement this seam.
pub use domain::Indicator;

// VS-1.2.1 work-1.02: the MTF-aligned, no-look-ahead candle feed (FR-5 /
// BACKLOG-4). `align(primary, htf)` is the backtest iteration substrate; each
// `AlignedBar` pairs a primary candle with the most-recent already-closed HTF
// bar (or `None`). REQUIRED under `deny(warnings)` + `pub(crate) mod domain` —
// an un-re-exported public domain type is a `dead_code` build error, not a
// warning. (C6: nothing consumes `AlignedBar::htf` this slice; it is the
// substrate validated by its own unit tests.)
pub use domain::{AlignedBar, align};

// VS-1.1.4 work-1.02: the strategy-tree entities + the `StrategyRepository` port
// (FR-4 / FR-11). `Strategy`/`StrategyVersion` are the persisted records (the
// immutable version's `dsl_original` is verbatim — FR-4); `StrategyId`/`VersionId`
// are the `#[serde(transparent)]` String id newtypes the adapter (1.03) fills
// with UUIDs (1.02 is uuid-free); `CreatedBy` is the `created_by` provenance enum;
// `NewVersion` is the create-request; `diff_versions`/`VersionDiff` are the pure
// FR-11 compare. `StrategyRepository` is the create+read-only persistence seam
// 1.03 implements and 1.05's CLI consumes. REQUIRED under `deny(warnings)` +
// `pub(crate) mod domain` — an un-re-exported public domain item is a `dead_code`
// build error, not a warning.
// The entity value types come from `domain::strategy::` directly (the module is
// `pub(crate)`, matching the `adapters::binance::`/`cli::fetch_data::` precedent);
// `StrategyRepository` lives in `domain::port`, re-exported via the `domain`
// curated surface like `MarketDataSource`.
pub use domain::StrategyRepository;
pub use domain::strategy::{
    AgentName, AgentNameError, AgentSubmission, AgentSubmissionId, CreatedBy, HypothesisError,
    NewAgentSubmission, NewVersion, Strategy, StrategyId, StrategyVersion, VersionDiff, VersionId,
    diff_versions,
};
// r2.s1.w1: the strategy-level `Hypothesis` an `agent_submission` carries is a
// DIFFERENT type from the coaching `Hypothesis` re-exported below (different
// bounds, different audit table) — it surfaces under the `AgentHypothesis`
// alias so neither name silently stands in for the other.
pub use domain::strategy::Hypothesis as AgentHypothesis;

// VS-1.1.2 work-2.01: the DSL grammar leaf + predicate layer. These are the
// strategy-as-data contract types (serde-tagged enums) the LLM builder tools
// (FR-3) target and later DSL items (2.02–2.05) compose. Re-exported on the
// same curated-surface pattern as the domain types above.
pub use domain::{
    Comparator, Condition, IndicatorSpec, PriceField, Series, SweepableValue, ValueSource,
};

// VS-1.1.2 work-2.02: the whole-strategy document layer. `StrategyDsl` is the
// top-level document the LLM composes (FR-3) and the backtester executes;
// `ExitRule`/`RiskParams`/`Direction` are its exit/risk vocabulary; the
// hand-rolled `SchemaVersion` (+ its parse error) carries FR-4's semver field.
// Re-exported on the same curated-surface pattern — REQUIRED under
// `deny(warnings)` + `pub(crate) mod domain` (an un-re-exported public domain
// type is a `dead_code` build error, not a warning).
pub use domain::{
    Direction, ExitRule, RiskParams, SchemaVersion, SchemaVersionParseError, StrategyDsl,
};

// r2.s2.w4 (b10, ADR-0024): the one ring-owned DSL display formatter —
// `render::strategy(&StrategyDsl) -> Rendered` plus the per-token functions the
// vocabulary test pins (`render::condition`, `render::exit`, …). The two Tauri
// DTO adapters delegate to it; `tests/dsl_render.rs` proves they can no longer
// disagree. REQUIRED under `deny(warnings)` — a `pub` module reachable only
// in-crate is a `dead_code` build error otherwise.
pub use domain::dsl::render;

// VS-1.1.2 work-2.03: the semantic-validation engine (FR-3 correctable rejection).
// `validate` is the entry point; `ValidatedDsl` is the newtype 2.04's compiler
// accepts (constructible ONLY via `validate`); `FieldError`/`ValidationCode`/
// `ValidationErrors` are the field-pathed, serde-serializable error collection
// that crosses the Tauri boundary later. REQUIRED under `deny(warnings)` +
// `pub(crate) mod domain` (an un-re-exported public domain type is a `dead_code`
// build error, not a warning).
pub use domain::{FieldError, ValidatedDsl, ValidationCode, ValidationErrors, validate};
// VS-1.1.2 work-2.05: the version-safe migration read-path (FR-4). `Migrator`
// detects a document's `schema_version`, migrates the JSON forward to `CURRENT`,
// preserves the verbatim `dsl_original`, and returns an UNvalidated
// current-version `StrategyDsl` (`Loaded`). `Migration`/`MigrationKind`/
// `MigrationError`/`LoadError` are its registry + error vocabulary. Re-exported
// on the same curated-surface pattern — REQUIRED under `deny(warnings)`.
pub use domain::{LoadError, Loaded, Migration, MigrationError, MigrationKind, Migrator};

// VS-1.1.2 work-2.04: the compiler → executable evaluator tree (FR-3 / BACKLOG-3).
// `compile(&ValidatedDsl) -> Result<CompiledStrategy, CompileError>` is the slice
// payoff (demo-2): it folds entry ∧ filters into one effective-entry predicate,
// resolves `Fixed` leaves, and yields the stateless evaluator tree the VS-1.2.x
// backtester walks per candle. `CompiledStrategy`/`CompiledCondition`/
// `CompiledValue`/`CompiledExit`/`CompiledRisk` are the tree types; `EvalContext`
// is the seam VS-1.1.3 indicators + the backtester implement; `stop_price`/
// `take_profit_price` are the pure direction-relative exit-geometry helpers.
// Re-exported on the same curated-surface pattern — REQUIRED under
// `deny(warnings)` + `pub(crate) mod domain`.
pub use domain::{
    CompileError, CompiledCondition, CompiledExit, CompiledRisk, CompiledStrategy, CompiledValue,
    EvalContext, atr_stop_price, compile, stop_price, take_profit_price,
};

// r1.s2.w1 (ADR-0021): the one-mutation framework. `apply(&StrategyDsl,
// &Mutation) -> Result<CandidateDsl, MutationError>` is the whole surface — a
// `CandidateDsl` exists only when the mutated strategy validated AND compiled,
// and every other outcome is a typed `MutationError` a coaching session can
// persist verbatim. `Mutation`/`ParamValue`/`ParamKind` are the parameter-only
// vocabulary (r1). Re-exported on the same curated-surface pattern — REQUIRED
// under `deny(warnings)` + `pub(crate) mod domain`, and the route r1.s2.w2/w3
// and r1.s4 reach the framework through.
pub use domain::{
    CandidateDsl, Mutation, MutationError, ParamKind, ParamValue, apply, sweepable_paths,
};

// r1.s2.w2 (ADR-0021): the coaching session domain. `CoachingSession` is the audit
// record of one coach turn — a `SessionOutcome` that is exactly one of a
// `Proposal` (typed `Mutation` + non-empty `Hypothesis` + `Disposition`) or a typed
// `CoachFailure`, never neither (the never-silence guarantee, made structural).
// `CoachingError` carries the two domain refusals (empty hypothesis, illegal
// disposition transition). Re-exported on the same curated-surface pattern —
// REQUIRED under `deny(warnings)` + `pub(crate) mod domain`.
pub use domain::{
    CoachContext, CoachFailure, CoachingError, CoachingSession, CoachingSessionId, Disposition,
    DispositionKind, Hypothesis, MfeMaeAggregates, Proposal, SessionOutcome,
};
// r1.s4.w4 (ADR-0010 / ADR-0018 / ADR-0019 / ADR-0021 as amended): the coach
// LIFECYCLE value types migration `0008` makes storable — the pre-call claim and
// its three semantic results, the one settling move, the typed accept failure, and
// the identity-free prepared acceptance whose child/run ids the adapter mints.
// Re-exported because `tests/migration_0008.rs` and `tests/coaching_repo.rs` drive
// the real adapters through them, and because an un-re-exported public domain type
// is a `dead_code` BUILD error under `deny(warnings)`.
pub use domain::{
    AcceptFailureStage, AcceptedCoachOutcome, CoachAcceptFailure, CoachRequestFingerprint,
    CoachSessionClaim, CoachSessionClaimResult, InitialCoachOutcome, PreparedBacktest,
    PreparedCoachAcceptance,
};
// r1.s4.w4: the `IdSource` port and its two adapters. Minting a row id became an
// injected dependency when the coach accept started minting child/run identity
// INSIDE its transaction: the in-memory test adapter has to mint "the same way" as
// the SQLite one, and two hidden `Uuid::new_v4()` calls cannot be the same way as
// anything.
pub use adapters::ids::{SeqIdSource, UuidIdSource};
pub use domain::IdSource;

// Binance bulk-ingest API surface (WI-1.1.1.02). The adapter module stays
// private (`mod adapters`); these curated re-exports are the entrypoints WI-05
// wires behind the CLI and the integration boundary consumes. Same pattern as
// the domain re-exports above: the implementation modules stay crate-internal,
// the public surface is explicit.
pub use adapters::binance::{
    BinanceDataSource, BulkMonthSource, FundingEvent, MonthData, MonthOutcome, MonthSource,
    decode_month, ingest_bulk, ingest_window, verify_archive_checksum,
};

// WI-1.1.1.03: the REST incremental top-up surface. `top_up_with` is the
// offline-testable seam (`tests/binance_incremental.rs` drives it over a fixture
// `PageSource` + `FakeClock`); `top_up_incremental` is the production wrapper
// WI-05 calls with the injected `Clock`; `TopUpBoundary` carries the snapshot's
// last-open + last-applied-funding timestamps; `PageSource` is the transport seam.
pub use adapters::binance::{PageSource, TopUpBoundary, top_up_incremental, top_up_with};

// WI-1.1.1.03: the Clock adapters. `SystemClock` is the production clock WI-05
// injects into `BinanceDataSource`; `FakeClock` is the deterministic test double
// the integration suite (`tests/binance_incremental.rs`) drives the closed-candle
// cutoff with (audit C5 — cutoff tested exclusively via FakeClock).
pub use adapters::clock::{FakeClock, SystemClock};

// WI-1.1.1.04: the persistence surface (immutable, content-versioned Parquet).
// Re-exported so the integration boundary (and `tests/parquet_roundtrip.rs`)
// can drive a full `CandleSeries` round-trip without reaching `pub(crate)`
// internals.
pub use adapters::store::{CandleStore, SnapshotProvenance};

// WI-1.1.1.05: the `fetch-data` orchestration surface. The end-to-end OFFLINE
// integration test (`tests/integration_fetch_data.rs`, the auto-demo proxy per
// audit C2) drives `run_fetch_data` over fixture seams + a `FakeClock`, never
// the live network. The CLI depends only on the `MarketDataSource` port + the
// store (NFR-9 / AC-6); these re-exports expose the seam without leaking the
// concrete adapter.
pub use cli::fetch_data::{Action, TfOutcome, TfSummary, ensure_one_tf, years_window_start_ms};
pub use cli::{FetchArgs, run_fetch_data};

// VS-1.3.1 work-1.05: the composition-root demo surface. The OFFLINE e2e test
// (`tests/llm_roundtrip_cli.rs`, the auto-demo per audit C2) drives the injectable
// core `run_llm_check_with` over a FAKE provider + a tempfile-`Db` repo (never a
// live `GlmProvider`, never network/Keychain — MASTER-SPEC §9.4); `LlmCheckOutcome`
// is the persisted-`LlmCall` + response it returns. `LlmArgs`/`run_llm_check` are
// the live-arm surface, re-exported like the other `cli::` entrypoints.
pub use cli::llm::{LlmArgs, LlmCheckOutcome, run_llm_check, run_llm_check_with};

// VS-1.3.2 work-2.05: the `pulse compose` composition-root + demo surface (FR-3 /
// FR-4 / NFR-6, README C8). The OFFLINE e2e (`tests/compose_cli.rs`, demo criterion
// 1) drives the injectable core `run_compose_with` over a FAKE provider + the REAL
// composer + REAL builder tools + a tempfile SQLite repo (never a live LLM —
// MASTER-SPEC §9.4); `ComposeWiring` bundles the LLM-side deps, `ComposeCliOutcome`
// is the persisted `StrategyVersion` + provenance it returns. `ComposeArgs` /
// `run_compose` are the live-arm surface, re-exported like the other `cli::`
// entrypoints. REQUIRED under `deny(warnings)` — a `pub` item unused outside its
// private `cli::compose` module is a `dead_code` build error, not a warning.
pub use cli::compose::{
    ComposeArgs, ComposeCliOutcome, ComposeWiring, run_compose, run_compose_with,
};

// r1.s2.w3: the `pulse coach <run-id>` composition-root + debug surface (ADR-0021 /
// ADR-0017). The three OFFLINE test binaries — `coach_turn` (demo `d6`),
// `coach_failures` (demo `d7`) and `coach_redaction` — drive the injectable core
// `run_coach_with` over a SCRIPTED provider + the REAL coach + REAL `apply()` +
// REAL repos on a tempfile SQLite. NEVER a live LLM: two of the three are ledger
// lines re-run at every future spine close. `CoachWiring` bundles the LLM-side
// deps, `CoachCliOutcome` is the recorded session + the stamped prompt version.
// REQUIRED under `deny(warnings)` — a `pub` item unused outside its private
// `cli::coach` module is a `dead_code` build error, not a warning.
pub use cli::coach::{CoachArgs, CoachCliOutcome, CoachWiring, run_coach, run_coach_with};
// r1.s4.w1 (#132): the SEALED turn's public surface is deliberately THREE items —
// its typed error, the process-local single-flight registry a caller holds across
// turns, and the pure request-fingerprint function. `run_coach_turn` itself, its
// request/settings values stay `pub(crate)`, and its two ports are `pub(crate)` in
// `domain::port` beside every other port (ADR-0015): the turn is reached
// through the composition root, which is what keeps the fragments `#132` names
// (a provider, a capture handle, a run, a trade vector, a version) unassemblable
// from outside. `Coach` and `Coach::new` are GONE, not merely narrowed —
// `scripts/check-coach-boundary.sh` is what keeps them gone.
pub use application::coach::{CoachTurnError, CoachTurnRegistry, coach_request_fingerprint};

// r1.s4.w2: the coach DECISION use case — modify / reject / accept over one
// coaching session. Re-exported on the `run_version_backtest` precedent (the module
// stays private and `lib.rs` curates what crosses): `tests/coach_decision.rs` drives
// this exact function over the REAL SQLite repositories, the REAL acceptance
// adapter, REAL `apply()` and the REAL engine, and an integration-test binary is a
// separate crate that cannot reach a `pub(crate)` item. Nothing here is a fragment
// the `#132` seal is about — the request carries a session id and an action, and
// every port it names is already public.
pub use application::coach_decision::{
    AcceptedCoachResult, CoachAction, CoachActionKind, CoachDecisionError, CoachDecisionOutcome,
    CoachDecisionRequest, run_coach_decision,
};

// VS-1.1.3 work-3.01: the indicator-adapter surface. `Ema` is the walking-skeleton
// `Indicator` adapter; `decimal_to_f64`/`f64_to_decimal_rounded`/`INDICATOR_SCALE`
// are the `Decimal↔f64` conversion seam (the ONLY place floats are allowed).
// Re-exported on the same curated-surface pattern — REQUIRED under
// `deny(warnings)` + `pub(crate) mod adapters` (an un-re-exported public adapter
// item unused outside its module is a `dead_code` build error). Consumed by
// 3.02 (RSI/ADX/MACD), 3.03 (engine/EvalContext), and 3.04/3.05 downstream.
pub use adapters::indicators::convert::{INDICATOR_SCALE, decimal_to_f64, f64_to_decimal_rounded};
pub use adapters::indicators::ema::Ema;

// VS-1.1.3 work-3.02: the RSI + MACD adapters — thin ta-rs wraps behind the same
// `Indicator` port. `Rsi` is Cutler's RSI (EMA-smoothed; 3.04 pins pandas-ta to
// `mamode="ema"`); `Macd` resolves a bare `Macd` spec to the MACD line
// (`EMA(fast) − EMA(slow)`), the v1 #18 default. REQUIRED under `deny(warnings)`
// + `pub(crate) mod adapters` — an un-re-exported public adapter struct unused
// outside its module is a `dead_code` build error (the 3.03 factory consumes
// them next round).
pub use adapters::indicators::macd::Macd;
pub use adapters::indicators::rsi::Rsi;
// VS-1.1.3 work-3.02b: the `Adx` adapter (Wilder `+DI`/`−DI`/ATR), built
// in-adapter because ta-rs v0.5.0 ships no ADX. REQUIRED under `deny(warnings)`
// + `pub(crate) mod adapters` (a new public adapter struct unused outside its
// module is a `dead_code` build error, not a warning); the 3.03 factory that
// consumes it is next round.
pub use adapters::indicators::adx::Adx;
// r2.s2.w2: the `Atr` adapter (Wilder true-range smoothing over the shared
// `wilder.rs` core, schema 1.1.0's `IndicatorSpec::Atr`). REQUIRED under
// `deny(warnings)` + `pub(crate) mod adapters` (a new public adapter struct
// unused outside its module is a `dead_code` build error, not a warning); the
// indicator engine's factory consumes it.
pub use adapters::indicators::atr::Atr;
// VS-1.1.3 work-3.03: the multi-indicator engine that implements the frozen
// `EvalContext` seam over real candles and streaming adapter values. Its
// readiness gate is load-bearing for warmup safety under the current boolean DSL
// evaluator.
pub use adapters::indicators::engine::{EngineError, IndicatorEngine};

// VS-1.2.1 work-1.03: deterministic, sequential backtest engine. The adapter
// owns the concrete indicator engine while composing the pure domain backtest
// types and money-math primitives.
pub use adapters::backtest::{BacktestConfig, first_fully_warm_bar_ms, run_backtest};

// VS-1.1.4 work-1.01: the SQLite persistence foundation. `Db` is the WAL pool
// wrapper (`with_path`/`open_default`/`pool`); `MIGRATOR` is the embedded
// `0001_init` migration set. REQUIRED under `deny(warnings)` + `pub(crate) mod
// adapters` — a new public adapter item unused outside its module is a `dead_code`
// build error, not a warning (VS-1.1.2/1.1.3 harvested gotcha). Their first
// in-crate consumers (1.03's repo + 1.04's backup wrapper) land next round and
// reach them through these re-exports; the new `DataError::{Db,Migration}`
// variants ride the existing `pub use domain::{... DataError ...}` re-export.
pub use adapters::db::{Db, MIGRATOR};

// VS-1.1.4 work-1.03: the SQLite `StrategyRepository` adapter. `SqliteStrategyRepo`
// implements the FR-11 strategy surface + the FR-4 immutable version write/read
// path over `query!`/`query_as!` (the committed `.sqlx/` cache). REQUIRED under
// `deny(warnings)` + `pub(crate) mod adapters` — a new public adapter type unused
// outside its module is a `dead_code` build error, not a warning (§4a-2: the
// `db/mod.rs` re-export alone is necessary but NOT sufficient). 1.05's CLI consumes
// it through the `StrategyRepository` port.
pub use adapters::db::SqliteStrategyRepo;
// VS-1.2.4 work-4.04: the SQLite `BacktestRunRepository` adapter. `SqliteBacktestRunRepo`
// implements the FR-6 persisted-run surface over `query!`/`query_as!` (the
// committed `.sqlx/` cache). REQUIRED under `deny(warnings)` + `pub(crate) mod
// adapters` — a new public adapter type unused outside its module is a `dead_code`
// build error, not a warning (the `db/mod.rs` re-export alone is necessary but NOT
// sufficient). 4.05's CLI consumes it through the `BacktestRunRepository` port.
// Append-only (keep-both with 1.03's re-export at merge).
pub use adapters::db::SqliteBacktestRunRepo;
// VS-1.3.1 work-1.02: the SQLite `LlmCallRepository` adapter. `SqliteLlmCallRepo`
// implements the FR-24 append-only ledger surface over `query!` (the committed
// `.sqlx/` cache), `created_at` from an injected `Clock`, Decimal-as-TEXT cost, and
// a `schema_version` read-reject. REQUIRED under `deny(warnings)` + `pub(crate) mod
// adapters` — a new public adapter type unused outside its module is a `dead_code`
// build error (the `db/mod.rs` re-export alone is necessary but NOT sufficient).
// 1.04's decorator constructs it; 1.05's demo reads a row back. Append-only
// (keep-both with 1.03's re-export at merge).
pub use adapters::db::SqliteLlmCallRepo;
// r1.s2.w2 (ADR-0021): the SQLite `CoachingRepository` adapter. `SqliteCoachingRepo`
// persists one coach turn per transaction — the session row plus its proposal row —
// with `created_at` from an injected `Clock`, the typed `Mutation` and `CoachFailure`
// stored as serde JSON, and a `schema_version` read-reject. REQUIRED under
// `deny(warnings)` + `pub(crate) mod adapters` (the `db/mod.rs` re-export alone is
// necessary but NOT sufficient). `w3`'s coach turn and `r1.s4`'s rail construct it.
pub use adapters::db::SqliteCoachingRepo;
// r1.s4.w1 (#132): the SQLite coach-turn projection — the adapter the composition
// root hands the sealed turn in place of the run + strategy repositories it used to
// coordinate itself. REQUIRED under `deny(warnings)` (the `db/mod.rs` re-export is
// necessary but not sufficient).
pub use adapters::db::SqliteCoachTurnSource;
// r1.s4.w4: the accept half of the coach seam — the real SQLite adapter and the
// deterministic in-memory test adapter at the same product-owned port.
pub use adapters::db::SqliteCoachAcceptanceRepo;
pub use adapters::memory::{InMemoryCoachAcceptanceRepo, MemoryAcceptedChild, MemoryCoachTurn};
// VS-1.1.4 work-1.04: the backup-before-migrate protocol surface. `open_migrated`
// is 1.05's single startup entry (migrate-then-open); `run_migrations_with_backup`
// + `undo_to` + `MigrationOutcome` are the protocol vocabulary tests + the
// integration boundary drive. REQUIRED under `deny(warnings)` + `pub(crate) mod
// adapters` — a new public adapter item unused outside its module is a `dead_code`
// build error, not a warning (VS-1.1.2 harvested gotcha); re-export ALL of them,
// not just the first. Append-only (keep-both with 1.03's re-exports at merge).
pub use adapters::db::{MigrationOutcome, open_migrated, run_migrations_with_backup, undo_to};

// VS-1.2.1 work-1.01: the pure backtester domain foundation (FR-5 / FR-6,
// BACKLOG-4). The trade-record entities (`Trade`/`Fill`/`ExitReason`/
// `TradeSource`), the run aggregate (`BacktestResult`), the error taxonomy
// (`BacktestError`, incl. `NoStopLoss` (G5/#20) + `UnsupportedExit` (C4)), and
// the pure `Decimal`-only money-math (`taker_fee`/`apply_slippage`+`Side`/
// `funding_payment`/`realized_pnl`/`realized_r`/`position_size`) +
// intra-bar collision (`resolve_intra_bar_exit`/`IntraBarExit`). The event loop
// (1.03) composes these; 1.04's CLI renders the result. REQUIRED under
// `deny(warnings)` + `pub(crate) mod domain` — an un-re-exported public domain
// type is a `dead_code` build error, not a warning. Kept additive (work-1.02's
// MTF feed extends the same `backtest` tree at the R1→R2 merge).
pub use domain::{
    BacktestError, BacktestResult, ExitReason, Fill, IntraBarExit, Side, Trade, TradeSource,
    apply_slippage, funding_payment, realized_pnl, realized_r, resolve_intra_bar_exit, taker_fee,
};

// VS-1.2.2 work-2.01: the shared, exchange-aware position sizer (FR-5 / NFR-3,
// BACKLOG-5) — the `pulse-broker` money-math home. `compute_position_size` is the
// **single** sizer sim + (future v3) live execution share (NFR-3 by
// construction). `SymbolFilters` (+ `unconstrained()`) is the exchange-filter
// value type; `SizingOutcome`/`SkipReason` are the skip-and-count substrate (2.04
// wires, 2.05 renders). `ExchangeAdapter` is the exchange-metadata port;
// `ExchangeError` its dedicated error (audit C5). REQUIRED under `deny(warnings)`
// + `pub(crate) mod domain` — an un-re-exported public domain type is a
// `dead_code` build error, not a warning.
//
// NFR-3 single-sizing-path hardening (VS-1.2.2 slice-close, close-audit finding):
// the pre-quantization core `risk_capped_qty` is **intentionally NOT re-exported**
// — it bypasses lot_step / min_qty / min_notional / exchange max-leverage, so
// exposing it publicly would make the single-path invariant a convention rather
// than construction. It stays crate-internal (the `pub(crate) mod domain` gate),
// reachable only by `compute_position_size` (its sole production caller) + the
// in-module proptests; the ONLY public sizing entry is `compute_position_size`.
pub use domain::{
    ExchangeAdapter, ExchangeError, SizingOutcome, SkipReason, SkippedEntryCounts, SymbolFilters,
    compute_position_size,
};

// VS-1.2.2 work-2.01: the `BinanceAdapter` exchange-metadata implementor
// (`adapters/broker`), returning pinned BTCUSDT USD-M futures filters. REQUIRED
// under `deny(warnings)` + `pub(crate) mod adapters` — a new public adapter type
// unused outside its module is a `dead_code` build error. 2.04 wires it into
// `run_backtest`'s sizing call through the `ExchangeAdapter` port.
pub use adapters::broker::BinanceAdapter;

// VS-1.2.2 work-2.03: the regime classifier surface (FR-5 / FR-6, BACKLOG-5).
// `Regime`/`RegimeBreakdown`/`RegimeCell`/`classify`/`ADX_TREND_THRESHOLD` are
// the pure domain half; `RegimeDetector` is the stateful adapter composing the
// VS-1.1.3 `Ema`/`Adx` adapters. REQUIRED under `deny(warnings)` + `pub(crate)
// mod domain`/`mod adapters` — an un-re-exported public item is a `dead_code`
// build error, not a warning. 2.04 steps the detector over the run + aggregates
// the breakdown onto `BacktestResult`; 2.05 renders it.
pub use adapters::backtest::RegimeDetector;
pub use domain::{ADX_TREND_THRESHOLD, Regime, RegimeBreakdown, RegimeCell, classify};

// VS-1.2.4 work-4.01: the derived read-only `SummaryStats` + equity curve surface
// (FR-6 / NFR-2, BACKLOG-4). `SummaryStats`/`EquityCurve`/`EquityPoint` are the
// pure-`Decimal`/`usize` headline read of a finished backtest, computed in
// `LoopState::into_result` and attached to `BacktestResult`; oracle-excluded
// (README C3) so the frozen baseline stays frozen by construction. REQUIRED under
// `deny(warnings)` + `pub(crate) mod domain` — an un-re-exported public domain
// type is a `dead_code` build error, not a warning. 4.02 adds Sharpe/Sortino onto
// the same struct; 4.03/4.04 persist these columns; 4.05 renders + rebuilds the
// curve on read via the SAME `EquityCurve::from_trades` constructor.
pub use domain::{EquityCurve, EquityPoint, SummaryStats};

// VS-1.2.4 work-4.04: the persisted backtest-run system-of-record (FR-6 / FR-7 /
// NFR-2). `BacktestRunRepository` is the create+read-only persistence port (the
// #39 ownership-on-write / re-validate-on-read / scoped corrupt-isolation seam);
// `BacktestRunId`/`PersistedRun`/`RunSummary` are the typed read-back projections
// (explicit columns per README C4, NOT a `BacktestResult` blob — #68 / D1).
// REQUIRED under `deny(warnings)` + `pub(crate) mod domain` — an un-re-exported
// public domain type is a `dead_code` build error, not a warning. 4.05's CLI
// consumes `save_run`/`get_run`/`latest_run_for_version` through the port.
// r1.s3.w2 (#110): the durable INPUT provenance value types ride the same
// re-export (an un-re-exported public domain type is a `dead_code` BUILD error
// under `deny(warnings)`), so `tests/backtest_provenance.rs` and W3's DTO can
// name them.
pub use domain::{
    BacktestInputs, BacktestRunId, BacktestRunRepository, CandleWindow, CandleWindowError,
    FundingConfig, OpenPositionMark, PersistedRun, RunSummary, SeriesEnd, SnapshotSelection,
};

// r2.s3.w3: walk-forward as a run kind (`rolling-oos/v1` + `wf-v1`, ADR-0025) —
// the domain types, the `WalkForwardRunRepository` port (one-transaction save +
// fail-closed read), and the use case. No MCP tool / Tauri command (w5's).
pub use application::walk_forward::{
    WalkForwardAppError, WalkForwardOutcome, WalkForwardRequest, run_walk_forward,
};
pub use domain::{
    FoldScheme, FoldVerdict, K_DEFAULT, K_MAX, K_MIN, N_MIN, RunVerdict, VerdictRule,
    WalkForwardError, WalkForwardFold, WalkForwardFoldDraft, WalkForwardMembership, WalkForwardRun,
    WalkForwardRunDraft, WalkForwardRunId, WalkForwardRunRepository, Z, fold_windows,
    folds_required,
};

// r1.s3.w3: the shared version-id backtest use case (#110's consumer, ledger line
// `d11`). Re-exported because `tests/tauri_backtest.rs` injects post-save read
// failures through the real ports, and because the desktop adapter maps this exact
// error taxonomy onto `BusError`. The module itself stays private.
pub use application::backtest::{
    BacktestAppError, BacktestOutcome, BacktestRequest, HISTOGRAM_BIN_COUNT,
    HISTOGRAM_BIN_WIDTH_STR, Histogram, HistogramBin, ReadBackFailure, ReadBackStage, SnapshotPins,
    histogram_bin_width, project_histogram, resolve_default_request, run_version_backtest,
};

// VS-1.3.1 work-1.01: the LLM domain ring (FR-23 / FR-24, README C1–C5). The
// PulseTrader-OWNED `LlmProvider` port + the message/response/usage/config value
// types + the dedicated `LlmError` + the pure cost model
// (`ModelPrice`/`PriceTable`) + the `LlmCall` ledger entity + its `LlmCallId`
// newtype — all free of any `PulseHive` dep (ADR-0012 insulation; AC-8). REQUIRED under
// `deny(warnings)` + `pub(crate) mod domain` — an un-re-exported public domain
// type is a `dead_code` BUILD error, not a warning (the `domain/mod.rs` re-export
// alone is necessary but NOT sufficient — the harvested dead-code gotcha). 1.02
// (persistence) + 1.03 (GLM adapter) + 1.04 (redacting-logging decorator) consume
// these through the `LlmProvider` port + the value/cost types.
pub use domain::{
    LlmBackend, LlmCall, LlmCallId, LlmConfig, LlmError, LlmProvider, LlmResponse, Message,
    ModelPrice, PriceTable, ReasoningEffort, TokenUsage, ToolCall,
};
// VS-1.3.2 work-2.01: the additive tool-calling transport type (FR-23 / FR-3).
// Mirror of the `domain/mod.rs` re-export (REQUIRED — the dead-code gotcha: the
// module-level re-export alone is necessary but NOT sufficient under
// `deny(warnings)`). Appended as its own line so the parallel 2.03 additions merge
// cleanly.
pub use domain::ToolDefinition;
// VS-1.3.1 work-1.02: the append-only `LlmCall` persistence port (FR-24, README C6)
// — create + read only, `DataError::Db`-erroring, immutability structural in the API
// (enforced by the migration-`0004` triggers). REQUIRED under `deny(warnings)` +
// `pub(crate) mod domain` — an un-re-exported public domain type is a `dead_code`
// BUILD error, not a warning (the `domain/mod.rs` re-export alone is necessary but
// NOT sufficient). 1.04's decorator writes through it; 1.05's demo reads a row back.
pub use domain::LlmCallRepository;
// r1.s2.w2 (ADR-0021 / audit C3): the coaching persistence port — the session row is
// the audit trail, so every coaching outcome persists as a proposal or a typed
// failure, and `llm_call_id` is `None` when no ledger row was correlated to the turn
// (not a proof that no call was attempted). REQUIRED under
// `deny(warnings)` + `pub(crate) mod domain`. `w3` writes through it; `r1.s4`'s rail
// reads and dispositions through it.
pub use domain::CoachingRepository;
// r1.s4.w4: the accept persistence port. Separate from `CoachingRepository`
// because it answers a different question — that one records what a TURN produced,
// this one records what a DECISION did — and because its two semantic operations
// carry an atomicity promise a turn-recording port has no business making.
pub use domain::CoachAcceptanceRepository;

// VS-1.3.1 work-1.03 / VS-1.3.2 work-2.01: the OpenAI-compatible transport adapter +
// the macOS Keychain READ accessor (README C8/C2, FR-23 / FR-3 / FR-1 / NFR-5).
// `OpenAiCompatProvider` (generalized from 1.03's `GlmProvider`, pointed at Ollama
// Cloud) is the anti-corruption layer implementing the `LlmProvider` port over the
// `PulseHive` OpenAI-compatible transport (the ONLY `PulseHive`-importing module —
// AC-9); `glm_api_key` sources the API key from the login Keychain (READ path only).
// REQUIRED under `deny(warnings)` + `pub(crate) mod adapters` — a new public adapter
// item unused outside its module is a `dead_code` BUILD error, not a warning. 1.04's
// decorator composes over the provider; 1.05's composition root injects the key.
pub use adapters::llm::openai_compat::OpenAiCompatProvider;
pub use adapters::secrets::glm_api_key;

// r1.s1.w2: the LLM credential resolver on the risk gate's registered surface.
// `ApiKey`/`CredentialSource`/`CredentialStatus` are the domain value half (an
// un-re-exported public domain type is a `dead_code` BUILD error under
// `deny(warnings)`); `CredentialSearch` + the `_in` injectable cores are the
// adapter half the out-of-crate suites (`tests/credential_source.rs`,
// `tests/credential_redaction.rs`) drive.
//
// The zero-arg `pub(crate)` wrappers (`resolve_llm_api_key` /
// `llm_credential_status`) are deliberately NOT re-exported: they read the real
// process environment. `ApiKey::expose()` also stays `pub(crate)`, so an
// out-of-crate caller can pass a key on but can never read one (least privilege).
pub use adapters::secrets::CredentialSearch;
pub use adapters::secrets::{llm_credential_status_in, resolve_llm_api_key_in};
pub use domain::{ApiKey, CredentialSource, CredentialStatus};

// VS-1.3.1 work-1.04: the redacting + cost-logging `LlmProvider` decorator
// (README C7, FR-24 / NFR-6). `RedactingLoggingProvider` wraps any inner provider
// (1.05 wraps `GlmProvider`), redacts the PERSISTED copy of the prompt/completion
// (grill OQ-A — the model still gets the real bytes), computes the `Decimal` cost
// from `usage` times the `PriceTable`, and writes an `LlmCall` through the
// `LlmCallRepository`; `Redactor` is the scoped, config-driven secret scrubber.
// REQUIRED under `deny(warnings)` + `pub(crate) mod adapters` — a new public
// adapter type unused outside its module is a `dead_code` BUILD error, not a
// warning (the `llm/mod.rs` re-export alone is necessary but NOT sufficient).
// r1.s4.w2 (#150): `Redactor` moved to `domain::redaction` — the pure text kernel
// crossed inward so the application ring keeps ADR-0015's ONE adapters import. The
// crate surface is unchanged: `pulse::Redactor` still resolves, to the same type.
pub use adapters::llm::redacting_logging::RedactingLoggingProvider;
pub use domain::Redactor;

// VS-1.3.2 work-2.04: the composer agent loop surface (FR-3 / FR-4, README C7). The
// `Composer<P>` orchestrator + `compose()` + the `ComposeOutcome` value it RETURNS +
// the streamed `ComposerEvent` + the dedicated `ComposerError` + the `LlmCallCapture`
// provenance handle. Mirror of the `agent/mod.rs` re-export (REQUIRED — the dead-code
// gotcha: the module-level `pub use` alone is necessary but NOT sufficient under
// `deny(warnings)`; a `pub` item unused outside its private module is a `dead_code`
// BUILD error). 2.05's `pulse compose` verb + composition root consume these.
pub use agent::{ComposeOutcome, Composer, ComposerError, ComposerEvent, LlmCallCapture};

// r1.s1.w1 (ADR-0020): the executable-topology surface. `launch_mode` is the pure
// argv decision (`tests/entry_topology.rs` drives both directions over it);
// `launch_mode_from_env` is the call the binary shim makes; `LaunchMode` is the
// two-variant answer. `run_desktop` is the GUI entry point the shim calls when the
// launch carries no user arguments. REQUIRED under `deny(warnings)` + private
// `mod entry`/`mod tauri` -- a `pub` item unused outside its private module is a
// `dead_code` BUILD error, not a warning (the harvested gotcha).
pub use entry::{LaunchMode, launch_mode, launch_mode_from_env};

// r1.s1.w1 (ADR-0020): the desktop-shell surface. NOTE the `crate::` qualifier -- at the
// crate root, the bare path `tauri::` is AMBIGUOUS between this crate's `mod tauri` (the
// outer ring) and the `tauri` DEPENDENCY, and rustc rejects it (E0659). `crate::tauri`
// names the ring unambiguously. The dependency deliberately keeps its own name because
// `#[tauri::command]` / `generate_handler!` expand to hard-coded `::tauri::` paths.
//
// `run_desktop` is the GUI entry point the binary shim dispatches to. Everything else is
// the command-bus contract `tests/tauri_bus_contract.rs` (AC-3/4/5) pins and `r1.s1.w3`,
// `r1.s1.w4` and `r1.s1.w5` code against: `BusError`/`BusErrorCode` (the ONE serializable
// error shape), `BusEvent`/`BusEventPayload`/`RunId`/`EventSink` (the per-invocation typed
// channel), `DesktopState` (managed state), `BUS_COMMANDS` (the append-only registration
// list), and the transport-free cores. REQUIRED under `deny(warnings)` + private
// `mod tauri` -- a `pub` item unused outside its private module is a `dead_code` BUILD
// error, not a warning (the harvested gotcha).
pub use crate::tauri::{
    BUS_COMMANDS, BacktestRunDto, BacktestRunRequest, BusError, BusErrorCode, BusEvent,
    BusEventPayload, CompareChildRunDto, CompareChildRunRequest, ComposeDeps, ComposeDslSummary,
    ComposeResult, ComposeStrategySummary, DesktopState, DslSummary, EquityPointDto, EventSink,
    HistogramBinDto, HistogramDto, LibraryOverview, LibraryRunSummary, LibraryStrategy,
    LibraryVersion, RegimeCellDto, RunId, ShellInfo, StreamOutcome, TradeRowDto, VersionStats,
    backtest_run_dto, compare_child_run_core, compose_strategy_core, demo_stream_core, dsl_summary,
    export_bindings, library_overview_core, run_backtest_version_core, run_desktop,
    shell_info_core, summarize_dsl,
};
// r1.s4.w3: the coach rail's wire contract, its two drivable cores and the `#141`
// single-flight latch. `tests/tauri_coach.rs` is a separate crate and drives the
// REAL cores through these — the `run_backtest_version_core` precedent, with the
// provider injected so the offline suite never needs a credential.
pub use crate::tauri::{
    AcceptFailureDto, AcceptedCoachDto, CoachActionDto, CoachCostDto, CoachDecisionDto,
    CoachDecisionRequestDto, CoachFailureDto, CoachSessionDto, CoachTurnDeps, CoachTurnRequestDto,
    MutationDto, OperationGuard, OperationKey, ProposalDto, ReadBackDto, ReadBackOk, SummaryDto,
    coach_decide_core, coach_turn_core,
};

/// Library entry point invoked by the thin binary shim (`src/main.rs`).
///
/// A thin **sync** entry (audit C1/C3): it delegates to [`cli::run`], which
/// parses args via `clap`, builds a multi-thread `tokio` runtime, and
/// `block_on`s the async `fetch-data` orchestration. There is no `#[tokio::main]`
/// and `main` stays the trivial `Result` → `ExitCode` shim.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on arg-parse failure, runtime-build failure, or
/// when any requested timeframe failed to fetch (non-zero exit, audit C4).
pub fn run() -> anyhow::Result<()> {
    cli::run()
}
