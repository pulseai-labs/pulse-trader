//! `CandleSeries` ⇄ Parquet encoding via Polars (audit C6, C9; AC-1, AC-2, AC-8).
//!
//! Schema (grill-locked Decimal-as-string): `open_time:i64`, `open/high/low/
//! close/volume: utf8`, `close_time:i64`, `funding_rate: utf8 (nullable)`. OHLCV
//! and funding are stored as their exact decimal string form so the round-trip
//! to `rust_decimal::Decimal` is byte-exact and the file is trivially
//! byte-stable. Provenance lands in file-level key-value metadata.

use std::io::Cursor;
use std::str::FromStr;

use polars::io::SerReader;
use polars::io::parquet::read::ParquetReader;
use polars::io::parquet::write::{KeyValueMetadata, ParquetWriter};
use polars::prelude::{Column, DataFrame, ParquetCompression, StatisticsOptions};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::{Candle, CandleSeries, DataError, Gap};

/// Key-value metadata key under which the JSON-encoded provenance block is stored.
const PROVENANCE_KEY: &str = "pulse_provenance";

/// The fixed data source tag embedded in every snapshot's provenance.
pub(crate) const SOURCE_TAG: &str = "binance-um";

/// Self-describing provenance embedded in each snapshot's Parquet KV metadata
/// (audit C9). Every field is content-derived, so it is deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotProvenance {
    /// Trading pair symbol (e.g. `BTCUSDT`).
    pub pair: String,
    /// `Binance` interval string (`15m` / `4h`).
    pub timeframe: String,
    /// Content-hash `data_version`.
    pub data_version: String,
    /// `CANDLE_SCHEMA_VERSION` the snapshot was written under.
    pub schema_version: u32,
    /// Data source tag (`binance-um`).
    pub source: String,
    /// `open_time` of the first candle, if any.
    pub first_open_ms: Option<i64>,
    /// `open_time` of the last candle, if any.
    pub last_open_ms: Option<i64>,
    /// Detected spacing gaps (reported, not rejected — audit C2).
    pub gaps: Vec<Gap>,
}

/// Map a Polars error to a domain `DataError::Io`.
fn io_err(context: &str, err: &impl std::fmt::Display) -> DataError {
    DataError::Io(format!("{context}: {err}"))
}

/// Build the in-memory `DataFrame` columns for a candle set.
fn to_dataframe(candles: &[Candle]) -> Result<DataFrame, DataError> {
    let open_time: Vec<i64> = candles.iter().map(|c| c.open_time).collect();
    let close_time: Vec<i64> = candles.iter().map(|c| c.close_time).collect();
    let open: Vec<String> = candles.iter().map(|c| c.open.to_string()).collect();
    let high: Vec<String> = candles.iter().map(|c| c.high.to_string()).collect();
    let low: Vec<String> = candles.iter().map(|c| c.low.to_string()).collect();
    let close: Vec<String> = candles.iter().map(|c| c.close.to_string()).collect();
    let volume: Vec<String> = candles.iter().map(|c| c.volume.to_string()).collect();
    let funding: Vec<Option<String>> = candles
        .iter()
        .map(|c| c.funding_rate.as_ref().map(ToString::to_string))
        .collect();

    DataFrame::new_infer_height(vec![
        Column::new("open_time".into(), open_time),
        Column::new("open".into(), open),
        Column::new("high".into(), high),
        Column::new("low".into(), low),
        Column::new("close".into(), close),
        Column::new("volume".into(), volume),
        Column::new("close_time".into(), close_time),
        Column::new("funding_rate".into(), funding),
    ])
    .map_err(|e| io_err("build dataframe", &e))
}

/// Encode a `CandleSeries` to in-memory Parquet bytes with provenance KV metadata.
pub(crate) fn encode(series: &CandleSeries, schema_version: u32) -> Result<Vec<u8>, DataError> {
    let mut df = to_dataframe(&series.candles)?;
    let provenance = build_provenance(series, schema_version)?;
    let kv = KeyValueMetadata::from_static(vec![(
        PROVENANCE_KEY.to_string(),
        serde_json::to_string(&provenance).map_err(|e| io_err("encode provenance", &e))?,
    )]);

    let mut buf: Vec<u8> = Vec::new();
    ParquetWriter::new(Cursor::new(&mut buf))
        // Fixed compression + no statistics ⇒ byte-stable output for a fixed
        // writer version (audit C6 / AC-2).
        .with_compression(ParquetCompression::Uncompressed)
        .with_statistics(StatisticsOptions::empty())
        .with_key_value_metadata(Some(kv))
        .finish(&mut df)
        .map_err(|e| io_err("write parquet", &e))?;
    Ok(buf)
}

/// Assemble the provenance block from the series and its validation gaps.
fn build_provenance(
    series: &CandleSeries,
    schema_version: u32,
) -> Result<SnapshotProvenance, DataError> {
    // Gaps are reported, not rejected; an unsorted/duplicate series is a real error.
    let gaps = series.validate()?;
    Ok(SnapshotProvenance {
        pair: series.pair.to_string(),
        timeframe: series.timeframe.binance_interval().to_string(),
        data_version: series.version.to_string(),
        schema_version,
        source: SOURCE_TAG.to_string(),
        first_open_ms: series.candles.first().map(|c| c.open_time),
        last_open_ms: series.candles.last().map(|c| c.open_time),
        gaps,
    })
}

