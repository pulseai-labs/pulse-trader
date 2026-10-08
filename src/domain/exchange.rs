//! `ExchangeError` — the exchange port's **dedicated** error taxonomy
//! (audit C5).
//!
//! Deliberately NOT [`BacktestError`](crate::domain::backtest::BacktestError):
//! the [`ExchangeAdapter`](crate::domain::port::ExchangeAdapter) port serves
//! **live execution** (v3) too, so coupling it to the backtest error domain would
//! be a smell. Callers map it into their own context (the CLI via `anyhow`, the
//! golden via `expect`). `thiserror`-derived for ergonomic `Display`/`Error`,
//! `serde`-serializable so it can cross the Tauri boundary later, and
//! `#[non_exhaustive]` so live-execution variants land additively.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Errors produced by an [`ExchangeAdapter`](crate::domain::port::ExchangeAdapter).
///
/// Minimal v1 surface — one variant. `#[non_exhaustive]` so v3 live-execution
/// adapters add variants (rate-limit, auth, network) without a breaking rewrite.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ExchangeError {
    /// The adapter has no filters pinned for the requested symbol. v1's
    /// `BinanceAdapter` knows BTCUSDT, ETHUSDT, SOLUSDT and XRPUSDT; any other
    /// pair lands here.
    #[error("unknown symbol: {0}")]
    UnknownSymbol(String),
}
