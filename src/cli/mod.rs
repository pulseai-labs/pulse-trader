//! CLI surface for `pulse` (WI-1.1.1.05): `clap` (derive) argument parsing +
//! the sync→async bridge + `fetch-data` dispatch.
//!
//! **Sync→async bridge (audit C3):** [`run`] is the thin **sync** entry the
//! binary shim calls. It parses args, builds a multi-thread `tokio` runtime, and
//! `block_on`s the async orchestration. There is **no** `#[tokio::main]`; `main`
//! stays the trivial shim mapping `Result` → `ExitCode`.
//!
//! **Output discipline (AC-4):** `--json` emits the grill-locked per-tf summary
//! schema; human mode prints a concise per-tf line. ANSI styling is suppressed
//! when `NO_COLOR` is set (or always, in this v1 — human output is plain).

pub(crate) mod backtest;
// r1.s2.w3: the coach composition root + the `pulse coach <run-id>` debug verb.
pub(crate) mod coach;
pub(crate) mod compose;
pub(crate) mod fetch_data;
pub(crate) mod indicators;
pub(crate) mod llm;
// r2.s1.w2: the `pulse mcp` composition root — validates `--agent-name`,
// resolves the data dir, opens the migrated DB and hands stdout to `mcp::serve`.
pub(crate) mod mcp;
pub(crate) mod runs;
pub(crate) mod strategy;

use clap::{Parser, Subcommand};

use crate::adapters::binance::BinanceDataSource;
use crate::adapters::clock::SystemClock;
use crate::adapters::db::{Db, default_db_path, open_migrated};
// r1.s3.w1 (#112): this module is ADR-0015's explicit composition-root exception —
// the ONE place that still names the concrete Parquet adapter. The use-case modules
// (`fetch_data`, `indicators`, `backtest`) depend on `CandleSeriesRepository` and are
// handed an implementation from here.
use crate::adapters::store::CandleStore;
use crate::domain::{CandleSeriesRepository, Pair, Timeframe};

use backtest::{BacktestArgs, run_backtest_cli};
use coach::{CoachArgs, run_coach};
use compose::{ComposeArgs, run_compose};
use fetch_data::{TfOutcome, TfSummary, ensure_one_tf};
use indicators::{IndicatorsArgs, run_indicators};
use llm::{LlmArgs, run_llm_check};
use mcp::{McpArgs, run_mcp};
use runs::{RunsArgs, run_runs};
use strategy::{StrategyArgs, run_strategy};

/// `pulse` — AI-orchestrated crypto-futures strategy development (v1 CLI `PoC`).
#[derive(Debug, Parser)]
#[command(
    name = "pulse",
    version,
    about,
    long_about = "pulse — AI-orchestrated crypto-futures strategy development (v1 CLI PoC).\n\nNot financial advice. See DISCLAIMER.md. Trading carries substantial risk of loss."
)]
pub struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    pub command: Command,
}

/// The `pulse` subcommands (v1 ships only `fetch-data`).
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Fetch + persist a versioned candle snapshot for a pair across timeframes.
    FetchData(FetchArgs),
    /// Render indicator values over a local candle snapshot.
    Indicators(IndicatorsArgs),
    /// Browse / create / clone / tag / pin / archive / compare strategies (FR-11).
    Strategy(StrategyArgs),
    /// Backtest a DSL strategy over a local candle snapshot + render the trade log (FR-5/FR-6).
    Backtest(BacktestArgs),
    /// List / show persisted backtest runs (FR-6 read verb, VS-1.2.4 work-4.05).
    Runs(RunsArgs),
    /// Round-trip a GLM chat through `PulseHive` + log a redacted `LlmCall`
    /// (the VS-1.3.1 composition-root demo, FR-23/FR-24/NFR-6).
    LlmCheck(LlmArgs),
    /// Compose a natural-language target into a persisted, attributable
    /// `StrategyVersion` via the composer (the VS-1.3.2 demo, FR-3/FR-4/NFR-6).
    Compose(ComposeArgs),
    /// Run ONE coach turn against a persisted backtest run and record it
    /// (r1.s2.w3). A developer/debug surface: it claims no user journey.
    Coach(CoachArgs),
    /// Serve MCP over stdio (r2.s1.w2): the seven read tools, the
    /// `pulse://dsl/schema` resource and file exports under the data dir.
    /// stdout is protocol traffic only — diagnostics go to stderr.
    Mcp(McpArgs),
}

