//! The eleven tools of `pulse mcp` (r2.s1.w2, w3; r2.s3.w5).
//!
//! Seven read tools (`w2`): five query the strategy/run repositories and two
//! read the candle store, with exports landing under the per-process exports
//! dir. Two write tools (`w3`): `submit_strategy_version` persists an
//! agent-authored DSL variant through the application submit use case, and
//! `run_backtest` runs a version through the shared application flow with an
//! optional `[from, to)` candle window. Two walk-forward tools (r2.s3.w5):
//! `run_walk_forward` walks a version over `rolling-oos/v1` folds through the
//! same shared flow and `get_walk_forward_run` reads one persisted run back —
//! both answer the ONE `WalkForwardRunDetail` shape, and every fold is an
//! ordinary `backtest_run` row the unchanged `list_runs`/`get_run` surfaces
//! resolve.
//!
//! Argument validation failures come back as tool errors in the
//! `{"field", "message"}` shape (the spec's `FieldError` contract); store/repo
//! failures carry just `message`. A DSL validation failure carries EVERY
//! `FieldError` under `errors[]` — the agent fixes one document, not one
//! error at a time.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::adapters::clock::SystemClock;
use crate::adapters::db::{SqliteBacktestRunRepo, SqliteStrategyRepo};
use crate::adapters::indicators::engine::IndicatorEngine;
use crate::application::backtest::{
    BacktestAppError, resolve_default_request, run_version_backtest,
};
use crate::application::mcp_read::{
    parse_indicator_specs, run_detail, run_list_entry, strategy_entry, version_detail,
    version_entry,
};
use crate::application::mcp_write::{
    SubmitError, SubmitRequest, SubmitTarget, submit_agent_version,
};
use crate::application::walk_forward::{WalkForwardAppError, WalkForwardRequest, run_walk_forward};
use crate::application::walk_forward_read::{
    load_fold_runs, rfc3339_secs, run_summary_of, walk_forward_run_detail,
};
use crate::domain::strategy::{StrategyVersion, VersionId};
use crate::domain::{
    BacktestError, BacktestRunId, BacktestRunRepository, CandleSeriesRepository, CandleWindow,
    CompiledValue, DataError, DataVersion, EvalContext, MfeMaeAggregates, Pair, PersistedRun,
    Series, StrategyRepository, Timeframe, ValidationCode, WalkForwardRunId,
    WalkForwardRunRepository,
};

use super::PulseMcp;
use super::export;

/// `list_strategies` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListStrategiesArgs {
    /// Include archived strategies (default false).
    #[serde(default)]
    include_archived: bool,
}

/// `get_version` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetVersionArgs {
    /// The strategy-version id.
    version_id: String,
}

/// `list_runs` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListRunsArgs {
    /// The strategy-version id whose runs to list.
    version_id: String,
}

/// `get_run` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetRunArgs {
    /// The backtest run id.
    run_id: String,
}

/// `export_trades` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExportTradesArgs {
    /// The backtest run id whose trade log to export.
    run_id: String,
}

/// The `export_candles` file format.
#[derive(Debug, Clone, Copy, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CandleExportFormat {
    /// Render the series as text rows (default).
    #[default]
    Csv,
    /// A byte copy of the stored Parquet snapshot file.
    Parquet,
}

/// `export_candles` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExportCandlesArgs {
    /// The trading pair symbol (e.g. `BTCUSDT`).
    pair: String,
    /// The candle timeframe (`15m`/`M15`, `4h`/`H4`).
    timeframe: String,
    /// The exact snapshot `data_version`; omitted means `HEAD`.
    #[serde(default)]
    data_version: Option<String>,
    /// `csv` (default) or `parquet`.
    #[serde(default)]
    format: CandleExportFormat,
}

/// `export_indicators` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExportIndicatorsArgs {
    /// The trading pair symbol (e.g. `BTCUSDT`).
    pair: String,
    /// The candle timeframe (`15m`/`M15`, `4h`/`H4`).
    timeframe: String,
    /// The exact snapshot `data_version`; omitted means `HEAD`.
    #[serde(default)]
    data_version: Option<String>,
    /// Indicator specs as `<kind>:<period>` (e.g. `rsi:14`, `ema:50`); an empty
    /// list selects the `rsi:14, ema:50, adx:14` default set.
    indicators: Vec<String>,
}

/// `submit_strategy_version` args (r2.s1.w3).
///
/// `dsl` is typed `serde_json::Map` so the advertised input schema says
/// `type: object` — the application `SubmitRequest` keeps the wider `Value`
/// and the use case serializes it to the stored `dsl_json` document.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitStrategyVersionArgs {
    /// The parent version this submission iterates on. Exactly one of
    /// `parent_version_id` / `strategy_name` must be given.
    #[serde(default)]
    parent_version_id: Option<String>,
    /// The name of a NEW strategy this submission roots. Exactly one of
    /// `parent_version_id` / `strategy_name` must be given.
    #[serde(default)]
    strategy_name: Option<String>,
    /// The DSL document as a JSON object.
    dsl: serde_json::Map<String, serde_json::Value>,
    /// The agent's stated hypothesis for this version (1–2000 chars).
    hypothesis: String,
}

