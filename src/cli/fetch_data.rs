//! `pulse fetch-data` orchestration (WI-1.1.1.05).
//!
//! Composes the [`MarketDataSource`] port (a [`BinanceDataSource`](crate::adapters::binance::BinanceDataSource)
//! in production) + the [`CandleSeriesRepository`] port (the Parquet adapter in
//! production) into the slice's user-facing seam. Since r1.s3.w1 (#112) the
//! orchestration depends on **two ports and no concrete type** (NFR-9 / AC-6);
//! `src/cli/mod.rs` is where an implementation is chosen.
//!
//! Per-(pair, tf) flow (grill + audit-locked, spec §3):
//! - **First run** (no `HEAD`): bulk over the `--years N` window
//!   ([`MarketDataSource::fetch_historical`]) **then** an immediate REST top-up
//!   to the clock cutoff ([`MarketDataSource::fetch_incremental`]) so the first
//!   snapshot is current. Commit the result. Action `bulk`.
//! - **Subsequent run** (`HEAD` present): read the prior snapshot, top up only
//!   newly-closed candles. If any closed → commit a new version (action
//!   `incremental`); if nothing newly closed → **`up-to-date` no-op**, not an
//!   error.
//! - **Ordering + crash-safety (audit C1):** snapshot-then-`HEAD` ordering is the
//!   repository's guarantee ([`CandleSeriesRepository::commit`]), not something
//!   this module sequences any more. A crash between the two still leaves a valid
//!   orphaned snapshot and an unchanged `HEAD`.
//! - **`--years N` window (audit C5):** start = floor to the first day of the
//!   month `N` years before the current UTC month.
//! - **Multi-tf (audit C4):** each tf is fetched independently; a failing tf is
//!   reported in its summary and the process exits non-zero, while successful
//!   tfs remain.

use chrono::{Datelike, TimeZone, Utc};
use serde::Serialize;

use crate::domain::{
    CandleSeriesRepository, Clock, DataError, MarketDataSource, Pair, StoredCandleSeries, Timeframe,
};

/// The action taken for one `(pair, tf)` this run (the `--json` `action` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// First run: bulk window + immediate top-up to now.
    Bulk,
    /// Subsequent run: newly-closed candles topped up.
    Incremental,
    /// Subsequent run with nothing newly closed — a no-op, not an error.
    UpToDate,
}

impl Action {
    /// The stable string form used in human output + the `--json` schema.
    fn as_str(self) -> &'static str {
        match self {
            Action::Bulk => "bulk",
            Action::Incremental => "incremental",
            Action::UpToDate => "up-to-date",
        }
    }
}

/// The grill-locked per-timeframe `--json` summary object.
///
/// Schema (spec §3, grill-locked): `{pair, timeframe, data_version, action,
/// candle_count, first_open_ms, last_open_ms, path, gap_count}`. Stable field
/// names + order so downstream tooling can depend on it.
#[derive(Debug, Clone, Serialize)]
pub struct TfSummary {
    /// Trading pair symbol.
    pub pair: String,
    /// `Binance` interval string (`15m` / `4h`).
    pub timeframe: String,
    /// The snapshot's content-hash `data_version` (the new HEAD).
    pub data_version: String,
    /// One of `bulk` / `incremental` / `up-to-date`.
    pub action: String,
    /// Number of candles in the snapshot.
    pub candle_count: usize,
    /// `open_time` of the first candle, if any.
    pub first_open_ms: Option<i64>,
    /// `open_time` of the last candle, if any.
    pub last_open_ms: Option<i64>,
    /// Absolute snapshot path.
    pub path: String,
    /// Number of detected spacing gaps (reported, not rejected — audit C2).
    pub gap_count: usize,
}

/// The outcome of one tf's orchestration: a summary on success, or the error on
/// failure (AC-8 — a failing tf still produces a `--json` entry).
pub enum TfOutcome {
    /// The tf's snapshot was ensured (written or already current).
    Ok(TfSummary),
    /// The tf failed; `summary` carries the partial entry (action + error) for
    /// the `--json` report and the process exits non-zero.
    Failed {
        /// The timeframe that failed.
        timeframe: String,
        /// The error message surfaced in the report.
        error: String,
    },
}

/// Compute the bulk window start (epoch ms) for `--years N`: floor to the first
/// day of the month `n_years` before the current UTC month (audit C5).
///
/// `now_ms` is the [`Clock`]'s "now" so the window is deterministic in tests.
#[must_use]
pub fn years_window_start_ms(now_ms: i64, n_years: u32) -> i64 {
    let now = Utc
        .timestamp_millis_opt(now_ms)
        .single()
        .unwrap_or_else(Utc::now);
    let target_year = now.year() - i32::try_from(n_years).unwrap_or(i32::MAX);
    // Floor to the first millisecond of the first day of that month, UTC.
    Utc.with_ymd_and_hms(target_year, now.month(), 1, 0, 0, 0)
        .single()
        .map_or(now_ms, |dt| dt.timestamp_millis())
}

