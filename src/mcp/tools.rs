//! The seven read tools of `pulse mcp` (r2.s1.w2).
//!
//! Every tool is read-only: five query the strategy/run repositories and two
//! read the candle store, with exports landing under the per-process exports
//! dir. `w3` appends the two write tools to this router.
//!
//! Argument validation failures come back as tool errors in the
//! `{"field", "message"}` shape (the spec's `FieldError` contract); store/repo
//! failures carry just `message`.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use crate::adapters::db::{SqliteBacktestRunRepo, SqliteStrategyRepo};
use crate::adapters::indicators::engine::IndicatorEngine;
use crate::application::mcp_read::{
    parse_indicator_specs, run_detail, run_list_entry, strategy_entry, version_detail,
    version_entry,
};
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestRunId, BacktestRunRepository, CandleSeriesRepository, CompiledValue, DataError,
    DataVersion, EvalContext, MfeMaeAggregates, Pair, StrategyRepository, Timeframe,
};

use super::PulseMcp;
use super::export;

/// `list_strategies` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ListStrategiesArgs {
    /// Include archived strategies (default false).
    #[serde(default)]
    include_archived: bool,
}

/// `get_version` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetVersionArgs {
    /// The strategy-version id.
    version_id: String,
}

/// `list_runs` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ListRunsArgs {
    /// The strategy-version id whose runs to list.
    version_id: String,
}

/// `get_run` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct GetRunArgs {
    /// The backtest run id.
    run_id: String,
}

/// `export_trades` args.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
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

/// One tool error in the `{"field", "message"}` shape.
fn field_error(field: &str, message: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(json!({ "field": field, "message": message.to_string() }))
}

/// One tool error carrying only `message` (store/repo failures — there is no
/// offending agent-supplied field to name).
fn tool_error(message: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(json!({ "message": message.to_string() }))
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

#[tool_router(vis = "pub(crate)")]
impl PulseMcp {
    /// List strategies with their version trees (parent-first order).
    #[tool(
        description = "List strategies with their version trees in parent-first order. Pass include_archived to include archived strategies."
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
        Ok(CallToolResult::structured(
            serde_json::to_value(entries).unwrap_or_else(|_| json!([])),
        ))
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
        match repo.get_version(&VersionId::new(args.version_id)).await {
            Ok(Some(version)) => Ok(CallToolResult::structured(
                serde_json::to_value(version_detail(&version)).unwrap_or_else(|_| json!({})),
            )),
            Ok(None) => Ok(field_error("version_id", "no such strategy version")),
            Err(e) => Ok(tool_error(e)),
        }
    }

    /// List the runs recorded against a strategy version.
    ///
    /// NOTE: `inputs` provenance lives only on the full run row, so each
    /// summary row is hydrated through `get_run` — the accepted N+1 (see
    /// report §10; a dedicated repository method is a future seam, not this
    /// work item).
    #[tool(
        description = "List the backtest runs recorded against a strategy version: headline stats plus persisted input provenance."
    )]
    async fn list_runs(
        &self,
        Parameters(args): Parameters<ListRunsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let version_id = VersionId::new(args.version_id);
        let repo = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let summaries = match repo.list_runs_for_version(&version_id).await {
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
        Ok(CallToolResult::structured(
            serde_json::to_value(entries).unwrap_or_else(|_| json!([])),
        ))
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
        let run_id = BacktestRunId::new(args.run_id);
        let repo = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let run = match repo.get_run(&run_id).await {
            Ok(Some(run)) => run,
            Ok(None) => return Ok(field_error("run_id", "no such backtest run")),
            Err(e) => return Ok(tool_error(e)),
        };
        let trades = match repo.get_trades(&run_id).await {
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
        let run_id = BacktestRunId::new(args.run_id);
        let repo = SqliteBacktestRunRepo::new(self.state.db.pool().clone());
        let trades = match repo.get_trades(&run_id).await {
            Ok(t) => t,
            Err(e) => return Ok(tool_error(e)),
        };
        let csv = export::trades_csv(&trades);
        match self
            .state
            .exports
            .write_csv("export_trades", run_id.as_str(), &csv)
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
                    "exit_reason", "source", "regime",
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
        description = "Export per-candle indicator values (e.g. rsi:14, ema:50) as CSV under the server exports dir: open_time plus one column per spec, blank while the engine warms."
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
                        .map(|spec| engine.current(&CompiledValue::Indicator(spec.clone())))
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
                "columns": labels,
            }))),
            Err(e) => Ok(tool_error(e)),
        }
    }
}
