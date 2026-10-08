//! `pulse fetch-data` orchestration (WI-1.1.1.05).
//!
//! Composes the [`MarketDataSource`] port (a [`BinanceDataSource`](crate::adapters::binance::BinanceDataSource)
//! in production) + the [`CandleSeriesRepository`] port (the Parquet adapter in
//! production) into the slice's user-facing seam. Since r1.s3.w1 (#112) the
//! orchestration depends on **two ports and no concrete type** (NFR-9 / AC-6);
//! `src/cli/mod.rs` is where an implementation is chosen.
//!
//! Per-(pair, tf) flow (grill + audit-locked, spec §3):
//! - **First run** (no `HEAD`): bulk over the resolved window (the `--years N`
//!   floor or the `--from` month floor)
//!   ([`MarketDataSource::fetch_historical`]) **then** an immediate REST top-up
//!   to the clock cutoff ([`MarketDataSource::fetch_incremental`]) so the first
//!   snapshot is current. Commit the result. Action `bulk`.
//! - **Subsequent run** (`HEAD` present): read the prior snapshot, top up only
//!   newly-closed candles. If any closed → commit a new version (action
//!   `incremental`); if nothing newly closed → **`up-to-date` no-op**, not an
//!   error.
//! - **Backfill run** (`HEAD` present, the resolved start **earlier** than its
//!   first candle, r4.s1.w2): bulk the missing earlier months over
//!   `[start, prior first candle's month + 1 ms)`, merge them onto the prior
//!   snapshot, top up to now, and commit **one** new snapshot (action
//!   `backfill`) — a new `data_version`, `HEAD` moves, the prior file stays.
//!   A start equal to or later than the first candle keeps the incremental path.
//! - **Ordering + crash-safety (audit C1):** snapshot-then-`HEAD` ordering is the
//!   repository's guarantee ([`CandleSeriesRepository::commit`]), not something
//!   this module sequences any more. A crash between the two still leaves a valid
//!   orphaned snapshot and an unchanged `HEAD`.
//! - **`--years N` / `--from` window (audit C5):** the caller resolves exactly
//!   one of the two; `--years N` starts at the first day of the month `N` years
//!   before the current UTC month, `--from <YYYY-MM-DD>` at the first day of the
//!   given date's UTC month.
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
    /// The requested start was earlier than `HEAD`'s first candle: the missing
    /// earlier months were bulk-fetched, the span topped up to now, and ONE new
    /// snapshot covering the whole span was committed (r4.s1.w2).
    Backfill,
    /// Subsequent run with nothing newly closed — a no-op, not an error.
    UpToDate,
}

impl Action {
    /// The stable string form used in human output + the `--json` schema.
    fn as_str(self) -> &'static str {
        match self {
            Action::Bulk => "bulk",
            Action::Incremental => "incremental",
            Action::Backfill => "backfill",
            Action::UpToDate => "up-to-date",
        }
    }
}

