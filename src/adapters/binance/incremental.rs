//! REST incremental top-up (WI-1.1.1.03): fetch only the candles newer than an
//! existing snapshot's last candle via the Binance USD-M Futures REST API,
//! dropping the still-forming final kline.
//!
//! Decisions locked here (spec §3):
//! - **Endpoints:** klines `GET /fapi/v1/klines?symbol&interval&startTime&limit`
//!   (forward-paginated); funding `GET /fapi/v1/fundingRate?symbol&startTime&limit`.
//! - **Closed-candle cutoff (grill / audit C5):** a fetched kline is persisted
//!   iff `close_time < clock.now_ms()`. Binance always returns the in-progress
//!   final kline; it is dropped, never written into the immutable snapshot.
//! - **`close_time` convention (audit C2):** `close_time = open_time + interval − 1 ms`,
//!   recomputed from `open_time` so it matches WI-02 (bulk) exactly regardless of
//!   what the REST payload carries. A boundary fixture locks it.
//! - **Forward pagination (audit C3):** each page advances `startTime` to one
//!   millisecond past the last returned `open_time`; the loop stops when a page
//!   yields no candle with a *newer* `open_time` (caught up) — the
//!   only-forming-candle case therefore terminates with zero persisted candles.
//! - **Funding boundary (grill):** funding is fetched with `startTime` =
//!   last-applied-funding-ts + 1 and WI-02's [`stamp_funding`] is applied to the
//!   **new candles only**, so the boundary event is never double-applied.
//! - **Reuse, not duplicate:** the transport is WI-02's [`BinanceClient`]; this
//!   module adds only the REST request shaping, JSON decode, cutoff, and
//!   pagination. Tests drive a fixture [`PageSource`] offline.
//!
//! Output contract (audit C1): this module returns only the **new** candles; the
//! sibling [`merge`](super::merge) module appends+dedups them onto the prior
//! series and re-validates, yielding the full merged series (persisted as a new
//! `data_version` by WI-04/05 — never appended in place).

use std::future::Future;
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::Deserialize;

use crate::domain::{Candle, Clock, DataError, Pair, Timeframe};

use super::client::BinanceClient;
use super::funding::{FundingEvent, stamp_funding};

/// Base host for the USD-M futures REST API.
const FAPI_BASE: &str = "https://fapi.binance.com";

/// Max klines Binance returns per `/fapi/v1/klines` page (the API hard cap is
/// 1500 for futures; we request the cap so a top-up rarely needs > 1 page).
const KLINES_LIMIT: u32 = 1500;

/// Max funding rows per `/fapi/v1/fundingRate` page (API cap is 1000).
const FUNDING_LIMIT: usize = 1000;

/// A source of raw REST response bodies, keyed by URL. The production impl is a
/// thin wrapper over WI-02's [`BinanceClient`]; tests inject scripted pages so
/// pagination + the cutoff run entirely offline (spec §3 "recorded REST JSON
/// fixtures").
pub trait PageSource {
    /// GET `url`, returning the raw response body bytes.
    ///
    /// # Errors
    ///
    /// [`DataError`] if the transport fails after retries (mapped by the WI-02
    /// client) or — for the fixture source — the URL is unscripted.
    fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, DataError>> + Send;
}

/// Production [`PageSource`] over WI-02's retry/backoff [`BinanceClient`] — no
/// second HTTP client is introduced.
pub(crate) struct RestPageSource {
    client: BinanceClient,
}

impl RestPageSource {
    /// Build a source over a fresh [`BinanceClient`].
    ///
    /// # Errors
    ///
    /// [`DataError::Io`] if the underlying HTTP client cannot be built.
    pub(crate) fn new() -> Result<Self, DataError> {
        Ok(Self {
            client: BinanceClient::new()?,
        })
    }
}

impl PageSource for RestPageSource {
    fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, DataError>> + Send {
        self.client.fetch(url)
    }
}

/// The production [`ClosedBarSource`](crate::domain::ClosedBarSource) for the
/// live paper runtime (r3.s4.w3, E1): the incremental REST top-up under the
/// runtime's port. Generic over its [`PageSource`] and [`Clock`], so a test
/// drives the whole adapter offline against recorded pages.
///
/// Funding is fetched from one millisecond past the boundary candle, so an
/// event already stamped onto the recorded boundary bar never re-stamps a newer
/// one (`BinanceDataSource`'s convention, `source.rs`).
pub struct RestClosedBars<P, C> {
    pages: P,
    clock: C,
}

