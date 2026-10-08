//! End-to-end OFFLINE integration test for `pulse fetch-data` (WI-1.1.1.05) —
//! the slice's **auto-demo proxy** (audit C2). It drives the full compose path
//! (bulk → REST top-up → Parquet write → re-read → `HEAD`) over recorded fixture
//! seams (`MonthSource` + `PageSource`) + a deterministic `FakeClock`, with **no
//! live network**. The true live `--years 2` run is the manual demo (`DEMO_RUNBOOK`).
//!
//! AC coverage (the offline `auto:` set):
//! - **AC-1** First run: bulk + REST top-up to the clock cutoff writes a
//!   versioned Parquet per TF **and sets `HEAD`**.
//! - **AC-2** Second run reads `HEAD`, tops up only newly-closed candles → new
//!   `data_version`; nothing newly closed → **`up-to-date` no-op**, not an error.
//! - **AC-3** The produced snapshot is OHLCV+funding complete + gap-free
//!   (`validate()` → `Ok`), asserted programmatically.
//! - **AC-5/AC-7** `HEAD` is the top-up base across runs; written **after** the
//!   snapshot; an orphaned snapshot does not move it.
//! - **AC-8** Multi-tf partial failure: M15 succeeds, H4 forced to fail → M15
//!   snapshot+`HEAD` written, H4 reported as error, `run_fetch_data` returns Err
//!   (the binary's non-zero exit, audit C4).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::future::Future;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{Datelike, TimeZone, Utc};

