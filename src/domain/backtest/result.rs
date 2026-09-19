//! `BacktestResult` — the pure in-memory aggregate the event loop (1.03)
//! produces: the trade log plus net P&L and the cost totals.
//!
//! This is the **whole** v1.2.1 output surface — no `SummaryStats`
//! (expectancy / Sharpe / drawdown / streaks) and no equity curve; those are
//! VS-1.2.4. All money figures are `Decimal` (NFR-2). 1.01 defines the shape;
//! 1.03 populates it; 1.04 renders it to stdout.

use std::fmt::Write as _;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::regime::{RegimeBreakdown, RegimeCell};
use super::stats::{EquityCurve, SummaryStats};
use super::trade::Trade;
use crate::domain::fingerprint::EngineFingerprint;
use crate::domain::sizing::SkippedEntryCounts;

/// The result of one backtest run: the trade log plus run-level totals.
///
/// `net_pnl` is the sum of each trade's `realized_pnl` (already net of costs);
/// the `*_total` fields are the run-wide cost roll-ups, surfaced separately so
/// the demo's "fees/funding/slippage are deducted" readout (1.04) has them
/// without re-summing the trade log.
///
/// Derives `PartialEq` but **not** `Eq`: as of VS-1.2.4 work-4.02 the nested
/// `summary: SummaryStats` carries `sharpe`/`sortino` `f64` fields, so `Eq` is no
/// longer derivable transitively. Determinism is unaffected — the two `f64`
/// fields are oracle-excluded (never fed to either hash); `Eq` was only used for
/// test equality, which `PartialEq` still provides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestResult {
    /// Every trade the run produced, in chronological order.
    pub trades: Vec<Trade>,
    /// Net P&L across the run (quote currency, already net of costs).
    pub net_pnl: Decimal,
    /// Total taker fees paid across the run.
    pub fees_total: Decimal,
    /// Total signed funding P&L delta across the run — **negative when positions
    /// paid funding, positive when they received** (matches `funding_payment`).
    pub funding_total: Decimal,
    /// Total adverse slippage cost across the run.
    pub slippage_total: Decimal,

    /// Per-regime trade-count + net-P&L breakdown over the run (FR-5, VS-1.2.2
    /// work-2.04), aggregated in `into_result` by feeding each trade's
    /// `(regime, realized_pnl)` to [`RegimeBreakdown::record`]. **Deliberately
    /// NOT a frozen golden constant** — it is threshold-on-`f64`-EMA/ADX derived
    /// and inherits the deferred #29 cross-arch determinism caveat (deterministic
    /// on the v1 pinned toolchain, not byte-portable). 2.05 renders it.
    ///
    /// `#[serde(default)]` (#68 / README C5): a result serialized before this
    /// field existed deserializes via [`RegimeBreakdown::default`].
    #[serde(default)]
    pub regime_breakdown: RegimeBreakdown,
    /// Per-reason tally of entries the exchange-constrained sizer suppressed over
    /// the run (audit C4): a bounded O(1) [`SkippedEntryCounts`] (sub-lot /
    /// sub-notional / leverage-capped), populated in `into_result`. 2.05 renders.
    ///
    /// `#[serde(default)]` (#68 / README C5): an old-shape result missing this
    /// field deserializes via [`SkippedEntryCounts::default`].
    #[serde(default)]
    pub skipped_entries: SkippedEntryCounts,

    /// The build-time identity of the engine that produced this run (FR-7 /
    /// NFR-2, VS-1.2.3 work-3.03). Populated from [`EngineFingerprint::current`]
    /// at construction in `LoopState::into_result`; surfaced in the human footer
    /// and the `--json` object. **Deliberately EXCLUDED from both
    /// [`result_content_hash`](BacktestResult::result_content_hash) and
    /// [`money_math_hash`](BacktestResult::money_math_hash)** (D4): the fingerprint
    /// encodes the per-target triple, so including it would make two architectures
    /// running the same backtest hash differently — it is the cross-run comparison
    /// *key*, not part of the byte-identity determinism oracle.
    ///
    /// `#[serde(default)]` (#68 / README C5): an old-shape result missing this
    /// field deserializes via [`EngineFingerprint::default`].
    #[serde(default)]
    pub engine_fingerprint: EngineFingerprint,

    /// The derived read-only summary statistics for this run (VS-1.2.4 work-4.01,
    /// FR-6 / README C1): trade counts, win rate, gross/net roll-ups, profit
    /// factor, expectancy, max drawdown, win/loss streaks, and the commission +
    /// funding totals. Computed as a pure `Decimal`/`usize` fold in
    /// `LoopState::into_result` **after** the totals loop, over the already-final
    /// trade log (D1). **Deliberately EXCLUDED from both
    /// [`result_content_hash`](BacktestResult::result_content_hash) and
    /// [`money_math_hash`](BacktestResult::money_math_hash)** (README C3/C8): it is
    /// a derived read of the totals, not new money-math, so it never reaches either
    /// hasher and the frozen baseline stays frozen by construction (#69 deferred).
    ///
    /// `#[serde(default)]` (#68 / README C5): an old-shape result missing this
    /// field deserializes via [`SummaryStats::default`].
    #[serde(default)]
    pub summary: SummaryStats,

    /// The derived, non-compounding equity curve for this run (VS-1.2.4 work-4.01,
    /// README C2): a leading `(run_start, starting_equity)` point then one point
    /// per closed trade, the equity stepping by each trade's `realized_pnl` off a
    /// constant base. Built in `LoopState::into_result` via
    /// [`EquityCurve::from_trades`] (the SINGLE reusable constructor 4.05 reuses on
    /// the read path). **Deliberately EXCLUDED from both content hashes** (README
    /// C3/C8) — derived read-only, never persisted as its own table.
    ///
    /// `#[serde(default)]` (#68 / README C5): an old-shape result missing this
    /// field deserializes via [`EquityCurve::default`] (an empty curve).
    #[serde(default)]
    pub equity_curve: EquityCurve,
}