/// Decode Parquet bytes back into the candle set.
pub(crate) fn decode_candles(bytes: &[u8]) -> Result<Vec<Candle>, DataError> {
    let df = ParquetReader::new(Cursor::new(bytes))
        .finish()
        .map_err(|e| io_err("read parquet", &e))?;

    let n = df.height();
    let open_time = i64_col(&df, "open_time")?;
    let close_time = i64_col(&df, "close_time")?;
    let open = str_col(&df, "open")?;
    let high = str_col(&df, "high")?;
    let low = str_col(&df, "low")?;
    let close = str_col(&df, "close")?;
    let volume = str_col(&df, "volume")?;
    let funding = str_col(&df, "funding_rate")?;

    let mut candles = Vec::with_capacity(n);
    for i in 0..n {
        candles.push(Candle {
            open_time: open_time.get(i).ok_or_else(|| miss("open_time", i))?,
            close_time: close_time.get(i).ok_or_else(|| miss("close_time", i))?,
            open: parse_decimal(open.get(i), "open", i)?,
            high: parse_decimal(high.get(i), "high", i)?,
            low: parse_decimal(low.get(i), "low", i)?,
            close: parse_decimal(close.get(i), "close", i)?,
            volume: parse_decimal(volume.get(i), "volume", i)?,
            funding_rate: match funding.get(i) {
                Some(s) => Some(
                    Decimal::from_str(s)
                        .map_err(|e| DataError::Parse(format!("funding_rate: {e}")))?,
                ),
                None => None,
            },
        });
    }
    Ok(candles)
}

/// Read the provenance KV metadata block from Parquet bytes (audit C9).
pub(crate) fn decode_provenance(bytes: &[u8]) -> Result<SnapshotProvenance, DataError> {
    let mut reader = ParquetReader::new(Cursor::new(bytes));
    let metadata = reader
        .get_metadata()
        .map_err(|e| io_err("read parquet metadata", &e))?;
    let kv = metadata
        .key_value_metadata
        .as_ref()
        .ok_or_else(|| DataError::Io("snapshot missing key-value metadata".to_string()))?;
    let raw = kv
        .iter()
        .find(|entry| entry.key == PROVENANCE_KEY)
        .and_then(|entry| entry.value.as_ref())
        .ok_or_else(|| DataError::Io("snapshot missing provenance metadata".to_string()))?;
    serde_json::from_str(raw).map_err(|e| io_err("decode provenance", &e))
}

/// Blank the footer `created_by` string so two writes from the SAME writer
/// version can be byte-compared — the audit-C6 byte-stability check
/// (`tests/parquet_roundtrip.rs`). It is deliberately NOT the store's snapshot
/// equivalence rule: blanking a string in place only aligns two files whose
/// `created_by` values have the same length, so a cross-writer-version
/// comparison goes through `content_equivalent` (decoded candles + provenance)
/// instead (#5).
pub(crate) fn normalize_writer_metadata(bytes: &[u8]) -> Result<Vec<u8>, DataError> {
    // `created_by` is written verbatim into the footer; replacing its bytes with
    // a fixed-length sentinel yields a writer-version-independent byte image.
    let mut reader = ParquetReader::new(Cursor::new(bytes));
    let created_by = reader
        .get_metadata()
        .map_err(|e| io_err("read parquet metadata", &e))?
        .created_by
        .clone();

    let Some(created_by) = created_by else {
        return Ok(bytes.to_vec());
    };
    Ok(replace_first(
        bytes,
        created_by.as_bytes(),
        &vec![b'\0'; created_by.len()],
    ))
}

/// Replace the first occurrence of `needle` with `replacement` (equal length).
fn replace_first(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut out = haystack.to_vec();
    if let Some(pos) = haystack.windows(needle.len()).position(|w| w == needle) {
        out[pos..pos + replacement.len()].copy_from_slice(replacement);
    }
    out
}

fn i64_col(df: &DataFrame, name: &str) -> Result<polars::prelude::Int64Chunked, DataError> {
    Ok(df
        .column(name)
        .map_err(|e| io_err(&format!("column {name}"), &e))?
        .i64()
        .map_err(|e| io_err(&format!("column {name} as i64"), &e))?
        .clone())
}

fn str_col(df: &DataFrame, name: &str) -> Result<polars::prelude::StringChunked, DataError> {
    Ok(df
        .column(name)
        .map_err(|e| io_err(&format!("column {name}"), &e))?
        .str()
        .map_err(|e| io_err(&format!("column {name} as str"), &e))?
        .clone())
}

fn parse_decimal(value: Option<&str>, field: &str, row: usize) -> Result<Decimal, DataError> {
    let s = value.ok_or_else(|| miss(field, row))?;
    Decimal::from_str(s).map_err(|e| DataError::Parse(format!("{field} row {row}: {e}")))
}

fn miss(field: &str, row: usize) -> DataError {
    DataError::Parse(format!("missing {field} at row {row}"))
}
