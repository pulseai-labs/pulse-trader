//! `pulse backtest` — the v1 CLI proof-of-concept demo surface (VS-1.2.1
//! work-1.04, FR-5/FR-6).
//!
//! Loads a compiled strategy from a **DSL JSON file** (G7), loads candles offline
//! from a [`CandleStore`], runs the deterministic backtest engine (1.03), and
//! renders a human-readable **trade log** (one row per trade: entry/exit times,
//! direction, prices, qty, fees, funding, slippage, P&L, realized R) plus a
//! cost-breakdown footer (trade count, gross/net P&L, total fees/funding/
//! slippage). This realizes the **user demo criterion**: "read the resulting
//! trade log and confirm fees/funding/slippage are deducted."
//!
//! **Load path (G7):** `--dsl <path>` → read file → version-safe [`Migrator`]
//! load → [`validate`] → [`compile`] → [`run_backtest`]. This `--dsl` path stays
//! **persistence-free + comparison-free + silent** (today's behavior verbatim).
//!
//! **DB-load path (`--version`, VS-1.2.4 work-4.05, FR-7):** `--version
//! <VersionId>` loads the compiled DSL from [`SqliteStrategyRepo`] by version id,
//! runs the same engine, **FR-7-compares** the new run's `engine_fingerprint`
//! against `latest_run_for_version`'s **before** inserting (any
//! [`EngineFingerprint::compare`] warning goes to **stderr** so stdout stays
//! byte-stable), **then always** persists the run (the
//! [`BacktestRun`](crate::PersistedRun) header and its trades) via
//! [`SqliteBacktestRunRepo`]. There is no `--save` flag — every versioned run is
//! recorded provenance (README C7).
//!
//! **C5 flag defaults** match [`BacktestConfig::default`]: `--fee-bps 4`
//! (0.04% Binance USDⓂ taker), `--slippage-bps 1`, `--equity 10000` (USDT).
//!
//! **Error surfacing (spec §3):** a `NoStopLoss` engine error, a missing
//! snapshot, or a bad `--dsl` surface as typed `anyhow` errors with a clear
//! message + non-zero exit (mapped to `ExitCode::FAILURE` by the binary shim),
//! never a panic.
//!
//! **Rendering** follows the `pulse indicators` tab-separated + summary-footer
//! style (`src/cli/indicators.rs`) for consistency.

use std::path::PathBuf;

use rust_decimal::Decimal;

use crate::adapters::backtest::{BacktestConfig, run_backtest};
// r1.s3.w3: the shared version-id use case. `src/cli/mod.rs` stays the composition
// root; this module only builds the request and renders the persisted answer.
use crate::adapters::broker::BinanceAdapter;
use crate::adapters::db::{Db, SqliteBacktestRunRepo, SqliteStrategyRepo};
use crate::application::backtest::{BacktestOutcome, BacktestRequest, run_version_backtest};
use crate::domain::strategy::VersionId;
use crate::domain::{
    BacktestResult, CandleSeries, CandleSeriesRepository, CompiledStrategy, Direction,
    EngineFingerprint, ExchangeAdapter as _, ExitReason, Migrator, Pair, Regime, SeriesEnd,
    StrategyDsl, Timeframe, Trade, compile, validate,
};

use super::parse_one_tf;

/// `pulse backtest` arguments — the strategy source (`--dsl <file>` OR
/// `--version <VersionId>`), the pair/timeframe(s), the candle store, and the C5
/// cost knobs (`--fee-bps` / `--slippage-bps` / `--equity`).
///
/// **Strategy source (exactly one of, a clap `ArgGroup`):**
/// - `--dsl <file>` — load a DSL JSON file; **persistence-free + comparison-free +
///   silent** (today's behavior verbatim, README C7).
/// - `--version <VersionId>` — load the compiled DSL from the strategy repo by
///   version id; **always** FR-7-compares the prior run then persists this run +
///   trades (VS-1.2.4 work-4.05, README C7). There is no `--save` flag.
///
/// `--pair` and `--tf` are required; `--htf`, `--store`, `--db`, and `--json` are
/// optional. See each field for its meaning + default.
#[derive(Debug, clap::Args)]
#[command(group(
    clap::ArgGroup::new("strategy_source")
        .required(true)
        .args(["dsl", "version"]),
))]
pub struct BacktestArgs {
    /// Path to the strategy DSL JSON document to backtest (G7 load path;
    /// persistence-free). Mutually exclusive with `--version`.
    #[arg(long)]
    pub dsl: Option<PathBuf>,
    /// Persisted strategy VERSION id to backtest (the `--version` load path,
    /// VS-1.2.4 work-4.05). Loads the compiled DSL from the strategy repo, then
    /// ALWAYS FR-7-compares the prior run + persists this run (README C7 — no
    /// `--save` flag). Mutually exclusive with `--dsl`.
    #[arg(long = "version")]
    pub version: Option<String>,
    /// `pulse.db` path override (defaults to the platform Application Support db).
    /// Used only by the `--version` persist/compare path; `global = true` so it
    /// parses in any position (mirror `StrategyArgs.db`).
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
    /// The trading pair symbol (e.g. `BTCUSDT`).
    #[arg(long)]
    pub pair: String,
    /// Primary candle timeframe (`M15`/`15m` or `H4`/`4h`).
    #[arg(long)]
    pub tf: String,
    /// Optional higher timeframe for MTF alignment (`M15`/`H4`). Omitted ⇒
    /// single-timeframe backtest.
    #[arg(long)]
    pub htf: Option<String>,
    /// `CandleStore` root directory. Defaults to the platform Application Support
    /// data dir; the demo points it at the fixture store.
    #[arg(long)]
    pub store: Option<PathBuf>,
    /// Starting account equity (quote currency). C5 default matches
    /// `BacktestConfig::default`.
    #[arg(long, default_value = "10000")]
    pub equity: Decimal,
    /// Taker fee in basis points. C5 default `4` = 0.04% Binance USDⓂ taker.
    #[arg(long = "fee-bps", default_value = "4")]
    pub fee_bps: Decimal,
    /// Adverse-fill slippage in basis points. C5 default `1`.
    #[arg(long = "slippage-bps", default_value = "1")]
    pub slippage_bps: Decimal,
    /// Emit the result as a structured JSON object instead of the tab-separated
    /// trade log.
    #[arg(long)]
    pub json: bool,
}