/// `pulse fetch-data <PAIR> --tf <M15,H4> --years <N> [--json]`.
#[derive(Debug, clap::Args)]
pub struct FetchArgs {
    /// The trading pair symbol (e.g. `BTCUSDT`).
    pub pair: String,
    /// Comma-separated timeframes to fetch (one snapshot per tf), e.g. `M15,H4`.
    #[arg(long, value_delimiter = ',')]
    pub tf: Vec<String>,
    /// Years of history to fetch (floored to the start of the month N years back, UTC).
    #[arg(long)]
    pub years: u32,
    /// Emit the per-tf summary as JSON instead of human-readable text.
    #[arg(long)]
    pub json: bool,
}

/// The library entry point (audit C3): a thin **sync** shim that builds a
/// multi-thread `tokio` runtime and `block_on`s the async orchestration. The
/// binary's `main` calls this and maps the `Result` to an exit code.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on arg-parse failure, runtime-build failure, or
/// when **any** requested timeframe failed to fetch (audit C4 — non-zero exit).
pub fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("build tokio runtime: {e}"))?;
    runtime.block_on(dispatch(cli))
}

/// Async dispatch over the parsed CLI.
async fn dispatch(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::FetchData(args) => {
            let store = CandleStore::with_default_base_dir()
                .map_err(|e| anyhow::anyhow!("resolve data dir: {e}"))?;
            let source = BinanceDataSource::live(SystemClock)
                .map_err(|e| anyhow::anyhow!("build binance source: {e}"))?;
            run_fetch_data(&source, &store, &SystemClock, &args).await
        }
        // r1.s3.w1: the fixture/default root resolution moved here — `run_indicators`
        // now takes a repository, not a directory.
        Command::Indicators(args) => {
            let base_dir = match &args.base_dir {
                Some(path) => path.clone(),
                None => default_fixture_base_dir()?,
            };
            run_indicators(&CandleStore::with_base_dir(base_dir), &args)
        }
        Command::Strategy(args) => {
            let db = open_db(args.db.as_deref()).await?;
            run_strategy(&db, &args).await
        }
        // VS-1.2.4 work-4.05 (D2): `run_backtest_cli` is now async. The `--version`
        // persist/compare path needs a migrated `pulse.db` (opened via
        // `open_migrated`, migrate-then-open — mirroring the `Strategy` arm); the
        // `--dsl` path is persistence-free, so the db is opened ONLY when
        // `--version` is set (a `--dsl` run must NOT create/migrate a real
        // Application Support `pulse.db` — it stays verbatim, README C7).
        Command::Backtest(args) => {
            let db = if args.version.is_some() {
                Some(open_db(args.db.as_deref()).await?)
            } else {
                None
            };
            // r1.s3.w1: the `--store` / default-root choice is composition, so it
            // lives here; `run_backtest_cli` only sees the port.
            let repo = match &args.store {
                Some(dir) => CandleStore::with_base_dir(dir.clone()),
                None => CandleStore::with_default_base_dir()
                    .map_err(|e| anyhow::anyhow!("resolve default candle-store dir: {e}"))?,
            };
            run_backtest_cli(db.as_ref(), &repo, &args).await
        }
        // VS-1.2.4 work-4.05 (D6): the `runs list/show` read verb always opens the
        // db via `open_migrated` (same migrate-then-open as the Strategy arm).
        Command::Runs(args) => {
            let db = open_db(args.db.as_deref()).await?;
            run_runs(&db, &args).await
        }
        // VS-1.3.1 work-1.05: the composition-root demo verb. The ledger write needs
        // a migrated `pulse.db` (opened via `open_migrated`, migrate-then-open —
        // mirroring the `Runs`/`Strategy` arms) so the redacted `LlmCall` persists.
        Command::LlmCheck(args) => {
            let db = open_db(args.db.as_deref()).await?;
            run_llm_check(Some(&db), &args).await
        }
        // VS-1.3.2 work-2.05: the composer composition-root demo verb. The ledger +
        // version writes need a migrated `pulse.db` (opened via `open_migrated`,
        // migrate-then-open — mirroring the `LlmCheck`/`Runs`/`Strategy` arms).
        Command::Compose(args) => {
            let db = open_db(args.db.as_deref()).await?;
            run_compose(Some(&db), &args).await
        }
        // r1.s2.w3: the coach debug verb. The session + ledger writes need a
        // migrated `pulse.db` (migrate-then-open, mirroring every other arm).
        Command::Coach(args) => {
            let db = open_db(args.db.as_deref()).await?;
            run_coach(Some(&db), &args).await
        }
        // r2.s1.w2: the MCP server arm. `run_mcp` validates `--agent-name`
        // FIRST (AC-10: invalid → non-zero before serving), then opens the
        // migrated db + resolves the data dir inside.
        Command::Mcp(args) => run_mcp(&args).await,
    }
}

