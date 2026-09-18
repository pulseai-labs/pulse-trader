//! `pulse runs` — the FR-6 persisted-backtest-run read verb (VS-1.2.4 work-4.05).
//!
//! The read half of the persistence wire-up: `pulse runs list --version <id>`
//! lists the run catalog for a strategy version (one tab row per run, mirroring
//! `strategy.rs`'s `verb_list`), and `pulse runs show <run-id>` prints a run
//! header + its [`SummaryStats`] + the **reconstructed equity-curve summary** +
//! the persisted trade log. It consumes the [`BacktestRunRepository`] port
//! generically (`<R: BacktestRunRepository>`, never `dyn`) over the [`Db`] pool —
//! a pure CONSUMER of 4.04's repo (it adds no repo method, no schema, no math).
//!
//! **Equity-curve reconstruction on read (D6, README C2/C4).** The equity curve
//! is NOT stored as rows (there is no `equity_curve_point` table); `runs show`
//! rebuilds it on read by calling 4.01's SAME [`EquityCurve::from_trades`]
//! constructor over the persisted `seq`-ordered trades + the run's persisted
//! `starting_equity`, then renders its SUMMARY (first/last equity + `max_drawdown`).
//! The summary is time-INDEPENDENT of the leading point (it depends only on
//! `starting_equity` + the trades' `realized_pnl`), so a defensible
//! `run_start_time_ms` (the first trade's entry-fill time, or `0` when there are
//! no trades) yields a curve whose summary EQUALS the in-memory one
//! (final `equity == starting_equity + net_pnl`).
//!
//! **Async (D2):** the [`BacktestRunRepository`] port is async, so [`run_runs`]
//! is `async fn` awaited inside the existing `dispatch` future — the `mod.rs`
//! sync→async bridge is reused (no new `#[tokio::main]`, no new runtime). The `Db`
//! is opened (migrate-then-open via `open_migrated`) by `mod.rs::dispatch`.

use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::adapters::db::Db;
use crate::adapters::db::SqliteBacktestRunRepo;
use crate::domain::backtest::EquityCurve;
use crate::domain::strategy::VersionId;
use crate::domain::{BacktestInputs, BacktestRunId, BacktestRunRepository, DataError};

use super::backtest::{TRADE_HEADER, dec, dec_opt, f64_opt, render_trade_row};

/// `pulse runs <SUBCOMMAND> [--db <path>]`.
///
/// `--db` mirrors `StrategyArgs.db` (`global = true` so it parses in any
/// position); omitted, `mod.rs::dispatch` resolves `default_db_path()` and opens
/// the pool via `open_migrated` BEFORE calling [`run_runs`].
#[derive(Debug, Args)]
pub struct RunsArgs {
    /// The runs subcommand to run.
    #[command(subcommand)]
    pub command: RunsCommand,
    /// `pulse.db` path override (defaults to the platform Application Support db).
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
}

/// The `runs` read verbs — `list` (catalog for a version) + `show` (one run).
#[derive(Debug, Subcommand)]
pub enum RunsCommand {
    /// List the persisted runs for a strategy version (FR-6 catalog).
    List {
        /// The strategy VERSION id whose runs to list.
        #[arg(long = "version")]
        version: String,
    },
    /// Show one persisted run: header + `SummaryStats` + the reconstructed
    /// equity-curve summary + the persisted trade log.
    Show {
        /// The backtest RUN id to show (positional).
        run: String,
    },
}

/// Map a `DataError` (repo boundary) to an `anyhow::Error` with a context label
/// (mirror `strategy.rs::db_err`). The binary shim turns the `Err` into a non-zero
/// `ExitCode::FAILURE`.
fn db_err(label: &str, e: &DataError) -> anyhow::Error {
    anyhow::anyhow!("{label}: {e}")
}

/// Orchestrate `pulse runs` over the [`BacktestRunRepository`] port. The `Db` pool
/// is opened (migrate-then-open) by the caller (`mod.rs::dispatch`); here we build
/// the production repo over its pool and dispatch the verb.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on a repo failure or a not-found run id (#65 — a
/// real error mapped to a non-zero exit, never a `debug_assert!`).
pub async fn run_runs(db: &Db, args: &RunsArgs) -> anyhow::Result<()> {
    let repo = SqliteBacktestRunRepo::new(db.pool().clone());
    match &args.command {
        RunsCommand::List { version } => verb_runs_list(&repo, version).await,
        RunsCommand::Show { run } => verb_runs_show(&repo, run).await,
    }
}

