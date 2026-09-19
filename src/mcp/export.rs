//! Filesystem exports for `pulse mcp` (r2.s1.w2).
//!
//! Every export lands under `<app data dir>/exports/<pid>-<start-unix-ms>/`,
//! created once at startup with mode `0700`, and is named
//! `<tool>-<id-or-pair-tf>-<seq>.csv|parquet`. The agent never supplies a path;
//! nothing is ever deleted.
//!
//! Rows are tab-separated — the same delimiter `pulse indicators` already
//! emits — so the `fills` JSON cell in the trades export needs no quoting.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::application::mcp_read::IndicatorColumn;
use crate::domain::{Candle, DataError, Trade};
use rust_decimal::Decimal;

/// The per-process exports directory: `<data_dir>/exports/<pid>-<start>/`.
///
/// `seq` makes each export's filename unique and deterministic within the
/// process lifetime.
#[derive(Debug)]
pub(crate) struct Exports {
    dir: PathBuf,
    seq: AtomicU64,
}

impl Exports {
    /// Create `<data_dir>/exports/<pid>-<start-unix-ms>/` with mode `0700` and
    /// return the handle. The directory is never cleaned up — exports are
    /// durable artifacts (the spec's "nothing is deleted").
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Io`] if the directory cannot be created,
    /// canonicalized, or the `0700` mode cannot be applied.
    pub(crate) fn create(data_dir: &Path) -> Result<Exports, DataError> {
        let start_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis());
        let dir = data_dir
            .join("exports")
            .join(format!("{}-{start_ms}", std::process::id()));
        fs::create_dir_all(&dir).map_err(|e| {
            DataError::Io(format!(
                "could not create exports dir {}: {e}",
                dir.display()
            ))
        })?;
        // Canonicalize AFTER create so the returned path is always absolute —
        // a relative `--data-dir` must never leak a relative export path.
        let dir = dir
            .canonicalize()
            .map_err(|e| DataError::Io(format!("could not canonicalize {}: {e}", dir.display())))?;
        set_owner_only(&dir)?;
        Ok(Exports {
            dir,
            seq: AtomicU64::new(0),
        })
    }

    /// Allocate the next export path: `<dir>/<tool>-<subject>-<seq>.<ext>`.
    /// `subject` is pre-sanitized by the caller (run/version ids, `PAIR-TF`).
    fn next_path(&self, tool: &str, subject: &str, ext: &str) -> PathBuf {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.dir.join(format!("{tool}-{subject}-{seq}.{ext}"))
    }

    /// Write `contents` to the next sequence path and return its absolute path.
    fn write(
        &self,
        tool: &str,
        subject: &str,
        ext: &str,
        contents: &[u8],
    ) -> Result<PathBuf, DataError> {
        let path = self.next_path(tool, subject, ext);
        fs::write(&path, contents).map_err(|e| {
            DataError::Io(format!("could not write export {}: {e}", path.display()))
        })?;
        set_owner_only(&path)?;
        Ok(path)
    }

    /// Write a `.csv` text export.
    pub(crate) fn write_csv(
        &self,
        tool: &str,
        subject: &str,
        csv: &str,
    ) -> Result<PathBuf, DataError> {
        self.write(tool, subject, "csv", csv.as_bytes())
    }

    /// Write a `.parquet` export — a byte copy of the snapshot file, per spec.
    pub(crate) fn write_parquet_copy(
        &self,
        tool: &str,
        subject: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, DataError> {
        self.write(tool, subject, "parquet", bytes)
    }
}

/// Render the trades CSV: one header naming every [`Trade`] field, one row per
/// trade in `seq` order. `fills` (the only non-scalar field) is a JSON cell;
/// `stop_price` (the only nullable one) is an empty cell for pre-0011 rows.
pub(crate) fn trades_csv(trades: &[Trade]) -> String {
    const HEADER: &str = "direction\tqty\tentry_price\texit_price\tentry_signal_time\tentry_fill_time\texit_signal_time\texit_fill_time\tfills\tfees_total\tfunding_total\tslippage_total\trealized_pnl\trealized_r\tmfe_r\tmae_r\texit_reason\tsource\tregime\tstop_price";
    let mut out = String::with_capacity(trades.len() * 192 + HEADER.len() + 1);
    out.push_str(HEADER);
    out.push('\n');
    for t in trades {
        let fills = serde_json::to_string(&t.fills).unwrap_or_else(|_| "[]".to_owned());
        // A String's fmt::Write impl cannot fail.
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            json_scalar(&t.direction),
            t.qty.normalize(),
            t.entry_price.normalize(),
            t.exit_price.normalize(),
            t.entry_signal_time,
            t.entry_fill_time,
            t.exit_signal_time,
            t.exit_fill_time,
            fills,
            t.fees_total.normalize(),
            t.funding_total.normalize(),
            t.slippage_total.normalize(),
            t.realized_pnl.normalize(),
            t.realized_r.normalize(),
            t.mfe_r.normalize(),
            t.mae_r.normalize(),
            json_scalar(&t.exit_reason),
            json_scalar(&t.source),
            json_scalar(&t.regime),
            t.stop_price
                .map_or_else(String::new, |s| s.normalize().to_string()),
        );
    }
    out
}

/// Render the candles CSV: one header naming every [`Candle`] field, one row
/// per candle in series order.
pub(crate) fn candles_csv(candles: &[Candle]) -> String {
    const HEADER: &str = "open_time\tclose_time\topen\thigh\tlow\tclose\tvolume\tfunding_rate";
    let mut out = String::with_capacity(candles.len() * 96 + HEADER.len() + 1);
    out.push_str(HEADER);
    out.push('\n');
    for c in candles {
        // A String's fmt::Write impl cannot fail.
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            c.open_time,
            c.close_time,
            c.open.normalize(),
            c.high.normalize(),
            c.low.normalize(),
            c.close.normalize(),
            c.volume.normalize(),
            c.funding_rate
                .map_or_else(String::new, |r| r.normalize().to_string()),
        );
    }
    out
}

/// Render the indicators CSV: `open_time` + one column per
/// [`IndicatorColumn`] label, blank while the engine warms — the row shape
/// `pulse indicators` already prints (without its summary row).
pub(crate) fn indicators_csv(
    candles: &[Candle],
    columns: &[IndicatorColumn],
    rows: &[Vec<Option<Decimal>>],
) -> String {
    let mut out = String::new();
    out.push_str("open_time");
    for column in columns {
        out.push('\t');
        out.push_str(&column.label);
    }
    out.push('\n');
    for (candle, values) in candles.iter().zip(rows) {
        out.push_str(&candle.open_time.to_string());
        for value in values {
            out.push('\t');
            if let Some(v) = value {
                out.push_str(&v.normalize().to_string());
            }
        }
        out.push('\n');
    }
    out
}

/// Serialize a unit-enum-ish value to its bare serde scalar text (`"long"` →
/// `long`) for a cell. `Debug` is the unreachable non-string fallback — every
/// type this is called with serializes to a JSON string.
fn json_scalar<T: serde::Serialize + std::fmt::Debug>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{value:?}"),
    }
}

/// `0700` on the exports dir, `0600` on each file — least privilege, the same
/// posture as the credential gate. Unix-only; on other platforms this is a
/// no-op (desktop parity is `w4`'s concern).
#[cfg(unix)]
fn set_owner_only(path: &Path) -> Result<(), DataError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if path.is_dir() { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|e| DataError::Io(format!("could not chmod {mode:o} {}: {e}", path.display())))
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) -> Result<(), DataError> {
    Ok(())
}
