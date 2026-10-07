//! Broker adapters (`adapters/broker`) — the `pulse-broker` exchange-metadata
//! home, realized as a **module** this slice (VS-1.2.2 work-2.01).
//!
//! [`BinanceAdapter`] implements the [`ExchangeAdapter`](crate::domain::ExchangeAdapter)
//! port over **pinned USD-M futures constants** — r4.s1.w2 pins one row each
//! for BTCUSDT, ETHUSDT, SOLUSDT and XRPUSDT; any other pair stays unknown.
//! The consts are pinned here (not fetched) for golden-fixture
//! reproducibility — a networked symbol-filter fetch is a later realism item.
//! The pure
//! [`compute_position_size`](crate::domain::compute_position_size) consumes the
//! returned [`SymbolFilters`](crate::domain::SymbolFilters).

use rust_decimal::Decimal;

use crate::domain::{ExchangeAdapter, ExchangeError, Pair, SymbolFilters};

/// `Binance` USD-M futures exchange adapter (pin table: `BTCUSDT`, `ETHUSDT`,
/// `SOLUSDT`, `XRPUSDT`).
///
/// Returns the pair's **pinned USD-M futures filters** from `symbol_filters`
/// and its pinned funding interval from `funding_interval_ms`. Any pair
/// outside the table yields [`ExchangeError::UnknownSymbol`].
#[derive(Debug, Clone, Copy, Default)]
pub struct BinanceAdapter;

/// One pinned symbol row: the filters the sizer runs under plus the symbol's
/// funding interval.
struct Pinned {
    filters: SymbolFilters,
    funding_interval_ms: i64,
}

impl BinanceAdapter {
    // ---- The pin table (r4.s1.w2) -----------------------------------------
    //
    // The three pairs added by r4.s1.w2 were read once from Binance and pinned
    // here (never fetched at run time), so a run is reproducible:
    //
    // - Filters: `https://fapi.binance.com/fapi/v1/exchangeInfo` (read 2026-10-07, UTC):
    //   `LOT_SIZE.stepSize`, `LOT_SIZE.minQty`, `MIN_NOTIONAL`.
    // - Max leverage: the symbol's top bracket
    //   (`riskBrackets[0].maxOpenPosLeverage`) from Binance's published
    //   leverage-bracket table
    //   (`https://www.binance.com/bapi/futures/v1/public/future/common/brackets`),
    //   read 2026-10-07, UTC — `/fapi/v1/leverageBracket` requires an API key,
    //   so the published table is the cited source (grill G6).
    // - Funding interval: `https://fapi.binance.com/fapi/v1/fundingInfo` (read
    //   2026-10-07, UTC) — 8h for all four rows, the single fixed interval per
    //   pair the engine's funding-bar precondition assumes (#45).
    //
    // BTCUSDT's row is the legacy pin and stays byte-identical: lot_step
    // 0.001, min_qty 0.001, min_notional 100, max_leverage 125. Today's
    // `exchangeInfo` reads BTCUSDT `MIN_NOTIONAL` 50 — the drift is recorded
    // in work item r4.s1.w2's report rather than silently re-pinned.

    /// The pinned row for `symbol`, or `None` when the adapter has no pin.
    fn pinned(symbol: &str) -> Option<Pinned> {
        match symbol {
            // lot_step 0.001 / min_qty 0.001 / MIN_NOTIONAL 100 / top tier 125.
            "BTCUSDT" => Some(Pinned {
                filters: SymbolFilters {
                    lot_step: Decimal::new(1, 3),
                    min_qty: Decimal::new(1, 3),
                    min_notional: Decimal::new(100, 0),
                    max_leverage: Decimal::new(125, 0),
                },
                funding_interval_ms: Self::FUNDING_INTERVAL_MS,
            }),
            // lot_step 0.001 / min_qty 0.001 / MIN_NOTIONAL 20 / top tier 150.
            "ETHUSDT" => Some(Pinned {
                filters: SymbolFilters {
                    lot_step: Decimal::new(1, 3),
                    min_qty: Decimal::new(1, 3),
                    min_notional: Decimal::new(20, 0),
                    max_leverage: Decimal::new(150, 0),
                },
                funding_interval_ms: Self::FUNDING_INTERVAL_MS,
            }),
            // lot_step 0.01 / min_qty 0.01 / MIN_NOTIONAL 5 / top tier 100.
            "SOLUSDT" => Some(Pinned {
                filters: SymbolFilters {
                    lot_step: Decimal::new(1, 2),
                    min_qty: Decimal::new(1, 2),
                    min_notional: Decimal::new(5, 0),
                    max_leverage: Decimal::new(100, 0),
                },
                funding_interval_ms: Self::FUNDING_INTERVAL_MS,
            }),
            // lot_step 0.1 / min_qty 0.1 / MIN_NOTIONAL 5 / top tier 100.
            "XRPUSDT" => Some(Pinned {
                filters: SymbolFilters {
                    lot_step: Decimal::new(1, 1),
                    min_qty: Decimal::new(1, 1),
                    min_notional: Decimal::new(5, 0),
                    max_leverage: Decimal::new(100, 0),
                },
                funding_interval_ms: Self::FUNDING_INTERVAL_MS,
            }),
            _ => None,
        }
    }

    /// Binance USD-M perpetual futures funding interval = 8 hours. Events land
    /// at 00:00 / 08:00 / 16:00 UTC (epoch multiples of 28 800 000 ms), so a
    /// candle whose half-open `[open_time, close_time)` span contains one of
    /// those instants is the funding bar the engine's ordering precondition
    /// checks for (r3.s1.w3). Pinned here like the filters above — never
    /// fetched — for golden-fixture reproducibility.
    const FUNDING_INTERVAL_MS: i64 = 28_800_000;

