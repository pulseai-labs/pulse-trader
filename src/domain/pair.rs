//! `Pair` — a trading pair symbol (e.g. `BTCUSDT`).

use serde::{Deserialize, Serialize};

use crate::domain::DataError;

/// A trading-pair symbol in `Binance`'s uppercase, separator-free form
/// (e.g. `BTCUSDT`). A thin newtype so the symbol cannot be confused with an
/// arbitrary string at call sites. v1 only uses `BTCUSDT`; the type is general.
///
/// **Invariant:** a `Pair` symbol matches `^[A-Z0-9]+$` (non-empty, uppercase,
/// separator-free). Untrusted input (e.g. a CLI argument) MUST be constructed
/// via [`Pair::parse`], which enforces this — the symbol is joined verbatim into
/// the on-disk store path (`<base>/candles/<PAIR>/…`), so an unvalidated symbol
/// (`../`, `/abs`, `a/b`, `..`) would escape or relocate the store root.
/// [`Pair::new`] is the unchecked constructor for trusted call sites (test
/// fixtures, and `Deserialize` of pairs that originated from our own Parquet/HEAD
/// and were validated at the CLI boundary on the way in).
// `PartialOrd`/`Ord` are the symbol's byte order — the live runtime keys its
// per-`(pair, timeframe)` fetch map on them (r3.s4.w3). The file is not an
// engine-source-set root, so the fingerprint is untouched.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Pair(String);

impl Pair {
    /// Wrap a raw symbol string as a `Pair` **without validation**.
    ///
    /// For trusted call sites only (test fixtures; symbols already validated at
    /// the untrusted boundary). Untrusted input MUST go through [`Pair::parse`].
    #[must_use]
    pub fn new(symbol: impl Into<String>) -> Self {
        Self(symbol.into())
    }

    /// Parse + validate an untrusted symbol into a `Pair`.
    ///
    /// Enforces the type invariant (`^[A-Z0-9]+$`, non-empty): rejects lowercase,
    /// path separators (`/`, `\`), dots, whitespace, and the empty string — so a
    /// CLI-supplied symbol can never escape or relocate the store root.
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Parse`] naming the rejected symbol when it does not
    /// match `^[A-Z0-9]+$`.
    pub fn parse(symbol: impl Into<String>) -> Result<Self, DataError> {
        let symbol = symbol.into();
        if !symbol.is_empty()
            && symbol
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            Ok(Self(symbol))
        } else {
            Err(DataError::Parse(format!(
                "invalid pair symbol {symbol:?}: must be non-empty and match ^[A-Z0-9]+$ \
                 (uppercase letters and digits only)"
            )))
        }
    }

    /// Borrow the underlying symbol string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Pair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::Pair;

    #[test]
    fn parse_accepts_uppercase_alnum_symbols() {
        assert_eq!(
            Pair::parse("BTCUSDT").expect("BTCUSDT ok").as_str(),
            "BTCUSDT"
        );
        assert_eq!(
            Pair::parse("ETHUSDT").expect("ETHUSDT ok").as_str(),
            "ETHUSDT"
        );
        // Digits are permitted (e.g. `1000SHIBUSDT`).
        assert!(Pair::parse("1000SHIBUSDT").is_ok());
    }

    #[test]
    fn parse_rejects_path_traversal_and_malformed_symbols() {
        // Each of these would escape or relocate `<base>/candles/<PAIR>/…`.
        for bad in [
            "../../etc", // relative traversal
            "/abs",      // absolute path
            "a/b",       // embedded separator
            "..",        // parent dir
            "btcusdt",   // lowercase
            "",          // empty
            "BTC USDT",  // whitespace
            "BTC.USDT",  // dot
            r"BTC\USDT", // backslash separator
        ] {
            assert!(
                Pair::parse(bad).is_err(),
                "symbol {bad:?} must be rejected (path-traversal guard)"
            );
        }
    }
}