/// Run a backtest and render the result. Two strategy-load paths share the same
/// engine pipeline + renderer; they differ only in where the compiled DSL comes
/// from and whether the run is persisted + FR-7-compared:
///
/// - **`--dsl <file>`** (G7, persistence-free): read the file → version-safe
///   [`Migrator`] load → [`validate`] → [`compile`] → run → render. Silent,
///   comparison-free, no DB touch (today's behavior verbatim, README C7).
/// - **`--version <VersionId>`** (VS-1.2.4 work-4.05, FR-7): load the compiled DSL
///   from [`SqliteStrategyRepo`] → run → render → FR-7-compare the prior run +
///   persist (in [`persist_and_compare`]). `db` MUST be `Some` for this path
///   (the dispatcher opens it via `open_migrated`).
///
/// `db` is `Some` only when the dispatcher opened a `pulse.db` for the `--version`
/// path; the `--dsl` path never touches it (so it stays persistence-free).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on: an unreadable / malformed `--dsl` file, an
/// unresolvable `--version` id (#65 real error), a DSL that fails semantic
/// validation, a compile error, an invalid pair/timeframe, a missing candle
/// snapshot, an engine error (e.g. `NoStopLoss`), or a persistence failure. Every
/// path surfaces a clear message + non-zero exit; nothing panics.
pub async fn run_backtest_cli<R>(
    db: Option<&Db>,
    repo: &R,
    args: &BacktestArgs,
) -> anyhow::Result<()>
where
    R: CandleSeriesRepository + Clone + Send + 'static,
{
    let pair = Pair::parse(args.pair.clone())
        .map_err(|e| anyhow::anyhow!("invalid pair argument: {e}"))?;
    let tf = parse_one_tf(&args.tf)?;
    let htf = match &args.htf {
        Some(raw) => Some(parse_one_tf(raw)?),
        None => None,
    };

    // Build + validate the cost config up front (it depends only on CLI scalars)
    // so an invalid --equity/--fee-bps/--slippage-bps fails fast — before the DSL
    // compile + snapshot reads — and is never masked by a later load error.
    // `run_backtest` re-validates it as the engine-boundary guarantee.
    let config = BacktestConfig {
        starting_equity: args.equity,
        taker_fee_bps: args.fee_bps,
        slippage_bps: args.slippage_bps,
    };
    config
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid cost configuration: {e}"))?;

    // r1.s3.w3: the `--version` path is now ONE shared use case, the same one the
    // desktop command runs. Only `--dsl` stays here: it has no `VersionId`, so it
    // cannot save a version-attributed run and cannot go through a flow that ends in
    // `save_run`. It remains persistence-free and comparison-free (README C7).
    if let Some(version) = args.version.as_ref() {
        let db =
            db.ok_or_else(|| anyhow::anyhow!("internal: --version path requires an open db"))?;
        return run_versioned(db, repo, args, version, &pair, tf, htf, config).await;
    }

    let compiled = load_dsl_strategy(args)?;

    let primary = load_series(repo, &pair, tf)?;
    let htf_series = match htf {
        Some(htf_tf) => Some(load_series(repo, &pair, htf_tf)?),
        None => None,
    };

    // Resolve the symbol's exchange filters through the `ExchangeAdapter` port —
    // this is where the port is exercised end-to-end (NFR-3); the engine itself
    // is a pure function of the resolved `SymbolFilters` value.
    let filters = BinanceAdapter::new()
        .symbol_filters(&pair)
        .map_err(|e| anyhow::anyhow!("resolve exchange filters for {pair}: {e}"))?;

    // The --dsl path runs whole snapshots — the series ends at data end, so a
    // still-open position force-closes as EndOfData (the windowed variant of
    // this ruling lives in the shared use case, r2.s1 G1).
    let result = run_backtest(
        &compiled,
        &primary,
        htf_series.as_ref(),
        &config,
        &filters,
        SeriesEnd::SnapshotEnd,
    )
    .map_err(|e| anyhow::anyhow!("backtest failed: {e}"))?;

    if args.json {
        render_json(&result)?;
    } else {
        render_human(&result);
    }
    Ok(())
}

/// The `--version` path: run the shared application use case, surface its FR-7
/// warning on stderr, and render the **persisted** result.
///
/// Rendering from the read-back rather than from memory is deliberate and is what
/// the desktop command does too: the versioned run's whole purpose is the row it
/// leaves behind, so the numbers the operator reads should be the numbers that were
/// stored. The `--dsl` path, which stores nothing, still renders its in-memory
/// result — there is nothing else it could render.
#[allow(clippy::too_many_arguments)]
async fn run_versioned<R>(
    db: &Db,
    repo: &R,
    args: &BacktestArgs,
    version: &str,
    pair: &Pair,
    tf: Timeframe,
    htf: Option<Timeframe>,
    config: BacktestConfig,
) -> anyhow::Result<()>
where
    R: CandleSeriesRepository + Clone + Send + 'static,
{
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let request = BacktestRequest {
        version_id: VersionId::new(version.to_owned()),
        pair: pair.clone(),
        primary_timeframe: tf,
        htf_timeframe: htf,
        config,
        // The CLI's explicit flags still drive the run directly — it does not
        // resolve defaults off a prior run and has no `--from/--to` window
        // surface this work item (the MCP tool does).
        snapshots: None,
        window: None,
    };
    let outcome = run_version_backtest(&strategies, repo, &BinanceAdapter::new(), &runs, &request)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // STDERR only (D4): stdout carries the byte-pinned footer/JSON.
    if let Some(warning) = outcome.fingerprint_warning.as_ref() {
        eprintln!("warning: {warning}");
    }

    let rendered = persisted_result(&outcome);
    if args.json {
        render_json(&rendered)?;
    } else {
        render_human(&rendered);
    }
    Ok(())
}