impl<P, C> RestClosedBars<P, C> {
    /// Compose over explicit seams (the offline-testable constructor).
    #[must_use]
    pub fn new(pages: P, clock: C) -> Self {
        Self { pages, clock }
    }
}

impl<C: Clock> RestClosedBars<RestPageSource, C> {
    /// The live source over a fresh [`RestPageSource`] (the sole
    /// network-touching seam — the production `pulse serve` path).
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Io`] if the underlying HTTP client cannot be built.
    pub fn live(clock: C) -> Result<Self, DataError> {
        Ok(Self {
            pages: RestPageSource::new()?,
            clock,
        })
    }
}

impl<P, C> crate::domain::ClosedBarSource for RestClosedBars<P, C>
where
    P: PageSource + Sync,
    C: Clock + Sync,
{
    fn closed_since(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        since_ms: i64,
    ) -> impl Future<Output = Result<Vec<Candle>, DataError>> + Send {
        let pair = pair.clone();
        let pages = &self.pages;
        let clock = &self.clock;
        async move {
            fetch_incremental_with(pages, clock, &pair, timeframe, since_ms, since_ms + 1).await
        }
    }
}

/// One raw funding row from `/fapi/v1/fundingRate`.
#[derive(Debug, Deserialize)]
struct RawFunding {
    #[serde(rename = "fundingTime")]
    funding_time: i64,
    #[serde(rename = "fundingRate")]
    funding_rate: Decimal,
}

/// Build the `/fapi/v1/klines` page URL for `(pair, tf)` starting at `start_ms`.
fn klines_url(pair: &Pair, tf: Timeframe, start_ms: i64) -> String {
    format!(
        "{FAPI_BASE}/fapi/v1/klines?symbol={}&interval={}&startTime={start_ms}&limit={KLINES_LIMIT}",
        pair.as_str(),
        tf.binance_interval(),
    )
}

/// Build the `/fapi/v1/klines` page URL for `pair` starting at `start_ms` and
/// bounded at `until_ms` when given (r4.s1.w4): the endpoint's `endTime` is
/// **inclusive**, so the bound is `until_ms - 1` — the page can then never
/// return a candle opening at or after the exclusive bound. `None` keeps the
/// unbounded top-up URL verbatim.
fn klines_url_until(pair: &Pair, tf: Timeframe, start_ms: i64, until_ms: Option<i64>) -> String {
    match until_ms {
        Some(until) => format!(
            "{FAPI_BASE}/fapi/v1/klines?symbol={}&interval={}&startTime={start_ms}&endTime={}&limit={KLINES_LIMIT}",
            pair.as_str(),
            tf.binance_interval(),
            until - 1,
        ),
        None => klines_url(pair, tf, start_ms),
    }
}

/// Build the `/fapi/v1/fundingRate` page URL for `pair` starting at `start_ms`.
fn funding_url(pair: &Pair, start_ms: i64) -> String {
    format!(
        "{FAPI_BASE}/fapi/v1/fundingRate?symbol={}&startTime={start_ms}&limit={FUNDING_LIMIT}",
        pair.as_str(),
    )
}

/// Decode a `/fapi/v1/klines` JSON body into [`Candle`]s, recomputing
/// `close_time = open_time + interval − 1 ms` (audit C2) and leaving
/// `funding_rate = None` (stamped later).
///
/// The REST payload is an array of 12-element heterogeneous arrays
/// `[open_time, open, high, low, close, volume, close_time, …]`. Only the
/// leading six fields are read; `close_time` (payload field 6) is **ignored**
/// and recomputed so the convention matches WI-02 bulk exactly regardless of
/// what the exchange returns (audit C2).
///
/// # Errors
///
/// [`DataError::Parse`] if the body is not an array of arrays, a row is shorter
/// than 6 fields, `open_time` is not an integer, or a price/volume field is not
/// a parseable decimal string.
fn decode_klines(body: &[u8], tf: Timeframe) -> Result<Vec<Candle>, DataError> {
    let rows: Vec<Vec<serde_json::Value>> = serde_json::from_slice(body)
        .map_err(|e| DataError::Parse(format!("malformed klines JSON: {e}")))?;
    let step = tf.duration_ms();
    rows.into_iter().map(|row| kline_row(&row, step)).collect()
}

/// Decode one raw kline row (the leading 6 fields) into a [`Candle`].
fn kline_row(row: &[serde_json::Value], step: i64) -> Result<Candle, DataError> {
    if row.len() < 6 {
        return Err(DataError::Parse(format!(
            "kline row has {} fields, expected at least 6",
            row.len()
        )));
    }
    let open_time = row[0]
        .as_i64()
        .ok_or_else(|| DataError::Parse("kline open_time is not an integer".to_string()))?;
    let open = kline_decimal(&row[1], "open")?;
    let high = kline_decimal(&row[2], "high")?;
    let low = kline_decimal(&row[3], "low")?;
    let close = kline_decimal(&row[4], "close")?;
    let volume = kline_decimal(&row[5], "volume")?;
    Ok(Candle {
        open_time,
        // audit C2: inclusive last millisecond, shared with WI-02 bulk.
        close_time: open_time + step - 1,
        open,
        high,
        low,
        close,
        volume,
        funding_rate: None,
    })
}

/// Parse a kline field as a `Decimal`. REST returns these as quoted strings; a
/// bare number is also accepted defensively.
fn kline_decimal(value: &serde_json::Value, field: &str) -> Result<Decimal, DataError> {
    let raw = match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => {
            return Err(DataError::Parse(format!(
                "kline {field} has unexpected JSON type: {other}"
            )));
        }
    };
    Decimal::from_str(raw.trim()).map_err(|e| DataError::Parse(format!("kline {field}: {e}")))
}

/// Decode a `/fapi/v1/fundingRate` JSON body into [`FundingEvent`]s.
///
/// # Errors
///
/// [`DataError::Parse`] if the body is not the expected array-of-objects shape.
fn decode_funding(body: &[u8]) -> Result<Vec<FundingEvent>, DataError> {
    let raw: Vec<RawFunding> = serde_json::from_slice(body)
        .map_err(|e| DataError::Parse(format!("malformed fundingRate JSON: {e}")))?;
    Ok(raw
        .into_iter()
        .map(|f| FundingEvent {
            calc_time: f.funding_time,
            rate: f.funding_rate,
        })
        .collect())
}

/// Forward-paginate `/fapi/v1/klines` from `since_ms`, collecting only candles
/// strictly newer than `since_ms` that are also **closed** per the [`Clock`]
/// cutoff (audit C3/C5).
///
/// Pagination (audit C3): each page advances `startTime` to one ms past the last
/// returned `open_time`; the loop stops as soon as a page surfaces no candle with
/// an `open_time > last_seen` (caught up). Klines whose `close_time >= now_ms`
/// (the still-forming final kline) are dropped, never persisted.
///
/// `until_ms` bounds the walk (r4.s1.w4): the page URL carries `endTime` and
/// only candles opening strictly before it are collected, so the interior-gap
/// fill downloads the gap rather than the years of candles after it. `None`
/// keeps the unbounded top-up exactly as it was.
///
/// # Errors
///
/// [`DataError`] from the transport (mapped by WI-02's client) or a JSON decode
/// failure.
async fn fetch_new_klines<S, C>(
    source: &S,
    clock: &C,
    pair: &Pair,
    tf: Timeframe,
    since_ms: i64,
    until_ms: Option<i64>,
) -> Result<Vec<Candle>, DataError>
where
    S: PageSource + Sync,
    C: Clock + Sync,
{
    let now_ms = clock.now_ms();
    let mut out: Vec<Candle> = Vec::new();
    // First page starts at the candle immediately after the snapshot boundary.
    let mut start_ms = since_ms + 1;

    loop {
        let body = source
            .get(&klines_url_until(pair, tf, start_ms, until_ms))
            .await?;
        let page = decode_klines(&body, tf)?;

        // The greatest open_time on this page; pagination terminates when no
        // candle advances past the boundary we already requested.
        let Some(max_open) = page.iter().map(|c| c.open_time).max() else {
            break; // empty page → caught up.
        };

        let mut advanced = false;
        for candle in page {
            if candle.open_time <= since_ms {
                continue; // already in the snapshot.
            }
            if let Some(until) = until_ms
                && candle.open_time >= until
            {
                continue; // at/past the exclusive bound → not this range.
            }
            advanced = true;
            // Closed-candle cutoff (grill / audit C5): drop the still-forming
            // kline. close_time is inclusive, so `< now_ms` means fully closed.
            if candle.close_time < now_ms {
                out.push(candle);
            }
        }

        if !advanced {
            break; // no candle newer than the boundary → caught up.
        }
        // Advance strictly past the last open_time seen (audit C3).
        let next_start = max_open + 1;
        if next_start <= start_ms {
            break; // defensive: no forward progress.
        }
        if let Some(until) = until_ms
            && next_start >= until
        {
            break; // the whole bounded range has been walked.
        }
        start_ms = next_start;
    }

    Ok(out)
}

/// Fetch funding events at/after `funding_since_ms` for `pair`.
///
/// Single page is sufficient for an incremental top-up (8-hourly funding vs. a
/// `< 1000`-row window); kept simple deliberately.
///
/// # Errors
///
/// [`DataError`] from the transport or a JSON decode failure.
async fn fetch_new_funding<S>(
    source: &S,
    pair: &Pair,
    funding_since_ms: i64,
) -> Result<Vec<FundingEvent>, DataError>
where
    S: PageSource + Sync,
{
    let body = source.get(&funding_url(pair, funding_since_ms)).await?;
    decode_funding(&body)
}

/// Walk full funding pages for a bounded historical gap. The ordinary
/// unbounded top-up retains its single-page behavior.
async fn fetch_bounded_funding<S>(
    source: &S,
    pair: &Pair,
    mut start_ms: i64,
    until_ms: i64,
) -> Result<Vec<FundingEvent>, DataError>
where
    S: PageSource + Sync,
{
    let mut events = Vec::new();
    while start_ms < until_ms {
        let page = fetch_new_funding(source, pair, start_ms).await?;
        let full = page.len() == FUNDING_LIMIT;
        let last = page.last().map(|event| event.calc_time);
        events.extend(page.into_iter().filter(|event| event.calc_time < until_ms));
        let Some(last) = last else {
            break;
        };
        if !full || last >= until_ms {
            break;
        }
        if last < start_ms {
            return Err(DataError::Parse(
                "funding pagination did not advance".to_owned(),
            ));
        }
        start_ms = last + 1;
    }
    Ok(events)
}

/// Fetch the incremental top-up: candles newer than `since_ms` (closed only) for
/// `(pair, tf)`, with funding fetched from `funding_since_ms` and stamped onto
/// the **new candles only** (grill — no double-application at the boundary).
///
/// Returns only the **new** candles (audit C1); merging onto the prior series is
/// [`super::merge::merge_new`]'s job. `funding_since_ms` is the caller's
/// last-applied funding timestamp **+ 1** (the caller owns that bookkeeping).
///
/// # Errors
///
/// [`DataError`] from the transport (WI-02 mapping) or a JSON decode failure.
pub(crate) async fn fetch_incremental_with<S, C>(
    source: &S,
    clock: &C,
    pair: &Pair,
    tf: Timeframe,
    since_ms: i64,
    funding_since_ms: i64,
) -> Result<Vec<Candle>, DataError>
where
    S: PageSource + Sync,
    C: Clock + Sync,
{
    fetch_incremental_bounded_with(source, clock, pair, tf, since_ms, None, funding_since_ms).await
}

/// [`fetch_incremental_with`] bounded to `(since_ms, until_ms)` — the
/// interior-gap fill's fetch (r4.s1.w4). `until_ms` is exclusive and bounds both
/// the REST walk (`endTime`) and the collected candles; `None` is the unbounded
/// top-up, byte-for-byte the original path.
///
/// # Errors
///
/// [`DataError`] from the transport (WI-02 mapping) or a JSON decode failure.
pub(crate) async fn fetch_incremental_bounded_with<S, C>(
    source: &S,
    clock: &C,
    pair: &Pair,
    tf: Timeframe,
    since_ms: i64,
    until_ms: Option<i64>,
    funding_since_ms: i64,
) -> Result<Vec<Candle>, DataError>
where
    S: PageSource + Sync,
    C: Clock + Sync,
{
    let mut candles = fetch_new_klines(source, clock, pair, tf, since_ms, until_ms).await?;
    // No newly-closed candles ⇒ a no-op: skip the funding fetch entirely so a
    // transient funding-endpoint error cannot turn an up-to-date run into a
    // failure (cross-confirmed by Codex + CodeRabbit). Nothing to stamp anyway.
    if candles.is_empty() {
        return Ok(candles);
    }
    let funding = match until_ms {
        Some(until) => fetch_bounded_funding(source, pair, funding_since_ms, until).await?,
        None => fetch_new_funding(source, pair, funding_since_ms).await?,
    };
    // Stamp funding on the NEW candles only (WI-02 sparse half-open rule).
    stamp_funding(&mut candles, &funding);
    Ok(candles)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        FundingEvent, PageSource, decode_funding, decode_klines, fetch_incremental_with,
        fetch_new_klines, funding_url, klines_url,
    };
    use crate::adapters::clock::FakeClock;
    use crate::domain::{Candle, DataError, Pair, Timeframe};
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::future::Future;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicU32, Ordering};

    const M15: i64 = 900_000;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    /// A scripted page source: maps an exact URL → response body. An unscripted
    /// URL yields an `Io` error (so a runaway pagination loop surfaces loudly).
    ///
    /// The hit counter is an `AtomicU32` so the type stays `Sync` — required by
    /// the `PageSource: Sync` bound on the fetch fns (the port's futures are
    /// `Send`/`spawn`-able per NFR-9).
    struct ScriptedPages {
        pages: HashMap<String, Vec<u8>>,
        hits: AtomicU32,
    }

    impl ScriptedPages {
        fn new(pages: HashMap<String, Vec<u8>>) -> Self {
            Self {
                pages,
                hits: AtomicU32::new(0),
            }
        }

        fn hit_count(&self) -> u32 {
            self.hits.load(Ordering::SeqCst)
        }
    }

    impl PageSource for ScriptedPages {
        fn get(&self, url: &str) -> impl Future<Output = Result<Vec<u8>, DataError>> + Send {
            let found = self.pages.get(url).cloned();
            self.hits.fetch_add(1, Ordering::SeqCst);
            async move { found.ok_or_else(|| DataError::Io(format!("unscripted URL: {url}"))) }
        }
    }

    fn klines_json(rows: &[(i64, &str)]) -> Vec<u8> {
        // Each row: [open_time, open, high, low, close, volume, close_time, ...]
        let body: Vec<String> = rows
            .iter()
            .map(|(open_time, close)| {
                format!(
                    "[{open_time},\"1\",\"1\",\"1\",\"{close}\",\"1\",{ct},\"0\",0,\"0\",\"0\",\"0\"]",
                    ct = open_time + M15 - 1
                )
            })
            .collect();
        format!("[{}]", body.join(",")).into_bytes()
    }

    fn empty_klines_json() -> Vec<u8> {
        b"[]".to_vec()
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

    fn btc() -> Pair {
        Pair::new("BTCUSDT")
    }

    // ---- AC-2/AC-1: decode recomputes close_time per audit C2 -------------

    #[test]
    fn decode_klines_recomputes_close_time_as_open_plus_interval_minus_one() {
        let body = klines_json(&[(1_700_000_000_000, "42050.75")]);
        let candles = decode_klines(&body, Timeframe::M15).expect("decode");
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].open_time, 1_700_000_000_000);
        // audit C2: open + 900_000 - 1, NOT whatever the payload's field 6 said.
        assert_eq!(candles[0].close_time, 1_700_000_000_000 + M15 - 1);
        assert_eq!(candles[0].close, dec("42050.75"));
        assert_eq!(candles[0].funding_rate, None);
    }

    #[test]
    fn decode_klines_rejects_malformed_json() {
        let err = decode_klines(b"not json", Timeframe::M15).expect_err("must reject");
        assert!(matches!(err, DataError::Parse(_)));
    }

    #[test]
    fn decode_funding_maps_funding_time_and_rate() {
        let body = funding_json(&[(1_700_000_900_000, "0.00010000")]);
        let events = decode_funding(&body).expect("decode funding");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].calc_time, 1_700_000_900_000);
        assert_eq!(events[0].rate, dec("0.00010000"));
    }

    // ---- AC-1: cutoff drops the still-forming final kline -----------------

    #[tokio::test]
    async fn cutoff_drops_the_still_forming_final_kline() {
        // Snapshot ends at T = 1_700_000_000_000 (one M15 candle already held).
        let since = 1_700_000_000_000;
        // Page returns two NEW candles: one fully closed, one still forming.
        let closed_open = since + M15; // close_time = since + 2*M15 - 1
        let forming_open = since + 2 * M15; // close_time = since + 3*M15 - 1
        let mut pages = HashMap::new();
        pages.insert(
            klines_url(&btc(), Timeframe::M15, since + 1),
            klines_json(&[(closed_open, "100"), (forming_open, "200")]),
        );
        // Next page (advance past forming_open) is empty → caught up.
        pages.insert(
            klines_url(&btc(), Timeframe::M15, forming_open + 1),
            empty_klines_json(),
        );
        let source = ScriptedPages::new(pages);
        // now = forming candle's open: the forming candle (close_time = open +
        // 3*M15 - 1 > now) is dropped; the closed one (close_time < now) kept.
        let clock = FakeClock::at(forming_open);

        let new = fetch_new_klines(&source, &clock, &btc(), Timeframe::M15, since, None)
            .await
            .expect("fetch new klines");
        assert_eq!(new.len(), 1, "only the closed candle is persisted");
        assert_eq!(new[0].open_time, closed_open);
    }

    // ---- AC-5/AC-3: pagination terminates; only-forming ⇒ zero ------------

    #[tokio::test]
    async fn only_forming_candle_yields_zero_and_terminates() {
        let since = 1_700_000_000_000;
        let forming_open = since + M15;
        let mut pages = HashMap::new();
        pages.insert(
            klines_url(&btc(), Timeframe::M15, since + 1),
            klines_json(&[(forming_open, "100")]),
        );
        // Advancing past the forming candle returns empty → clean stop.
        pages.insert(
            klines_url(&btc(), Timeframe::M15, forming_open + 1),
            empty_klines_json(),
        );
        let source = ScriptedPages::new(pages);
        let clock = FakeClock::at(forming_open); // the lone candle is still forming.

        let new = fetch_new_klines(&source, &clock, &btc(), Timeframe::M15, since, None)
            .await
            .expect("fetch");
        assert!(new.is_empty(), "only-forming case ⇒ zero persisted candles");
    }

    #[tokio::test]
    async fn pagination_walks_multiple_pages_then_stops() {
        let since = 0;
        // Page 1: candles at M15, 2*M15. Page 2 (start = 2*M15 + 1): 3*M15.
        // now is far in the future so nothing is "still forming".
        let mut pages = HashMap::new();
        pages.insert(
            klines_url(&btc(), Timeframe::M15, since + 1),
            klines_json(&[(M15, "1"), (2 * M15, "2")]),
        );
        pages.insert(
            klines_url(&btc(), Timeframe::M15, 2 * M15 + 1),
            klines_json(&[(3 * M15, "3")]),
        );
        pages.insert(
            klines_url(&btc(), Timeframe::M15, 3 * M15 + 1),
            empty_klines_json(),
        );
        let source = ScriptedPages::new(pages);
        let clock = FakeClock::at(100 * M15);

        let new = fetch_new_klines(&source, &clock, &btc(), Timeframe::M15, since, None)
            .await
            .expect("fetch");
        let times: Vec<i64> = new.iter().map(|c| c.open_time).collect();
        assert_eq!(times, vec![M15, 2 * M15, 3 * M15]);
        assert_eq!(
            source.hit_count(),
            3,
            "two data pages + one empty terminator"
        );
    }

    #[tokio::test]
    async fn transport_error_propagates_as_data_error() {
        // No pages scripted → the first GET errors (Io).
        let source = ScriptedPages::new(HashMap::new());
        let clock = FakeClock::at(100 * M15);
        let err = fetch_new_klines(&source, &clock, &btc(), Timeframe::M15, 0, None)
            .await
            .expect_err("unscripted URL errors");
        assert!(matches!(err, DataError::Io(_)));
    }

    // ---- AC-4: funding fetched at boundary+1, stamped on new candles only --

    #[tokio::test]
    async fn funding_stamped_on_new_candles_only_at_boundary() {
        let since = 0;
        let last_funding_ts = 0; // already applied to the snapshot's candle.
        // New candles at M15 (funding lands on its open) and 2*M15.
        let mut pages = HashMap::new();
        pages.insert(
            klines_url(&btc(), Timeframe::M15, since + 1),
            klines_json(&[(M15, "1"), (2 * M15, "2")]),
        );
        pages.insert(
            klines_url(&btc(), Timeframe::M15, 2 * M15 + 1),
            empty_klines_json(),
        );
        // Funding fetched from last+1; event on the first NEW candle's open.
        pages.insert(
            funding_url(&btc(), last_funding_ts + 1),
            funding_json(&[(M15, "0.00050000")]),
        );
        let source = ScriptedPages::new(pages);
        let clock = FakeClock::at(100 * M15);

        let new = fetch_incremental_with(
            &source,
            &clock,
            &btc(),
            Timeframe::M15,
            since,
            last_funding_ts + 1,
        )
        .await
        .expect("incremental");
        assert_eq!(new.len(), 2);
        assert_eq!(
            new[0].funding_rate,
            Some(dec("0.00050000")),
            "funding stamped on the new candle that opens at the event ts"
        );
        assert_eq!(
            new[1].funding_rate, None,
            "no forward-fill onto later candle"
        );
    }

    #[test]
    fn urls_carry_the_expected_query_params() {
        let k = klines_url(&btc(), Timeframe::M15, 123);
        assert!(k.contains("/fapi/v1/klines?symbol=BTCUSDT&interval=15m&startTime=123&limit="));
        let f = funding_url(&btc(), 456);
        assert!(f.contains("/fapi/v1/fundingRate?symbol=BTCUSDT&startTime=456&limit="));
    }

    // A direct FundingEvent reference keeps the import meaningful even if the
    // decode test is later refactored.
    #[test]
    fn funding_event_is_constructible() {
        let e = FundingEvent {
            calc_time: 1,
            rate: dec("0.1"),
        };
        assert_eq!(e.calc_time, 1);
    }

    // Reference Candle so the import is load-bearing regardless of refactors.
    #[test]
    fn candle_is_constructible() {
        let c = Candle {
            open_time: 0,
            close_time: M15 - 1,
            open: Decimal::ONE,
            high: Decimal::ONE,
            low: Decimal::ONE,
            close: Decimal::ONE,
            volume: Decimal::ONE,
            funding_rate: None,
        };
        assert_eq!(c.close_time, M15 - 1);
    }
    #[tokio::test]
    async fn bounded_gap_fetch_stamps_funding_from_two_full_pages() {
        let until = 2001 * M15;
        let first: Vec<_> = (1..=1000).map(|i| (i * M15, "0.1")).collect();
        let second: Vec<_> = (1001..=2000).map(|i| (i * M15, "0.2")).collect();
        let mut pages = HashMap::new();
        pages.insert(
            super::klines_url_until(&btc(), Timeframe::M15, 1, Some(until)),
            klines_json(&first),
        );
        pages.insert(
            super::klines_url_until(&btc(), Timeframe::M15, 1000 * M15 + 1, Some(until)),
            klines_json(&second),
        );
        pages.insert(
            super::klines_url_until(&btc(), Timeframe::M15, 2000 * M15 + 1, Some(until)),
            empty_klines_json(),
        );
        pages.insert(funding_url(&btc(), 1), funding_json(&first));
        pages.insert(funding_url(&btc(), 1000 * M15 + 1), funding_json(&second));
        pages.insert(
            funding_url(&btc(), 2000 * M15 + 1),
            funding_json(&[(until, "0.9")]),
        );
        let source = ScriptedPages::new(pages);
        let candles = super::fetch_incremental_bounded_with(
            &source,
            &FakeClock::at(3000 * M15),
            &btc(),
            Timeframe::M15,
            0,
            Some(until),
            1,
        )
        .await
        .unwrap();
        assert_eq!(candles.len(), 2000);
        assert!(
            candles[..1000]
                .iter()
                .all(|c| c.funding_rate == Some(dec("0.1")))
        );
        assert!(
            candles[1000..]
                .iter()
                .all(|c| c.funding_rate == Some(dec("0.2")))
        );
    }
}