/// `runs list --version <id>` — one tab row per persisted run for the version,
/// catalog-ordered (`ORDER BY created_at, id`, #40). Mirrors `verb_list`
/// (`strategy.rs:250`).
async fn verb_runs_list<R: BacktestRunRepository>(repo: &R, version: &str) -> anyhow::Result<()> {
    let version_id = VersionId::new(version.to_owned());
    let runs = repo
        .list_runs_for_version(&version_id)
        .await
        .map_err(|e| db_err("list runs for version", &e))?;
    for r in &runs {
        println!(
            "run\t{}\tversion={}\tcreated_at={}\tnet_pnl={}\texpectancy={}\ttrades={}\tfingerprint={}",
            r.id.as_str(),
            r.strategy_version_id.as_str(),
            r.created_at,
            dec(r.net_pnl),
            dec(r.expectancy),
            r.trade_count,
            r.engine_fingerprint,
        );
    }
    Ok(())
}

/// `runs show <run-id>` — the run header + the headline `SummaryStats` + the
/// reconstructed equity-curve summary + the persisted trade log. Mirrors
/// `verb_show` (`strategy.rs:269`). A not-found run id is a non-zero exit (#65).
async fn verb_runs_show<R: BacktestRunRepository>(repo: &R, run: &str) -> anyhow::Result<()> {
    let run_id = BacktestRunId::new(run.to_owned());
    let persisted = repo
        .get_run(&run_id)
        .await
        .map_err(|e| db_err("get run", &e))?
        .ok_or_else(|| anyhow::anyhow!("no such run `{run}`"))?;
    let trades = repo
        .get_trades(&run_id)
        .await
        .map_err(|e| db_err("get trades", &e))?;

    // Run header + provenance.
    println!(
        "run\t{}\tversion={}\tcreated_at={}\tengine_fingerprint={}\ttarget={}\tcontent_hash={}",
        persisted.id.as_str(),
        persisted.strategy_version_id.as_str(),
        persisted.created_at,
        persisted.engine_fingerprint,
        persisted.engine_target,
        persisted.result_content_hash,
    );

    // r1.s3.w2 (#110): what the run CONSUMED, on its own stable line. A legacy row
    // says so in words rather than printing blanks that read like zeros.
    println!("{}", render_inputs(persisted.inputs.as_ref()));

    // Run-level money totals (the cost readout, FR-6).
    println!(
        "totals\tstarting_equity={}\tnet_pnl={}\tfees_total={}\tfunding_total={}\tslippage_total={}",
        dec(persisted.starting_equity),
        dec(persisted.net_pnl),
        dec(persisted.fees_total),
        dec(persisted.funding_total),
        dec(persisted.slippage_total),
    );

    // The headline `SummaryStats` (expectancy / win rate / profit factor / Sharpe /
    // max drawdown / streaks) — the user's "read the stats" demo criterion. The
    // two Option stats render the `—` sentinel when None (mirror the footer, D5).
    let s = &persisted.summary;
    println!(
        "stats\texpectancy={}\twin_rate={}\tprofit_factor={}\tsharpe={}\tsortino={}\tmax_drawdown={}\tmax_win_streak={}\tmax_loss_streak={}\ttrade_count={}",
        dec(s.expectancy),
        dec(s.win_rate),
        dec_opt(s.profit_factor),
        f64_opt(s.sharpe),
        f64_opt(s.sortino),
        dec(s.max_drawdown),
        s.max_win_streak,
        s.max_loss_streak,
        s.trade_count,
    );

    // Reconstruct the equity curve on read (D6, README C2/C4) via 4.01's SAME
    // constructor over the persisted seq-ordered trades + the run's starting
    // equity — NO second builder, NO stored equity_curve_point table. The summary
    // (first/last equity + max_drawdown) is time-independent of the leading point,
    // so a defensible run_start_time_ms (the first trade's entry-fill time, else 0)
    // yields final equity == starting_equity + net_pnl (the AC-18 value-equality).
    let run_start_time_ms = trades.first().map_or(0, |t| t.entry_fill_time);
    let curve = EquityCurve::from_trades(run_start_time_ms, persisted.starting_equity, &trades);
    let first_equity = curve
        .0
        .first()
        .map_or(persisted.starting_equity, |p| p.equity);
    let last_equity = curve
        .0
        .last()
        .map_or(persisted.starting_equity, |p| p.equity);
    println!(
        "equity_curve\tpoints={}\tfirst_equity={}\tlast_equity={}\tmax_drawdown={}",
        curve.0.len(),
        dec(first_equity),
        dec(last_equity),
        dec(curve.max_drawdown()),
    );

    // The persisted trade log, rendered through the SAME `render_trade_row` the
    // live backtest footer uses (D6 — one trade renderer).
    println!("{TRADE_HEADER}");
    for trade in &trades {
        println!("{}", render_trade_row(trade));
    }
    Ok(())
}

