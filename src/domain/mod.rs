//! Domain layer (innermost ring): pure value types, the `MarketDataSource`
//! port, and validation logic. Zero I/O — no `reqwest`/`sqlx`/`polars`/`tokio`
//! in non-test paths (the port's `Send` test uses tokio as a dev-dependency).
//!
//! Dependency policy is "zero I/O", not "zero deps": `serde`, `rust_decimal`,
//! `thiserror`, and `chrono` are permitted.

// VS-1.2.1: the pure-domain backtester foundation — money-math + trade entities
// (work-1.01) and the MTF-aligned, no-look-ahead candle feed (work-1.02).
// `pub(crate)` (matching the `dsl`/`strategy` nested-module precedent) so
// `lib.rs` can curate the public surface via the `domain::backtest::` path.
pub(crate) mod backtest;
mod candle;
mod clock;
// r1.s2.w2 (ADR-0021): the coaching session domain — never-silence outcomes, the
// typed failure taxonomy, and the disposition state machine. Pure + zero-I/O; the
// `CoachingRepository` port lives in `port` beside the other ports.
mod coaching;
mod dsl;
mod error;
// VS-1.2.3 work-3.01: the build-time `EngineFingerprint` domain newtype (FR-7 /
// NFR-2). Pure accessor over `build.rs`-baked env (`PULSE_ENGINE_FINGERPRINT` /
// `PULSE_TARGET_TRIPLE`) plus the FR-7 `compare()` warning mechanism (built but
// unwired this slice — VS-1.2.4 surfaces it).
mod fingerprint;
// r1.s4.w4: the `IdSource` port — "a fresh opaque row id" as an injected
// dependency, so the coach accept's transaction-minted child/run ids are
// deterministic under test the way `Clock` made `created_at` deterministic.
mod ids;
// VS-1.2.2 work-2.01: the dedicated exchange-port error taxonomy (audit C5).
mod exchange;
mod indicator;
// VS-1.3.1 work-1.01: the LLM domain ring (FR-23 / FR-24). `llm` holds the value
// types + dedicated error + pure cost model; `llm_call` the append-only ledger
// entity. Both pure + zero-I/O + free of any `PulseHive` dep (ADR-0012); the
// `LlmProvider` port lives in `port` beside the other ports.
mod llm;
mod llm_call;
mod pair;
mod port;
mod series;
// VS-1.2.2 work-2.01: the shared, exchange-aware position sizer (FR-5 / NFR-3,
// BACKLOG-5) — the `pulse-broker` money-math home as a module. `pub(crate)`
// (matching the `dsl`/`strategy`/`backtest` precedent) so `lib.rs` can curate the
// public surface; the types leave the crate only via the explicit re-exports.
pub(crate) mod sizing;
// `pub(crate)` (matching the `adapters`/`cli` nested-module precedent) so the
// curated `lib.rs` surface can re-export via the `domain::strategy::` path — the
// types still leave the crate only through the explicit `pub use` re-exports.
pub(crate) mod strategy;
// VS-1.3.2 slice-close FIX C: the ONE structural secret-token heuristic shared by
// the compose-time redactor (`agent::composer`) AND the at-rest ledger scrubber
// (`adapters::llm::redacting_logging`), so the persisted copy is never weaker than
// the compose-time scrub. Pure string logic, zero-I/O — a domain-kernel utility.
mod secret;
// r1.s4.w2 (#150, ADR-0012 / ADR-0015 / ADR-0016): the PURE text-redaction kernel.
// `pub(crate)` so `adapters::llm`'s decorator can reach the placeholder constant and
// the two message-level helpers; `Redactor` itself is re-exported below. Moving it
// inward is what returns the application ring to ADR-0015's ONE deliberate adapters
// import. Provider concerns and credential HANDLING did not move.
pub(crate) mod redaction;
mod timeframe;
mod version;