/// Resolve the `pulse.db` path (the `--db` override or `default_db_path()`) then
/// open the pool via 1.04's `open_migrated` (migrate-then-open).
///
/// Gate-7 C3 startup wiring (§4a-3): on a migration failure this REFUSES TO START
/// (`DataError::Migration` → `anyhow` → non-zero exit, MASTER-SPEC §7.4). The pure
/// openers (`Db::with_path`/`open_default`) do NOT migrate and are deliberately
/// NOT used here. Shared by the `Strategy`, `Backtest --version`, and `Runs` arms.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] if the default db path cannot be resolved or the
/// migrate-then-open fails.
async fn open_db(db_override: Option<&std::path::Path>) -> anyhow::Result<Db> {
    let path = match db_override {
        Some(p) => p.to_path_buf(),
        None => default_db_path().map_err(|e| anyhow::anyhow!("resolve db path: {e}"))?,
    };
    open_migrated(&path)
        .await
        .map_err(|e| anyhow::anyhow!("open db: {e}"))
}

/// Orchestrate `fetch-data` over the injected ports (NFR-9 / AC-6 — this fn names
/// only the `MarketDataSource`, `Clock` and `CandleSeriesRepository` bounds, never
/// a concrete adapter). Each tf is fetched **independently**; a failing tf is
/// reported and the process exits non-zero, while successful tfs remain
/// (audit C4 / AC-8).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] iff at least one tf failed (after all tfs have
/// been attempted + reported).
pub async fn run_fetch_data<S, C, R>(
    source: &S,
    repo: &R,
    clock: &C,
    args: &FetchArgs,
) -> anyhow::Result<()>
where
    S: crate::domain::MarketDataSource,
    C: crate::domain::Clock,
    R: CandleSeriesRepository,
{
    // Validate the untrusted CLI symbol before it can reach the store-path layer
    // (it is joined verbatim into `<base>/candles/<PAIR>/…`; an unvalidated
    // `../`, `/abs`, or `a/b` would escape/relocate the store root).
    let pair = Pair::parse(args.pair.clone())
        .map_err(|e| anyhow::anyhow!("invalid pair argument: {e}"))?;
    validate_years(args.years)?;
    let timeframes = parse_timeframes(&args.tf)?;

    let mut summaries: Vec<TfSummary> = Vec::new();
    let mut failures: Vec<(String, String)> = Vec::new();

    for tf in timeframes {
        match ensure_one_tf(source, repo, clock, &pair, tf, args.years).await {
            TfOutcome::Ok(summary) => summaries.push(summary),
            TfOutcome::Failed { timeframe, error } => failures.push((timeframe, error)),
        }
    }

    render(&summaries, &failures, args.json);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "{} timeframe(s) failed: {}",
            failures.len(),
            failures
                .iter()
                .map(|(tf, _)| tf.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// The largest accepted `--years` window. Binance USD-M Futures launched in
/// 2019, so any value beyond this is unrealistic; capping it keeps the
/// year-arithmetic in [`fetch_data::years_window_start_ms`] well-defined (an
/// extreme `n_years` would underflow `target_year` and silently degrade to a
/// current-month-only window).
const MAX_YEARS: u32 = 50;

/// Validate the `--years` argument is within the accepted range so an
/// unrealistic value errors predictably rather than silently producing a
/// current-month-only window.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the offending value + accepted range when
/// `years > MAX_YEARS`.
fn validate_years(years: u32) -> anyhow::Result<()> {
    if years > MAX_YEARS {
        anyhow::bail!("--years {years} is out of range (accepted: 0..={MAX_YEARS})");
    }
    Ok(())
}

/// Parse the comma-separated `--tf` values into [`Timeframe`]s.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on an unknown timeframe token or an empty list.
fn parse_timeframes(raw: &[String]) -> anyhow::Result<Vec<Timeframe>> {
    if raw.is_empty() {
        anyhow::bail!("--tf requires at least one timeframe (e.g. M15,H4)");
    }
    // Dedupe (order-preserving) so `--tf M15,M15` doesn't fetch the same
    // snapshot twice (CodeRabbit).
    let mut out: Vec<Timeframe> = Vec::new();
    for t in raw {
        let tf = parse_one_tf(t)?;
        if !out.contains(&tf) {
            out.push(tf);
        }
    }
    Ok(out)
}

/// The default `pulse indicators` candle root: the committed `BTCUSDT` fixture
/// store under the current working directory (r1.s3.w1 — moved here from
/// `indicators`, which no longer resolves roots or constructs adapters).
fn default_fixture_base_dir() -> anyhow::Result<std::path::PathBuf> {
    Ok(std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("resolve current directory for default fixture: {e}"))?
        .join("tests/fixtures/btcusdt-1m-store"))
}

/// Parse one timeframe token (case-insensitive: `M15`/`15m`, `H4`/`4h`).
pub(crate) fn parse_one_tf(token: &str) -> anyhow::Result<Timeframe> {
    match token.trim().to_ascii_uppercase().as_str() {
        "M15" | "15M" => Ok(Timeframe::M15),
        "H4" | "4H" => Ok(Timeframe::H4),
        other => anyhow::bail!("unknown timeframe '{other}' (expected M15 or H4)"),
    }
}

/// Render the per-tf outcomes. `--json` emits one JSON object per line (the
/// grill-locked schema); human mode prints a plain per-tf line. ANSI styling is
/// suppressed under `NO_COLOR` (AC-4); v1 human output is plain text either way,
/// and `--json` never carries ANSI, so neither path emits escape codes here.
fn render(summaries: &[TfSummary], failures: &[(String, String)], json: bool) {
    if json {
        for summary in summaries {
            // Infallible in practice: TfSummary is a flat struct of owned
            // primitives. A defensive `if let` keeps the no-panic invariant.
            if let Ok(line) = serde_json::to_string(summary) {
                println!("{line}");
            }
        }
        for (tf, error) in failures {
            let entry = serde_json::json!({
                "timeframe": tf,
                "action": "error",
                "error": error,
            });
            println!("{entry}");
        }
    } else {
        let bold = ansi_emphasis();
        let reset = if bold.is_empty() { "" } else { "\x1b[0m" };
        for summary in summaries {
            println!(
                "{bold}{} {}{reset}: {} ({} candles, {} gaps) -> {}",
                summary.pair,
                summary.timeframe,
                summary.action,
                summary.candle_count,
                summary.gap_count,
                summary.data_version,
            );
        }
        for (tf, error) in failures {
            eprintln!("{tf}: ERROR {error}");
        }
    }
}

/// The ANSI emphasis prefix for human-mode tf headers, or an empty string when
/// `NO_COLOR` suppresses styling (AC-4).
fn ansi_emphasis() -> &'static str {
    if color_enabled() { "\x1b[1m" } else { "" }
}