/// Rebuild a renderable [`BacktestResult`] from the SAVED run and trades.
///
/// Every field is a persisted column or a pure projection of the persisted trades —
/// nothing here is carried over from the pre-save in-memory value.
fn persisted_result(outcome: &BacktestOutcome) -> BacktestResult {
    let run = &outcome.run;
    BacktestResult {
        trades: outcome.trades.clone(),
        net_pnl: run.net_pnl,
        fees_total: run.fees_total,
        funding_total: run.funding_total,
        slippage_total: run.slippage_total,
        regime_breakdown: run.regime_breakdown,
        skipped_entries: run.skipped_entries,
        open_position: run.open_position.clone(),
        engine_fingerprint: EngineFingerprint::from_stored(run.engine_fingerprint.clone()),
        summary: run.summary.clone(),
        equity_curve: outcome.equity_curve(),
    }
}

/// Resolve the compiled strategy for the `--dsl` path.
///
/// The clap `ArgGroup` (`required(true)`, mutually exclusive) guarantees exactly one
/// of `--dsl` / `--version` is set, and `--version` returned earlier through the
/// shared use case — so by here the only legal shape is `--dsl`. The unreachable
/// both/neither arms still fail-closed with a clear `anyhow` error rather than panic
/// (#65).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on a bad `--dsl` file or, defensively, on an arg
/// combination `clap` should already have refused.
fn load_dsl_strategy(args: &BacktestArgs) -> anyhow::Result<CompiledStrategy> {
    match (&args.dsl, &args.version) {
        (Some(path), None) => load_compiled_strategy(path),
        (Some(_), Some(_)) => {
            anyhow::bail!("--dsl and --version are mutually exclusive (pick one)")
        }
        (None, _) => anyhow::bail!("exactly one of --dsl or --version is required"),
    }
}

/// Parse a DSL document through the **version-safe migrator** (FR-4), matching the
/// golden fixture's read path. A future `schema_version` (`> CURRENT`) is refused
/// rather than silently reinterpreted under current semantics; an older version is
/// migrated forward. (The previous raw `serde_json::from_str::<StrategyDsl>` would
/// have accepted any structurally-matching future document.)
fn parse_dsl(raw: &str) -> anyhow::Result<StrategyDsl> {
    let loaded = Migrator::v1()
        .load(raw)
        .map_err(|e| anyhow::anyhow!("load DSL (version-safe): {e}"))?;
    Ok(loaded.dsl)
}

/// Read + parse + validate + compile a DSL JSON file into a `CompiledStrategy`.
fn load_compiled_strategy(
    path: &std::path::Path,
) -> anyhow::Result<crate::domain::CompiledStrategy> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("read DSL file {}: {e}", path.display()))?;
    let dsl =
        parse_dsl(&raw).map_err(|e| anyhow::anyhow!("parse DSL file {}: {e}", path.display()))?;
    let validated =
        validate(&dsl).map_err(|e| anyhow::anyhow!("strategy failed validation: {e}"))?;
    compile(&validated).map_err(|e| anyhow::anyhow!("compile strategy: {e}"))
}

/// Refuse a series the backtester cannot interpret correctly: structural
/// corruption (unsorted / duplicate `open_time`) or any spacing **gap**. The
/// engine + indicator stream assume a gap-free, contiguous series (it does not
/// detect or fill holes), so a gapped snapshot would silently skew signals,
/// holding periods, and funding accrual — fail fast instead.
fn reject_if_gapped(series: &CandleSeries) -> anyhow::Result<()> {
    let gaps = series.validate().map_err(|e| {
        anyhow::anyhow!(
            "candle series for {} {} is structurally invalid: {e}",
            series.pair,
            series.timeframe.binance_interval()
        )
    })?;
    if let Some(first) = gaps.first() {
        anyhow::bail!(
            "candle series for {} {} has {} spacing gap(s) (first: expected open_time {}, found {}); \
             the backtester requires a gap-free series — re-fetch the snapshot",
            series.pair,
            series.timeframe.binance_interval(),
            gaps.len(),
            first.expected,
            first.found
        );
    }
    Ok(())
}

/// Load the `HEAD` snapshot for `(pair, tf)` through the repository port, erroring
/// clearly when no snapshot exists or when the series is not gap-free.
fn load_series<R>(repo: &R, pair: &Pair, tf: Timeframe) -> anyhow::Result<CandleSeries>
where
    R: CandleSeriesRepository,
{
    let stored = repo
        .load_head(pair, tf)
        .map_err(|e| {
            anyhow::anyhow!(
                "read HEAD snapshot for {pair} {}: {e}",
                tf.binance_interval()
            )
        })?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no HEAD snapshot for {pair} {} in the candle store",
                tf.binance_interval()
            )
        })?;
    reject_if_gapped(&stored.series)?;
    Ok(stored.series)
}

/// The tab-separated trade-log header (names every cost column the demo reads,
/// plus the per-trade MFE/MAE excursion + entry regime — VS-1.2.2 work-2.05).
/// `pub(super)` so `runs show` (`src/cli/runs.rs`) prints the SAME header above
/// the persisted trade log (D6).
pub(super) const TRADE_HEADER: &str = "entry_time\texit_time\tdir\tentry_price\texit_price\tqty\tfees\tfunding\tslippage\tpnl\tR\texit_reason\tmfe_r\tmae_r\tregime";

