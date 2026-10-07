//! r4.s1.w1 (#5) — a snapshot re-write after a writer-version change stays
//! idempotent.
//!
//! `CandleStore::write_snapshot` reconciles an incoming write against an
//! existing file at the same content-addressed path by comparing stored
//! *content*, not raw bytes. A snapshot on disk written by a different writer
//! version — same candles, same provenance, a different-length footer
//! `created_by` string — must still be recognized as the same snapshot and
//! return the idempotent no-op success; a same-path file holding *different*
//! candles must still be refused with `SnapshotExists`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::str::FromStr;

use pulse::{Candle, CandleSeries, CandleStore, DataError, Pair, Timeframe};
use rust_decimal::Decimal;
use tempfile::TempDir;

/// The `created_by` string the Polars Parquet writer embeds in the footer
/// (`polars-parquet`'s Arrow file writer hard-codes it).
const WRITER_TAG: &[u8] = b"Polars";

fn candle(open_time: i64, close: &str) -> Candle {
    Candle {
        open_time,
        close_time: open_time + 899_999,
        open: Decimal::from_str("42000.5").unwrap(),
        high: Decimal::from_str("42100.0").unwrap(),
        low: Decimal::from_str("41950.25").unwrap(),
        close: Decimal::from_str(close).unwrap(),
        volume: Decimal::from_str("12.34567").unwrap(),
        funding_rate: None,
    }
}

/// A series whose `version` is the content hash the store re-derives on write.
fn series(closes: &[&str]) -> CandleSeries {
    let candles: Vec<Candle> = closes
        .iter()
        .enumerate()
        .map(|(i, close)| {
            candle(
                i64::try_from(i).expect("row index fits i64") * 900_000,
                close,
            )
        })
        .collect();
    let mut s = CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe: Timeframe::M15,
        version: pulse::DataVersion::new("placeholder"),
        candles,
    };
    s.version = CandleStore::content_version(&s.pair, s.timeframe, &s.candles);
    s
}

fn store() -> (CandleStore, TempDir) {
    let tmp = TempDir::new().expect("create tempdir");
    let store = CandleStore::with_base_dir(tmp.path().to_path_buf());
    (store, tmp)
}

/// Rewrite the footer's `created_by` string to `new`, repairing the Thrift
/// length prefix and the trailing footer-length field so the file still parses —
/// what a snapshot written by a different writer version looks like on disk
/// (#5). The data pages and every row-group offset are untouched.
fn rewrite_created_by(bytes: &[u8], new: &str) -> Vec<u8> {
    assert!(bytes.len() > 12, "not a parquet file");
    assert_eq!(&bytes[bytes.len() - 4..], b"PAR1", "parquet magic");
    let len = bytes.len();
    let meta_len = u32::from_le_bytes(bytes[len - 8..len - 4].try_into().unwrap()) as usize;
    let footer_start = len - 8 - meta_len;
    let footer = &bytes[footer_start..len - 8];

    let pos = footer
        .windows(WRITER_TAG.len())
        .position(|w| w == WRITER_TAG)
        .expect("the writer's created_by tag must appear in the footer");
    // Thrift compact protocol: an unsigned-varint length precedes the bytes.
    // Both tags stay below 128 bytes, so the prefix is one byte here.
    assert_eq!(
        footer[pos - 1] as usize,
        WRITER_TAG.len(),
        "created_by is length-prefixed by its varint length byte"
    );
    assert!(new.len() < 128, "the test keeps the one-byte varint form");

    let mut out = bytes[..footer_start].to_vec();
    out.extend_from_slice(&footer[..pos - 1]);
    out.push(u8::try_from(new.len()).expect("length < 128 asserted above"));
    out.extend_from_slice(new.as_bytes());
    out.extend_from_slice(&footer[pos + WRITER_TAG.len()..]);
    out.extend_from_slice(
        &u32::try_from(out.len() - footer_start)
            .expect("footer fits u32")
            .to_le_bytes(),
    );
    out.extend_from_slice(b"PAR1");
    out
}

/// #5: a re-write of the same candles under a writer whose `created_by` has a
/// different length is the idempotent success, not `SnapshotExists`.
#[test]
fn a_rewrite_after_a_writer_version_change_is_idempotent() {
    let (store, _tmp) = store();
    let s = series(&["42001.0", "42002.0", "42003.0"]);
    let path = store.snapshot_path(&s.pair, s.timeframe, &s.version);

    let bytes = store.encode_snapshot(&s).expect("encode");
    let plant = rewrite_created_by(&bytes, "parquet-rs version 53.0.0 (legacy)");
    assert_ne!(
        plant, bytes,
        "the planted file must differ in the footer writer metadata"
    );
    std::fs::create_dir_all(path.parent().unwrap()).expect("create snapshot dir");
    std::fs::write(&path, &plant).expect("plant the older-writer snapshot");

    store
        .write_snapshot(&s)
        .expect("the same candles under another writer version must be the idempotent success");
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        plant,
        "the idempotent path is a no-op — the on-disk snapshot is untouched"
    );
}

/// #5's other half: a same-path file holding different candles is still refused.
#[test]
fn a_rewrite_with_different_candles_is_refused() {
    let (store, _tmp) = store();
    let s = series(&["42001.0", "42002.0"]);
    let other = series(&["42001.0", "42002.0", "42003.0"]);
    let path = store.snapshot_path(&s.pair, s.timeframe, &s.version);

    std::fs::create_dir_all(path.parent().unwrap()).expect("create snapshot dir");
    std::fs::write(&path, store.encode_snapshot(&other).expect("encode other"))
        .expect("plant a same-path file with different candles");

    let err = store
        .write_snapshot(&s)
        .expect_err("different candles at the same path must be refused");
    assert!(
        matches!(err, DataError::SnapshotExists { .. }),
        "expected SnapshotExists, got {err:?}"
    );
}