// VS-1.2.1 backtester domain surface: money-math + entities (work-1.01) and the
// no-look-ahead candle feed (work-1.02). Re-exported here so `lib.rs` can curate
// the crate surface; an un-re-exported public domain type is a `dead_code` BUILD
// error under `deny(warnings)`. `AlignedBar` borrows from the input series, so
// its lifetime is tied to the caller's `CandleSeries`.
pub use backtest::{
    AlignedBar, BacktestError, BacktestResult, ExitReason, Fill, IntraBarExit, Side, Trade,
    TradeSource, align, apply_slippage, funding_payment, realized_pnl, realized_r,
    resolve_intra_bar_exit, taker_fee,
};
// VS-1.2.2 work-2.03: the pure regime surface (EMA50/200 + ADX14 classifier).
// Re-exported here so `lib.rs` can curate the crate surface; an un-re-exported
// public domain type is a `dead_code` BUILD error under `deny(warnings)`.
pub use backtest::{ADX_TREND_THRESHOLD, Regime, RegimeBreakdown, RegimeCell, classify};
// VS-1.2.4 work-4.01: the derived read-only summary stats + equity curve surface
// (FR-6 / NFR-2). Re-exported so `lib.rs` can curate the crate surface; an
// un-re-exported public domain type is a `dead_code` BUILD error under
// `deny(warnings)`.
pub use backtest::{EquityCurve, EquityPoint, SummaryStats};
// VS-1.2.4 work-4.04: the persisted backtest-run projection types (FR-6 / FR-7 /
// NFR-2). `BacktestRunId`/`PersistedRun`/`RunSummary` are the typed read-back
// projections the `BacktestRunRepository` port returns; re-exported so `lib.rs`
// can curate the crate surface — an un-re-exported public domain type is a
// `dead_code` BUILD error under `deny(warnings)`.
pub use backtest::{
    BacktestInputs, BacktestRunId, CandleWindow, CandleWindowError, FundingConfig, PersistedRun,
    RunSummary, SeriesEnd, SnapshotSelection,
};
pub use candle::Candle;
pub use clock::Clock;
// VS-1.1.3 work-3.01: the streaming `Indicator` port (FR-5) — the seam every
// concrete indicator adapter implements and the backtester reads through.
pub use dsl::{
    Comparator, Condition, DSL_SCHEMA_VERSION, Direction, ExitRule, IndicatorSpec, PriceField,
    RiskParams, SchemaVersion, SchemaVersionParseError, StrategyDsl, SweepableValue, ValueSource,
};
pub use indicator::Indicator;
// VS-1.1.2 work-2.03: the semantic-validation surface (FR-3 correctable rejection).
pub use dsl::{FieldError, ValidatedDsl, ValidationCode, ValidationErrors, validate};
// VS-1.1.2 work-2.05: the version-safe migration read-path (FR-4).
pub use dsl::{LoadError, Loaded, Migration, MigrationError, MigrationKind, Migrator};
// r1.s2.w1 (ADR-0021): the one-mutation framework the coach and r1.s4's accept
// path both stand on. Re-exported so `lib.rs` can curate the crate surface — an
// un-re-exported public domain type is a `dead_code` BUILD error under
// `deny(warnings)`.
pub use dsl::{
    CandidateDsl, Mutation, MutationError, ParamKind, ParamValue, apply, sweepable_paths,
};
// r1.s2.w2 (ADR-0021): the coaching session domain. Re-exported so `lib.rs` can
// curate the crate surface — an un-re-exported public domain type is a `dead_code`
// BUILD error under `deny(warnings)`.
// r1.s4.w4 adds the lifecycle half: the pre-call claim, the settling move, and the
// accept outcome pair.
pub use coaching::{
    AcceptFailureStage, AcceptedCoachOutcome, CoachAcceptFailure, CoachContext, CoachFailure,
    CoachRequestFingerprint, CoachSessionClaim, CoachSessionClaimResult, CoachTurnProjection,
    CoachingError, CoachingSession, CoachingSessionId, Disposition, DispositionKind, Hypothesis,
    InitialCoachOutcome, MfeMaeAggregates, PreparedBacktest, PreparedCoachAcceptance, ProjectedRun,
    Proposal, SessionOutcome,
};
pub use ids::IdSource;
// VS-1.1.2 work-2.04: the compiler → executable evaluator tree (FR-3). `compile`
// turns a `ValidatedDsl` into a `CompiledStrategy` the backtester walks; the
// `Compiled*` types + `EvalContext` seam + pure exit-geometry helpers are its
// surface.
pub use dsl::{
    CompileError, CompiledCondition, CompiledExit, CompiledRisk, CompiledStrategy, CompiledValue,
    EvalContext, compile, stop_price, take_profit_price,
};
pub use error::{DataError, ValidationError};
// VS-1.2.3 work-3.01: the build-time engine identity (FR-7 / NFR-2). Re-exported
// here so `lib.rs` can curate the crate surface; an un-re-exported public domain
// type is a `dead_code` BUILD error under `deny(warnings)`.
pub use fingerprint::EngineFingerprint;
pub use pair::Pair;
// VS-1.1.4 work-1.02: the `StrategyRepository` port (FR-4 / FR-11) alongside
// `MarketDataSource`. The strategy entity value types are surfaced to `lib.rs`
// via the `pub(crate) mod strategy` path directly (matching the
// `adapters::binance::` precedent), so they are NOT re-listed here.
pub use port::{
    BacktestRunRepository, CandleSeriesRepository, CoachAcceptanceRepository, CoachingRepository,
    ExchangeAdapter, LlmCallRepository, LlmProvider, MarketDataSource, StrategyRepository,
};
// r1.s4.w1 (ADR-0015, one home for ports): the sealed coach turn's two ports. They
// live in `port` like every other port and are re-exported `pub(crate)` rather than
// `pub` because the use case they serve is crate-internal — the composition root
// picks the implementations, and no consumer outside this crate names them.
pub(crate) use port::{AttributedCoachProvider, CoachTurnSource};
// VS-1.3.2 slice-close FIX C: the shared secret-token heuristic. `pub(crate)` (an
// internal cross-ring utility, not a public API surface) — used by the composer
// (agent ring) + the redacting-logging decorator (adapters ring), so it is never
// dead code under `deny(warnings)`.
pub(crate) use secret::looks_like_secret_token;
// r1.s1.w2: the credential value types on the risk gate's registered surface —
// `ApiKey` (opaque by construction: no `Display`, no value-revealing `Debug`),
// `CredentialSource` (the persisted `key_source` audit label) and
// `CredentialStatus` (the value-free banner read `r1.s1.w5` renders). `pub`
// (unlike `looks_like_secret_token`) because they appear in public signatures that
// cross the crate boundary; `lib.rs` mirrors these — an un-re-exported public
// domain type is a `dead_code` BUILD error under `deny(warnings)`.
pub use secret::{ApiKey, CredentialSource, CredentialStatus};
// VS-1.3.1 work-1.01: the LLM domain ring surface (FR-23 / FR-24, README C2–C5).
// The message/response/usage/config value types + the dedicated `LlmError` + the
// pure cost model (`ModelPrice`/`PriceTable`), and the `LlmCall` ledger entity +
// its `LlmCallId` newtype. Re-exported here so `lib.rs` can curate the crate
// surface — an un-re-exported public domain type is a `dead_code` BUILD error
// under `deny(warnings)`. 1.02–1.04 consume these through the `LlmProvider` port.
pub use llm::{
    LlmBackend, LlmConfig, LlmError, LlmResponse, Message, ModelPrice, PriceTable, ReasoningEffort,
    TokenUsage, ToolCall,
};
// VS-1.3.2 work-2.01: the additive tool-calling transport type (FR-23 / FR-3).
// Appended as its own line (NOT folded into the block above) so the parallel 2.03
// re-export additions merge cleanly. Re-exported here so `lib.rs` can curate the
// crate surface — an un-re-exported public domain type is a `dead_code` BUILD error.
pub use llm::ToolDefinition;
// r1.s4.w2 (#150): the scoped, config-driven secret scrubber, now a domain kernel.
// Re-exported here so `lib.rs` can keep the SAME `pulse::Redactor` surface every
// composition root and test binary already names — the move is an address change,
// not an API change.
pub use llm_call::{LlmCall, LlmCallId};
pub use redaction::Redactor;
// r1.s4.w1: the attributed-call pair the sealed coach turn's provider port
// returns. `pub(crate)`: a crate-internal use case's vocabulary (ADR-0015).
pub(crate) use llm_call::{AttributedCall, AttributedCallError};
// VS-1.2.2 work-2.01: the shared sizer surface (FR-5 / NFR-3, BACKLOG-5).
// `compute_position_size` is the single exchange-constrained sizing entry; the
// `SymbolFilters` value type + its `unconstrained()` ctor, and the
// `SizingOutcome`/`SkipReason` skip-and-count substrate (2.04 wires, 2.05
// renders). The dedicated `ExchangeError` (audit C5) rides the `ExchangeAdapter`
// port. Re-exported so `lib.rs` can curate the crate surface — an un-re-exported
// public domain type is a `dead_code` BUILD error under `deny(warnings)`.
// NFR-3 hardening (slice-close): the pre-quantization core `risk_capped_qty` is
// NOT re-exported — it bypasses the exchange constraints, so it stays
// crate-internal (used only by `compute_position_size` + the in-module proptests).
pub use exchange::ExchangeError;
pub use series::{CandleSeries, Gap, StoredCandleSeries};
pub use sizing::{
    SizingOutcome, SkipReason, SkippedEntryCounts, SymbolFilters, compute_position_size,
};
pub use timeframe::Timeframe;
pub use version::DataVersion;