/// Render the human-readable trade log + the cost-breakdown footer + the
/// regime-breakdown block + the skipped-entries line (VS-1.2.2 work-2.05).
fn render_human(result: &BacktestResult) {
    println!("{TRADE_HEADER}");
    for trade in &result.trades {
        println!("{}", render_trade_row(trade));
    }
    println!("{}", render_footer(result));
    for line in render_regime_breakdown(result) {
        println!("{line}");
    }
    println!("{}", render_skipped_entries(result));
}

/// One tab-separated trade row. Decimals are normalized so trailing zeros do not
/// clutter the readout (matching the `indicators` renderer convention).
///
/// `pub(super)` so the `runs show` read verb (`src/cli/runs.rs`) renders the
/// persisted trade log through the SAME row formatter (D6 — one trade renderer,
/// no second formatter).
pub(super) fn render_trade_row(trade: &Trade) -> String {
    [
        trade.entry_fill_time.to_string(),
        trade.exit_fill_time.to_string(),
        direction_label(trade.direction).to_owned(),
        dec(trade.entry_price),
        dec(trade.exit_price),
        dec(trade.qty),
        dec(trade.fees_total),
        dec(trade.funding_total),
        dec(trade.slippage_total),
        dec(trade.realized_pnl),
        dec(trade.realized_r),
        exit_reason_label(trade.exit_reason).to_owned(),
        dec(trade.mfe_r),
        dec(trade.mae_r),
        regime_label(trade.regime).to_owned(),
    ]
    .join("\t")
}

/// The summary footer: trade count + gross/net P&L + run-level cost totals (the
/// "costs are deducted" readout, FR-6). `gross` is the P&L before costs; the engine
/// nets it as `net = gross_slipped + funding − fees` with slippage embedded in the
/// slipped fills, so inverting gives `gross = net + fees − funding + slippage`.
/// `funding_total` is SIGNED (negative when the position pays funding), so it is
/// SUBTRACTED here — adding it would move gross the wrong way by `2 × |funding|`.
fn render_footer(result: &BacktestResult) -> String {
    let gross = result.net_pnl + result.fees_total - result.funding_total + result.slippage_total;
    let s = &result.summary;
    [
        "summary".to_owned(),
        format!("trades={}", result.trades.len()),
        format!("gross_pnl={}", dec(gross)),
        format!("net_pnl={}", dec(result.net_pnl)),
        format!("fees_total={}", dec(result.fees_total)),
        format!("funding_total={}", dec(result.funding_total)),
        format!("slippage_total={}", dec(result.slippage_total)),
        // VS-1.2.4 work-4.05 (FR-6, D5): the headline `SummaryStats` cells appended
        // ADDITIVELY — the existing cells above + the three fingerprint cells below
        // stay byte-identical in spelling + order (the footer tests pin them). The
        // two `Option` stats (`profit_factor`, `sharpe`) render the `—` sentinel
        // when `None` (mirror `strategy.rs:281`); Decimal stats use `dec()`.
        format!("expectancy={}", dec(s.expectancy)),
        format!("win_rate={}", dec(s.win_rate)),
        format!("profit_factor={}", dec_opt(s.profit_factor)),
        format!("sharpe={}", f64_opt(s.sharpe)),
        format!("max_drawdown={}", dec(s.max_drawdown)),
        format!("max_win_streak={}", s.max_win_streak),
        format!("max_loss_streak={}", s.max_loss_streak),
        // FR-7 / NFR-2 (3.03): surface the build-time engine identity — the hex
        // fingerprint plus the target triple (arch) — and the byte-stable content
        // hash, so two runs are comparable (matching fingerprint) and reproducible
        // (matching content hash). The content hash EXCLUDES the fingerprint (D4).
        format!("engine_fingerprint={}", result.engine_fingerprint.as_str()),
        format!("target={}", EngineFingerprint::target()),
        format!("content_hash={}", result.result_content_hash()),
    ]
    .join("\t")
}

/// The per-regime breakdown block (FR-5, VS-1.2.2 work-2.05): one
/// `regime=<label>\ttrades=<n>\tnet_pnl=<dec>` line per regime that has at least
/// one trade. `unknown` is a **first-class** cell (#16) — it is included only
/// when it actually holds trades (pre-EMA200-warmup entries), never silently
/// merged into `ranging`. A run with zero trades yields no lines (the
/// `skipped_entries` line still prints separately for observability).
fn render_regime_breakdown(result: &BacktestResult) -> Vec<String> {
    [
        Regime::TrendingUp,
        Regime::TrendingDown,
        Regime::Ranging,
        Regime::Unknown,
    ]
    .into_iter()
    .filter_map(|regime| {
        let cell = result.regime_breakdown.cell(regime);
        if cell.trade_count == 0 {
            return None;
        }
        Some(
            [
                format!("regime={}", regime_label(regime)),
                format!("trades={}", cell.trade_count),
                format!("net_pnl={}", dec(cell.net_pnl)),
            ]
            .join("\t"),
        )
    })
    .collect()
}

/// The skipped-entries observability line (audit C4, VS-1.2.2 work-2.05): always
/// prints `skipped_entries=<total()>` so a user staring at a low trade count can
/// tell "no signals" from "too small to size". When `total() > 0`, the per-reason
/// breakdown (`sub_lot` / `sub_notional` / `leverage_capped`) is appended.
fn render_skipped_entries(result: &BacktestResult) -> String {
    let counts = &result.skipped_entries;
    let total = counts.total();
    let mut cells = vec![format!("skipped_entries={total}")];
    if total > 0 {
        cells.push(format!("sub_lot={}", counts.sub_lot));
        cells.push(format!("sub_notional={}", counts.sub_notional));
        cells.push(format!("leverage_capped={}", counts.leverage_capped));
    }
    cells.join("\t")
}