/// The `(year, month)` calendar month (UTC) immediately before the one `now_ms`
/// falls in — the month Binance may not have published a bulk archive for yet.
fn previous_month(now_ms: i64) -> (i32, u32) {
    let now = Utc
        .timestamp_millis_opt(now_ms)
        .single()
        .unwrap_or_else(Utc::now);
    if now.month() == 1 {
        (now.year() - 1, 12)
    } else {
        (now.year(), now.month() - 1)
    }
}

/// Ensure the snapshot for one `(pair, tf)`, returning a summary (or a failure
/// entry on error — never panics; the caller aggregates exit status, AC-8).
///
/// `now_ms` is read once from the clock so the window + cutoff are deterministic.
pub async fn ensure_one_tf<S, C, R>(
    source: &S,
    repo: &R,
    clock: &C,
    pair: &Pair,
    tf: Timeframe,
    n_years: u32,
) -> TfOutcome
where
    S: MarketDataSource,
    C: Clock,
    R: CandleSeriesRepository,
{
    let now_ms = clock.now_ms();
    match ensure_inner(source, repo, pair, tf, n_years, now_ms).await {
        Ok(summary) => TfOutcome::Ok(summary),
        Err(e) => TfOutcome::Failed {
            timeframe: tf.binance_interval().to_string(),
            error: e.to_string(),
        },
    }
}

/// The fallible body of [`ensure_one_tf`] (kept ≤ 80 lines; helpers below).
async fn ensure_inner<S, R>(
    source: &S,
    repo: &R,
    pair: &Pair,
    tf: Timeframe,
    n_years: u32,
    now_ms: i64,
) -> Result<TfSummary, DataError>
where
    S: MarketDataSource,
    R: CandleSeriesRepository,
{
    // ONE port call resolves HEAD and the snapshot it names. A broken pointer is
    // an error here, not an `Ok(None)` that would look like a first run and
    // silently re-bulk the whole window.
    match repo.load_head(pair, tf)? {
        None => first_run(source, repo, pair, tf, n_years, now_ms).await,
        Some(prior) => subsequent_run(source, repo, pair, tf, prior).await,
    }
}

/// First run: bulk over the years window, then top up to now; write snapshot,
/// then `HEAD` (audit C1).
async fn first_run<S, R>(
    source: &S,
    repo: &R,
    pair: &Pair,
    tf: Timeframe,
    n_years: u32,
    now_ms: i64,
) -> Result<TfSummary, DataError>
where
    S: MarketDataSource,
    R: CandleSeriesRepository,
{
    let start_ms = years_window_start_ms(now_ms, n_years);
    // Bulk covers COMPLETE months only — exclude the current (incomplete) month,
    // which data.binance.vision has not published a monthly archive for yet; the
    // REST top-up below fills it (audit C5). `years_window_start_ms(_, 0)` floors
    // `now` to the first day of the current UTC month. Passing `now_ms` here (the
    // original bug) made the bulk range include the current month → WI-02's
    // "expected month absent after listing" error on the live `--years 2` run.
    let bulk_end_ms = years_window_start_ms(now_ms, 0);
    // The month that just ended can still be unpublished (#289): name it as the one
    // month the bulk loader may leave to the REST top-up below, which covers it.
    // Only that exact month is exempt — any other absent month is a coverage hole
    // (audit C2), so a bulk range that wrongly reached the current month still
    // fails (audit C5).
    let lag_month = previous_month(now_ms);
    let mut series = source
        .fetch_historical_lagging(pair, tf, start_ms, bulk_end_ms, lag_month)
        .await?;
    // Immediate top-up to "now" (closed candles only) so the first snapshot is
    // current (grill). Empty bulk ⇒ anchor the top-up at the requested window
    // start (`start_ms - 1` so the candle opening at `start_ms` is included),
    // NOT epoch 0 — else an empty-bulk run (e.g. `--years 0` in an unpublished
    // current month) would back-fill from Binance's earliest candle.
    let since = series.candles.last().map_or(start_ms - 1, |c| c.open_time);
    let new = source.fetch_incremental(pair, tf, since).await?;
    if !new.is_empty() {
        series = crate::adapters::binance::merge::merge_new(&series, new)?.0;
    }
    if series.candles.is_empty() {
        // Nothing fetched (e.g. `--years 0` right after a UTC month rollover,
        // before the first candle closes). The repository's zero-candle contract
        // is exactly the behaviour this branch needs: it persists no snapshot and
        // sets no `HEAD` — else the next run would read an empty prior and
        // back-fill from epoch (CodeRabbit) — and returns the derived identity
        // with NO locator, so the reported `path` is empty rather than naming a
        // Parquet that does not exist (Codex P2).
        let stored = repo.commit(pair, tf, series.candles)?;
        return summarize(&stored, Action::UpToDate);
    }
    persist(repo, pair, tf, series, Action::Bulk)
}