impl BacktestResult {
    /// The **money-math** content hash: a SHA-256 over the byte-exact `Decimal` +
    /// `usize` + enum money output ONLY — the trade log, the four run-level
    /// `Decimal` totals (`net_pnl` / `fees_total` / `funding_total` /
    /// `slippage_total`), and the `skipped_entries` counts. **Excludes the
    /// `regime_breakdown`** (the f64-derived component) and **excludes the
    /// `engine_fingerprint`** (D4).
    ///
    /// This is the always-byte-exact half of the structured/composable pair (D3):
    /// every input is a `Decimal` (rendered through [`Decimal::normalize`] so
    /// `0.10` and `0.1` hash identically — no `-0`/`NaN`/`Inf` to canonicalize),
    /// a `usize`, an `i64`, or an enum — never a serialized `f64`. 3.04's
    /// conservative fallback (carve regime out of the determinism oracle if a
    /// cross-arch regime-classification divergence is ever observed) is the
    /// one-line swap of [`result_content_hash`](Self::result_content_hash) for
    /// this function.
    #[must_use]
    pub fn money_math_hash(&self) -> String {
        let mut hasher = Sha256::new();
        Self::feed_money_math(&mut hasher, self);
        finalize_hex(hasher)
    }

    /// The **full** content hash (the determinism oracle 3.04 asserts on, D2): the
    /// money-math base (see [`money_math_hash`](Self::money_math_hash)) with the
    /// `regime_breakdown` folded in. **Excludes the `engine_fingerprint`** (D4) so
    /// two architectures running the same backtest yield the SAME content hash —
    /// the fingerprint is the cross-run comparison key, not part of the oracle.
    ///
    /// Composability (D3): the money-math feed is byte-identical to
    /// [`money_math_hash`](Self::money_math_hash)'s, then the regime breakdown is
    /// appended; the regime cells are `{ trade_count: usize, net_pnl: Decimal }`
    /// over the four fixed `Regime` variants in a fixed order, so no `f64` enters
    /// the digest — the regime path contributes only through byte-exact
    /// `Decimal`/`usize` values whose *classification* 3.02 makes deterministic.
    #[must_use]
    pub fn result_content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        Self::feed_money_math(&mut hasher, self);
        Self::feed_regime_breakdown(&mut hasher, &self.regime_breakdown);
        finalize_hex(hasher)
    }

    /// Feed the money-math component into `hasher`: the trade log, the four
    /// run-level `Decimal` totals, and the skipped-entry counts. Field order is
    /// fixed and every field is length-delimited or fixed-width so no two distinct
    /// results collide via boundary ambiguity (mirrors the canonical encoding in
    /// `crate::adapters::store::version`). The `engine_fingerprint` is NOT fed
    /// (D4).
    fn feed_money_math(hasher: &mut Sha256, result: &Self) {
        hasher.update((result.trades.len() as u64).to_be_bytes());
        for trade in &result.trades {
            feed_trade(hasher, trade);
        }
        feed_decimal(hasher, result.net_pnl);
        feed_decimal(hasher, result.fees_total);
        feed_decimal(hasher, result.funding_total);
        feed_decimal(hasher, result.slippage_total);
        feed_usize(hasher, result.skipped_entries.sub_lot);
        feed_usize(hasher, result.skipped_entries.sub_notional);
        feed_usize(hasher, result.skipped_entries.leverage_capped);
    }

    /// Feed the f64-derived regime breakdown into `hasher`: the four fixed
    /// [`Regime`](crate::domain::Regime) cells in a fixed order, each contributing
    /// its `trade_count: usize` + `net_pnl: Decimal` (both byte-exact — no `f64`).
    fn feed_regime_breakdown(hasher: &mut Sha256, breakdown: &RegimeBreakdown) {
        for cell in [
            breakdown.trending_up(),
            breakdown.trending_down(),
            breakdown.ranging(),
            breakdown.unknown(),
        ] {
            feed_regime_cell(hasher, cell);
        }
    }
}