/// Version of the `CandleSeries` on-disk schema (audit C7).
///
/// WI-01 defined no schema version; WI-1.1.1.04 introduces it additively so it
/// can be folded into the content-hash `data_version` (see
/// `crate::adapters::store`). Bumping this constant on any future schema change
/// forces every snapshot to a new `data_version`, preventing a stale snapshot
/// from being mistaken for one written under the new schema.
pub const CANDLE_SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{DataError, ValidationError};

    #[test]
    fn data_error_skeleton_variants_exist_and_serialize() {
        // The audit-C5 documented skeleton: Validation{Unsorted,Duplicate}, Gap, Parse, Io.
        // VS-1.1.4 work-1.01 extends it additively with the SQLite tier's `Db`
        // (connection/query/trigger-ABORT) + `Migration` (apply/verify/backup)
        // variants — both `String`-payload so the domain stays free of
        // `sqlx::Error` (which is not `Serialize`), exactly as `Io` avoids
        // `std::io::Error`.
        let cases = vec![
            DataError::Validation(ValidationError::Unsorted {
                earlier: 1,
                later: 0,
            }),
            DataError::Validation(ValidationError::Duplicate(7)),
            DataError::Gap {
                expected: 900_000,
                found: 1_800_000,
            },
            DataError::Parse("bad decimal".to_string()),
            DataError::Io("disk full".to_string()),
            DataError::Db("near \"SELCT\": syntax error".to_string()),
            DataError::Migration("0001_init failed to apply".to_string()),
        ];

        for err in cases {
            // serde round-trip (errors must cross the Tauri boundary later).
            let json = serde_json::to_string(&err).expect("serialize DataError");
            let back: DataError = serde_json::from_str(&json).expect("deserialize DataError");
            assert_eq!(err, back);
            // thiserror Display is non-empty.
            assert!(!err.to_string().is_empty());
        }
    }
}
