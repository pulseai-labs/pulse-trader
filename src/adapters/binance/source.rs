//! `BinanceDataSource` — the concrete [`MarketDataSource`] that composes WI-02
//! bulk ingest, WI-03 REST top-up, and the WI-03 [`Clock`] behind the WI-01 port
//! (NFR-9, AC-6).
//!
//! The struct is **generic over its two transport seams + clock** so the full
//! compose path runs offline over recorded fixtures + a [`FakeClock`] (audit C2):
//! `M: MonthSource` (bulk archives), `P: PageSource` (REST pages), `C: Clock`.
//! Production wires the live seams via [`BinanceDataSource::live`]; the
//! integration suite injects fixture seams. Either way the orchestration (the
//! CLI in [`crate::cli`]) only ever sees the [`MarketDataSource`] trait — it
//! never names the concrete type (AC-6 swap test).
//!
//! - [`MarketDataSource::fetch_historical`] → bulk over the month window via
//!   [`ingest_window_with_lag`], fail-closed; [`MarketDataSource::fetch_historical_lagging`]
//!   is the same with the caller's one named publication-lag month left to the
//!   REST top-up (#289).
//! - [`MarketDataSource::fetch_incremental`] → REST top-up via [`top_up_with`],
//!   returning only the **new closed** candles (the merge onto the prior snapshot
//!   is the orchestration's job, via [`crate::adapters::store::CandleStore`]).
//!
//! The held [`Clock`] supplies the closed-candle cutoff (audit C5) so the
//! still-forming final kline is never persisted.

use std::future::Future;

use crate::domain::MarketDataSource;
use crate::domain::{Candle, CandleSeries, Clock, DataError, DataVersion, Pair, Timeframe};

use super::incremental::{RestPageSource, fetch_incremental_with};
use super::{BulkMonthSource, MonthSource, PageSource, ingest_window_with_lag};

/// A [`MarketDataSource`] backed by Binance USD-M Futures: bulk monthly archives
/// (`M`) for history + REST pages (`P`) for the incremental top-up, with a
/// [`Clock`] (`C`) supplying the closed-candle cutoff.
pub struct BinanceDataSource<M, P, C> {
    bulk: M,
    pages: P,
    clock: C,
}

impl<M, P, C> BinanceDataSource<M, P, C> {
    /// Compose a source from explicit seams (the offline-testable constructor:
    /// the integration suite injects a fixture [`MonthSource`] + [`PageSource`] +
    /// [`FakeClock`](crate::domain::Clock)).
    #[must_use]
    pub fn new(bulk: M, pages: P, clock: C) -> Self {
        Self { bulk, pages, clock }
    }
}

impl<M, P, C> BinanceDataSource<M, P, C>
where
    M: MonthSource + Sync,
    P: PageSource + Sync,
    C: Clock + Sync,
{
    /// The bulk window for `[start_ms, end_ms)`: a list of `(year, month)` months
    /// ingested through [`ingest_window_with_lag`]. A placeholder version is
    /// carried; the persist layer re-derives the content-hash `data_version`
    /// (audit C1, WI-04). `publication_lag_month` is the caller's opt-in (#289);
    /// `None` is the fail-closed audit-C2 behaviour.
    fn bulk_window(
        &self,
        pair: &Pair,
        tf: Timeframe,
        start_ms: i64,
        end_ms: i64,
        publication_lag_month: Option<(i32, u32)>,
    ) -> impl Future<Output = Result<CandleSeries, DataError>> + Send {
        let months = months_in_range(start_ms, end_ms);
        let version = DataVersion::new("pending");
        let pair = pair.clone();
        async move {
            let (series, _gaps) = ingest_window_with_lag(
                &self.bulk,
                &pair,
                tf,
                &version,
                &months,
                publication_lag_month,
            )
            .await?;
            Ok(series)
        }
    }
}

impl<C: Clock> BinanceDataSource<BulkMonthSource, RestPageSource, C> {
    /// Wire the **live** Binance seams (bulk `data.binance.vision` + REST
    /// `fapi.binance.com`) with the caller-supplied production [`Clock`]
    /// ([`SystemClock`](crate::domain::Clock), audit C4). This is the only path
    /// that touches the network; the offline suite uses [`Self::new`] instead.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Io`] if either HTTP client cannot be built.
    pub fn live(clock: C) -> Result<Self, DataError> {
        Ok(Self {
            bulk: BulkMonthSource::new()?,
            pages: RestPageSource::new()?,
            clock,
        })
    }
}