/// Feed one [`Trade`]'s byte-exact fields into `hasher` in a fixed order. Every
/// field is `Decimal` / `i64` / enum — never an `f64`. `Decimal`s go through
/// [`feed_decimal`] (normalized); enums via their fixed `u8` discriminant tag;
/// the fill log is length-prefixed.
fn feed_trade(hasher: &mut Sha256, trade: &Trade) {
    hasher.update([direction_tag(trade.direction)]);
    feed_decimal(hasher, trade.qty);
    feed_decimal(hasher, trade.entry_price);
    feed_decimal(hasher, trade.exit_price);
    hasher.update(trade.entry_signal_time.to_be_bytes());
    hasher.update(trade.entry_fill_time.to_be_bytes());
    hasher.update(trade.exit_signal_time.to_be_bytes());
    hasher.update(trade.exit_fill_time.to_be_bytes());
    hasher.update((trade.fills.len() as u64).to_be_bytes());
    for fill in &trade.fills {
        feed_decimal(hasher, fill.price);
        feed_decimal(hasher, fill.qty);
        hasher.update(fill.time_ms.to_be_bytes());
        feed_decimal(hasher, fill.fee);
    }
    feed_decimal(hasher, trade.fees_total);
    feed_decimal(hasher, trade.funding_total);
    feed_decimal(hasher, trade.slippage_total);
    feed_decimal(hasher, trade.realized_pnl);
    feed_decimal(hasher, trade.realized_r);
    feed_decimal(hasher, trade.mfe_r);
    feed_decimal(hasher, trade.mae_r);
    hasher.update([exit_reason_tag(trade.exit_reason)]);
    hasher.update([trade_source_tag(trade.source)]);
    hasher.update([regime_tag(trade.regime)]);
}

/// Feed one [`RegimeCell`] (`{ trade_count: usize, net_pnl: Decimal }`).
fn feed_regime_cell(hasher: &mut Sha256, cell: RegimeCell) {
    feed_usize(hasher, cell.trade_count);
    feed_decimal(hasher, cell.net_pnl);
}

/// Length-prefixed feed of a `Decimal` in its **normalized** UTF-8 string form so
/// two arithmetically-equal totals (`0.10` vs `0.1`) hash identically. `Decimal`
/// has no `-0`/`NaN`/`Inf`, so `.normalize()` is a total canonicalization.
fn feed_decimal(hasher: &mut Sha256, value: Decimal) {
    feed_str(hasher, &value.normalize().to_string());
}

/// Length-prefixed `usize` feed (fixed-width as a `u64` big-endian).
fn feed_usize(hasher: &mut Sha256, value: usize) {
    hasher.update((value as u64).to_be_bytes());
}

/// Length-prefixed string feed: an 8-byte big-endian length then the UTF-8 bytes,
/// so concatenation is unambiguous (mirrors `store::version::feed_str`).
fn feed_str(hasher: &mut Sha256, s: &str) {
    hasher.update((s.len() as u64).to_be_bytes());
    hasher.update(s.as_bytes());
}