/// `run_backtest` args (r2.s1.w3). `from`/`to` are RFC 3339 UTC timestamps —
/// both or neither.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunBacktestArgs {
    /// The strategy-version id to run.
    version_id: String,
    /// Inclusive window start, RFC 3339 (e.g. `2025-03-01T00:00:00Z`). Must be
    /// paired with `to`.
    #[serde(default)]
    from: Option<String>,
    /// Exclusive window end, RFC 3339. Must be paired with `from`.
    #[serde(default)]
    to: Option<String>,
}

/// `run_walk_forward` args (r2.s3.w5). `from`/`to` are RFC 3339 UTC timestamps —
/// **each independent**, unlike `run_backtest`'s both-or-neither window: an
/// omitted `from` defaults to the first fully-warm bar, an omitted `to` to the
/// snapshot's last candle's `close_time`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunWalkForwardArgs {
    /// The strategy-version id to walk forward.
    version_id: String,
    /// The counted span's inclusive start, RFC 3339; omitted defaults to the
    /// first fully-warm bar.
    #[serde(default)]
    from: Option<String>,
    /// The counted span's exclusive end, RFC 3339; omitted defaults to the
    /// snapshot's last candle's close.
    #[serde(default)]
    to: Option<String>,
    /// The fold count — `2..=12`; omitted defaults to 6. Wider than the legal
    /// `u8` on purpose: an out-of-range value must reach the domain's
    /// `KOutOfRange` refusal (a `field_error` naming `k`), not die in decoding.
    ///
    /// `i32`, matching the Tauri transport: the two surfaces carry one wire type
    /// for `k`, and it is the widest signed one specta exports (the `BigInt`-style
    /// integers are refused, so `i64` would break the bindings export rather than
    /// this decode).
    #[serde(default)]
    k: Option<i32>,
}

/// `get_walk_forward_run` args (r2.s3.w5).
#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetWalkForwardRunArgs {
    /// The walk-forward run id.
    walk_forward_run_id: String,
}

/// One tool error in the `{"field", "message"}` shape.
fn field_error(field: &str, message: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(json!({ "field": field, "message": message.to_string() }))
}

/// One tool error carrying only `message` (store/repo failures — there is no
/// offending agent-supplied field to name).
fn tool_error(message: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(json!({ "message": message.to_string() }))
}

/// The list-tool result seam (#183): the MCP spec types `structuredContent`
/// as a JSON object — Claude Code's tools/call validator refuses a bare
/// array — so the entries go under a named key. `CallToolResult::structured`
/// renders the same object into the content text block, keeping the two
/// consistent.
///
/// A serialization failure refuses with `{"message"}` — it never degrades
/// into a successful `{key: []}`: an empty list is a real answer, not a
/// fallback (the same refuse-don't-return-empty rule [`resolve_version`]
/// carries for unknown identifiers). The envelope is built by insertion, not
/// `json!` — the macro would re-serialize the `Value` just produced.
fn structured_list(key: &'static str, entries: impl serde::Serialize) -> CallToolResult {
    let entries = match serde_json::to_value(entries) {
        Ok(entries) => entries,
        Err(e) => return tool_error(e),
    };
    let mut envelope = serde_json::Map::new();
    envelope.insert(key.to_owned(), entries);
    CallToolResult::structured(serde_json::Value::Object(envelope))
}

/// Parse a timeframe token the way the wire names it (`15m`/`4h`), accepting
/// the CLI spellings (`M15`/`H4`) as aliases.
fn parse_timeframe(raw: &str) -> Result<Timeframe, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "15m" | "m15" => Ok(Timeframe::M15),
        "4h" | "h4" => Ok(Timeframe::H4),
        other => Err(format!("unknown timeframe {other:?} (expected 15m or 4h)")),
    }
}

/// Parse + path-check an optional `data_version` argument.
fn parse_data_version(raw: Option<String>) -> Result<Option<DataVersion>, String> {
    raw.map(|tag| DataVersion::parse(tag).map_err(|e| format!("invalid data_version: {e}")))
        .transpose()
}

/// The `run_id` boundary check for export tools (r2.s1 F4).
/// [`BacktestRunId::new`] is the unchecked constructor for adapter-minted ids,
/// and a wire-supplied id is joined verbatim into the export path by
/// `Exports::next_path` — so the single-portable-path-component rule
/// [`DataVersion::parse`] applies to snapshot tags applies here too: empty,
/// `.`, `..`, a `/` or `\\` separator, or a NUL byte is refused before any
/// file is written.
fn parse_run_id(raw: String) -> Result<BacktestRunId, String> {
    let unsafe_reason = if raw.is_empty() {
        Some("it is empty")
    } else if raw == "." || raw == ".." {
        Some("it is a relative path component")
    } else if raw.contains('/') || raw.contains('\\') {
        Some("it contains a path separator")
    } else if raw.contains('\0') {
        Some("it contains a NUL byte")
    } else {
        None
    };
    match unsafe_reason {
        None => Ok(BacktestRunId::new(raw)),
        Some(reason) => Err(format!(
            "invalid run_id {raw:?}: {reason}; a run id is joined verbatim \
             into the export path and must be a single portable path component"
        )),
    }
}