/// Whether ANSI color output is enabled: disabled when `NO_COLOR` is set in the
/// environment (the de-facto `NO_COLOR` convention, AC-4).
fn color_enabled() -> bool {
    std::env::var_os("NO_COLOR").is_none()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Cli, MAX_YEARS, parse_one_tf, parse_timeframes, validate_years};
    use crate::domain::Timeframe;
    use clap::Parser;

    // ---- Fix 5: --years range guard (CodeRabbit Minor) ---------------------

    #[test]
    fn validate_years_accepts_a_sane_window() {
        assert!(validate_years(2).is_ok(), "2 years is a normal request");
        assert!(
            validate_years(0).is_ok(),
            "0 (current month only) is allowed"
        );
        assert!(
            validate_years(MAX_YEARS).is_ok(),
            "the cap itself is allowed"
        );
    }

    #[test]
    fn validate_years_rejects_an_unrealistic_window() {
        // An extreme value must error predictably (naming the value + range),
        // NOT silently degrade to a current-month-only window.
        let err = validate_years(u32::MAX).expect_err("u32::MAX years must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains(&u32::MAX.to_string()),
            "error names the offending value: {msg}"
        );
        assert!(
            msg.contains(&MAX_YEARS.to_string()),
            "error names the accepted upper bound: {msg}"
        );
        // Just over the cap is also rejected.
        assert!(validate_years(MAX_YEARS + 1).is_err());
    }

    // ---- clap parsing ------------------------------------------------------

    #[test]
    fn parses_fetch_data_with_comma_separated_tfs() {
        let cli = Cli::try_parse_from([
            "pulse",
            "fetch-data",
            "BTCUSDT",
            "--tf",
            "M15,H4",
            "--years",
            "1",
        ])
        .expect("parse");
        let super::Command::FetchData(args) = cli.command else {
            panic!("expected fetch-data command");
        };
        assert_eq!(args.pair, "BTCUSDT");
        assert_eq!(args.tf, vec!["M15".to_string(), "H4".to_string()]);
        assert_eq!(args.years, 1);
        assert!(!args.json);
    }

    #[test]
    fn parses_json_flag() {
        let cli = Cli::try_parse_from([
            "pulse",
            "fetch-data",
            "BTCUSDT",
            "--tf",
            "M15",
            "--years",
            "2",
            "--json",
        ])
        .expect("parse");
        let super::Command::FetchData(args) = cli.command else {
            panic!("expected fetch-data command");
        };
        assert!(args.json);
    }

    // ---- timeframe parsing -------------------------------------------------

    #[test]
    fn parses_tf_tokens_case_insensitively() {
        assert_eq!(parse_one_tf("M15").unwrap(), Timeframe::M15);
        assert_eq!(parse_one_tf("15m").unwrap(), Timeframe::M15);
        assert_eq!(parse_one_tf("H4").unwrap(), Timeframe::H4);
        assert_eq!(parse_one_tf("4h").unwrap(), Timeframe::H4);
    }

    #[test]
    fn rejects_unknown_timeframe() {
        assert!(parse_one_tf("D1").is_err());
        assert!(parse_timeframes(&[]).is_err());
    }

    // ---- AC-4: NO_COLOR suppresses color ----------------------------------

    #[test]
    fn no_color_env_disables_color() {
        // SAFETY: single-threaded test; we set+restore the var around the check.
        // The function reads NO_COLOR once; this proves the suppression branch.
        let prev = std::env::var_os("NO_COLOR");
        unsafe {
            std::env::set_var("NO_COLOR", "1");
        }
        assert!(!super::color_enabled(), "NO_COLOR set ⇒ color disabled");
        unsafe {
            match prev {
                Some(v) => std::env::set_var("NO_COLOR", v),
                None => std::env::remove_var("NO_COLOR"),
            }
        }
    }
}