/// The grill-locked per-timeframe `--json` summary object.
///
/// Schema (spec §3, grill-locked): `{pair, timeframe, data_version, action,
/// candle_count, first_open_ms, last_open_ms, path, gap_count}`. Stable field
/// names + order so downstream tooling can depend on it. r4.s1.w4 adds
/// `filled_candle_count`: the candles this run filled into interior archive
/// holes from REST (0 when nothing was filled).
#[derive(Debug, Clone, Serialize)]
pub struct TfSummary {
    /// Trading pair symbol.
    pub pair: String,
    /// `Binance` interval string (`15m` / `4h`).
    pub timeframe: String,
    /// The snapshot's content-hash `data_version` (the new HEAD).
    pub data_version: String,
    /// One of `bulk` / `incremental` / `backfill` / `up-to-date`.
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
    /// The candles this run filled into interior gaps from REST (r4.s1.w4).
    pub filled_candle_count: usize,
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

/// Parse a `--from <YYYY-MM-DD>` value (UTC calendar date) and floor it to the
/// first millisecond of its month — the bulk archive's granularity
/// (r4.s1.w2). The caller resolves exactly one of `--years` / `--from` into
/// this start epoch-ms; `ensure_one_tf` takes the resolved value.
///
/// # Errors
///
/// Returns [`DataError::Parse`] naming the rejected value when it is not a
/// `YYYY-MM-DD` calendar date.
pub fn from_window_start_ms(raw: &str) -> Result<i64, DataError> {
    let date = chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").map_err(|e| {
        DataError::Parse(format!(
            "invalid --from {raw:?}: {e} (expected YYYY-MM-DD, a UTC calendar date)"
        ))
    })?;
    Utc.with_ymd_and_hms(date.year(), date.month(), 1, 0, 0, 0)
        .single()
        .map(|dt| dt.timestamp_millis())
        .ok_or_else(|| DataError::Parse(format!("invalid --from {raw:?}: no such UTC month")))
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
/// `start_ms` is the **resolved** window start (epoch ms) — the `--years`
/// floor or the `--from` month floor, resolved once by the caller (r4.s1.w2).
/// `now_ms` is read once from the clock so the window + cutoff are deterministic.
pub async fn ensure_one_tf<S, C, R>(
    source: &S,
    repo: &R,
    clock: &C,
    pair: &Pair,
    tf: Timeframe,
    start_ms: i64,
) -> TfOutcome
where
    S: MarketDataSource + Sync,
    C: Clock,
    R: CandleSeriesRepository,
{
    let now_ms = clock.now_ms();
    match ensure_inner(source, repo, pair, tf, start_ms, now_ms).await {
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
    start_ms: i64,
    now_ms: i64,
) -> Result<TfSummary, DataError>
where
    S: MarketDataSource + Sync,
    R: CandleSeriesRepository,
{
    // ONE port call resolves HEAD and the snapshot it names. A broken pointer is
    // an error here, not an `Ok(None)` that would look like a first run and
    // silently re-bulk the whole window.
    match repo.load_head(pair, tf)? {
        None => first_run(source, repo, pair, tf, start_ms, now_ms).await,
        Some(prior) => match prior.series.candles.first().map(|c| c.open_time) {
            // r4.s1.w2: a start EARLIER than HEAD's first candle backfills the
            // missing earlier months; equal or later keeps today's incremental
            // top-up (grill Q1 — prod's BTCUSDT starts 2024-06 and must reach
            // 2021-01-01).
            Some(prior_first) if start_ms < prior_first => {
                backfill_run(source, repo, pair, tf, prior, start_ms).await
            }
            _ => subsequent_run(source, repo, pair, tf, prior).await,
        },
    }
}

/// First run: bulk over the resolved window, then top up to now; write
/// snapshot, then `HEAD` (audit C1).
async fn first_run<S, R>(
    source: &S,
    repo: &R,
    pair: &Pair,
    tf: Timeframe,
    start_ms: i64,
    now_ms: i64,
) -> Result<TfSummary, DataError>
where
    S: MarketDataSource + Sync,
    R: CandleSeriesRepository,
{
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
        return summarize(&stored, Action::UpToDate, 0);
    }
    // r4.s1.w4: fill the interior holes the archive left (the SOL/XRP case),
    // bounded to each gap's range, before the one snapshot is written.
    let (series, filled) = with_interior_gaps_filled(source, pair, tf, series).await?;
    persist(repo, pair, tf, series, Action::Bulk, filled)
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
    S: MarketDataSource + Sync,
    R: CandleSeriesRepository,
{
    let since = prior.series.candles.last().map_or(-1, |c| c.open_time);
    let new = source.fetch_incremental(pair, tf, since).await?;
    if new.is_empty() {
        // Nothing newly closed: still look for interior gaps in HEAD (r4.s1.w4)
        // — an archive hole does not heal itself, and the run that only fills
        // one writes the one new snapshot. Otherwise the up-to-date no-op
        // (NOT an error), HEAD unchanged, and the reported `path` is the
        // locator HEAD was already resolved through.
        let (series, filled) =
            with_interior_gaps_filled(source, pair, tf, prior.series.clone()).await?;
        if filled == 0 {
            return summarize(&prior, Action::UpToDate, 0);
        }
        return persist(repo, pair, tf, series, Action::Backfill, filled);
    }
    let (merged, _gaps) = crate::adapters::binance::merge::merge_new(&prior.series, new)?;
    let (merged, filled) = with_interior_gaps_filled(source, pair, tf, merged).await?;
    persist(repo, pair, tf, merged, Action::Incremental, filled)
}

/// Backfill run (r4.s1.w2): the requested start is EARLIER than `HEAD`'s first
/// candle, so the months before it are missing from the snapshot.
///
/// The missing months come from the **same bulk path** a first run uses —
/// `[start_ms, month start of the prior first candle + 1 ms)`, the `+ 1 ms`
/// making `months_in_range`'s inclusive `end - 1` naming name the prior first
/// candle's own month (whose early days the snapshot lacks) — and are merged
/// onto the prior snapshot (dedup on `open_time`, the freshly-fetched copy
/// winning). The merged span is then topped up to now, every other month's
/// publication lag included, and ONE new snapshot is committed: a new
/// content-hash `data_version`, `HEAD` moves, and the prior snapshot file stays
/// on disk (ADR-0018 — existing runs keep pointing at their own data versions).
///
/// When neither the earlier months nor the top-up add a candle, the request is
/// already satisfied: the `up-to-date` no-op, `HEAD` unchanged (an identical
/// content hash would name the same version anyway — the branch says so
/// honestly instead of re-committing a version that already exists).
async fn backfill_run<S, R>(
    source: &S,
    repo: &R,
    pair: &Pair,
    tf: Timeframe,
    prior: StoredCandleSeries,
    start_ms: i64,
) -> Result<TfSummary, DataError>
where
    S: MarketDataSource + Sync,
    R: CandleSeriesRepository,
{
    let Some(prior_first) = prior.series.candles.first().map(|c| c.open_time) else {
        // Unreachable through `commit` (a zero-candle series writes no HEAD):
        // treat headless content like today's incremental path.
        return subsequent_run(source, repo, pair, tf, prior).await;
    };
    let bulk_end_ms = month_start_ms(prior_first) + 1;
    let bulk = source
        .fetch_historical(pair, tf, start_ms, bulk_end_ms)
        .await?;
    let (merged, _gaps) = crate::adapters::binance::merge::merge_new(&prior.series, bulk.candles)?;
    // Top up to now from the merged series' last candle — the backfill also
    // closes the span up to the clock cutoff, in the same commit (r4.s1.w2).
    let since = merged.candles.last().map_or(start_ms - 1, |c| c.open_time);
    let new = source.fetch_incremental(pair, tf, since).await?;
    let merged = if new.is_empty() {
        merged
    } else {
        crate::adapters::binance::merge::merge_new(&merged, new)?.0
    };
    if merged.candles == prior.series.candles {
        return summarize(&prior, Action::UpToDate, 0);
    }
    // r4.s1.w4: the same interior-hole fill the other two paths run — a
    // backfill that lands months around a hole still leaves the hole.
    let (merged, filled) = with_interior_gaps_filled(source, pair, tf, merged).await?;
    persist(repo, pair, tf, merged, Action::Backfill, filled)
}

/// The first millisecond of `ms`'s UTC calendar month — the monthly bulk
/// archive's granularity. An invalid instant falls back to `ms` itself (the
/// same degradation `years_window_start_ms` takes).
#[must_use]
fn month_start_ms(ms: i64) -> i64 {
    let Some(instant) = Utc.timestamp_millis_opt(ms).single() else {
        return ms;
    };
    Utc.with_ymd_and_hms(instant.year(), instant.month(), 1, 0, 0, 0)
        .single()
        .map_or(ms, |dt| dt.timestamp_millis())
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
    filled: usize,
) -> Result<TfSummary, DataError>
where
    R: CandleSeriesRepository,
{
    let stored = repo.commit(pair, tf, series.candles)?;
    summarize(&stored, action, filled)
}

/// Build the `--json`/human summary from a stored series.
///
/// The grill-locked field set/types are unchanged (r4.s1.w4 adds
/// `filled_candle_count`). `path` is the repository's display locator — the
/// snapshot's absolute path for a persisted series, and the empty string for the
/// zero-candle outcome, where no snapshot exists and naming one would point at a
/// Parquet that was never written (Codex P2).
fn summarize(
    stored: &StoredCandleSeries,
    action: Action,
    filled: usize,
) -> Result<TfSummary, DataError> {
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
        filled_candle_count: filled,
    })
}