/// Emit the full [`BacktestResult`] as a structured JSON object (`--json`).
fn render_json(result: &BacktestResult) -> anyhow::Result<()> {
    let line = serde_json::to_string_pretty(result)
        .map_err(|e| anyhow::anyhow!("serialize backtest result: {e}"))?;
    println!("{line}");
    Ok(())
}

/// A compact, trailing-zero-normalized decimal rendering. `pub(super)` so the
/// `runs show` read verb renders the reconstructed equity-curve summary + the
/// `SummaryStats` Decimal cells with the SAME normalization (D6).
pub(super) fn dec(value: Decimal) -> String {
    value.normalize().to_string()
}

/// The render sentinel for an absent `Option` stat (mirror `strategy.rs:281`'s
/// `—` convention) — used by the additive `SummaryStats` footer + the `runs show`
/// read verb for `profit_factor` / `sharpe` when `None`.
pub(super) const ABSENT_SENTINEL: &str = "—";

/// Render an `Option<Decimal>` stat (`profit_factor`): the normalized decimal when
/// `Some`, else the `—` sentinel (D5).
pub(super) fn dec_opt(value: Option<Decimal>) -> String {
    value.map_or_else(|| ABSENT_SENTINEL.to_owned(), dec)
}

/// Render an `Option<f64>` stat (`sharpe`/`sortino`): the `f64` (finite by the C1
/// `None`-when-degenerate contract) when `Some`, else the `—` sentinel (D5).
pub(super) fn f64_opt(value: Option<f64>) -> String {
    value.map_or_else(|| ABSENT_SENTINEL.to_owned(), |x| x.to_string())
}

/// Human-readable trade-side label.
fn direction_label(direction: Direction) -> &'static str {
    match direction {
        Direction::Long => "long",
        Direction::Short => "short",
    }
}

/// Human-readable exit-reason label.
fn exit_reason_label(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::StopLoss => "stop_loss",
        ExitReason::TakeProfit => "take_profit",
        ExitReason::Signal => "signal",
        ExitReason::EndOfData => "end_of_data",
    }
}