/// The shared resolve-and-refuse seam for wire-supplied `version_id` args
/// (G5/T19): every tool that takes a strategy-version identifier loads it
/// here, so an unknown id refuses with `{"field": "version_id"}` rather than
/// degenerating into a successful empty result. `submit_strategy_version`'s
/// `parent_version_id` and `run_backtest`'s `version_id` resolve inside their
/// application use cases (`resolve_target`, `resolve_default_request`), which
/// refuse through the same field-error shape — routing them through this
/// helper too would only double the read.
async fn resolve_version(
    strategies: &SqliteStrategyRepo<SystemClock>,
    raw: String,
) -> Result<StrategyVersion, CallToolResult> {
    match strategies.get_version(&VersionId::new(raw)).await {
        Ok(Some(version)) => Ok(version),
        Ok(None) => Err(field_error("version_id", "no such strategy version")),
        Err(e) => Err(tool_error(e)),
    }
}

/// The `run_id` half of the seam: [`parse_run_id`]'s single-path-component
/// rule (F4 — the id is joined verbatim into the export path) AND existence —
/// `get_trades`/`list_runs_for_version` return empty collections for unknown
/// ids, so without this check a typo reads as a genuine empty result.
async fn resolve_run(
    runs: &SqliteBacktestRunRepo<SystemClock>,
    raw: String,
) -> Result<PersistedRun, CallToolResult> {
    let run_id = match parse_run_id(raw) {
        Ok(id) => id,
        Err(e) => return Err(field_error("run_id", e)),
    };
    match runs.get_run(&run_id).await {
        Ok(Some(run)) => Ok(run),
        Ok(None) => Err(field_error("run_id", "no such backtest run")),
        Err(e) => Err(tool_error(e)),
    }
}

/// Parse one RFC 3339 window bound to epoch millis.
fn parse_rfc3339_ms(raw: &str) -> Result<i64, String> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.timestamp_millis())
        .map_err(|e| format!("invalid RFC 3339 timestamp {raw:?}: {e}"))
}