// ---------------------------------------------------------------------------
// r4.s1.w4: the interior-gap fill
// ---------------------------------------------------------------------------

/// Fill `series`' interior gaps from the REST klines endpoint, one bounded
/// incremental fetch per gap, and report how many candles were filled.
///
/// A "gap" is a spacing discontinuity `validate()` reports between two adjacent
/// candles: the missing candles are `[gap.expected, gap.found)`, so the fetch
/// starts at `gap.expected - 1` (the endpoint returns candles strictly newer)
/// and is bounded at `gap.found`. The fetch is the ordinary incremental path —
/// funding included — and only its in-range candles are kept. A gap REST cannot
/// fill stays: `validate()` still reports it, the summary says so, and
/// `load_series` keeps refusing the snapshot.
///
/// # Errors
///
/// Returns [`DataError`] from the transport or a JSON decode failure.
async fn fill_interior_gaps<S>(
    source: &S,
    pair: &Pair,
    tf: Timeframe,
    series: &crate::domain::CandleSeries,
) -> Result<(Vec<crate::domain::Candle>, usize), DataError>
where
    S: MarketDataSource + Sync,
{
    let gaps = series.validate()?;
    if gaps.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let mut filled = Vec::new();
    for gap in gaps {
        let candles = source
            .fetch_incremental_until(pair, tf, gap.expected - 1, gap.found)
            .await?;
        filled.extend(candles);
    }
    let count = filled.len();
    Ok((filled, count))
}

