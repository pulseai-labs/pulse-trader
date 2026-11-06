//! Generator for `tests/fixtures/solusdt-cipher-store/` (r6.s1.w1) — NOT part of
//! the normal suite. Run it once by hand:
//!
//! ```text
//! cargo test --test gen_solusdt_cipher_store -- --ignored --nocapture
//! ```
//!
//! It reads the local Binance USDT-M SOLUSDT 15m and 4h series through
//! `CandleStore` (`read_head` → `read_snapshot`, base dir
//! `~/.local/share/pulsetrader`), keeps the candles whose `open_time` is in
//! `[2026-06-01 00:00 UTC, 2026-10-08 00:00 UTC)`, and writes one fresh snapshot
//! per timeframe plus its `HEAD` into the fixture dir through the store's own
//! writer — the layout of `tests/fixtures/btcusdt-1m-store/`.
//!
//! The window ends before freeze `30f98b21`'s holdout (2026-10-10), so no
//! holdout candle enters the repo.
//!
//! The committed fixture's `data_version`s (content hashes of the filtered
//! candles under `CANDLE_SCHEMA_VERSION`):
//!
//! - SOLUSDT 15m: `2c371237ce5fcebf`
//! - SOLUSDT 4h:  `6ba93644ceed9b80`
//!
//! The generator is deterministic: the same source candles give the same
//! versions, and the writer is idempotent.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::{fs, path::PathBuf};

use pulse::{CandleSeries, CandleStore, Pair, Timeframe};

/// First included `open_time`: 2026-06-01 00:00 UTC, in ms.
const WINDOW_START_MS: i64 = 1_780_272_000_000;
/// First EXCLUDED `open_time`: 2026-10-08 00:00 UTC, in ms.
const WINDOW_END_MS: i64 = 1_791_417_600_000;

fn fixture_base() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/solusdt-cipher-store")
}

/// Read the whole local series for one timeframe, filter it to the window and
/// write it into the fixture store. Returns the new `data_version`.
fn regenerate(
    source: &CandleStore,
    target: &CandleStore,
    timeframe: Timeframe,
    step_ms: i64,
) -> String {
    let pair = Pair::new("SOLUSDT");
    let head = source
        .read_head(&pair, timeframe)
        .expect("read the local HEAD")
        .expect("the local store has a SOLUSDT HEAD for this timeframe");
    let full = source
        .read_snapshot(&pair, timeframe, &head)
        .expect("read the local snapshot");

    let candles: Vec<_> = full
        .candles
        .into_iter()
        .filter(|c| (WINDOW_START_MS..WINDOW_END_MS).contains(&c.open_time))
        .collect();

    // The window must be complete: every bar from the start to the last one
    // before the end, none missing, none repeated.
    let expected = usize::try_from((WINDOW_END_MS - WINDOW_START_MS) / step_ms).unwrap();
    assert_eq!(
        candles.len(),
        expected,
        "{timeframe:?}: the window should hold {expected} bars"
    );
    for (i, candle) in candles.iter().enumerate() {
        let want = WINDOW_START_MS + i64::try_from(i).unwrap() * step_ms;
        assert_eq!(
            candle.open_time, want,
            "{timeframe:?}: bar {i} is out of place"
        );
    }

    let version = CandleStore::content_version(&pair, timeframe, &candles);
    let series = CandleSeries {
        pair: pair.clone(),
        timeframe,
        version: version.clone(),
        candles,
    };
    target
        .write_snapshot(&series)
        .expect("write the fixture snapshot");
    target
        .write_head(&pair, timeframe, &version)
        .expect("write the fixture HEAD");
    version.as_str().to_owned()
}

#[test]
#[ignore = "one-off generator: reads ~/.local/share/pulsetrader and writes tests/fixtures/solusdt-cipher-store"]
fn generate_solusdt_cipher_store() {
    let source = CandleStore::with_default_base_dir().expect("resolve the local store");
    let base = fixture_base();
    // Start from an empty fixture dir so exactly one snapshot per timeframe
    // remains (a regenerate must not leave the previous snapshot behind).
    if base.exists() {
        fs::remove_dir_all(&base).expect("clear the old fixture store");
    }
    let target = CandleStore::with_base_dir(base);

    let v15 = regenerate(&source, &target, Timeframe::M15, 900_000);
    let v4h = regenerate(&source, &target, Timeframe::H4, 14_400_000);
    println!("SOLUSDT 15m data_version: {v15}");
    println!("SOLUSDT 4h  data_version: {v4h}");
}