/// Finalize the digest into a 64-char lowercase hex string.
fn finalize_hex(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        // `write!` to a String is infallible; the result is discarded.
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Fixed `u8` discriminant tag for [`Direction`](crate::domain::Direction).
fn direction_tag(direction: crate::domain::Direction) -> u8 {
    match direction {
        crate::domain::Direction::Long => 0,
        crate::domain::Direction::Short => 1,
    }
}

/// Fixed `u8` discriminant tag for [`super::ExitReason`].
fn exit_reason_tag(reason: super::ExitReason) -> u8 {
    match reason {
        super::ExitReason::StopLoss => 0,
        super::ExitReason::TakeProfit => 1,
        super::ExitReason::Signal => 2,
        super::ExitReason::EndOfData => 3,
    }
}

/// Fixed `u8` discriminant tag for [`super::TradeSource`].
fn trade_source_tag(source: super::TradeSource) -> u8 {
    match source {
        super::TradeSource::Backtest => 0,
    }
}

/// Fixed `u8` discriminant tag for [`Regime`](crate::domain::Regime).
fn regime_tag(regime: crate::domain::Regime) -> u8 {
    match regime {
        crate::domain::Regime::TrendingUp => 0,
        crate::domain::Regime::TrendingDown => 1,
        crate::domain::Regime::Ranging => 2,
        crate::domain::Regime::Unknown => 3,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{BacktestResult, EquityCurve, SummaryStats};
    use crate::domain::backtest::{EquityPoint, RegimeBreakdown};
    use crate::domain::fingerprint::EngineFingerprint;
    use crate::domain::sizing::SkippedEntryCounts;
    use rust_decimal::Decimal;

    fn empty_result() -> BacktestResult {
        BacktestResult {
            trades: Vec::new(),
            net_pnl: Decimal::ZERO,
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            regime_breakdown: crate::domain::backtest::RegimeBreakdown::new(),
            skipped_entries: crate::domain::sizing::SkippedEntryCounts::new(),
            engine_fingerprint: EngineFingerprint::current(),
            summary: SummaryStats::default(),
            equity_curve: EquityCurve::default(),
        }
    }

    /// A non-vacuous result: one trade, non-zero totals, and a populated regime
    /// cell, so the structured hash actually exercises every feed branch (trades,
    /// the four totals, skipped-entry counts, and the folded regime breakdown).
    fn nonempty_result() -> BacktestResult {
        use crate::domain::backtest::RegimeBreakdown;
        use crate::domain::sizing::{SkipReason, SkippedEntryCounts};
        use crate::domain::{Direction, ExitReason, Regime};
        use crate::domain::{Fill, Trade, TradeSource};

        let trade = Trade {
            direction: Direction::Long,
            qty: Decimal::new(5, 1),
            entry_price: Decimal::new(30_000, 0),
            exit_price: Decimal::new(33_000, 0),
            entry_signal_time: 1,
            entry_fill_time: 2,
            exit_signal_time: 3,
            exit_fill_time: 4,
            fills: vec![Fill {
                price: Decimal::new(30_000, 0),
                qty: Decimal::new(5, 1),
                time_ms: 2,
                fee: Decimal::new(6, 0),
            }],
            fees_total: Decimal::new(12, 0),
            funding_total: Decimal::new(1, 0),
            slippage_total: Decimal::new(3, 0),
            realized_pnl: Decimal::new(1_484, 0),
            realized_r: Decimal::new(2, 0),
            mfe_r: Decimal::new(25, 1),
            mae_r: Decimal::new(-5, 1),
            exit_reason: ExitReason::TakeProfit,
            source: TradeSource::Backtest,
            regime: Regime::TrendingUp,
        };
        let mut regime_breakdown = RegimeBreakdown::new();
        regime_breakdown.record(Regime::TrendingUp, trade.realized_pnl);
        let mut skipped_entries = SkippedEntryCounts::new();
        skipped_entries.record(SkipReason::SubLot);
        let trades = vec![trade.clone()];
        // Populate the derived read-only surfaces so the exclusion-guard +
        // serde-round-trip tests are non-vacuous (a real, non-default
        // summary/equity_curve). Built from the same trade log + totals the
        // engine produces — NEVER fed into either hasher (README C3/C8).
        let equity_curve = EquityCurve::from_trades(0, Decimal::new(10_000, 0), &trades);
        let summary = SummaryStats::from_trades(
            &trades,
            trade.realized_pnl,
            trade.fees_total,
            trade.funding_total,
            &equity_curve,
        );
        BacktestResult {
            trades,
            net_pnl: trade.realized_pnl,
            fees_total: trade.fees_total,
            funding_total: trade.funding_total,
            slippage_total: trade.slippage_total,
            regime_breakdown,
            skipped_entries,
            engine_fingerprint: EngineFingerprint::current(),
            summary,
            equity_curve,
        }
    }

    #[test]
    fn empty_result_has_no_trades_and_zero_totals() {
        let r = empty_result();
        assert!(r.trades.is_empty());
        assert_eq!(r.net_pnl, Decimal::ZERO);
        assert_eq!(r.fees_total, Decimal::ZERO);
    }

    #[test]
    fn result_serde_round_trips() {
        let r = nonempty_result();
        let json = serde_json::to_string(&r).expect("serialize BacktestResult");
        let back: BacktestResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(r, back);
    }

    /// AC-1 (`result_hash`): `result_content_hash()` is a well-formed SHA-256 hex
    /// digest (64 lowercase hex chars) over a non-vacuous result, and a DIFFERENT
    /// math output yields a DIFFERENT hash (the oracle is content-sensitive, not a
    /// constant). NFR-2: this is the cross-arch byte-identity oracle 3.04 asserts on.
    #[test]
    fn result_hash_is_well_formed_and_content_sensitive() {
        let r = nonempty_result();
        let hash = r.result_content_hash();
        assert_eq!(
            hash.len(),
            64,
            "content hash must be a 64-char sha2-256 hex"
        );
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "content hash must be lowercase hex, got {hash:?}"
        );
        // A different money output must move the hash.
        let mut other = r.clone();
        other.net_pnl += Decimal::ONE;
        assert_ne!(
            hash,
            other.result_content_hash(),
            "a different net_pnl must change the content hash"
        );
        // And so must a different regime breakdown (the folded f64-derived part).
        let mut regime_diff = r.clone();
        regime_diff
            .regime_breakdown
            .record(crate::domain::Regime::Ranging, Decimal::new(7, 0));
        assert_ne!(
            hash,
            regime_diff.result_content_hash(),
            "a different regime breakdown must change the content hash (D2)"
        );
    }

    /// AC-2 (`hash_is_stable`): the same result hashes IDENTICALLY across repeated
    /// calls (the determinism oracle is a pure function of the result, with the
    /// Decimal `.normalize()` canonicalization making `0.10` and `0.1` collide).
    /// NFR-2.
    #[test]
    fn content_hash_is_stable_across_repeated_calls() {
        let r = nonempty_result();
        let first = r.result_content_hash();
        for _ in 0..8 {
            assert_eq!(
                first,
                r.result_content_hash(),
                "result_content_hash must be a stable pure function of the result"
            );
        }
        // Decimal canonicalization: 0.10 and 0.1 are arithmetically equal and MUST
        // hash identically (the `.normalize()` discipline).
        let mut a = r.clone();
        a.net_pnl = Decimal::from_str_exact("0.10").expect("parse 0.10");
        let mut b = r.clone();
        b.net_pnl = Decimal::from_str_exact("0.1").expect("parse 0.1");
        assert_eq!(
            a.result_content_hash(),
            b.result_content_hash(),
            "0.10 and 0.1 must hash identically (normalized Decimal canonicalization)"
        );
    }

    /// AC-3 (`money_math_hash`): the composable money-only half (D3). It is a
    /// well-formed hex digest, it is STABLE, and — crucially — it is INDEPENDENT of
    /// the `regime_breakdown` (so 3.04's fallback can swap to it in one line) while
    /// `result_content_hash()` is NOT independent of regime. NFR-2.
    #[test]
    fn money_math_hash_is_composable_and_regime_independent() {
        let r = nonempty_result();
        let money = r.money_math_hash();
        assert_eq!(money.len(), 64, "money_math_hash must be a 64-char hex");
        assert_eq!(money, r.money_math_hash(), "money_math_hash must be stable");

        // Changing ONLY the regime breakdown must NOT move money_math_hash (it
        // excludes regime) but MUST move result_content_hash (it folds regime in).
        let mut regime_diff = r.clone();
        regime_diff
            .regime_breakdown
            .record(crate::domain::Regime::Ranging, Decimal::new(9, 0));
        assert_eq!(
            money,
            regime_diff.money_math_hash(),
            "money_math_hash must exclude the regime breakdown (D3)"
        );
        assert_ne!(
            r.result_content_hash(),
            regime_diff.result_content_hash(),
            "result_content_hash must fold the regime breakdown in (D2)"
        );
        // Changing the money math MUST move money_math_hash.
        let mut money_diff = r.clone();
        money_diff.fees_total += Decimal::ONE;
        assert_ne!(
            money,
            money_diff.money_math_hash(),
            "a different fees_total must change money_math_hash"
        );
    }

    /// D4 (`content_hash_excludes_fingerprint`): two results that differ ONLY in
    /// their `engine_fingerprint` produce the SAME `result_content_hash()` AND the
    /// same `money_math_hash()` — so two architectures running the same backtest
    /// agree on the content hash. The fingerprint is the cross-run comparison key,
    /// never part of the determinism oracle. NFR-2.
    #[test]
    fn content_hash_excludes_fingerprint() {
        let base = nonempty_result();
        let mut other = base.clone();
        // A clearly-different (and clearly non-current) fingerprint.
        other.engine_fingerprint = EngineFingerprint::from_raw_for_test("f".repeat(64));
        assert_ne!(
            base.engine_fingerprint, other.engine_fingerprint,
            "the two results must genuinely differ in their fingerprint"
        );
        assert_eq!(
            base.result_content_hash(),
            other.result_content_hash(),
            "result_content_hash must EXCLUDE engine_fingerprint (D4)"
        );
        assert_eq!(
            base.money_math_hash(),
            other.money_math_hash(),
            "money_math_hash must EXCLUDE engine_fingerprint (D4)"
        );
    }

    /// AC-8 / D2 (`content_hash_excludes_summary_and_equity_curve`): two results
    /// that differ ONLY in their derived `summary` / `equity_curve` produce the
    /// SAME `result_content_hash()` AND the same `money_math_hash()` — the slice's
    /// HARD oracle-exclusion invariant (README C3/C8). The new fields are a derived
    /// read of the already-final totals, NOT new money-math, so they never reach
    /// either hasher and the frozen baseline stays frozen *by construction* (#69
    /// deferred; `result.rs`'s hash feed is untouched). Mirrors
    /// `content_hash_excludes_fingerprint`. NFR-2.
    #[test]
    fn content_hash_excludes_summary_and_equity_curve() {
        let base = nonempty_result();
        let base_content = base.result_content_hash();
        let base_money = base.money_math_hash();

        // Perturb ONLY the summary: a clearly-different SummaryStats (non-default
        // trade_count + a different max_drawdown / streaks) — nothing else changes.
        let mut summary_diff = base.clone();
        summary_diff.summary = SummaryStats {
            trade_count: 999,
            win_count: 7,
            loss_count: 3,
            win_rate: Decimal::new(7, 1),
            gross_profit: Decimal::new(12_345, 0),
            gross_loss: Decimal::new(678, 0),
            net_pnl: Decimal::new(11_667, 0),
            profit_factor: Some(Decimal::new(18, 1)),
            avg_win: Decimal::new(1_763, 0),
            avg_loss: Decimal::new(226, 0),
            expectancy: Decimal::new(11, 0),
            max_drawdown: Decimal::new(4_242, 0),
            max_win_streak: 5,
            max_loss_streak: 2,
            commission_total: Decimal::new(99, 0),
            funding_total: Decimal::new(-13, 0),
            sharpe: Some(1.234),
            sortino: Some(2.345),
        };
        assert_ne!(
            base.summary, summary_diff.summary,
            "the two results must genuinely differ in their summary"
        );

        // Perturb ONLY the equity_curve: a clearly-different series.
        let mut curve_diff = base.clone();
        curve_diff.equity_curve = EquityCurve(vec![
            EquityPoint {
                time_ms: 0,
                equity: Decimal::new(10_000, 0),
            },
            EquityPoint {
                time_ms: 123_456,
                equity: Decimal::new(99_999, 0),
            },
        ]);
        assert_ne!(
            base.equity_curve, curve_diff.equity_curve,
            "the two results must genuinely differ in their equity_curve"
        );

        // BOTH hashes must be byte-identical across all three results (D2).
        for (label, other) in [("summary", &summary_diff), ("equity_curve", &curve_diff)] {
            assert_eq!(
                base_content,
                other.result_content_hash(),
                "result_content_hash must EXCLUDE {label} (D2/README C3) — the \
                 frozen baseline stays frozen by construction"
            );
            assert_eq!(
                base_money,
                other.money_math_hash(),
                "money_math_hash must EXCLUDE {label} (D2/README C3)"
            );
        }
    }

    /// AC-4 / D4 (`summary_excluded_from_content_hash`): the slice's HARD
    /// oracle-exclusion invariant for the 4.02 f64 fields specifically — two
    /// results differing ONLY in `summary.sharpe` / `summary.sortino` produce the
    /// SAME `result_content_hash()` AND the same `money_math_hash()`. Folding an
    /// f64 bit-pattern into the oracle would (a) make two architectures running
    /// the same backtest hash differently and (b) move the frozen `49702fd5…`
    /// baseline — so Sharpe/Sortino must never reach either hasher. This is
    /// distinct from 4.01's `content_hash_excludes_summary_and_equity_curve` (the
    /// whole-summary guard); this one is the sharpe/sortino-specific proof.
    /// Mirrors `content_hash_excludes_fingerprint`. NFR-2.
    #[test]
    fn summary_excluded_from_content_hash() {
        let base = nonempty_result();
        let base_content = base.result_content_hash();
        let base_money = base.money_math_hash();

        // Perturb ONLY summary.sharpe / summary.sortino — every other byte of the
        // result (trades, totals, regime, fingerprint, the rest of summary, the
        // equity_curve) is untouched.
        let mut sharpe_diff = base.clone();
        sharpe_diff.summary.sharpe = Some(99.999);
        sharpe_diff.summary.sortino = Some(-42.0);
        assert_ne!(
            base.summary, sharpe_diff.summary,
            "the two results must genuinely differ in summary.sharpe/sortino"
        );
        // Also cover the None ⇄ Some flip so neither bit-pattern nor presence leaks.
        let mut none_diff = base.clone();
        none_diff.summary.sharpe = None;
        none_diff.summary.sortino = None;

        for (label, other) in [
            ("sharpe/sortino values", &sharpe_diff),
            ("None flip", &none_diff),
        ] {
            assert_eq!(
                base_content,
                other.result_content_hash(),
                "result_content_hash must EXCLUDE summary.sharpe/sortino ({label}, D4) — \
                 no f64 bit-pattern in the byte-identity oracle (NFR-2)"
            );
            assert_eq!(
                base_money,
                other.money_math_hash(),
                "money_math_hash must EXCLUDE summary.sharpe/sortino ({label}, D4)"
            );
        }
    }

    /// AC-9 / D6 (`deserialize_old_result_shape_via_defaults`): a JSON object
    /// written before the post-hoc fields existed — carrying ONLY the original
    /// `trades` + the four totals, and missing `regime_breakdown`,
    /// `skipped_entries`, `engine_fingerprint`, `summary`, and `equity_curve` —
    /// still deserializes, the absent fields filling in via `#[serde(default)]`
    /// (#68 / README C5). This is the serde-evolution discipline that keeps a
    /// pre-VS-1.2.4 persisted result readable.
    #[test]
    fn deserialize_old_result_shape_via_defaults() {
        // The minimal pre-evolution shape: trades + the four Decimal totals only.
        // (Decimal serializes as a string under serde-with-str, matching `Candle`.)
        let old_json = r#"{
            "trades": [],
            "net_pnl": "0",
            "fees_total": "0",
            "funding_total": "0",
            "slippage_total": "0"
        }"#;

        let result: BacktestResult = serde_json::from_str(old_json)
            .expect("old-shape result deserializes via serde defaults");

        // The five post-hoc fields filled in from their defaults.
        assert!(result.trades.is_empty());
        assert_eq!(result.net_pnl, Decimal::ZERO);
        assert_eq!(result.regime_breakdown, RegimeBreakdown::default());
        assert_eq!(result.skipped_entries, SkippedEntryCounts::default());
        // engine_fingerprint defaults to the current build's fingerprint.
        assert_eq!(result.engine_fingerprint, EngineFingerprint::current());
        assert_eq!(result.summary, SummaryStats::default());
        assert_eq!(result.equity_curve, EquityCurve::default());
    }
}