    /// The pair's pinned funding interval in milliseconds.
    ///
    /// Any pair outside the pin table yields [`ExchangeError::UnknownSymbol`] —
    /// the engine turns that into a typed refusal rather than defaulting an
    /// interval it does not know (#45).
    ///
    /// # Errors
    ///
    /// [`ExchangeError::UnknownSymbol`] when the adapter has no funding
    /// interval pinned for `pair`.
    pub fn funding_interval_ms(&self, pair: &Pair) -> Result<i64, ExchangeError> {
        Self::pinned(pair.as_str())
            .map(|row| row.funding_interval_ms)
            .ok_or_else(|| ExchangeError::UnknownSymbol(pair.as_str().to_owned()))
    }

    /// Construct a new adapter. Stateless.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl ExchangeAdapter for BinanceAdapter {
    fn symbol_filters(&self, pair: &Pair) -> Result<SymbolFilters, ExchangeError> {
        Self::pinned(pair.as_str())
            .map(|row| row.filters)
            .ok_or_else(|| ExchangeError::UnknownSymbol(pair.as_str().to_owned()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::BinanceAdapter;
    use crate::domain::{ExchangeAdapter as _, ExchangeError, Pair, SymbolFilters};
    use rust_decimal::Decimal;

    #[test]
    fn btcusdt_returns_pinned_usdm_filters() {
        let adapter = BinanceAdapter::new();
        let filters = adapter
            .symbol_filters(&Pair::new("BTCUSDT"))
            .expect("BTCUSDT filters");
        assert_eq!(
            filters,
            SymbolFilters {
                lot_step: Decimal::new(1, 3),       // 0.001
                min_qty: Decimal::new(1, 3),        // 0.001
                min_notional: Decimal::new(100, 0), // 100
                max_leverage: Decimal::new(125, 0), // 125
            }
        );
    }

    /// A pair outside the pin table (r4.s1.w2: the table holds BTCUSDT, ETHUSDT,
    /// SOLUSDT and XRPUSDT) still refuses.
    #[test]
    fn unknown_pair_errors_unknown_symbol() {
        let adapter = BinanceAdapter::new();
        let err = adapter
            .symbol_filters(&Pair::new("DOGEUSDT"))
            .expect_err("a pair outside the pin table is unknown");
        assert_eq!(err, ExchangeError::UnknownSymbol("DOGEUSDT".to_owned()));
    }

    /// Every pinned pair answers its dated `exchangeInfo` / bracket-table row —
    /// BTCUSDT byte-identical to the legacy pin.
    #[test]
    fn pinned_filter_rows_match_the_dated_read() {
        let adapter = BinanceAdapter::new();
        for (symbol, expected) in [
            (
                "BTCUSDT",
                SymbolFilters {
                    lot_step: Decimal::new(1, 3),
                    min_qty: Decimal::new(1, 3),
                    min_notional: Decimal::new(100, 0),
                    max_leverage: Decimal::new(125, 0),
                },
            ),
            (
                "ETHUSDT",
                SymbolFilters {
                    lot_step: Decimal::new(1, 3),
                    min_qty: Decimal::new(1, 3),
                    min_notional: Decimal::new(20, 0),
                    max_leverage: Decimal::new(150, 0),
                },
            ),
            (
                "SOLUSDT",
                SymbolFilters {
                    lot_step: Decimal::new(1, 2),
                    min_qty: Decimal::new(1, 2),
                    min_notional: Decimal::new(5, 0),
                    max_leverage: Decimal::new(100, 0),
                },
            ),
            (
                "XRPUSDT",
                SymbolFilters {
                    lot_step: Decimal::new(1, 1),
                    min_qty: Decimal::new(1, 1),
                    min_notional: Decimal::new(5, 0),
                    max_leverage: Decimal::new(100, 0),
                },
            ),
        ] {
            assert_eq!(
                adapter.symbol_filters(&Pair::new(symbol)).expect(symbol),
                expected,
                "{symbol}'s pinned filters"
            );
        }
    }

    #[test]
    fn btcusdt_funding_interval_is_pinned_at_eight_hours() {
        let adapter = BinanceAdapter::new();
        let interval = adapter
            .funding_interval_ms(&Pair::new("BTCUSDT"))
            .expect("BTCUSDT funding interval");
        assert_eq!(interval, 28_800_000, "USD-M perpetuals fund every 8h");
    }

    /// Every pinned pair answers 8h — `fundingInfo` (2026-10-07) showed the
    /// single fixed interval the engine's funding-bar precondition assumes.
    #[test]
    fn pinned_funding_intervals_are_eight_hours() {
        let adapter = BinanceAdapter::new();
        for symbol in ["BTCUSDT", "ETHUSDT", "SOLUSDT", "XRPUSDT"] {
            assert_eq!(
                adapter
                    .funding_interval_ms(&Pair::new(symbol))
                    .expect(symbol),
                28_800_000,
                "{symbol} funds every 8h"
            );
        }
    }

    #[test]
    fn unknown_pair_funding_interval_errors_unknown_symbol() {
        let adapter = BinanceAdapter::new();
        let err = adapter
            .funding_interval_ms(&Pair::new("DOGEUSDT"))
            .expect_err("a pair outside the pin table has no pinned funding interval");
        assert_eq!(err, ExchangeError::UnknownSymbol("DOGEUSDT".to_owned()));
    }
}
