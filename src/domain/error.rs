//! `DataError` — the data-pipeline error taxonomy (audit C5).
//!
//! WI-01 ships the documented *initial skeleton*. Downstream work items extend
//! it **additively** (e.g. WI-02 adds `Http`, WI-04 adds `SnapshotExists`) — no
//! variant is renamed or removed, so the shared file never needs a rewrite.
//! `thiserror`-derived for ergonomic `Display`/`Error`, and `serde`-serializable
//! so errors can cross the `Tauri` boundary later. No library path panics: the
//! crate denies `clippy::unwrap_used` / `expect_used`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced by the data pipeline (domain layer).
///
/// Structural-corruption variants (`Validation`) are *rejections*; `Gap` is a
/// *report* surfaced through `Ok` by [`CandleSeries::validate`], not raised as
/// an error (audit C2). It is kept as a variant so adapters that fetch with a
/// strict gap policy can still raise it explicitly.
///
/// [`CandleSeries::validate`]: crate::domain::CandleSeries::validate
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum DataError {
    /// A `CandleSeries` is structurally corrupt and cannot be trusted.
    #[error("candle series validation failed: {0}")]
    Validation(#[from] ValidationError),

    /// A spacing discontinuity between two adjacent candles.
    ///
    /// Reported (not rejected) by validation; an error variant only so strict
    /// adapter policies can raise it deliberately.
    #[error("gap in candle series: expected open_time {expected} ms, found {found} ms")]
    Gap {
        /// The `open_time` the next candle was expected at (epoch ms).
        expected: i64,
        /// The `open_time` actually found (epoch ms).
        found: i64,
    },

    /// A value could not be parsed (e.g. a malformed Decimal from the exchange).
    #[error("parse error: {0}")]
    Parse(String),

    /// An I/O failure (filesystem, network — represented as a message so the
    /// domain stays free of `std::io::Error`'s non-`Serialize` payload).
    #[error("io error: {0}")]
    Io(String),

    /// A snapshot already exists at the target `(pair, timeframe, data_version)`
    /// path but its on-disk content differs from the data being written (audit
    /// C5 / WI-1.1.1.04). Because `data_version` is a content hash, an *identical*
    /// re-write is a no-op success — this variant fires only for the pathological
    /// same-path-different-content case (collision or corruption).
    #[error("snapshot already exists with differing content at {path}")]
    SnapshotExists {
        /// The conflicting snapshot path.
        path: String,
    },

    /// An agent root submit asked for a `strategy` name that is already taken
    /// (r2.s1.w3). The refusal is decided inside the write transaction under
    /// the write lock — an atomic re-check, not a `list_strategies` pre-read a
    /// concurrent create could interleave with. `strategy.name` carries no
    /// schema-level UNIQUE (same-named human strategies are legal), so the
    /// check lives at this write boundary, the only place collision-freedom is
    /// promised.
    #[error("a strategy named {name} already exists")]
    StrategyNameTaken {
        /// The colliding name.
        name: String,
    },

    /// A SQLite/sqlx failure (connection, query, trigger ABORT). The sqlx error is
    /// flattened to a message so the domain stays free of `sqlx::Error` (VS-1.1.4
    /// work-1.01; sqlx lives only in `adapters::db`). A `RAISE(ABORT, ...)` from
    /// the `strategy_version` immutability triggers (FR-4) surfaces here.
    #[error("database error: {0}")]
    Db(String),

    /// A migration-protocol failure (apply/verify/backup-restore). Surfaced by the
    /// backup-before-migrate wrapper (1.04); the variant is declared here in
    /// work-1.01 so the shared error file never needs a later rewrite (NFR-12).
    #[error("migration error: {0}")]
    Migration(String),
}

/// The specific kinds of structural corruption a `CandleSeries` can exhibit.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ValidationError {
    /// Candle `open_time`s are not monotonically increasing.
    #[error("candles are not sorted by open_time (found {later} ms after {earlier} ms)")]
    Unsorted {
        /// The earlier candle's `open_time` (epoch ms).
        earlier: i64,
        /// The out-of-order candle's `open_time` (epoch ms).
        later: i64,
    },

    /// Two candles share the same `open_time`.
    #[error("duplicate open_time {0} ms")]
    Duplicate(i64),
}