use pulse::{
    BinanceDataSource, Candle, CandleSeries, CandleSeriesRepository, CandleStore, DataError,
    DataVersion, FakeClock, FetchArgs, FundingEvent, MarketDataSource, MonthData, MonthOutcome,
    MonthSource, PageSource, Pair, TfOutcome, Timeframe, ensure_one_tf, run_fetch_data,
    years_window_start_ms,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

const M15: i64 = 900_000;

// A deterministic "now" far past the fixture data so every fixture candle is
// closed except the explicitly still-forming one.
const NOW_MS: i64 = 1_700_010_000_000;

// The bulk month's three contiguous M15 candles, opening at these timestamps.
const BULK_OPEN_0: i64 = 1_700_000_000_000;
const BULK_OPEN_1: i64 = 1_700_000_900_000;
const BULK_OPEN_2: i64 = 1_700_001_800_000;
// The funding event that lands on BULK_OPEN_1 (on-boundary, sparse).
const BULK_FUNDING_TS: i64 = 1_700_000_900_000;

// The two newly-closed candles the first-run top-up discovers, plus a still-
// forming one (dropped by the cutoff).
const NEW_CLOSED_1: i64 = 1_700_002_700_000;
const NEW_CLOSED_2: i64 = 1_700_003_600_000;
const STILL_FORMING: i64 = 1_700_009_999_000;

fn dec(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn btc() -> Pair {
    Pair::new("BTCUSDT")
}

// ---- Fixture bulk source --------------------------------------------------

/// A fixture [`MonthSource`] returning, for any month asked, one in-memory month
/// of three contiguous M15 candles + an on-boundary funding event. Stands in for
/// the recorded `data.binance.vision` archive (spec §3 — offline).
struct FixtureBulk;

impl MonthSource for FixtureBulk {
    fn load_month(
        &self,
        _pair: &Pair,
        _tf: Timeframe,
        _year: i32,
        _month: u32,
    ) -> impl Future<Output = Result<MonthOutcome, DataError>> {
        std::future::ready(Ok(MonthOutcome::Loaded(MonthData {
            candles: vec![
                bulk_candle(BULK_OPEN_0),
                bulk_candle(BULK_OPEN_1),
                bulk_candle(BULK_OPEN_2),
            ],
            funding: vec![FundingEvent {
                calc_time: BULK_FUNDING_TS,
                rate: dec("0.00010000"),
            }],
        })))
    }
}

fn bulk_candle(open_time: i64) -> Candle {
    Candle {
        open_time,
        close_time: open_time + M15 - 1,
        open: dec("42000.5"),
        high: dec("42100.0"),
        low: dec("41950.25"),
        close: dec("42050.75"),
        volume: dec("12.34567"),
        funding_rate: None,
    }
}

// ---- Fixture REST page source ---------------------------------------------

/// A fixture [`PageSource`] keyed by the `startTime` marker (klines/funding), so
/// the test does not hard-code the full host. Two scripts: one that discovers
/// new candles (first-run + second-run top-up), one that is caught up.
struct FixtureRest {
    by_marker: HashMap<String, Vec<u8>>,
}

impl FixtureRest {
    /// The top-up page set that discovers the two newly-closed candles + funding.
    fn with_new_candles() -> Self {
        let mut m: HashMap<String, Vec<u8>> = HashMap::new();
        // klines from BULK_OPEN_2 + 1 → the two closed + the forming candle.
        m.insert(
            format!("klines:{}", BULK_OPEN_2 + 1),
            klines_json(&[
                (NEW_CLOSED_1, NEW_CLOSED_1 + M15 - 1),
                (NEW_CLOSED_2, NEW_CLOSED_2 + M15 - 1),
                (STILL_FORMING, STILL_FORMING + M15 - 1),
            ]),
        );
        // After advancing past the last open → empty (caught up).
        m.insert(format!("klines:{}", STILL_FORMING + 1), b"[]".to_vec());
        // funding fetched from BULK_OPEN_2 + 1 (the candle boundary): one event
        // landing on NEW_CLOSED_1.
        m.insert(
            format!("funding:{}", BULK_OPEN_2 + 1),
            funding_json(&[(NEW_CLOSED_1, "0.00012500")]),
        );
        Self { by_marker: m }
    }

    /// The caught-up page set: the only candle beyond the snapshot is the still-
    /// forming one (dropped), so the run is a no-op (`up-to-date`).
    fn caught_up() -> Self {
        let mut m: HashMap<String, Vec<u8>> = HashMap::new();
        m.insert(
            format!("klines:{}", NEW_CLOSED_2 + 1),
            klines_json(&[(STILL_FORMING, STILL_FORMING + M15 - 1)]),
        );
        m.insert(format!("klines:{}", STILL_FORMING + 1), b"[]".to_vec());
        m.insert(format!("funding:{}", NEW_CLOSED_2 + 1), b"[]".to_vec());
        Self { by_marker: m }
    }

    /// The top-up after a bulk phase whose last candle is `LAG_BULK_OPEN_1` and
    /// whose final month (2023-11) was unpublished: REST supplies the skipped
    /// month in two pages of candles, then an empty page.
    fn paginated_after_month_lag() -> Self {
        let mut m: HashMap<String, Vec<u8>> = HashMap::new();
        m.insert(
            format!("klines:{}", LAG_BULK_OPEN_1 + 1),
            klines_json(&[
                (NOV_1_MS, NOV_1_MS + M15 - 1),
                (NOV_1_MS + M15, NOV_1_MS + 2 * M15 - 1),
                (NOV_1_MS + 2 * M15, NOV_1_MS + 3 * M15 - 1),
            ]),
        );
        m.insert(
            format!("klines:{}", NOV_1_MS + 2 * M15 + 1),
            klines_json(&[
                (NOV_1_MS + 3 * M15, NOV_1_MS + 4 * M15 - 1),
                (NOV_1_MS + 4 * M15, NOV_1_MS + 5 * M15 - 1),
            ]),
        );
        m.insert(format!("klines:{}", NOV_1_MS + 4 * M15 + 1), b"[]".to_vec());
        m.insert(format!("funding:{}", LAG_BULK_OPEN_1 + 1), b"[]".to_vec());
        Self { by_marker: m }
    }

    fn marker_for(url: &str) -> String {
        let endpoint = if url.contains("/fapi/v1/fundingRate") {
            "funding"
        } else {
            "klines"
        };
        let start = url
            .split("startTime=")
            .nth(1)
            .and_then(|s| s.split('&').next())
            .unwrap_or("?");
        format!("{endpoint}:{start}")
    }
}

impl PageSource for FixtureRest {
    fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, DataError>> + Send {
        let body = self.by_marker.get(&Self::marker_for(url)).cloned();
        async move { body.ok_or_else(|| DataError::Io(format!("unscripted REST URL: {url}"))) }
    }
}

fn klines_json(rows: &[(i64, i64)]) -> Vec<u8> {
    let body: Vec<String> = rows
        .iter()
        .map(|(open, close)| {
            format!(
                "[{open},\"42000.5\",\"42100.0\",\"41950.25\",\"42050.75\",\"1.0\",{close},\"0\",1,\"0\",\"0\",\"0\"]"
            )
        })
        .collect();
    format!("[{}]", body.join(",")).into_bytes()
}

fn funding_json(rows: &[(i64, &str)]) -> Vec<u8> {
    let body: Vec<String> = rows
        .iter()
        .map(|(t, rate)| {
            format!("{{\"symbol\":\"BTCUSDT\",\"fundingTime\":{t},\"fundingRate\":\"{rate}\",\"markPrice\":\"1\"}}")
        })
        .collect();
    format!("[{}]", body.join(",")).into_bytes()
}

fn store() -> (CandleStore, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let store = CandleStore::with_base_dir(tmp.path().to_path_buf());
    (store, tmp)
}

fn args(json: bool) -> FetchArgs {
    FetchArgs {
        pair: "BTCUSDT".to_string(),
        tf: vec!["M15".to_string()],
        years: Some(1),
        from: None,
        json,
    }
}

/// The same request through `--from <YYYY-MM-DD>` (r4.s1.w2): the run resolves
/// its start as the month floor of the given date.
fn from_args(date: &str) -> FetchArgs {
    FetchArgs {
        pair: "BTCUSDT".to_string(),
        tf: vec!["M15".to_string()],
        years: None,
        from: Some(date.to_string()),
        json: false,
    }
}

// ---- AC-1: first run does bulk + top-up → snapshot + HEAD ------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac1_first_run_writes_versioned_snapshot_and_sets_head() {
    let (store, _tmp) = store();
    let source = BinanceDataSource::new(
        FixtureBulk,
        FixtureRest::with_new_candles(),
        FakeClock::at(NOW_MS),
    );

    run_fetch_data(&source, &store, &FakeClock::at(NOW_MS), &args(false))
        .await
        .expect("first run succeeds");

    // HEAD is set (AC-1) and points at a real snapshot.
    let head = store
        .read_head(&btc(), Timeframe::M15)
        .expect("read HEAD")
        .expect("HEAD set after first run");
    assert!(
        store.snapshot_exists(&btc(), Timeframe::M15, &head),
        "HEAD points at a written snapshot"
    );

    // The snapshot holds the 3 bulk candles + the 2 newly-closed top-up candles;
    // the still-forming candle was dropped by the cutoff (AC-1, audit C5).
    let series = store
        .read_snapshot(&btc(), Timeframe::M15, &head)
        .expect("read snapshot");
    let opens: Vec<i64> = series.candles.iter().map(|c| c.open_time).collect();
    assert_eq!(
        opens,
        vec![
            BULK_OPEN_0,
            BULK_OPEN_1,
            BULK_OPEN_2,
            NEW_CLOSED_1,
            NEW_CLOSED_2
        ],
        "still-forming candle ({STILL_FORMING}) must NOT be persisted"
    );
}

// ---- AC-3: the produced snapshot is complete + gap-free --------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac3_produced_snapshot_validates_and_carries_funding() {
    let (store, _tmp) = store();
    let source = BinanceDataSource::new(
        FixtureBulk,
        FixtureRest::with_new_candles(),
        FakeClock::at(NOW_MS),
    );
    run_fetch_data(&source, &store, &FakeClock::at(NOW_MS), &args(false))
        .await
        .expect("first run");

    let head = store.read_head(&btc(), Timeframe::M15).unwrap().unwrap();
    let series = store
        .read_snapshot(&btc(), Timeframe::M15, &head)
        .expect("read snapshot");

    // AC-3: gap-free (contiguous M15).
    let gaps = series.validate().expect("validate Ok");
    assert!(gaps.is_empty(), "contiguous snapshot has no gaps: {gaps:?}");

    // Funding present + correctly aligned (sparse, on-boundary): the bulk event
    // on BULK_OPEN_1 and the top-up event on NEW_CLOSED_1.
    let funded: Vec<(i64, Option<Decimal>)> = series
        .candles
        .iter()
        .map(|c| (c.open_time, c.funding_rate))
        .collect();
    assert_eq!(funded[1], (BULK_OPEN_1, Some(dec("0.00010000"))));
    assert_eq!(funded[3], (NEW_CLOSED_1, Some(dec("0.00012500"))));
    assert_eq!(funded[0].1, None, "sparse: no forward-fill");
}