/// [`fill_interior_gaps`] applied to the series about to be written: merges the
/// filled candles in (dedup on `open_time`, the freshly-fetched copy winning)
/// and returns the series plus the filled count. Nothing filled ⇒ the series is
/// returned untouched, so the caller's no-op branch stays exact.
///
/// # Errors
///
/// As [`fill_interior_gaps`], plus a merge failure.
async fn with_interior_gaps_filled<S>(
    source: &S,
    pair: &Pair,
    tf: Timeframe,
    series: crate::domain::CandleSeries,
) -> Result<(crate::domain::CandleSeries, usize), DataError>
where
    S: MarketDataSource + Sync,
{
    let (filled, count) = fill_interior_gaps(source, pair, tf, &series).await?;
    if filled.is_empty() {
        return Ok((series, 0));
    }
    let (merged, _gaps) = crate::adapters::binance::merge::merge_new(&series, filled)?;
    Ok((merged, count))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Action, TfSummary, from_window_start_ms, years_window_start_ms};
    use chrono::{Datelike, TimeZone, Timelike, Utc};

    // ---- r4.s1.w2: --from floors to the first of its UTC month -------------

    #[test]
    fn from_window_floors_to_the_first_of_its_utc_month() {
        // 2021-01-15 → 2021-01-01T00:00:00Z.
        assert_eq!(
            from_window_start_ms("2021-01-15").unwrap(),
            1_609_459_200_000
        );
        // A month start is idempotent.
        assert_eq!(
            from_window_start_ms("2021-01-01").unwrap(),
            1_609_459_200_000
        );
        // The last day of a month still floors to that month's first.
        assert_eq!(
            from_window_start_ms("2024-06-30").unwrap(),
            1_717_200_000_000
        );
    }

    #[test]
    fn from_window_rejects_a_non_date() {
        for raw in ["not-a-date", "2021-13-01", "20210101", "2021-01-32", ""] {
            assert!(
                from_window_start_ms(raw).is_err(),
                "{raw:?} must be refused"
            );
        }
    }

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
            filled_candle_count: 0,
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