/// Human-readable market-regime label (mirrors [`exit_reason_label`]; matches the
/// `snake_case` serde tags so the human + JSON readouts stay consistent).
/// `Unknown` is a first-class label (#16), never collapsed into `ranging`.
fn regime_label(regime: Regime) -> &'static str {
    match regime {
        Regime::TrendingUp => "trending_up",
        Regime::TrendingDown => "trending_down",
        Regime::Ranging => "ranging",
        Regime::Unknown => "unknown",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{EngineFingerprint, render_json};
    use super::{
        TRADE_HEADER, dec, dec_opt, f64_opt, parse_dsl, regime_label, reject_if_gapped,
        render_footer, render_regime_breakdown, render_skipped_entries, render_trade_row,
    };
    use crate::domain::{
        BacktestResult, Candle, CandleSeries, Comparator, Condition, DataVersion, Direction,
        EquityCurve, ExitReason, ExitRule, Fill, Pair, PriceField, Regime, RegimeBreakdown,
        RiskParams, SchemaVersion, Series, SkipReason, SkippedEntryCounts, StrategyDsl,
        SummaryStats, SweepableValue, Timeframe, Trade, TradeSource, ValueSource,
    };
    use rust_decimal::Decimal;

    /// A valid, current-schema price-only strategy (no indicators) for load tests.
    fn sample_dsl() -> StrategyDsl {
        StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "t".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Price {
                    series: Series::Primary,
                    field: PriceField::Close,
                },
                op: Comparator::Gt,
                rhs: ValueSource::Constant {
                    value: Decimal::ZERO,
                },
            },
            filters: vec![],
            exits: vec![
                ExitRule::StopLoss {
                    distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
                },
                ExitRule::TakeProfit {
                    target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
                },
            ],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        }
    }

    fn m15_candle(open_time: i64) -> Candle {
        Candle {
            open_time,
            close_time: open_time + 900_000 - 1,
            open: Decimal::new(100, 0),
            high: Decimal::new(101, 0),
            low: Decimal::new(99, 0),
            close: Decimal::new(100, 0),
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    fn m15_series(open_times: &[i64]) -> CandleSeries {
        CandleSeries {
            pair: Pair::new("BTCUSDT"),
            timeframe: Timeframe::M15,
            version: DataVersion::new("test"),
            candles: open_times.iter().map(|t| m15_candle(*t)).collect(),
        }
    }

    fn sample_trade() -> Trade {
        Trade {
            direction: Direction::Long,
            qty: Decimal::new(5, 1), // 0.5
            entry_price: Decimal::new(30_000, 0),
            exit_price: Decimal::new(33_000, 0),
            entry_signal_time: 1,
            entry_fill_time: 2,
            exit_signal_time: 3,
            exit_fill_time: 4,
            fills: vec![Fill {
                price: Decimal::new(30_000, 0),
                qty: Decimal::new(5, 1),
                time_ms: 2,
                fee: Decimal::new(6, 0),
            }],
            fees_total: Decimal::new(12, 0),
            funding_total: Decimal::new(1, 0),
            slippage_total: Decimal::new(3, 0),
            realized_pnl: Decimal::new(1_484, 0),
            realized_r: Decimal::new(2, 0),
            mfe_r: Decimal::new(25, 1), // 2.5; render of MFE/MAE is 2.05's job
            mae_r: Decimal::new(-5, 1), // -0.5
            exit_reason: ExitReason::TakeProfit,
            source: TradeSource::Backtest,
            regime: Regime::TrendingUp,
            stop_price: Some(Decimal::new(28_500, 0)),
        }
    }

    #[test]
    fn header_names_every_cost_column() {
        assert!(TRADE_HEADER.contains("fees"));
        assert!(TRADE_HEADER.contains("funding"));
        assert!(TRADE_HEADER.contains("slippage"));
        assert!(TRADE_HEADER.contains("pnl"));
    }

    #[test]
    fn trade_row_is_tab_separated_with_all_fields() {
        let row = render_trade_row(&sample_trade());
        let cells = row.split('\t').collect::<Vec<_>>();
        // Same arity as the header (15 columns: 12 cost/identity + mfe_r/mae_r/regime).
        assert_eq!(cells.len(), TRADE_HEADER.split('\t').count());
        assert_eq!(cells[2], "long");
        assert_eq!(cells[6], "12"); // fees
        assert_eq!(cells[7], "1"); // funding
        assert_eq!(cells[8], "3"); // slippage
        assert_eq!(cells[11], "take_profit");
        // The three VS-1.2.2 work-2.05 columns, trailing-zero-normalized.
        assert_eq!(cells[12], "2.5"); // mfe_r
        assert_eq!(cells[13], "-0.5"); // mae_r
        assert_eq!(cells[14], "trending_up"); // regime
    }

    #[test]
    fn header_names_mfe_mae_regime_columns() {
        // The 2.05 readout columns the e2e/user demo reads.
        assert!(TRADE_HEADER.contains("mfe_r"));
        assert!(TRADE_HEADER.contains("mae_r"));
        assert!(TRADE_HEADER.contains("regime"));
    }

    #[test]
    fn regime_label_mirrors_snake_case_serde_tags() {
        assert_eq!(regime_label(Regime::TrendingUp), "trending_up");
        assert_eq!(regime_label(Regime::TrendingDown), "trending_down");
        assert_eq!(regime_label(Regime::Ranging), "ranging");
        // Unknown is a first-class label (#16), never collapsed into ranging.
        assert_eq!(regime_label(Regime::Unknown), "unknown");
    }

    #[test]
    fn regime_breakdown_renders_one_line_per_present_regime() {
        let mut breakdown = RegimeBreakdown::new();
        breakdown.record(Regime::TrendingUp, Decimal::new(150, 0));
        breakdown.record(Regime::TrendingUp, Decimal::new(50, 0));
        breakdown.record(Regime::Ranging, Decimal::new(-20, 0));
        // Unknown deliberately left empty here → must NOT appear.
        let result = BacktestResult {
            trades: vec![],
            net_pnl: Decimal::new(180, 0),
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            regime_breakdown: breakdown,
            skipped_entries: SkippedEntryCounts::new(),
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: crate::domain::SummaryStats::default(),
            equity_curve: crate::domain::EquityCurve::default(),
        };
        let lines = render_regime_breakdown(&result);
        // Only the two regimes with trades appear (trending_down + unknown absent).
        assert_eq!(lines.len(), 2, "lines were: {lines:?}");
        let joined = lines.join("\n");
        assert!(joined.contains("regime=trending_up\ttrades=2\tnet_pnl=200"));
        assert!(joined.contains("regime=ranging\ttrades=1\tnet_pnl=-20"));
        assert!(
            !joined.contains("trending_down"),
            "empty regimes must be omitted"
        );
        assert!(
            !joined.contains("unknown"),
            "empty unknown cell must be omitted"
        );
    }

    #[test]
    fn regime_breakdown_renders_unknown_as_first_class_cell() {
        // A trade that opened pre-warmup lands in the Unknown cell — it MUST render
        // (#16): not silently merged into ranging, not dropped.
        let mut breakdown = RegimeBreakdown::new();
        breakdown.record(Regime::Unknown, Decimal::new(7, 0));
        let result = BacktestResult {
            trades: vec![],
            net_pnl: Decimal::new(7, 0),
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            regime_breakdown: breakdown,
            skipped_entries: SkippedEntryCounts::new(),
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: crate::domain::SummaryStats::default(),
            equity_curve: crate::domain::EquityCurve::default(),
        };
        let lines = render_regime_breakdown(&result);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("regime=unknown\ttrades=1\tnet_pnl=7"));
    }

    #[test]
    fn skipped_entries_line_always_prints_zero() {
        // Observability: even with zero suppressed entries the line prints so a
        // user can distinguish "no signals" from "too small to size".
        let result = BacktestResult {
            trades: vec![],
            net_pnl: Decimal::ZERO,
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            regime_breakdown: RegimeBreakdown::new(),
            skipped_entries: SkippedEntryCounts::new(),
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: crate::domain::SummaryStats::default(),
            equity_curve: crate::domain::EquityCurve::default(),
        };
        let line = render_skipped_entries(&result);
        assert_eq!(line, "skipped_entries=0");
    }

    #[test]
    fn skipped_entries_line_breaks_down_per_reason_when_nonzero() {
        let mut counts = SkippedEntryCounts::new();
        counts.record(SkipReason::SubLot);
        counts.record(SkipReason::SubLot);
        counts.record(SkipReason::SubNotional);
        counts.record(SkipReason::LeverageCapZero);
        let result = BacktestResult {
            trades: vec![],
            net_pnl: Decimal::ZERO,
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            regime_breakdown: RegimeBreakdown::new(),
            skipped_entries: counts,
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: crate::domain::SummaryStats::default(),
            equity_curve: crate::domain::EquityCurve::default(),
        };
        let line = render_skipped_entries(&result);
        assert!(line.contains("skipped_entries=4"), "line was: {line}");
        assert!(line.contains("sub_lot=2"), "line was: {line}");
        assert!(line.contains("sub_notional=1"), "line was: {line}");
        assert!(line.contains("leverage_capped=1"), "line was: {line}");
    }

    #[test]
    fn footer_reports_counts_net_and_gross() {
        let result = BacktestResult {
            trades: vec![sample_trade()],
            net_pnl: Decimal::new(1_484, 0),
            fees_total: Decimal::new(12, 0),
            funding_total: Decimal::new(1, 0),
            slippage_total: Decimal::new(3, 0),
            regime_breakdown: RegimeBreakdown::new(),
            skipped_entries: SkippedEntryCounts::new(),
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: crate::domain::SummaryStats::default(),
            equity_curve: crate::domain::EquityCurve::default(),
        };
        let footer = render_footer(&result);
        assert!(footer.contains("trades=1"));
        assert!(footer.contains("net_pnl=1484"));
        // funding is SIGNED (engine: net = gross_slipped + funding − fees), so gross
        // before costs = net + fees − funding + slippage = 1484 + 12 − 1 + 3 = 1498.
        assert!(footer.contains("gross_pnl=1498"), "footer was: {footer}");
        assert!(footer.contains("fees_total=12"));
    }

    /// A `SummaryStats` with every headline field populated (and the two `Option`
    /// stats `Some`) so the additive-footer test can assert the new cells render
    /// the real values, not the sentinel.
    fn populated_summary() -> SummaryStats {
        SummaryStats {
            trade_count: 3,
            win_count: 2,
            loss_count: 1,
            win_rate: Decimal::new(666, 3), // 0.666
            gross_profit: Decimal::new(300, 0),
            gross_loss: Decimal::new(100, 0),
            net_pnl: Decimal::new(200, 0),
            profit_factor: Some(Decimal::new(3, 0)),
            avg_win: Decimal::new(150, 0),
            avg_loss: Decimal::new(100, 0),
            expectancy: Decimal::new(6666, 2), // 66.66
            max_drawdown: Decimal::new(40, 0),
            max_win_streak: 2,
            max_loss_streak: 1,
            commission_total: Decimal::new(12, 0),
            funding_total: Decimal::new(1, 0),
            sharpe: Some(1.25),
            sortino: Some(2.5),
        }
    }

    /// AC-9 (`footer_is_additive_keeps_existing_lines`): the footer renders the NEW
    /// headline `SummaryStats` cells (expectancy / win rate / profit factor /
    /// Sharpe / max drawdown / streaks) WITHOUT moving or re-spelling the existing
    /// cells — the three pinned fingerprint/target/content-hash cells AND the cost
    /// cells stay byte-identical (D5). The `Option` stats render their real value
    /// when `Some`, the `—` sentinel when `None`.
    #[test]
    fn footer_is_additive_keeps_existing_lines() {
        let result = BacktestResult {
            trades: vec![sample_trade()],
            net_pnl: Decimal::new(1_484, 0),
            fees_total: Decimal::new(12, 0),
            funding_total: Decimal::new(1, 0),
            slippage_total: Decimal::new(3, 0),
            regime_breakdown: RegimeBreakdown::new(),
            skipped_entries: SkippedEntryCounts::new(),
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: populated_summary(),
            equity_curve: EquityCurve::default(),
        };
        let footer = render_footer(&result);

        // (a) the existing cost cells stay byte-identical in spelling.
        assert!(footer.contains("trades=1"), "footer was: {footer}");
        assert!(footer.contains("gross_pnl=1498"), "footer was: {footer}");
        assert!(footer.contains("net_pnl=1484"), "footer was: {footer}");
        assert!(footer.contains("fees_total=12"), "footer was: {footer}");
        assert!(footer.contains("funding_total=1"), "footer was: {footer}");
        assert!(footer.contains("slippage_total=3"), "footer was: {footer}");

        // (b) the three pinned engine-identity cells survive byte-identical (D5).
        let fp = EngineFingerprint::current();
        assert!(
            footer.contains(&format!("engine_fingerprint={}", fp.as_str())),
            "the pinned engine_fingerprint cell must survive byte-identical; footer was: {footer}"
        );
        assert!(
            footer.contains(&format!("target={}", EngineFingerprint::target())),
            "the pinned target cell must survive byte-identical; footer was: {footer}"
        );
        assert!(
            footer.contains(&format!("content_hash={}", result.result_content_hash())),
            "the pinned content_hash cell must survive byte-identical; footer was: {footer}"
        );

        // (c) the NEW additive headline-SummaryStats cells appear with real values.
        assert!(footer.contains("expectancy=66.66"), "footer was: {footer}");
        assert!(footer.contains("win_rate=0.666"), "footer was: {footer}");
        assert!(footer.contains("profit_factor=3"), "footer was: {footer}");
        assert!(footer.contains("sharpe=1.25"), "footer was: {footer}");
        assert!(footer.contains("max_drawdown=40"), "footer was: {footer}");
        assert!(footer.contains("max_win_streak=2"), "footer was: {footer}");
        assert!(footer.contains("max_loss_streak=1"), "footer was: {footer}");

        // (d) the `None` Option stats render the `—` sentinel (mirror strategy.rs).
        let mut none_stats = populated_summary();
        none_stats.profit_factor = None;
        none_stats.sharpe = None;
        let none_result = BacktestResult {
            summary: none_stats,
            ..result
        };
        let none_footer = render_footer(&none_result);
        assert!(
            none_footer.contains("profit_factor=—"),
            "absent profit_factor must render the sentinel; footer was: {none_footer}"
        );
        assert!(
            none_footer.contains("sharpe=—"),
            "absent sharpe must render the sentinel; footer was: {none_footer}"
        );
    }

    /// AC-10 (`persist_wiring_excludes_content_hash`, mirror
    /// `content_hash_excludes_fingerprint`): the persist/footer/stats wiring reads
    /// `result.summary` / `result.equity_curve`, but those fields are
    /// oracle-excluded (README C3/C8 / #69 frozen), so two results differing ONLY
    /// in their `summary` + `equity_curve` produce the SAME `result_content_hash()`.
    /// This guards the C8 freeze at the 4.05 seam: rendering the footer + persisting
    /// the run cannot move the content hash (the freeze stays frozen by
    /// construction). Local NON-PERTURBATION proof — NOT the literal cross-arch
    /// baseline (#62: that is the CI matrix's job; no `49702fd5…` pinned here).
    #[test]
    fn persist_wiring_excludes_content_hash() {
        // Base: default (empty) summary + curve — the pre-4.05 shape.
        let base = BacktestResult {
            trades: vec![sample_trade()],
            net_pnl: Decimal::new(1_484, 0),
            fees_total: Decimal::new(12, 0),
            funding_total: Decimal::new(1, 0),
            slippage_total: Decimal::new(3, 0),
            regime_breakdown: RegimeBreakdown::new(),
            skipped_entries: SkippedEntryCounts::new(),
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: SummaryStats::default(),
            equity_curve: EquityCurve::default(),
        };
        // Other: identical EXCEPT a fully-populated summary + a non-empty equity
        // curve (exactly what the footer renders + `save_run` persists).
        let mut other = base.clone();
        other.summary = populated_summary();
        other.equity_curve = EquityCurve::from_trades(
            0,
            Decimal::new(10_000, 0),
            std::slice::from_ref(&sample_trade()),
        );

        assert_ne!(
            base.summary, other.summary,
            "the two results must genuinely differ in their summary"
        );
        assert_ne!(
            base.equity_curve, other.equity_curve,
            "the two results must genuinely differ in their equity curve"
        );
        assert_eq!(
            base.result_content_hash(),
            other.result_content_hash(),
            "result_content_hash must EXCLUDE summary + equity_curve (the persist/footer/stats \
             wiring cannot move the C8-frozen oracle)"
        );
    }

    /// The `Option`-stat render helpers: a `Some` renders the value, a `None` the
    /// `—` sentinel (mirror `strategy.rs:281`).
    #[test]
    fn option_stat_helpers_render_value_or_sentinel() {
        assert_eq!(dec_opt(Some(Decimal::new(25, 1))), "2.5");
        assert_eq!(dec_opt(None), "—");
        assert_eq!(f64_opt(Some(1.5)), "1.5");
        assert_eq!(f64_opt(None), "—");
    }

    /// AC-4 (`render_includes_fingerprint`): the engine fingerprint reaches BOTH
    /// render surfaces — the human footer (`render_footer`) carries a non-empty
    /// `engine_fingerprint=…`, the target arch (`target=…`), and the byte-stable
    /// `content_hash=…`; and the `--json` object (the exact serialization
    /// `render_json` emits) carries the `engine_fingerprint` field. FR-7 / NFR-2.
    #[test]
    fn render_includes_fingerprint() {
        let result = BacktestResult {
            trades: vec![sample_trade()],
            net_pnl: Decimal::new(1_484, 0),
            fees_total: Decimal::new(12, 0),
            funding_total: Decimal::new(1, 0),
            slippage_total: Decimal::new(3, 0),
            regime_breakdown: RegimeBreakdown::new(),
            skipped_entries: SkippedEntryCounts::new(),
            open_position: None,
            engine_fingerprint: EngineFingerprint::current(),
            summary: crate::domain::SummaryStats::default(),
            equity_curve: crate::domain::EquityCurve::default(),
        };

        // Human footer: the fingerprint, the target arch, and the content hash.
        let footer = render_footer(&result);
        let fp = EngineFingerprint::current();
        assert!(
            footer.contains(&format!("engine_fingerprint={}", fp.as_str())),
            "footer must carry the engine fingerprint, was: {footer}"
        );
        assert!(!fp.as_str().is_empty(), "fingerprint must be non-empty");
        assert!(
            footer.contains(&format!("target={}", EngineFingerprint::target())),
            "footer must carry the target arch, was: {footer}"
        );
        assert!(
            footer.contains(&format!("content_hash={}", result.result_content_hash())),
            "footer must carry the byte-stable content hash, was: {footer}"
        );

        // `--json`: the field flows through the whole-result serialization that
        // `render_json` emits. Assert against the identical serializer call so the
        // `--json` object demonstrably carries the fingerprint.
        let json = serde_json::to_string_pretty(&result).expect("serialize result");
        assert!(
            json.contains("engine_fingerprint"),
            "json must carry the engine_fingerprint field, was: {json}"
        );
        assert!(
            json.contains(fp.as_str()),
            "json must carry the fingerprint hex value, was: {json}"
        );
        // `render_json` itself succeeds (prints to stdout, returns Ok).
        render_json(&result).expect("render_json must succeed");
    }

    #[test]
    fn dec_normalizes_trailing_zeros() {
        assert_eq!(dec(Decimal::new(2_00, 2)), "2");
        assert_eq!(dec(Decimal::new(5, 1)), "0.5");
    }

    #[test]
    fn gapped_series_is_rejected_clean_series_passes() {
        let step = 900_000; // M15 duration in ms
        let clean = m15_series(&[0, step, 2 * step]);
        assert!(reject_if_gapped(&clean).is_ok());

        // Skips the 2*step bar → a one-interval hole the backtester must refuse.
        let gapped = m15_series(&[0, step, 3 * step]);
        assert!(reject_if_gapped(&gapped).is_err());
    }

    #[test]
    fn parse_dsl_accepts_current_and_refuses_future_schema_version() {
        let json = serde_json::to_string(&sample_dsl()).unwrap();
        // A current-schema document loads through the version-safe migrator.
        assert!(parse_dsl(&json).is_ok());

        // A future major (> CURRENT) must be REFUSED, not silently reinterpreted
        // under current semantics (the raw `serde_json::from_str` it replaced would
        // have accepted it).
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v["schema_version"] = serde_json::Value::String("2.0.0".to_owned());
        let future = serde_json::to_string(&v).unwrap();
        assert!(
            parse_dsl(&future).is_err(),
            "a future schema_version must be refused by the version-safe loader"
        );
    }
}