// ---- AC-2 + AC-5/AC-7: second run reads HEAD, tops up; then up-to-date no-op-

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac2_second_run_reads_head_then_up_to_date_no_op() {
    let (store, _tmp) = store();

    // Run 1: bulk only (the REST top-up finds nothing new yet — caught up at the
    // bulk boundary). Build a clock just past BULK_OPEN_2 so nothing new closes.
    let clock_run1 = FakeClock::at(BULK_OPEN_2 + M15 + 1);
    let mut m: HashMap<String, Vec<u8>> = HashMap::new();
    m.insert(format!("klines:{}", BULK_OPEN_2 + 1), b"[]".to_vec());
    m.insert(format!("funding:{}", BULK_OPEN_2 + 1), b"[]".to_vec());
    let source1 = BinanceDataSource::new(FixtureBulk, FixtureRest { by_marker: m }, clock_run1);
    run_fetch_data(&source1, &store, &clock_run1, &args(false))
        .await
        .expect("run 1 (bulk)");
    let head1 = store.read_head(&btc(), Timeframe::M15).unwrap().unwrap();
    let series1 = store.read_snapshot(&btc(), Timeframe::M15, &head1).unwrap();
    assert_eq!(series1.candles.len(), 3, "bulk-only snapshot");

    // Run 2: HEAD present ⇒ subsequent run. The clock now exposes two newly-
    // closed candles. New data ⇒ a NEW data_version + HEAD moves (AC-2).
    let source2 = BinanceDataSource::new(
        FixtureBulk,
        FixtureRest::with_new_candles(),
        FakeClock::at(NOW_MS),
    );
    run_fetch_data(&source2, &store, &FakeClock::at(NOW_MS), &args(false))
        .await
        .expect("run 2 (incremental)");
    let head2 = store.read_head(&btc(), Timeframe::M15).unwrap().unwrap();
    assert_ne!(
        head1, head2,
        "incremental top-up mints a new data_version (AC-2)"
    );
    assert!(
        store.snapshot_exists(&btc(), Timeframe::M15, &head1),
        "prior snapshot retained (immutable)"
    );
    let series2 = store.read_snapshot(&btc(), Timeframe::M15, &head2).unwrap();
    assert_eq!(series2.candles.len(), 5, "3 bulk + 2 newly-closed");

    // Run 3: nothing newly closed ⇒ up-to-date NO-OP (HEAD unchanged), NOT error.
    let source3 =
        BinanceDataSource::new(FixtureBulk, FixtureRest::caught_up(), FakeClock::at(NOW_MS));
    run_fetch_data(&source3, &store, &FakeClock::at(NOW_MS), &args(false))
        .await
        .expect("run 3 (up-to-date no-op is not an error)");
    let head3 = store.read_head(&btc(), Timeframe::M15).unwrap().unwrap();
    assert_eq!(
        head2, head3,
        "up-to-date no-op leaves HEAD unchanged (AC-2)"
    );
}

// ---- AC-8: multi-tf partial failure → M15 ok, H4 fails, run returns Err -----

/// A source that succeeds for M15 (delegating to the fixture compose) but fails
/// `fetch_historical` for H4 (the injected failure, audit C4 / AC-8).
struct H4FailingSource {
    inner: BinanceDataSource<FixtureBulk, FixtureRest, FakeClock>,
}

