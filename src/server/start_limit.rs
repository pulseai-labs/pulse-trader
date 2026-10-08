//! The `pulse serve` start counter (r4.s2.w1, G5).
//!
//! launchd has no start limit: `KeepAlive` relaunches forever, throttled only by
//! `ThrottleInterval`. The bound therefore lives in the server, on the same
//! arithmetic #342 gave the systemd unit — 3 starts in 900 s — and rides two
//! files in the data dir:
//!
//! - **`<data dir>/serve-starts`** — one UNIX-second start time per line, pruned
//!   to the window on every write. The file is written through a temporary file
//!   plus rename, so a crash mid-write cannot leave a SHORTER log (which would
//!   weaken the bound silently).
//! - **`<data dir>/serve-start-limit`** — the marker written when a start is
//!   refused, holding the UTC time and the count. `KeepAlive { SuccessfulExit =
//!   false }` makes the EXIT CODE the decision: a refused start exits 0, so
//!   launchd stops relaunching, and the service stays down until
//!   `just prod-reset` removes the marker and the log and kickstarts the agent.
//!
//! The window is the sliding one systemd applies to `StartLimitIntervalSec`: an
//! old start counts while `now - started < window`, so a start exactly one
//! window after an earlier one does not count it.
//!
//! The clock is injected ([`record_start`]'s `now_unix_secs`), so the whole
//! window arithmetic is testable without waiting;
//! `tests/launchd_units.rs` drives it and drives the real binary's exit 0.
//!
//! **Failure policy.** An IO failure is *fail-open*: `serve` logs one named line
//! and starts anyway. Refusing to serve because a counter file could not be
//! written would not stop the relaunch loop either — a non-zero exit relaunches
//! under `KeepAlive { SuccessfulExit = false }` — and it would take a healthy
//! server down for a scratch file. The one thing an IO failure never does is
//! undo a reached DECISION: when the count is over the limit, the start is
//! refused even if its marker could not be written (`marker_written: false`).
//!
//! Nothing here is Linux-only and nothing here binds, so the whole module runs
//! on the Mini as it does on draco-desk.

use std::fmt::Write as _;
use std::path::Path;
use std::str::FromStr;

/// The start log's file name, under the data dir.
pub const START_LOG_NAME: &str = "serve-starts";

/// The refusal marker's file name, under the data dir.
pub const START_MARKER_NAME: &str = "serve-start-limit";

/// `--start-limit <N>/<SECONDS>` — how many starts are allowed inside the
/// window.
///
/// Parity with `deploy/pulse-serve.service`'s `StartLimitBurst` /
/// `StartLimitIntervalSec` (#342), which systemd enforces itself (the unit
/// passes no such flag). launchd has no equivalent, so there the server counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartLimit {
    /// The number of starts allowed inside the window.
    pub starts: u64,
    /// The sliding window, in seconds.
    pub window_secs: u64,
}

/// Why a `--start-limit` value did not parse.
#[derive(Debug, thiserror::Error)]
pub enum StartLimitParseError {
    /// The value is not `<N>/<SECONDS>`.
    #[error("expected <N>/<SECONDS> (e.g. 3/900), got '{0}'")]
    Shape(String),
    /// One side is not a positive whole number.
    #[error("both sides must be positive whole numbers, got '{0}'")]
    Number(String),
}

impl FromStr for StartLimit {
    type Err = StartLimitParseError;

    /// Parse `3/900`. Both sides must be positive: `0/900` is not "off", it is
    /// a typo, and a refusal is louder than a silently unbounded service.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (starts, window) = value
            .split_once('/')
            .ok_or_else(|| StartLimitParseError::Shape(value.to_owned()))?;
        let starts =
            positive(starts).ok_or_else(|| StartLimitParseError::Number(value.to_owned()))?;
        let window_secs =
            positive(window).ok_or_else(|| StartLimitParseError::Number(value.to_owned()))?;
        Ok(Self {
            starts,
            window_secs,
        })
    }
}

fn positive(part: &str) -> Option<u64> {
    let value: u64 = part.trim().parse().ok()?;
    (value > 0).then_some(value)
}