/// The `run_backtest` `from`/`to` pair: both or neither, `from < to`, into the
/// domain [`CandleWindow`]. Every refusal attaches to `window` (the pair is
/// one argument semantically) except the unparseable bound, which names itself.
fn parse_window(
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Option<CandleWindow>, CallToolResult> {
    match (from, to) {
        (None, None) => Ok(None),
        (Some(_), None) | (None, Some(_)) => Err(field_error(
            "window",
            "from and to must both be given, or neither — a window needs two bounds",
        )),
        (Some(from), Some(to)) => {
            let from_ms = parse_rfc3339_ms(from).map_err(|e| field_error("from", e))?;
            let to_ms = parse_rfc3339_ms(to).map_err(|e| field_error("to", e))?;
            let window = CandleWindow::new(from_ms, to_ms)
                .map_err(|e| field_error("window", format!("from must be before to: {e}")))?;
            Ok(Some(window))
        }
    }
}

/// A [`ValidationCode`] as the wire's `snake_case` string, derived from `Debug`
/// so a future `#[non_exhaustive]` variant still renders its own name.
fn validation_code_snake(code: ValidationCode) -> String {
    let pascal = format!("{code:?}");
    let mut out = String::with_capacity(pascal.len() + 4);
    for (i, c) in pascal.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Map a [`SubmitError`] onto the tool-error shapes: field errors keep their
/// path, a DSL validation failure expands to `errors[]` carrying EVERY
/// `FieldError`, and storage failures carry `message` only.
fn submit_error_result(err: SubmitError) -> CallToolResult {
    match err {
        SubmitError::Field { path, message } => field_error(&path, message),
        SubmitError::Load { path, message } | SubmitError::Compile { path, message } => {
            field_error(path, message)
        }
        SubmitError::Validation(errors) => {
            let details: Vec<serde_json::Value> = errors
                .errors()
                .iter()
                .map(|e| {
                    json!({
                        "field": e.path,
                        "code": validation_code_snake(e.code),
                        "message": e.message,
                    })
                })
                .collect();
            CallToolResult::structured_error(json!({
                "field": "dsl",
                "message": format!("{} validation error(s)", details.len()),
                "errors": details,
            }))
        }
        SubmitError::Data(e) => tool_error(e),
    }
}

/// Map a [`BacktestAppError`] onto the tool-error shapes for `run_backtest`:
/// an empty window and a missing version name their argument, a stored-DSL
/// failure names `dsl`, and everything else carries `message` only.
fn backtest_error_result(err: &BacktestAppError) -> CallToolResult {
    match err {
        BacktestAppError::WindowEmpty { .. } => field_error("window", err),
        BacktestAppError::VersionNotFound(_) => field_error("version_id", err),
        BacktestAppError::DslInvalid(_) | BacktestAppError::CompileFailed(_) => {
            field_error("dsl", err)
        }
        // r2.s2.w2 + round-1 fix F1: the strategy needs an HTF series the
        // request lacked, or the `inputs.htf` selection is not strictly higher
        // than the primary timeframe — the error's `field` already carries
        // `"inputs.htf"`, so surface it verbatim.
        BacktestAppError::HtfRequired { field } | BacktestAppError::HtfNotHigher { field, .. } => {
            field_error(field, err)
        }
        // r2.s2 round-2 fix G1 + round-5: a different-pair or stale
        // (too-short) HTF series is refused by the engine (the request itself
        // carries only one pair, so no app-layer variant exists) — surface
        // both on the same `inputs.htf` field.
        BacktestAppError::Engine(
            BacktestError::HtfPairMismatch { .. } | BacktestError::HtfCoverageShort { .. },
        ) => field_error("inputs.htf", err),
        _ => tool_error(err),
    }
}

/// Parse ONE independent RFC 3339 bound — `run_walk_forward`'s `from`/`to`,
/// each optional on its own (unlike `run_backtest`'s both-or-neither pair,
/// which [`parse_window`] owns).
fn parse_bound(field: &'static str, raw: Option<&str>) -> Result<Option<i64>, CallToolResult> {
    raw.map(|s| parse_rfc3339_ms(s).map_err(|e| field_error(field, e)))
        .transpose()
}

/// Map a [`WalkForwardAppError`] onto the tool-error shapes for the two
/// walk-forward tools: the shared-path failures delegate to
/// [`backtest_error_result`] verbatim (a fold run IS an ordinary run, so its
/// failure vocabulary is the same), the request refusals name their field, and
/// everything else — including the saved-but-unreadable pair, whose prose
/// already names the persisted run id — carries `message` only.
fn walk_forward_error_result(err: &WalkForwardAppError) -> CallToolResult {
    match err {
        WalkForwardAppError::Shared(e) => backtest_error_result(e),
        // The domain taxonomy has exactly one variant today —
        // `WalkForwardError::KOutOfRange` — so `k` is the field.
        WalkForwardAppError::Domain(_) => field_error("k", err),
        // `field` is `"from"`, and the refusal also names the earliest allowed
        // bound as RFC 3339 — a wire timestamp, not the raw ms the Display
        // carries.
        WalkForwardAppError::FromBeforeWarm {
            field,
            earliest_allowed_ms,
            ..
        } => field_error(
            field,
            format!(
                "{err} — earliest allowed: {}",
                rfc3339_secs(*earliest_allowed_ms)
            ),
        ),
        // The mirror of `FromBeforeWarm`: an explicit `to` past the snapshot
        // names its field and renders the latest allowed bound as RFC 3339.
        WalkForwardAppError::ToPastSnapshot {
            field,
            latest_allowed_ms,
            ..
        } => field_error(
            field,
            format!(
                "{err} — latest allowed: {}",
                rfc3339_secs(*latest_allowed_ms)
            ),
        ),
        WalkForwardAppError::InvalidRange { field, .. } => field_error(field, err),
        // The empty fold's window `to` is the bound that starves it (ruling
        // (d) on `src/application/walk_forward.rs`'s field-less variant).
        WalkForwardAppError::FoldEmpty { .. } => field_error("to", err),
        // `NeverWarm` names no argument — the strategy warms nowhere on this
        // snapshot — and `Persist`/`SavedButReadBack*`/`Internal` already say
        // what they are in prose.
        _ => tool_error(err),
    }
}

#[tool_router(vis = "pub(crate)")]
impl PulseMcp {
    /// List strategies with their version trees (parent-first order).
    #[tool(
        description = "List strategies with their version trees in parent-first order. The result is an object carrying the tree under `strategies`. Pass include_archived to include archived strategies."
    )]
    async fn list_strategies(
        &self,
        Parameters(args): Parameters<ListStrategiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let repo = SqliteStrategyRepo::new(self.state.db.pool().clone());
        let strategies = match repo.list_strategies(args.include_archived).await {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(e)),
        };
        let mut entries = Vec::with_capacity(strategies.len());
        for strategy in &strategies {
            let versions = match repo.version_tree(&strategy.id).await {
                Ok(v) => v.iter().map(version_entry).collect(),
                Err(e) => return Ok(tool_error(e)),
            };
            entries.push(strategy_entry(strategy, versions));
        }
        Ok(structured_list("strategies", entries))
    }

    /// Fetch one immutable strategy version by id.
    #[tool(
        description = "Fetch one strategy version by id: the migrated DSL document, the verbatim original, its hash and provenance."
    )]
    async fn get_version(
        &self,
        Parameters(args): Parameters<GetVersionArgs>,
    ) -> Result<CallToolResult, McpError> {
        let repo = SqliteStrategyRepo::new(self.state.db.pool().clone());
        let version = match resolve_version(&repo, args.version_id).await {
            Ok(version) => version,
            Err(result) => return Ok(result),
        };
        Ok(CallToolResult::structured(
            serde_json::to_value(version_detail(&version)).unwrap_or_else(|_| json!({})),
        ))
    }

    /// List the runs recorded against a strategy version.
    ///
    /// NOTE: `inputs` provenance lives only on the full run row, so each
    /// summary row is hydrated through `get_run` — the accepted N+1 (see
    /// report §10; a dedicated repository method is a future seam, not this
    /// work item).
    #[tool(
        description = "List the backtest runs recorded against a strategy version: headline stats plus persisted input provenance. The result is an object carrying the rows under `runs`."
    )]
    async fn list_runs(
        &self,
        Parameters(args): Parameters<ListRunsArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Resolve first: `list_runs_for_version` returns `[]` for an unknown
        // version, which a typo would silently read as "no runs yet" (G5).
        let strategies = SqliteStrategyRepo::new(self.state.db.pool().clone());
        let version = match resolve_version(&strategies, args.version_id).await {
            Ok(version) => version,
            Err(result) => return Ok(result),
        };
        let repo = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let summaries = match repo.list_runs_for_version(&version.id).await {
            Ok(s) => s,
            Err(e) => return Ok(tool_error(e)),
        };
        let mut entries = Vec::with_capacity(summaries.len());
        for summary in &summaries {
            match repo.get_run(&summary.id).await {
                Ok(Some(run)) => entries.push(run_list_entry(&run)),
                Ok(None) => {
                    return Ok(tool_error(format!(
                        "run {} listed but not readable (store corruption)",
                        summary.id.as_str()
                    )));
                }
                Err(e) => return Ok(tool_error(e)),
            }
        }
        Ok(structured_list("runs", entries))
    }

    /// Fetch one persisted run: summary, regime breakdown, skipped entries,
    /// MFE/MAE aggregates, inputs and integrity fields. No trades inline.
    #[tool(
        description = "Fetch one backtest run: summary stats, regime breakdown, skipped entries, MFE/MAE aggregates, persisted inputs and integrity fields."
    )]
    async fn get_run(
        &self,
        Parameters(args): Parameters<GetRunArgs>,
    ) -> Result<CallToolResult, McpError> {
        let repo = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let run = match resolve_run(&repo, args.run_id).await {
            Ok(run) => run,
            Err(result) => return Ok(result),
        };
        let trades = match repo.get_trades(&run.id).await {
            Ok(t) => t,
            Err(e) => return Ok(tool_error(e)),
        };
        let aggregates = MfeMaeAggregates::from_trades(&trades);
        Ok(CallToolResult::structured(
            serde_json::to_value(run_detail(&run, &aggregates)).unwrap_or_else(|_| json!({})),
        ))
    }

    /// Export a run's full trade log as CSV under the exports dir.
    #[tool(
        description = "Export a backtest run's full trade log as CSV under the server exports dir. Returns the absolute path, row count and column names."
    )]
    async fn export_trades(
        &self,
        Parameters(args): Parameters<ExportTradesArgs>,
    ) -> Result<CallToolResult, McpError> {
        // One seam for the whole input: the id must be a safe single path
        // component AND name a real run — `get_trades` returns an empty vec
        // for an unknown id, which would otherwise write a header-only export
        // indistinguishable from a genuine zero-trade run.
        let repo = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let run = match resolve_run(&repo, args.run_id).await {
            Ok(run) => run,
            Err(result) => return Ok(result),
        };
        let trades = match repo.get_trades(&run.id).await {
            Ok(t) => t,
            Err(e) => return Ok(tool_error(e)),
        };
        let csv = export::trades_csv(&trades);
        match self
            .state
            .exports
            .write_csv("export_trades", run.id.as_str(), &csv)
        {
            Ok(path) => Ok(CallToolResult::structured(json!({
                "path": path,
                "rows": trades.len(),
                "columns": [
                    "direction", "qty", "entry_price", "exit_price",
                    "entry_signal_time", "entry_fill_time",
                    "exit_signal_time", "exit_fill_time",
                    "fills", "fees_total", "funding_total", "slippage_total",
                    "realized_pnl", "realized_r", "mfe_r", "mae_r",
                    "exit_reason", "source", "regime", "stop_price",
                ],
            }))),
            Err(e) => Ok(tool_error(e)),
        }
    }

    /// Export a candle snapshot as CSV rows, or a byte copy of the Parquet
    /// file. `data_version` omitted selects `HEAD`.
    #[tool(
        description = "Export a candle snapshot under the server exports dir: CSV rows, or a byte copy of the Parquet file when format=parquet. data_version omitted selects HEAD."
    )]
    async fn export_candles(
        &self,
        Parameters(args): Parameters<ExportCandlesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pair = match Pair::parse(args.pair.clone()) {
            Ok(p) => p,
            Err(e) => return Ok(field_error("pair", e)),
        };
        let tf = match parse_timeframe(&args.timeframe) {
            Ok(tf) => tf,
            Err(e) => return Ok(field_error("timeframe", e)),
        };
        let version = match parse_data_version(args.data_version) {
            Ok(v) => v,
            Err(e) => return Ok(field_error("data_version", e)),
        };
        let store = self.state.candles.clone();
        let loaded = match tokio::task::spawn_blocking(move || match &version {
            Some(v) => store.load_version(&pair, tf, v),
            None => store.load_head(&pair, tf)?.ok_or_else(|| {
                DataError::Io(format!(
                    "no HEAD snapshot for {pair} {}",
                    tf.binance_interval()
                ))
            }),
        })
        .await
        {
            Ok(Ok(stored)) => stored,
            Ok(Err(e)) => return Ok(tool_error(e)),
            Err(e) => return Ok(tool_error(format!("export_candles worker failed: {e}"))),
        };
        let series = loaded.series;
        let data_version = series.version.as_str().to_owned();
        let subject = format!("{}-{}", series.pair, tf.binance_interval());
        match args.format {
            CandleExportFormat::Csv => {
                let rows = series.candles.len();
                let csv = export::candles_csv(&series.candles);
                match self
                    .state
                    .exports
                    .write_csv("export_candles", &subject, &csv)
                {
                    Ok(path) => Ok(CallToolResult::structured(json!({
                        "path": path,
                        "rows": rows,
                        "data_version": data_version,
                        "timeframe": tf.binance_interval(),
                        "pair": series.pair.to_string(),
                    }))),
                    Err(e) => Ok(tool_error(e)),
                }
            }
            CandleExportFormat::Parquet => {
                // Byte copy of the immutable snapshot file — no re-encode.
                let snapshot_path =
                    self.state
                        .candles
                        .snapshot_path(&series.pair, tf, &series.version);
                let bytes = match std::fs::read(&snapshot_path) {
                    Ok(b) => b,
                    Err(e) => {
                        return Ok(tool_error(format!(
                            "could not read snapshot {}: {e}",
                            snapshot_path.display()
                        )));
                    }
                };
                match self
                    .state
                    .exports
                    .write_parquet_copy("export_candles", &subject, &bytes)
                {
                    Ok(path) => Ok(CallToolResult::structured(json!({
                        "path": path,
                        "rows": series.candles.len(),
                        "data_version": data_version,
                        "timeframe": tf.binance_interval(),
                        "pair": series.pair.to_string(),
                    }))),
                    Err(e) => Ok(tool_error(e)),
                }
            }
        }
    }

    /// Export one row per candle of the requested indicator specs.
    #[tool(
        description = "Export per-candle indicator values (e.g. rsi:14, ema:50, atr:14) as CSV under the server exports dir: open_time plus one column per spec, blank while the engine warms."
    )]
    async fn export_indicators(
        &self,
        Parameters(args): Parameters<ExportIndicatorsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pair = match Pair::parse(args.pair.clone()) {
            Ok(p) => p,
            Err(e) => return Ok(field_error("pair", e)),
        };
        let tf = match parse_timeframe(&args.timeframe) {
            Ok(tf) => tf,
            Err(e) => return Ok(field_error("timeframe", e)),
        };
        let version = match parse_data_version(args.data_version) {
            Ok(v) => v,
            Err(e) => return Ok(field_error("data_version", e)),
        };
        let columns = match parse_indicator_specs(&args.indicators) {
            Ok(c) => c,
            Err(e) => return Ok(field_error("indicators", e)),
        };
        let store = self.state.candles.clone();
        let specs: Vec<_> = columns.iter().map(|c| c.spec.clone()).collect();
        let (series, rows) = match tokio::task::spawn_blocking(move || {
            let stored = match &version {
                Some(v) => store.load_version(&pair, tf, v),
                None => store.load_head(&pair, tf)?.ok_or_else(|| {
                    DataError::Io(format!(
                        "no HEAD snapshot for {pair} {}",
                        tf.binance_interval()
                    ))
                }),
            }?;
            let series = stored.series;
            let mut engine =
                IndicatorEngine::from_specs(&specs).map_err(|e| DataError::Io(e.to_string()))?;
            let mut rows = Vec::with_capacity(series.candles.len());
            for candle in &series.candles {
                engine.step(candle);
                rows.push(
                    specs
                        .iter()
                        .map(|spec| {
                            engine.current(&CompiledValue::Indicator {
                                series: Series::Primary,
                                spec: spec.clone(),
                            })
                        })
                        .collect::<Vec<_>>(),
                );
            }
            Ok::<_, DataError>((series, rows))
        })
        .await
        {
            Ok(Ok(pair_result)) => pair_result,
            Ok(Err(e)) => return Ok(tool_error(e)),
            Err(e) => return Ok(tool_error(format!("export_indicators worker failed: {e}"))),
        };
        let csv = export::indicators_csv(&series.candles, &columns, &rows);
        let subject = format!("{}-{}", series.pair, tf.binance_interval());
        let labels: Vec<&str> = columns.iter().map(|c| c.label.as_str()).collect();
        match self
            .state
            .exports
            .write_csv("export_indicators", &subject, &csv)
        {
            Ok(path) => Ok(CallToolResult::structured(json!({
                "path": path,
                "rows": series.candles.len(),
                "data_version": series.version.as_str(),
                "timeframe": tf.binance_interval(),
                "pair": series.pair.to_string(),
                "columns": labels,
            }))),
            Err(e) => Ok(tool_error(e)),
        }
    }

    /// Persist an agent-authored DSL variant (r2.s1.w3).
    ///
    /// Exactly one target: `parent_version_id` clones under an existing
    /// version; `strategy_name` roots a new strategy (duplicate names are
    /// refused). The version writes as `created_by: external_agent` with an
    /// `agent_submission` row carrying the resolved identity name and the
    /// hypothesis. The identity is read once and the mutex guard dropped
    /// before any `.await`.
    #[tool(
        description = "Submit a strategy DSL variant with a hypothesis: parent_version_id clones under an existing version, strategy_name roots a new strategy (exactly one). Persists the version as external_agent plus its agent_submission row."
    )]
    async fn submit_strategy_version(
        &self,
        Parameters(args): Parameters<SubmitStrategyVersionArgs>,
    ) -> Result<CallToolResult, McpError> {
        // The identity is a lock-read, never an await-held guard.
        let agent_name = self.identity_lock().name.clone();

        let target = match (args.parent_version_id, args.strategy_name) {
            (Some(parent_id), None) => SubmitTarget::Parent(VersionId::new(parent_id)),
            (None, Some(strategy_name)) => SubmitTarget::Root { strategy_name },
            (Some(_), Some(_)) => {
                return Ok(field_error(
                    "parent_version_id",
                    "pass exactly one of parent_version_id or strategy_name, not both",
                ));
            }
            (None, None) => {
                return Ok(field_error(
                    "parent_version_id",
                    "exactly one of parent_version_id or strategy_name is required",
                ));
            }
        };

        let repo = SqliteStrategyRepo::new(self.state.db.pool().clone());
        let outcome = match submit_agent_version(
            &repo,
            SubmitRequest {
                target,
                dsl: serde_json::Value::Object(args.dsl),
                hypothesis: args.hypothesis,
                agent_name,
            },
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => return Ok(submit_error_result(e)),
        };

        let mut payload = json!({
            "version_id": outcome.version.id.as_str(),
            "strategy_id": outcome.strategy_id.as_str(),
            "version_hash": outcome.version.version_hash,
            "created_by": "external_agent",
            "agent_name": outcome.submission.agent_name.as_str(),
            "submission_id": outcome.submission.id.as_str(),
            "created_at": outcome.submission.created_at.to_rfc3339(),
        });
        if let Some(parent) = outcome.version.parent_version_id.as_ref() {
            payload["parent_version_id"] = json!(parent.as_str());
        }
        Ok(CallToolResult::structured(payload))
    }

    /// Run a version through the shared backtest flow (r2.s1.w3).
    ///
    /// The request resolves through [`resolve_default_request`]: the version's
    /// parent's latest run (then its own, then the app defaults) supplies the
    /// pair, timeframes, cost model and exact snapshot pins. `from`/`to` are an
    /// optional RFC 3339 window — both or neither — the counted half-open
    /// slice `[from, to)`: both series still load from the snapshot's start so
    /// the engines step every bar before `from` and arrive warm (r2.s3.w2);
    /// the window and the lead-in start record on the run's `inputs.window`
    /// and `inputs.lead_in_from`.
    #[tool(
        description = "Run a backtest of one strategy version. Optional from/to (RFC 3339, both or neither) count only the candles in [from, to) while indicators warm on the full history before `from`. Defaults resolve from the version's parent run, then its own latest run, then app defaults."
    )]
    async fn run_backtest(
        &self,
        Parameters(args): Parameters<RunBacktestArgs>,
    ) -> Result<CallToolResult, McpError> {
        let window = match parse_window(args.from.as_deref(), args.to.as_deref()) {
            Ok(window) => window,
            Err(result) => return Ok(result),
        };

        let strategies = SqliteStrategyRepo::new(self.state.db.pool().clone());
        let runs = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let version_id = VersionId::new(args.version_id);
        let request = match resolve_default_request(&strategies, &runs, &version_id, window).await {
            Ok(request) => request,
            Err(e @ BacktestAppError::VersionNotFound(_)) => {
                return Ok(field_error("version_id", e));
            }
            Err(e) => return Ok(backtest_error_result(&e)),
        };
        let outcome = match run_version_backtest(
            &strategies,
            &self.state.candles,
            &self.state.exchange,
            &runs,
            &request,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => return Ok(backtest_error_result(&e)),
        };

        let mut payload = json!({
            "run_id": outcome.run.id.as_str(),
            "version_id": outcome.run.strategy_version_id.as_str(),
            "run": serde_json::to_value(run_detail(
                &outcome.run,
                &MfeMaeAggregates::from_trades(&outcome.trades),
            ))
            .unwrap_or_else(|_| json!({})),
        });
        // Only when Some — the wire never carries a null warning slot.
        if let Some(warning) = outcome.fingerprint_warning.as_ref() {
            payload["fingerprint_warning"] = json!(warning);
        }
        Ok(CallToolResult::structured(payload))
    }

    /// Walk a persisted strategy version over `rolling-oos/v1` folds (r2.s3.w5).
    ///
    /// `from`/`to` are INDEPENDENT RFC 3339 bounds — either may be given alone:
    /// `from` defaults to the first fully-warm bar, `to` to the snapshot's last
    /// candle's `close_time`. `k` defaults to `K_DEFAULT` (6). Pair, timeframes,
    /// cost model and the exact snapshot pins resolve through the same
    /// [`resolve_default_request`] seam `run_backtest` uses — the surfaces run a
    /// version identically — and the answer is the one `WalkForwardRunDetail`
    /// shape, built from the saved rows.
    #[tool(
        description = "Walk one strategy version forward: rolling-oos/v1 cuts the counted span into K contiguous out-of-sample folds (k in 2..=12, default 6) and judges the run under wf-v1. Each fold is an ordinary persisted windowed backtest run with full-history lead-in — visible through list_runs and get_run with its walk_forward membership. Optional from/to are RFC 3339 bounds given independently: `from` defaults to the first fully-warm bar, `to` to the snapshot's last close."
    )]
    async fn run_walk_forward(
        &self,
        Parameters(args): Parameters<RunWalkForwardArgs>,
    ) -> Result<CallToolResult, McpError> {
        let from_ms = match parse_bound("from", args.from.as_deref()) {
            Ok(bound) => bound,
            Err(result) => return Ok(result),
        };
        let to_ms = match parse_bound("to", args.to.as_deref()) {
            Ok(bound) => bound,
            Err(result) => return Ok(result),
        };

        let strategies = SqliteStrategyRepo::new(self.state.db.pool().clone());
        let runs = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let version_id = VersionId::new(args.version_id);
        // The shared resolver supplies pair, timeframes, costs and pins; the
        // walk-forward's own `from`/`to` are NOT its `window` (the backtest
        // window is a both-or-neither pair — these bounds are independent), so
        // `window` stays `None` here and the counted span resolves inside.
        let resolved = match resolve_default_request(&strategies, &runs, &version_id, None).await {
            Ok(request) => request,
            Err(e @ BacktestAppError::VersionNotFound(_)) => {
                return Ok(field_error("version_id", e));
            }
            Err(e) => return Ok(backtest_error_result(&e)),
        };
        let request = WalkForwardRequest {
            version_id,
            pair: resolved.pair,
            primary_timeframe: resolved.primary_timeframe,
            htf_timeframe: resolved.htf_timeframe,
            config: resolved.config,
            snapshots: resolved.snapshots,
            from_ms,
            to_ms,
            k: args.k,
        };
        let outcome = match run_walk_forward(
            &strategies,
            &self.state.candles,
            &self.state.exchange,
            &runs,
            &request,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => return Ok(walk_forward_error_result(&e)),
        };
        let detail = match walk_forward_run_detail(&outcome.run, &outcome.fold_summaries) {
            Ok(detail) => detail,
            Err(e) => return Ok(walk_forward_error_result(&e)),
        };
        Ok(CallToolResult::structured(
            serde_json::to_value(detail).unwrap_or_else(|_| json!({})),
        ))
    }

    /// Fetch one persisted walk-forward run — the SAME `WalkForwardRunDetail`
    /// `run_walk_forward` answers with, assembled from the stored rows: the
    /// parent's provenance and verdict, plus each fold's run re-read through
    /// the ordinary run log (`get_run` per `backtest_run_id`, fail closed).
    #[tool(
        description = "Fetch one walk-forward run by id: the same WalkForwardRunDetail shape run_walk_forward returns — scheme, rule, counted span, the wf-v1 verdict and one row per fold (window, fold verdict, and the fold's ordinary run summary)."
    )]
    async fn get_walk_forward_run(
        &self,
        Parameters(args): Parameters<GetWalkForwardRunArgs>,
    ) -> Result<CallToolResult, McpError> {
        let runs = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let run = match runs
            .get_walk_forward_run(&WalkForwardRunId::new(args.walk_forward_run_id))
            .await
        {
            Ok(Some(run)) => run,
            Ok(None) => {
                return Ok(field_error(
                    "walk_forward_run_id",
                    "no such walk-forward run",
                ));
            }
            Err(e) => return Ok(tool_error(e)),
        };
        let fold_runs = match load_fold_runs(&runs, &run).await {
            Ok(fold_runs) => fold_runs,
            Err(e) => return Ok(walk_forward_error_result(&e)),
        };
        let summaries: Vec<_> = fold_runs.iter().map(run_summary_of).collect();
        let detail = match walk_forward_run_detail(&run, &summaries) {
            Ok(detail) => detail,
            Err(e) => return Ok(walk_forward_error_result(&e)),
        };
        Ok(CallToolResult::structured(
            serde_json::to_value(detail).unwrap_or_else(|_| json!({})),
        ))
    }
}