impl<M, P, C> MarketDataSource for BinanceDataSource<M, P, C>
where
    M: MonthSource + Sync,
    P: PageSource + Sync,
    C: Clock + Sync,
{
    fn fetch_historical(
        &self,
        pair: &Pair,
        tf: Timeframe,
        start_ms: i64,
        end_ms: i64,
    ) -> impl Future<Output = Result<CandleSeries, DataError>> + Send {
        self.bulk_window(pair, tf, start_ms, end_ms, None)
    }

    fn fetch_historical_lagging(
        &self,
        pair: &Pair,
        tf: Timeframe,
        start_ms: i64,
        end_ms: i64,
        publication_lag_month: (i32, u32),
    ) -> impl Future<Output = Result<CandleSeries, DataError>> + Send {
        self.bulk_window(pair, tf, start_ms, end_ms, Some(publication_lag_month))
    }

    fn fetch_incremental(
        &self,
        pair: &Pair,
        tf: Timeframe,
        since_ms: i64,
    ) -> impl Future<Output = Result<Vec<Candle>, DataError>> + Send {
        // Closed-candle cutoff comes from the held clock (audit C5); funding is
        // fetched one ms past the snapshot boundary and stamped on NEW candles
        // only — a funding event already applied to the prior boundary candle
        // (open_time <= since_ms) never lands on a new candle (open_time >
        // since_ms), so there is no double-application (grill rule).
        let pair = pair.clone();
        async move {
            fetch_incremental_with(&self.pages, &self.clock, &pair, tf, since_ms, since_ms + 1)
                .await
        }
    }
}