/// Render the run's input provenance as one stable tab-separated line
/// (r1.s3.w2, #110).
///
/// A run with no higher timeframe prints `htf=none`, which is a fact about the run
/// rather than a gap in the record. A row written before migration `0006` prints
/// `inputs unavailable (legacy run)` — the honest statement. It deliberately does
/// NOT print empty fields: a blank `data_version` reads like a value, and the whole
/// point of #110 is that a run either names the snapshot it used or admits it
/// cannot.
fn render_inputs(inputs: Option<&BacktestInputs>) -> String {
    let Some(i) = inputs else {
        return "inputs\tunavailable (legacy run, predates migration 0006)".to_owned();
    };
    let htf = i.htf.as_ref().map_or_else(
        || "htf=none".to_owned(),
        |htf| {
            format!(
                "htf={}\thtf_data_version={}",
                htf.timeframe.binance_interval(),
                htf.data_version
            )
        },
    );
    format!(
        "inputs\tpair={}\tprimary={}\tprimary_data_version={}\t{htf}\tfee_bps={}\tslippage_bps={}\tfunding={}",
        i.pair,
        i.primary.timeframe.binance_interval(),
        i.primary.data_version,
        dec(i.taker_fee_bps),
        dec(i.slippage_bps),
        funding_token(i.funding),
    )
}

/// The stable display token for the funding discriminant — the same
/// `snake_case` string the column stores, so the CLI and the database agree.
fn funding_token(funding: crate::domain::FundingConfig) -> &'static str {
    match funding {
        crate::domain::FundingConfig::SnapshotRates => "snapshot_rates",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{RunsArgs, RunsCommand};
    use crate::cli::{Cli, Command};
    use clap::Parser;

    /// Helper: parse a `pulse runs …` command line and return the `RunsArgs`.
    fn parse_runs(command_line: &[&str]) -> RunsArgs {
        let cli = Cli::try_parse_from(command_line).expect("parse runs command line");
        match cli.command {
            Command::Runs(runs_args) => runs_args,
            other => panic!("expected a runs command, got {other:?}"),
        }
    }

    #[test]
    fn parses_runs_list_with_version() {
        let args = parse_runs(&["pulse", "runs", "list", "--version", "vid-1"]);
        let RunsCommand::List { version } = args.command else {
            panic!("expected list");
        };
        assert_eq!(version, "vid-1");
    }

    #[test]
    fn parses_runs_show_with_positional_run() {
        let args = parse_runs(&["pulse", "runs", "show", "run-1"]);
        let RunsCommand::Show { run } = args.command else {
            panic!("expected show");
        };
        assert_eq!(run, "run-1");
    }

    #[test]
    fn parses_db_override_globally() {
        let args = parse_runs(&["pulse", "runs", "--db", "/tmp/x.db", "show", "run-9"]);
        assert_eq!(
            args.db.as_deref().map(std::path::Path::to_str),
            Some(Some("/tmp/x.db"))
        );
        let after = parse_runs(&["pulse", "runs", "show", "run-9", "--db", "/tmp/y.db"]);
        assert_eq!(
            after.db.as_deref().map(std::path::Path::to_str),
            Some(Some("/tmp/y.db")),
            "global --db parses AFTER the verb too"
        );
    }

    /// The provenance line is ONE tab-separated record per the doc contract on
    /// `render_inputs` — every field, including the HTF pair, is its own
    /// `\t`-delimited field. (The HTF branch shipped space-separated for a round;
    /// this pin is why it cannot again.)
    #[test]
    fn render_inputs_separates_every_field_with_a_tab() {
        use crate::domain::{
            BacktestInputs, DataVersion, FundingConfig, Pair, SnapshotSelection, Timeframe,
        };
        use rust_decimal::Decimal;

        let inputs = BacktestInputs {
            pair: Pair::new("BTCUSDT"),
            primary: SnapshotSelection {
                timeframe: Timeframe::M15,
                data_version: DataVersion::new("primarytag"),
            },
            htf: Some(SnapshotSelection {
                timeframe: Timeframe::H4,
                data_version: DataVersion::new("htftag"),
            }),
            taker_fee_bps: Decimal::new(5, 2),
            slippage_bps: Decimal::new(2, 2),
            funding: FundingConfig::SnapshotRates,
            window: None,
        };

        assert_eq!(
            super::render_inputs(Some(&inputs)),
            "inputs\tpair=BTCUSDT\tprimary=15m\tprimary_data_version=primarytag\
             \thtf=4h\thtf_data_version=htftag\tfee_bps=0.05\tslippage_bps=0.02\
             \tfunding=snapshot_rates"
        );

        // The htf=none contrast: same tab discipline, no second snapshot field.
        let mut single = inputs;
        single.htf = None;
        assert_eq!(
            super::render_inputs(Some(&single)),
            "inputs\tpair=BTCUSDT\tprimary=15m\tprimary_data_version=primarytag\thtf=none\
             \tfee_bps=0.05\tslippage_bps=0.02\tfunding=snapshot_rates"
        );
    }
}