/// Subsequent run: read the prior snapshot, top up only newly-closed candles.
async fn subsequent_run<S, R>(
    source: &S,
    repo: &R,
    pair: &Pair,
    tf: Timeframe,
    prior: StoredCandleSeries,
) -> Result<TfSummary, DataError>
where
    S: MarketDataSource,
    R: CandleSeriesRepository,
{
    let since = prior.series.candles.last().map_or(-1, |c| c.open_time);
    let new = source.fetch_incremental(pair, tf, since).await?;
    if new.is_empty() {
        // Nothing newly closed ⇒ up-to-date no-op (NOT an error). HEAD unchanged,
        // and the reported `path` is the locator HEAD was already resolved through.
        return summarize(&prior, Action::UpToDate);
    }
    let (merged, _gaps) = crate::adapters::binance::merge::merge_new(&prior.series, new)?;
    persist(repo, pair, tf, merged, Action::Incremental)
}

/// Commit the merged candle set through the repository port. Identity derivation
/// (ADR-0009's content hash) and the snapshot-then-`HEAD` ordering (audit C1) are
/// the repository's guarantees now — this function just hands over the candles.
fn persist<R>(
    repo: &R,
    pair: &Pair,
    tf: Timeframe,
    series: crate::domain::CandleSeries,
    action: Action,
) -> Result<TfSummary, DataError>
where
    R: CandleSeriesRepository,
{
    let stored = repo.commit(pair, tf, series.candles)?;
    summarize(&stored, action)
}

/// Build the `--json`/human summary from a stored series.
///
/// The grill-locked field set/types are unchanged. `path` is the repository's
/// display locator — the snapshot's absolute path for a persisted series, and the
/// empty string for the zero-candle outcome, where no snapshot exists and naming
/// one would point at a Parquet that was never written (Codex P2).
fn summarize(stored: &StoredCandleSeries, action: Action) -> Result<TfSummary, DataError> {
    let series = &stored.series;
    let gaps = series.validate()?;
    Ok(TfSummary {
        pair: series.pair.to_string(),
        timeframe: series.timeframe.binance_interval().to_string(),
        data_version: series.version.to_string(),
        action: action.as_str().to_string(),
        candle_count: series.candles.len(),
        first_open_ms: series.candles.first().map(|c| c.open_time),
        last_open_ms: series.candles.last().map(|c| c.open_time),
        path: stored.storage_location.clone().unwrap_or_default(),
        gap_count: gaps.len(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Action, TfSummary, years_window_start_ms};
    use chrono::{Datelike, TimeZone, Timelike, Utc};

    // ---- C5: --years N floors to the start of the month N years back, UTC --

    #[test]
    fn years_window_floors_to_start_of_month_n_years_back() {
        // now = 2026-05-31T13:00:00Z, N = 2 → 2024-05-01T00:00:00Z.
        let now = Utc.with_ymd_and_hms(2026, 5, 31, 13, 0, 0).unwrap();
        let start_ms = years_window_start_ms(now.timestamp_millis(), 2);
        let start = Utc.timestamp_millis_opt(start_ms).single().unwrap();
        assert_eq!(start.year(), 2024);
        assert_eq!(start.month(), 5);
        assert_eq!(start.day(), 1);
        assert_eq!((start.hour(), start.minute(), start.second()), (0, 0, 0));
    }

    #[test]
    fn years_window_one_year_back() {
        let now = Utc.with_ymd_and_hms(2026, 1, 15, 9, 30, 0).unwrap();
        let start_ms = years_window_start_ms(now.timestamp_millis(), 1);
        let start = Utc.timestamp_millis_opt(start_ms).single().unwrap();
        assert_eq!((start.year(), start.month(), start.day()), (2025, 1, 1));
    }

    // ---- AC-4: the --json summary serializes with the locked schema --------

    #[test]
    fn tf_summary_serializes_with_the_locked_schema() {
        let summary = TfSummary {
            pair: "BTCUSDT".to_string(),
            timeframe: "15m".to_string(),
            data_version: "deadbeefcafef00d".to_string(),
            action: "bulk".to_string(),
            candle_count: 3,
            first_open_ms: Some(0),
            last_open_ms: Some(1_800_000),
            path: "/tmp/candles/BTCUSDT/15m/deadbeefcafef00d.parquet".to_string(),
            gap_count: 0,
        };
        let json = serde_json::to_value(&summary).expect("serialize");
        // Every grill-locked field is present under its exact name.
        for key in [
            "pair",
            "timeframe",
            "data_version",
            "action",
            "candle_count",
            "first_open_ms",
            "last_open_ms",
            "path",
            "gap_count",
        ] {
            assert!(json.get(key).is_some(), "missing field {key}: {json}");
        }
        assert_eq!(json["candle_count"], 3);
        assert_eq!(json["action"], "bulk");
    }

    #[test]
    fn action_strings_are_stable_kebab_case() {
        assert_eq!(Action::Bulk.as_str(), "bulk");
        assert_eq!(Action::Incremental.as_str(), "incremental");
        assert_eq!(Action::UpToDate.as_str(), "up-to-date");
    }
}