/// Decompose a half-open `[start_ms, end_ms)` epoch-ms range into the ordered
/// list of `(year, month)` calendar months it spans (UTC). The window is the
/// unit [`ingest_window_with_lag`] consults the [`MonthSource`] over.
///
/// Months are produced inclusive of the month containing `start_ms` up to and
/// including the month containing `end_ms - 1`. An empty or inverted range
/// yields no months.
fn months_in_range(start_ms: i64, end_ms: i64) -> Vec<(i32, u32)> {
    use chrono::{Datelike, TimeZone, Utc};
    if end_ms <= start_ms {
        return Vec::new();
    }
    let start = Utc.timestamp_millis_opt(start_ms).single();
    let last = Utc.timestamp_millis_opt(end_ms - 1).single();
    let (Some(start), Some(last)) = (start, last) else {
        return Vec::new();
    };

    let mut months = Vec::new();
    let (mut y, mut m) = (start.year(), start.month());
    let (ly, lm) = (last.year(), last.month());
    while (y, m) <= (ly, lm) {
        months.push((y, m));
        if m == 12 {
            y += 1;
            m = 1;
        } else {
            m += 1;
        }
    }
    months
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{BinanceDataSource, months_in_range};
    use crate::adapters::binance::{
        FundingEvent, MonthData, MonthOutcome, MonthSource, PageSource,
    };
    use crate::adapters::clock::FakeClock;
    use crate::domain::{
        Candle, CandleSeries, DataError, DataVersion, MarketDataSource, Pair, Timeframe,
    };
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::future::Future;

    const M15: i64 = 900_000;

    fn btc() -> Pair {
        Pair::new("BTCUSDT")
    }

    fn candle(open_time: i64) -> Candle {
        Candle {
            open_time,
            close_time: open_time + M15 - 1,
            open: Decimal::ONE,
            high: Decimal::ONE,
            low: Decimal::ONE,
            close: Decimal::ONE,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    /// A bulk source that returns one month of two candles for any month asked.
    struct OneMonthBulk;
    impl MonthSource for OneMonthBulk {
        fn load_month(
            &self,
            _pair: &Pair,
            _tf: Timeframe,
            _year: i32,
            _month: u32,
        ) -> impl Future<Output = Result<MonthOutcome, DataError>> {
            std::future::ready(Ok(MonthOutcome::Loaded(MonthData {
                candles: vec![candle(0), candle(M15)],
                funding: vec![FundingEvent {
                    calc_time: 0,
                    rate: Decimal::new(1, 4),
                }],
            })))
        }
    }

    /// A scripted REST page source keyed by exact URL.
    struct ScriptedPages(HashMap<String, Vec<u8>>);
    impl PageSource for ScriptedPages {
        fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, DataError>> + Send {
            let body = self.0.get(url).cloned();
            async move { body.ok_or_else(|| DataError::Io(format!("unscripted: {url}"))) }
        }
    }

    // ---- AC-6: BinanceDataSource satisfies MarketDataSource ----------------

    /// Generic consumption proves the orchestration depends on the PORT, not the
    /// concrete type (NFR-9): if the bound were not satisfied this would not
    /// compile.
    async fn first_via<S: MarketDataSource>(source: &S) -> Result<CandleSeries, DataError> {
        source
            .fetch_historical(&btc(), Timeframe::M15, 0, M15)
            .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binance_source_satisfies_the_port_for_historical() {
        let source = BinanceDataSource::new(
            OneMonthBulk,
            ScriptedPages(HashMap::new()),
            FakeClock::at(0),
        );
        let series = first_via(&source).await.expect("historical via port");
        assert_eq!(series.candles.len(), 2);
        assert_eq!(series.candles[0].open_time, 0);
        // Funding stamped on the on-boundary candle (sparse).
        assert_eq!(series.candles[0].funding_rate, Some(Decimal::new(1, 4)));
    }

    // ---- AC-6/AC-1: incremental via the port returns new CLOSED candles ----

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incremental_via_the_port_drops_the_forming_kline() {
        // since = 0; one closed candle at M15, one still-forming at 2*M15.
        let mut pages = HashMap::new();
        let klines = format!(
            "[[{o1},\"1\",\"1\",\"1\",\"1\",\"1\",{c1},\"0\",0,\"0\",\"0\",\"0\"],\
              [{o2},\"1\",\"1\",\"1\",\"1\",\"1\",{c2},\"0\",0,\"0\",\"0\",\"0\"]]",
            o1 = M15,
            c1 = 2 * M15 - 1,
            o2 = 2 * M15,
            c2 = 3 * M15 - 1,
        );
        pages.insert(
            format!(
                "https://fapi.binance.com/fapi/v1/klines?symbol=BTCUSDT&interval=15m&startTime={}&limit=1500",
                1
            ),
            klines.into_bytes(),
        );
        pages.insert(
            format!(
                "https://fapi.binance.com/fapi/v1/klines?symbol=BTCUSDT&interval=15m&startTime={}&limit=1500",
                2 * M15 + 1
            ),
            b"[]".to_vec(),
        );
        pages.insert(
            "https://fapi.binance.com/fapi/v1/fundingRate?symbol=BTCUSDT&startTime=1&limit=1000"
                .to_string(),
            b"[]".to_vec(),
        );
        // now = 2*M15: the forming candle (close_time = 3*M15-1 >= now) is dropped.
        let source =
            BinanceDataSource::new(OneMonthBulk, ScriptedPages(pages), FakeClock::at(2 * M15));
        let new = source
            .fetch_incremental(&btc(), Timeframe::M15, 0)
            .await
            .expect("incremental via port");
        assert_eq!(new.len(), 1, "only the closed candle is returned");
        assert_eq!(new[0].open_time, M15);
    }

    // ---- AC-6: a fake source is swappable behind the same port -------------

    struct FakeSource;
    impl MarketDataSource for FakeSource {
        fn fetch_historical(
            &self,
            pair: &Pair,
            tf: Timeframe,
            _s: i64,
            _e: i64,
        ) -> impl Future<Output = Result<CandleSeries, DataError>> {
            std::future::ready(Ok(CandleSeries {
                pair: pair.clone(),
                timeframe: tf,
                version: DataVersion::new("fake"),
                candles: Vec::new(),
            }))
        }
        fn fetch_incremental(
            &self,
            _p: &Pair,
            _t: Timeframe,
            _s: i64,
        ) -> impl Future<Output = Result<Vec<Candle>, DataError>> {
            std::future::ready(Ok(Vec::new()))
        }
    }

    #[tokio::test]
    async fn a_fake_source_swaps_in_behind_the_same_port() {
        let series = first_via(&FakeSource).await.expect("fake via port");
        assert!(series.candles.is_empty());
    }

    // ---- months_in_range decomposition ------------------------------------

    #[test]
    fn months_in_range_spans_calendar_months_utc() {
        use chrono::{TimeZone, Utc};
        let start = Utc.with_ymd_and_hms(2023, 11, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2024, 2, 1, 0, 0, 0).unwrap();
        let months = months_in_range(start.timestamp_millis(), end.timestamp_millis());
        assert_eq!(months, vec![(2023, 11), (2023, 12), (2024, 1)]);
    }

    #[test]
    fn months_in_range_single_month() {
        use chrono::{TimeZone, Utc};
        let start = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let end = Utc.with_ymd_and_hms(2024, 1, 15, 0, 0, 0).unwrap();
        let months = months_in_range(start.timestamp_millis(), end.timestamp_millis());
        assert_eq!(months, vec![(2024, 1)]);
    }

    #[test]
    fn months_in_range_empty_when_inverted() {
        assert!(months_in_range(100, 100).is_empty());
        assert!(months_in_range(200, 100).is_empty());
    }
}