/// What [`record_start`] decided about this `pulse serve` start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartGuard {
    /// The start was recorded inside the window; the server proceeds.
    Proceed,
    /// The start is refused: `pulse serve` prints a named message and exits 0.
    /// `count` is the number of starts inside the window (the marker's own
    /// recorded count when the marker pre-existed). `marker_written` is `false`
    /// only when a tripped limit could not record its marker — the refusal
    /// stands anyway.
    Limited {
        /// Starts inside the window, this one included.
        count: u64,
        /// Whether `<data dir>/serve-start-limit` holds the trip.
        marker_written: bool,
    },
}

/// Record one `pulse serve` start and decide whether it may run.
///
/// The order is the operator's recovery order: a start with the marker already
/// present is refused WITHOUT appending to the log, so `just prod-reset`'s
/// removal of both files is what re-arms the agent.
///
/// # Errors
///
/// [`std::io::Error`] when the data dir cannot be read or written. The caller's
/// policy is fail-open (log one line, start anyway): see the module docs.
pub fn record_start(
    data_dir: &Path,
    limit: StartLimit,
    now_unix_secs: u64,
) -> std::io::Result<StartGuard> {
    let marker = data_dir.join(START_MARKER_NAME);
    if marker.exists() {
        return Ok(StartGuard::Limited {
            count: read_marker_count(&marker).unwrap_or(0),
            marker_written: true,
        });
    }

    let log = data_dir.join(START_LOG_NAME);
    let mut entries = read_entries(&log)?;
    entries.retain(|started| now_unix_secs.saturating_sub(*started) < limit.window_secs);
    entries.push(now_unix_secs);
    let count = u64::try_from(entries.len()).unwrap_or(u64::MAX);
    write_entries(&log, &entries)?;

    if count > limit.starts {
        let marker_written = write_marker(&marker, now_unix_secs, count, limit).is_ok();
        return Ok(StartGuard::Limited {
            count,
            marker_written,
        });
    }
    Ok(StartGuard::Proceed)
}

/// The wall clock [`record_start`] is called with in production: UNIX seconds.
/// A clock before the epoch reads as 0.
#[must_use]
pub fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The starts recorded in the log, oldest first. A missing file is empty; a
/// torn or hand-edited line is skipped rather than fatal — this is a bound, not
/// a ledger.
fn read_entries(path: &Path) -> std::io::Result<Vec<u64>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .filter_map(|line| line.trim().parse::<u64>().ok())
            .collect()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err),
    }
}

fn write_entries(path: &Path, entries: &[u64]) -> std::io::Result<()> {
    let text = entries.iter().fold(String::new(), |mut text, started| {
        // Writing into a `String` cannot fail.
        let _ = writeln!(text, "{started}");
        text
    });
    // Write-then-rename: the log is either the old one or the new one.
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, text)?;
    set_private_mode(&temp)?;
    std::fs::rename(&temp, path)
}

/// Write the refusal marker: the UTC time, the UNIX time, the count and the
/// window, one `key=value` per line — greppable from a forced-command check
/// (w4) and readable by a human.
fn write_marker(
    path: &Path,
    now_unix_secs: u64,
    count: u64,
    limit: StartLimit,
) -> std::io::Result<()> {
    let utc = chrono::DateTime::<chrono::Utc>::from_timestamp(epoch_secs_i64(now_unix_secs), 0)
        .map_or_else(
            || format!("unix {now_unix_secs}"),
            |at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
    let text = format!(
        "utc={utc}\nunix={now_unix_secs}\nstarts={count}\nwindow_seconds={}\nlimit={}\n",
        limit.window_secs, limit.starts
    );
    std::fs::write(path, text)?;
    set_private_mode(path)
}

/// The marker's recorded count, if it carries one — used only to make the
/// refusal message specific. A marker without one still refuses the start.
fn read_marker_count(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .find_map(|line| line.trim().strip_prefix("starts=")?.parse::<u64>().ok())
}

fn epoch_secs_i64(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

/// Both files live under the data dir, which on the Mini is mode 0700; 0600 is
/// the same discipline one level down, so no other local user reads the counts
/// even if the directory's mode is loosened.
#[cfg(unix)]
fn set_private_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}