impl MarketDataSource for H4FailingSource {
    async fn fetch_historical(
        &self,
        pair: &Pair,
        tf: Timeframe,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<CandleSeries, DataError> {
        if tf == Timeframe::H4 {
            return Err(DataError::Io("injected H4 bulk failure".to_string()));
        }
        self.inner
            .fetch_historical(pair, tf, start_ms, end_ms)
            .await
    }

    async fn fetch_incremental(
        &self,
        pair: &Pair,
        tf: Timeframe,
        since_ms: i64,
    ) -> Result<Vec<Candle>, DataError> {
        if tf == Timeframe::H4 {
            return Err(DataError::Io("injected H4 incremental failure".to_string()));
        }
        self.inner.fetch_incremental(pair, tf, since_ms).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac8_multi_tf_partial_failure_exits_non_zero_but_keeps_m15() {
    let (store, _tmp) = store();
    let source = H4FailingSource {
        inner: BinanceDataSource::new(
            FixtureBulk,
            FixtureRest::with_new_candles(),
            FakeClock::at(NOW_MS),
        ),
    };
    let args = FetchArgs {
        pair: "BTCUSDT".to_string(),
        tf: vec!["M15".to_string(), "H4".to_string()],
        years: Some(1),
        from: None,
        json: true,
    };

    // The process must return Err (the binary maps this to a non-zero exit, C4).
    let result = run_fetch_data(&source, &store, &FakeClock::at(NOW_MS), &args).await;
    assert!(result.is_err(), "any failing tf ⇒ non-zero exit (AC-8)");

    // M15 still wrote its snapshot + HEAD (independent per-tf, audit C4).
    let m15_head = store
        .read_head(&btc(), Timeframe::M15)
        .expect("read M15 HEAD")
        .expect("M15 HEAD set despite H4 failure");
    assert!(store.snapshot_exists(&btc(), Timeframe::M15, &m15_head));

    // H4 wrote nothing — no HEAD, no snapshot.
    assert!(
        store
            .read_head(&btc(), Timeframe::H4)
            .expect("read H4 HEAD")
            .is_none(),
        "failed H4 left no HEAD"
    );
}

// ---- Fix 4: no-data first run reports no snapshot path (was never written) --

/// A source whose bulk window AND incremental top-up both yield zero candles —
/// e.g. `--years 0` right after a UTC month rollover, before the first candle
/// closes. Drives `first_run` into the empty-candles branch.
struct EmptySource;

impl MarketDataSource for EmptySource {
    fn fetch_historical(
        &self,
        pair: &Pair,
        tf: Timeframe,
        _start_ms: i64,
        _end_ms: i64,
    ) -> impl Future<Output = Result<CandleSeries, DataError>> {
        std::future::ready(Ok(CandleSeries {
            pair: pair.clone(),
            timeframe: tf,
            version: DataVersion::new("empty"),
            candles: Vec::new(),
        }))
    }

    fn fetch_incremental(
        &self,
        _pair: &Pair,
        _tf: Timeframe,
        _since_ms: i64,
    ) -> impl Future<Output = Result<Vec<Candle>, DataError>> {
        std::future::ready(Ok(Vec::new()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fix4_no_data_first_run_reports_empty_path_and_writes_nothing() {
    let (store, tmp) = store();
    let clock = FakeClock::at(NOW_MS);

    let outcome = ensure_one_tf(
        &EmptySource,
        &store,
        &clock,
        &btc(),
        Timeframe::M15,
        years_window_start_ms(NOW_MS, 0),
    )
    .await;
    let TfOutcome::Ok(summary) = outcome else {
        panic!("no-data first run must be Ok (up-to-date no-op), not a failure");
    };

    assert_eq!(summary.action, "up-to-date", "no-data ⇒ up-to-date");
    assert_eq!(summary.candle_count, 0, "no candles");
    assert_eq!(
        summary.path, "",
        "no snapshot was written ⇒ path must be empty, not a nonexistent Parquet"
    );

    // And no snapshot/HEAD landed on disk.
    assert!(
        store
            .read_head(&btc(), Timeframe::M15)
            .expect("read HEAD ok")
            .is_none(),
        "no-data run must not set HEAD"
    );
    let candles_dir = tmp.path().join("candles");
    let wrote_parquet = walk_has_parquet(&candles_dir);
    assert!(
        !wrote_parquet,
        "no-data run must not write any .parquet file"
    );
}

/// Recursively check whether any `.parquet` file exists under `dir` (helper for
/// the no-data assertion). Returns false if the dir does not exist.
fn walk_has_parquet(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if walk_has_parquet(&path) {
                return true;
            }
        } else if path.extension().is_some_and(|e| e == "parquet") {
            return true;
        }
    }
    false
}

// ---- AC-7 (reinforce): HEAD written AFTER snapshot; orphan does not move it -

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac7_orphaned_snapshot_does_not_move_head_next_run_reads_prior() {
    let (store, _tmp) = store();
    // First run sets a real HEAD.
    let source = BinanceDataSource::new(
        FixtureBulk,
        FixtureRest::with_new_candles(),
        FakeClock::at(NOW_MS),
    );
    run_fetch_data(&source, &store, &FakeClock::at(NOW_MS), &args(false))
        .await
        .expect("first run");
    let head_before = store.read_head(&btc(), Timeframe::M15).unwrap().unwrap();

    // Simulate a crash-between: a NEW snapshot file lands but HEAD is never moved.
    let orphan = DataVersion::new("0123456789abcdef");
    let orphan_path = store.snapshot_path(&btc(), Timeframe::M15, &orphan);
    std::fs::create_dir_all(orphan_path.parent().unwrap()).unwrap();
    std::fs::write(&orphan_path, b"orphan-snapshot").unwrap();

    // Next read of HEAD is still the prior version (audit C1 / AC-7).
    let head_after = store.read_head(&btc(), Timeframe::M15).unwrap().unwrap();
    assert_eq!(head_before, head_after, "orphan did not move HEAD");
    assert!(
        orphan_path.exists(),
        "orphan snapshot retained, GC-able later"
    );
}

// ---- #289: an unpublished month that just ended must not block fetch-data ----

/// 2023-12-02 00:00:00 UTC — the second day of a month. With `years: 1` the bulk
/// window is 2022-12 ..= 2023-11 and 2023-11 is the month that just ended.
const EARLY_MONTH_NOW_MS: i64 = 1_701_475_200_000;
/// 2022-12-01 and 2023-12-01 00:00:00 UTC: the half-open bulk window `first_run`
/// asks for at `EARLY_MONTH_NOW_MS`.
const LAG_WINDOW_START_MS: i64 = 1_669_852_800_000;
const LAG_WINDOW_END_MS: i64 = 1_701_388_800_000;
/// 2023-11-01 00:00:00 UTC, the first instant of the month that just ended.
const NOV_1_MS: i64 = 1_698_796_800_000;
// The two candles every loaded bulk month carries: the last two of October.
const LAG_BULK_OPEN_0: i64 = NOV_1_MS - 2 * M15;
const LAG_BULK_OPEN_1: i64 = NOV_1_MS - M15;

/// A bulk [`MonthSource`] mimicking the publication lag of `data.binance.vision`:
/// every month loads the same `opens` candles except the listed `absent`
/// months, which are a `404`. `absent_hits` counts the `404`s it served.
struct LaggingBulk {
    absent: Vec<(i32, u32)>,
    opens: Vec<i64>,
    absent_hits: Arc<AtomicUsize>,
}

impl LaggingBulk {
    fn new(absent: Vec<(i32, u32)>, opens: &[i64]) -> Self {
        Self {
            absent,
            opens: opens.to_vec(),
            absent_hits: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl MonthSource for LaggingBulk {
    fn load_month(
        &self,
        _pair: &Pair,
        _tf: Timeframe,
        year: i32,
        month: u32,
    ) -> impl Future<Output = Result<MonthOutcome, DataError>> {
        std::future::ready(if self.absent.contains(&(year, month)) {
            self.absent_hits.fetch_add(1, Ordering::SeqCst);
            Ok(MonthOutcome::Absent)
        } else {
            Ok(MonthOutcome::Loaded(MonthData {
                candles: self.opens.iter().copied().map(bulk_candle).collect(),
                funding: vec![],
            }))
        })
    }
}

fn lag_bulk(absent: Vec<(i32, u32)>) -> LaggingBulk {
    LaggingBulk::new(absent, &[LAG_BULK_OPEN_0, LAG_BULK_OPEN_1])
}

fn assert_failed_with(outcome: &TfOutcome, expected: &str) {
    match outcome {
        TfOutcome::Failed { error, .. } => {
            assert!(error.contains(expected), "unexpected error: {error}");
        }
        TfOutcome::Ok(_) => panic!("expected the run to fail with: {expected}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_289_unpublished_month_that_just_ended_is_covered_by_rest_top_up() {
    // The month that just ended (2023-11) has no archive yet; every earlier
    // month loads the last two October candles. The REST top-up must supply
    // the whole skipped month, over three pages (two with candles, one empty).
    let (store, _tmp) = store();
    let bulk = lag_bulk(vec![(2023, 11)]);
    let absent_hits = Arc::clone(&bulk.absent_hits);
    let source = BinanceDataSource::new(
        bulk,
        FixtureRest::paginated_after_month_lag(),
        FakeClock::at(EARLY_MONTH_NOW_MS),
    );

    run_fetch_data(
        &source,
        &store,
        &FakeClock::at(EARLY_MONTH_NOW_MS),
        &args(false),
    )
    .await
    .expect("an unpublished final bulk month must not fail fetch-data (#289)");

    assert_eq!(
        absent_hits.load(Ordering::SeqCst),
        1,
        "the bulk source must have been asked for the unpublished month"
    );
    let head = store
        .read_head(&btc(), Timeframe::M15)
        .expect("read HEAD")
        .expect("HEAD set");
    let series = store
        .read_snapshot(&btc(), Timeframe::M15, &head)
        .expect("read snapshot");
    let opens: Vec<i64> = series.candles.iter().map(|c| c.open_time).collect();
    // The two bulk candles, then five candles inside the skipped month — every
    // one of them from REST, across two pages.
    let expected: Vec<i64> = [LAG_BULK_OPEN_0, LAG_BULK_OPEN_1]
        .into_iter()
        .chain((0..5).map(|i| NOV_1_MS + i * M15))
        .collect();
    assert_eq!(opens, expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_289_loader_is_fail_closed_unless_the_caller_names_the_lag_month() {
    // The bulk loader's default stays the audit-C2 refusal. Only a caller that
    // names the month it has a REST top-up for gets the final-month skip.
    let source = BinanceDataSource::new(
        lag_bulk(vec![(2023, 11)]),
        FixtureRest::paginated_after_month_lag(),
        FakeClock::at(EARLY_MONTH_NOW_MS),
    );
    let c2 = DataError::Io("expected month 2023-11 absent after the pair was listed".into());

    let err = source
        .fetch_historical(
            &btc(),
            Timeframe::M15,
            LAG_WINDOW_START_MS,
            LAG_WINDOW_END_MS,
        )
        .await
        .expect_err("no opt-in: a final absent month is a coverage hole");
    assert_eq!(err, c2);

    // Naming some other month does not exempt this one.
    let err = source
        .fetch_historical_lagging(
            &btc(),
            Timeframe::M15,
            LAG_WINDOW_START_MS,
            LAG_WINDOW_END_MS,
            (2023, 10),
        )
        .await
        .expect_err("the named month is not the absent one");
    assert_eq!(err, c2);

    let series = source
        .fetch_historical_lagging(
            &btc(),
            Timeframe::M15,
            LAG_WINDOW_START_MS,
            LAG_WINDOW_END_MS,
            (2023, 11),
        )
        .await
        .expect("the named final month may be absent");
    assert_eq!(series.candles.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_289_interior_hole_still_fails_fetch_data() {
    // 2023-05 is absent but 2023-06.. load: a real coverage hole (audit C2).
    let (store, _tmp) = store();
    let source = BinanceDataSource::new(
        lag_bulk(vec![(2023, 5)]),
        FixtureRest::paginated_after_month_lag(),
        FakeClock::at(EARLY_MONTH_NOW_MS),
    );

    let outcome = ensure_one_tf(
        &source,
        &store,
        &FakeClock::at(EARLY_MONTH_NOW_MS),
        &btc(),
        Timeframe::M15,
        years_window_start_ms(EARLY_MONTH_NOW_MS, 1),
    )
    .await;
    assert_failed_with(
        &outcome,
        "expected month 2023-05 absent after the pair was listed",
    );
    assert!(
        store
            .read_head(&btc(), Timeframe::M15)
            .expect("read HEAD")
            .is_none(),
        "a failed run sets no HEAD"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_289_two_unpublished_trailing_months_still_fail_fetch_data() {
    let (store, _tmp) = store();
    let source = BinanceDataSource::new(
        lag_bulk(vec![(2023, 10), (2023, 11)]),
        FixtureRest::paginated_after_month_lag(),
        FakeClock::at(EARLY_MONTH_NOW_MS),
    );

    let outcome = ensure_one_tf(
        &source,
        &store,
        &FakeClock::at(EARLY_MONTH_NOW_MS),
        &btc(),
        Timeframe::M15,
        years_window_start_ms(EARLY_MONTH_NOW_MS, 1),
    )
    .await;
    assert_failed_with(
        &outcome,
        "expected month 2023-10 absent after the pair was listed",
    );
}

// ---- Regression: current incomplete month excluded from bulk (audit C5) -----

/// The live `--years 2` run failed because `first_run` included the current
/// (incomplete) month in the bulk range: `data.binance.vision` has no archive for
/// it, so WI-02 raised "expected month absent after listing". [`LaggingBulk`]
/// reproduces that with the current month absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn regression_current_incomplete_month_excluded_from_bulk() {
    // first_run must bound bulk to COMPLETE months ([start, current_month)) and
    // leave the current month to the REST top-up (audit C5). If it asks the bulk
    // source for the current month, that month is Absent → "expected month absent
    // after listing" → the run errors. This is the live-demo regression.
    let (store, _tmp) = store();
    let now = Utc.timestamp_millis_opt(NOW_MS).single().unwrap();
    let bulk = LaggingBulk::new(
        vec![(now.year(), now.month())],
        &[BULK_OPEN_0, BULK_OPEN_1, BULK_OPEN_2],
    );
    // REST top-up at the bulk boundary returns nothing new (caught up).
    let mut m: HashMap<String, Vec<u8>> = HashMap::new();
    m.insert(format!("klines:{}", BULK_OPEN_2 + 1), b"[]".to_vec());
    m.insert(format!("funding:{}", BULK_OPEN_2 + 1), b"[]".to_vec());
    let source = BinanceDataSource::new(bulk, FixtureRest { by_marker: m }, FakeClock::at(NOW_MS));

    run_fetch_data(&source, &store, &FakeClock::at(NOW_MS), &args(false))
        .await
        .expect("current incomplete month must be excluded from bulk (audit C5)");

    let head = store
        .read_head(&btc(), Timeframe::M15)
        .expect("read HEAD")
        .expect("HEAD set");
    assert!(store.snapshot_exists(&btc(), Timeframe::M15, &head));
}

// ---- r4.s1.w2: `--from` and the backward backfill ---------------------------

/// 2023-06-01T00:00:00Z — the first run's requested start (month-floored).
const JUN_2023_MS: i64 = 1_685_577_600_000;
/// 2021-01-01T00:00:00Z — the backfill's requested start.
const JAN_2021_MS: i64 = 1_609_459_200_000;
/// 2023-11-01T00:00:00Z — the first millisecond of `NOW_MS`'s UTC month, the
/// bulk window's exclusive end for a first run at `NOW_MS` (audit C5).
const NOV_2023_MS: i64 = 1_698_796_800_000;

/// A scripted [`MarketDataSource`] that records every range/since it is asked
/// for and answers from per-call FIFO queues — the offline bulk + top-up stub
/// the `--from` backfill tests drive (never the network).
struct ScriptedSource {
    bulk_calls: Mutex<Vec<(i64, i64)>>,
    incremental_calls: Mutex<Vec<i64>>,
    bulk_replies: Mutex<std::collections::VecDeque<Vec<i64>>>,
    incremental_replies: Mutex<std::collections::VecDeque<Vec<i64>>>,
}

impl ScriptedSource {
    fn new(bulk_replies: Vec<Vec<i64>>, incremental_replies: Vec<Vec<i64>>) -> Self {
        Self {
            bulk_calls: Mutex::new(Vec::new()),
            incremental_calls: Mutex::new(Vec::new()),
            bulk_replies: Mutex::new(bulk_replies.into()),
            incremental_replies: Mutex::new(incremental_replies.into()),
        }
    }

    fn bulk_calls(&self) -> Vec<(i64, i64)> {
        lock(&self.bulk_calls).clone()
    }

    fn incremental_calls(&self) -> Vec<i64> {
        lock(&self.incremental_calls).clone()
    }
}

/// Lock a test stub's queue, tolerating poisoning: a stub has no invariant to
/// protect, and the canonical crate does not depend on `parking_lot`, so the
/// guard is recovered rather than unwrapped (`rs-parking-lot`).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl MarketDataSource for ScriptedSource {
    fn fetch_historical(
        &self,
        pair: &Pair,
        tf: Timeframe,
        start_ms: i64,
        end_ms: i64,
    ) -> impl Future<Output = Result<CandleSeries, DataError>> {
        lock(&self.bulk_calls).push((start_ms, end_ms));
        let opens = lock(&self.bulk_replies).pop_front().unwrap_or_default();
        let series = CandleSeries {
            pair: pair.clone(),
            timeframe: tf,
            version: DataVersion::new("scripted"),
            candles: opens.into_iter().map(bulk_candle).collect(),
        };
        std::future::ready(Ok(series))
    }

    fn fetch_incremental(
        &self,
        _pair: &Pair,
        _tf: Timeframe,
        since_ms: i64,
    ) -> impl Future<Output = Result<Vec<Candle>, DataError>> {
        lock(&self.incremental_calls).push(since_ms);
        let opens = lock(&self.incremental_replies)
            .pop_front()
            .unwrap_or_default();
        std::future::ready(Ok(opens.into_iter().map(bulk_candle).collect()))
    }
}

/// Count every `.parquet` file under `dir` (recursively) — "commits once" means
/// exactly one new snapshot file beside the retained prior one.
fn count_parquet(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                count_parquet(&path)
            } else {
                usize::from(path.extension().is_some_and(|e| e == "parquet"))
            }
        })
        .sum()
}

/// AC-5 (r4.s1.w2): a `--from` earlier than HEAD's first candle backfills the
/// missing earlier months through the same bulk path — the range ends at the
/// prior first candle's month (`+ 1 ms`, so the inclusive-end month walk names
/// that month) — tops up to now, and commits ONE new snapshot: HEAD moves to a
/// new `data_version` and the prior file stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_from_backfills_the_missing_months_and_moves_head_once() {
    let (store, tmp) = store();
    let clock = FakeClock::at(NOW_MS);
    let source = ScriptedSource::new(
        vec![
            // Run 1 (first run from 2023-06-01): the 2023-06 month.
            vec![JUN_2023_MS, JUN_2023_MS + M15],
            // Run 2 (backfill from 2021-01-01): the earlier months, ending on
            // the prior first candle's own candle.
            vec![JAN_2021_MS, JAN_2021_MS + M15, JUN_2023_MS],
        ],
        vec![vec![], vec![]],
    );

    run_fetch_data(&source, &store, &clock, &from_args("2023-06-01"))
        .await
        .expect("the first --from run writes its snapshot");
    let head1 = store
        .read_head(&btc(), Timeframe::M15)
        .expect("read HEAD")
        .expect("HEAD set after the first run");

    // The backfill: `ensure_one_tf` answers the summary the `--json` renderer
    // prints, so the action string is assertable here.
    let outcome = ensure_one_tf(&source, &store, &clock, &btc(), Timeframe::M15, JAN_2021_MS).await;
    let TfOutcome::Ok(summary) = outcome else {
        panic!("the backfill run must succeed");
    };

    // The bulk ranges: run 1 = [2023-06-01, month start of now); run 2 = the
    // requested start up to the prior first candle's month + 1 ms.
    let bulk = source.bulk_calls();
    assert_eq!(bulk.len(), 2, "one bulk call per run: {bulk:?}");
    assert_eq!(
        bulk[0],
        (JUN_2023_MS, NOV_2023_MS),
        "run 1's --from bulk window"
    );
    assert_eq!(
        bulk[1],
        (JAN_2021_MS, JUN_2023_MS + 1),
        "the backfill bulk ends at the prior first candle's month + 1 ms"
    );
    // The backfill's top-up starts at the merged last candle (the prior
    // snapshot's own tail), not at the requested start.
    assert_eq!(
        source.incremental_calls()[1],
        JUN_2023_MS + M15,
        "the backfill tops up to now from the merged series' last candle"
    );

    assert_eq!(summary.action, "backfill", "the action names the backfill");
    assert_eq!(summary.first_open_ms, Some(JAN_2021_MS));

    let head2 = store
        .read_head(&btc(), Timeframe::M15)
        .expect("read HEAD")
        .expect("HEAD set after the backfill");
    assert_ne!(head1, head2, "the backfill mints a new data_version");
    assert!(
        store.snapshot_exists(&btc(), Timeframe::M15, &head1),
        "the prior snapshot file is kept (immutable)"
    );
    let series = store
        .read_snapshot(&btc(), Timeframe::M15, &head2)
        .expect("read backfilled snapshot");
    let opens: Vec<i64> = series.candles.iter().map(|c| c.open_time).collect();
    assert_eq!(
        opens,
        vec![
            JAN_2021_MS,
            JAN_2021_MS + M15,
            JUN_2023_MS,
            JUN_2023_MS + M15
        ],
        "the new snapshot covers the whole span, deduped on open_time"
    );
    assert_eq!(
        count_parquet(&tmp.path().join("candles")),
        2,
        "exactly one new snapshot file beside the retained prior one"
    );
}

/// AC-5's other half (reading 3): a `--from` that is **not** earlier than
/// HEAD's first candle keeps today's incremental path — no bulk call, the
/// top-up from the prior's last candle, and the `up-to-date` no-op when
/// nothing newly closed (HEAD unchanged, no new version).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_from_later_than_head_stays_incremental() {
    let (store, _tmp) = store();
    let clock = FakeClock::at(NOW_MS);
    let source = ScriptedSource::new(
        vec![vec![JUN_2023_MS, JUN_2023_MS + M15]],
        vec![vec![], vec![]],
    );

    run_fetch_data(&source, &store, &clock, &from_args("2023-06-01"))
        .await
        .expect("the first --from run");
    let head1 = store
        .read_head(&btc(), Timeframe::M15)
        .expect("read HEAD")
        .expect("HEAD set");

    let outcome = ensure_one_tf(
        &source,
        &store,
        &clock,
        &btc(),
        Timeframe::M15,
        JUN_2023_MS + M15,
    )
    .await;
    let TfOutcome::Ok(summary) = outcome else {
        panic!("a later start keeps the incremental path");
    };
    assert_eq!(summary.action, "up-to-date", "nothing newly closed ⇒ no-op");
    assert_eq!(
        source.bulk_calls().len(),
        1,
        "a later start never calls the bulk source again"
    );
    assert_eq!(
        store.read_head(&btc(), Timeframe::M15).expect("read HEAD"),
        Some(head1),
        "the no-op leaves HEAD unchanged"
    );
}

// ---------------------------------------------------------------------------
// r4.s1.w4 / AC-3: `fetch_fills_interior_gaps_*` — the archive-hole fill
// ---------------------------------------------------------------------------

/// The gapped prior snapshot's base instant: six M15 candles at `base + k·M15`
/// for `k ∈ {0,1,2,5,7}`, i.e. TWO interior holes — `[3, 5)` (two candles) and
/// `[6, 7)` (one) — exactly the SOL/XRP archive-hole shape.
const GAP_BASE_MS: i64 = 1_699_900_000_000;

fn gap_open(k: i64) -> i64 {
    GAP_BASE_MS + k * M15
}

/// A scripted REST stub keyed by the `FixtureRest` marker (`klines:<startTime>`
/// / `funding:<startTime>`), with the caller's own page set — the fill's bounded
/// walk carries `endTime`, which the marker parser ignores, so one page per
/// range is all it needs.
struct FillRest {
    by_marker: HashMap<String, Vec<u8>>,
}

impl FillRest {
    /// The top-up is caught up (nothing newly closed) and each hole's bounded
    /// fetch is served at its own marker, with funding on the first filled
    /// candle.
    fn with_two_holes() -> Self {
        let mut m: HashMap<String, Vec<u8>> = HashMap::new();
        // The incremental top-up: nothing newer than the prior's last candle.
        m.insert(format!("klines:{}", gap_open(7) + 1), b"[]".to_vec());
        // Hole 1: the candles at k = 3 and 4, with a funding event on k = 3.
        m.insert(
            format!("klines:{}", gap_open(3)),
            klines_json(&[
                (gap_open(3), gap_open(3) + M15 - 1),
                (gap_open(4), gap_open(4) + M15 - 1),
            ]),
        );
        m.insert(
            format!("funding:{}", gap_open(3)),
            funding_json(&[(gap_open(3), "0.00012500")]),
        );
        // The bounded walk's follow-up page for hole 1: nothing left before the
        // bound (what the endpoint answers once the last in-range candle is
        // behind the walk).
        m.insert(format!("klines:{}", gap_open(4) + 1), b"[]".to_vec());
        // Hole 2: the candle at k = 6, no funding.
        m.insert(
            format!("klines:{}", gap_open(6)),
            klines_json(&[(gap_open(6), gap_open(6) + M15 - 1)]),
        );
        m.insert(format!("klines:{}", gap_open(6) + 1), b"[]".to_vec());
        m.insert(format!("funding:{}", gap_open(6)), b"[]".to_vec());
        Self { by_marker: m }
    }
}

impl PageSource for FillRest {
    fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, DataError>> + Send {
        let body = self.by_marker.get(&FixtureRest::marker_for(url)).cloned();
        async move { body.ok_or_else(|| DataError::Io(format!("unscripted REST URL: {url}"))) }
    }
}

/// r4.s1.w4 / AC-3: a snapshot with two interior archive holes is filled from
/// REST — one bounded incremental fetch per range — into ONE new snapshot: the
/// new `data_version` becomes `HEAD`, the old snapshot file stays untouched (and
/// still carries its holes), the summary reports the filled count, and
/// `gap_count` reports what is left (zero). The stub is local; no network.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_fills_interior_gaps_from_rest_into_one_new_snapshot() {
    let (store, _tmp) = store();
    let pair = btc();
    let clock = FakeClock::at(NOW_MS);

    // The gapped prior: k ∈ {0,1,2,5,7}.
    let prior_candles: Vec<Candle> = [0, 1, 2, 5, 7]
        .iter()
        .map(|k| bulk_candle(gap_open(*k)))
        .collect();
    let prior = store
        .commit(&pair, Timeframe::M15, prior_candles)
        .expect("a gapped prior commits (gaps are reported, not rejected)");
    let prior_version = prior.series.version.clone();
    let prior_path = prior
        .storage_location
        .clone()
        .expect("the prior has a path");
    assert_eq!(
        prior.series.validate().expect("the prior validates").len(),
        2,
        "the fixture starts with exactly two interior holes"
    );

    let source = BinanceDataSource::new(
        FixtureBulk,
        FillRest::with_two_holes(),
        FakeClock::at(NOW_MS),
    );
    let outcome = ensure_one_tf(&source, &store, &clock, &pair, Timeframe::M15, gap_open(0)).await;
    let TfOutcome::Ok(summary) = outcome else {
        panic!("the fill run succeeds");
    };

    assert_eq!(summary.filled_candle_count, 3, "two + one candles filled");
    assert_eq!(summary.gap_count, 0, "nothing is left to report");
    assert_eq!(summary.candle_count, 8, "five prior + three filled");
    assert_eq!(summary.action, "backfill", "the fill wrote a new snapshot");
    assert_ne!(
        summary.data_version,
        prior_version.as_str(),
        "the fill wrote a new content-hashed version"
    );

    // HEAD moved to the filled snapshot, which is contiguous.
    let head = store
        .load_head(&pair, Timeframe::M15)
        .expect("read HEAD")
        .expect("HEAD set");
    assert_eq!(head.series.version.as_str(), summary.data_version);
    assert!(
        head.series
            .validate()
            .expect("the filled series validates")
            .is_empty(),
        "the filled snapshot is gap-free"
    );
    assert_eq!(head.series.candles.len(), 8);

    // The filled candles carry their funding (the fill goes through the
    // funding-aware incremental path).
    let filled = head
        .series
        .candles
        .iter()
        .find(|c| c.open_time == gap_open(3))
        .expect("the first filled candle is present");
    assert_eq!(
        filled.funding_rate,
        Some(dec("0.00012500")),
        "the fill stamps funding on the filled candles"
    );

    // The old snapshot file stays, and the old version still reads back with
    // its holes — a stored snapshot is never mutated.
    assert!(
        std::path::Path::new(&prior_path).exists(),
        "the prior snapshot file stays on disk"
    );
    let old = store
        .load_version(&pair, Timeframe::M15, &prior_version)
        .expect("the prior version still loads");
    assert_eq!(
        old.series.candles.len(),
        5,
        "the prior content is untouched"
    );
    assert_eq!(
        old.series
            .validate()
            .expect("the prior still validates")
            .len(),
        2,
        "the prior still carries its two holes"
    );
}

/// r4.s1.w4 / AC-3: a gap REST cannot fill stays a gap — reported, never
/// invented — while the rest of the run still lands: one hole filled, one left,
/// `gap_count` 1, and the snapshot still refuses at `load_series`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_leaves_an_unfillable_interior_gap_reported() {
    let (store, _tmp) = store();
    let pair = btc();
    let clock = FakeClock::at(NOW_MS);

    let prior_candles: Vec<Candle> = [0, 1, 2, 5, 7]
        .iter()
        .map(|k| bulk_candle(gap_open(*k)))
        .collect();
    store
        .commit(&pair, Timeframe::M15, prior_candles)
        .expect("a gapped prior commits");

    // Hole 1's range answers with an EMPTY page (REST has nothing there); hole
    // 2 is served normally.
    let mut m: HashMap<String, Vec<u8>> = HashMap::new();
    m.insert(format!("klines:{}", gap_open(7) + 1), b"[]".to_vec());
    m.insert(format!("klines:{}", gap_open(3)), b"[]".to_vec());
    m.insert(
        format!("klines:{}", gap_open(6)),
        klines_json(&[(gap_open(6), gap_open(6) + M15 - 1)]),
    );
    m.insert(format!("klines:{}", gap_open(6) + 1), b"[]".to_vec());
    m.insert(format!("funding:{}", gap_open(6)), b"[]".to_vec());
    let source = BinanceDataSource::new(
        FixtureBulk,
        FillRest { by_marker: m },
        FakeClock::at(NOW_MS),
    );

    let outcome = ensure_one_tf(&source, &store, &clock, &pair, Timeframe::M15, gap_open(0)).await;
    let TfOutcome::Ok(summary) = outcome else {
        panic!("the run still succeeds around an unfillable hole");
    };
    assert_eq!(summary.filled_candle_count, 1, "only hole 2 was fillable");
    assert_eq!(
        summary.gap_count, 1,
        "the unfillable hole is still reported"
    );
    assert_eq!(summary.candle_count, 6, "five prior + one filled");

    let head = store
        .load_head(&pair, Timeframe::M15)
        .expect("read HEAD")
        .expect("HEAD set");
    let remaining = head.series.validate().expect("the series validates");
    assert_eq!(remaining.len(), 1, "the hole stays in the written snapshot");
    assert_eq!(
        remaining[0].expected,
        gap_open(3),
        "and it is the right hole"
    );
}
