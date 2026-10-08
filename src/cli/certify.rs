//! `pulse certify` — the operator's local freeze administration (r4.s1.w4, F1/C4).
//!
//! `pulse certify freeze --holdout-start <YYYY-MM-DD> --h <N> --alpha <decimal>
//! --test <name>` opens ONE immutable freeze record: the holdout start (the
//! given UTC date, midnight), the hypothesis budget H, the measured alpha and
//! the C1 holdout test's name and z rule. It refuses when a freeze is already
//! open, when the holdout start is not later than the last close (F1: a spent
//! holdout is never reused), and when H is outside `1..=12`.
//!
//! `pulse certify close-freeze` writes `closed_at_ms` once on the open freeze —
//! the guard lifts, and the holdout is spent for good. `pulse certify status`
//! prints the open freeze or `no open freeze`.
//!
//! These are local administration on the server host, like `pulse token`:
//! operator-only, and never reachable through MCP or the server API. The record
//! is the freeze the campaign's guard reads, so the command prints it back in
//! full on stdout (the operator's one durable account of it); errors go to
//! stderr through `main`'s non-zero exit.

use std::path::PathBuf;
use std::str::FromStr;

use chrono::{Datelike as _, SecondsFormat, TimeZone, Utc};
use clap::Subcommand;
use rust_decimal::Decimal;

use crate::adapters::db::SqliteCertificationFreezeRepo;
use crate::domain::{FreezeRecord, OpenFreezeRequest};

/// `pulse certify {freeze,close-freeze,status}` — the freeze lifecycle.
#[derive(Debug, Subcommand)]
pub enum CertifyAction {
    /// Open a freeze: record the holdout start, H, alpha and the C1 test.
    Freeze {
        /// The holdout's start, `YYYY-MM-DD` (UTC, floored to midnight).
        #[arg(long, value_name = "YYYY-MM-DD")]
        holdout_start: String,
        /// The hypothesis budget H (`1..=12`; lower-only, never raised).
        #[arg(long)]
        h: u8,
        /// The measured alpha as a decimal (stored normalized, Decimal-as-TEXT).
        #[arg(long)]
        alpha: String,
        /// The C1 holdout test's name and z rule, verbatim.
        #[arg(long)]
        test: String,
        /// Path to `pulse.db`. Defaults to the platform data path.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Close the open freeze, once, at the campaign's end.
    CloseFreeze {
        /// Path to `pulse.db`. Defaults to the platform data path.
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Print the open freeze, or `no open freeze`.
    Status {
        /// Path to `pulse.db`. Defaults to the platform data path.
        #[arg(long)]
        db: Option<PathBuf>,
    },
}

/// `pulse certify ...` argument root.
#[derive(Debug, clap::Args)]
pub struct CertifyArgs {
    /// The subcommand to run.
    #[command(subcommand)]
    pub action: CertifyAction,
}

/// Run one `pulse certify` action against the migrated database.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] — printed to stderr by `main` with a non-zero
/// exit — when the date/alpha arguments do not parse, the DB fails
/// migrate-then-open, or the store refuses (a freeze is open, the holdout start
/// is not after the last close, H is out of range, no freeze is open).
pub(crate) async fn run_certify(args: &CertifyArgs) -> anyhow::Result<()> {
    match &args.action {
        CertifyAction::Freeze {
            holdout_start,
            h,
            alpha,
            test,
            db,
        } => {
            let holdout_start_ms = holdout_start_ms(holdout_start)?;
            let alpha = Decimal::from_str(alpha.trim())
                .map_err(|e| anyhow::anyhow!("invalid --alpha {alpha:?}: {e}"))?
                .normalize()
                .to_string();
            let holdout_test = test.trim().to_owned();
            if holdout_test.is_empty() {
                anyhow::bail!("--test must not be empty: name the C1 test and its z rule");
            }
            let db_handle = super::open_db(db.as_deref()).await?;
            let repo = SqliteCertificationFreezeRepo::new(db_handle.pool().clone());
            let record = repo
                .open(&OpenFreezeRequest {
                    holdout_start_ms,
                    h: *h,
                    alpha,
                    holdout_test,
                })
                .await
                .map_err(|e| anyhow::anyhow!("certify freeze refused: {e}"))?;
            print_record(&record);
        }
        CertifyAction::CloseFreeze { db } => {
            let db_handle = super::open_db(db.as_deref()).await?;
            let repo = SqliteCertificationFreezeRepo::new(db_handle.pool().clone());
            let record = repo
                .close()
                .await
                .map_err(|e| anyhow::anyhow!("certify close-freeze refused: {e}"))?;
            print_record(&record);
        }
        CertifyAction::Status { db } => {
            let db_handle = super::open_db(db.as_deref()).await?;
            let repo = SqliteCertificationFreezeRepo::new(db_handle.pool().clone());
            match repo
                .open_freeze()
                .await
                .map_err(|e| anyhow::anyhow!("certify status failed: {e}"))?
            {
                Some(record) => print_record(&record),
                None => println!("no open freeze"),
            }
        }
    }
    Ok(())
}

/// Parse a `--holdout-start <YYYY-MM-DD>` UTC date into its midnight epoch ms.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the rejected value when it is not a
/// `YYYY-MM-DD` calendar date (or the instant does not exist).
fn holdout_start_ms(raw: &str) -> anyhow::Result<i64> {
    let date = chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").map_err(|e| {
        anyhow::anyhow!(
            "invalid --holdout-start {raw:?}: {e} (expected YYYY-MM-DD, a UTC calendar date)"
        )
    })?;
    Utc.with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .map(|dt| dt.timestamp_millis())
        .ok_or_else(|| anyhow::anyhow!("invalid --holdout-start {raw:?}: no such UTC midnight"))
}

/// Print the record in full — one field per line, timestamps as RFC 3339 UTC
/// with the raw epoch ms beside them.
fn print_record(record: &FreezeRecord) {
    println!("freeze id: {}", record.id);
    println!("holdout_start: {}", rfc3339(record.holdout_start_ms));
    println!("h: {}", record.h);
    println!("alpha: {}", record.alpha);
    println!("holdout_test: {}", record.holdout_test);
    println!("opened_at: {}", rfc3339(record.opened_at_ms));
    match record.closed_at_ms {
        Some(ms) => println!("closed_at: {}", rfc3339(ms)),
        None => println!("closed_at: -"),
    }
}

/// An epoch-ms instant as RFC 3339 UTC seconds, with the raw ms appended.
fn rfc3339(ms: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(dt) => format!(
            "{} ({ms} ms)",
            dt.to_rfc3339_opts(SecondsFormat::Secs, true)
        ),
        None => format!("{ms} ms"),
    }
}
