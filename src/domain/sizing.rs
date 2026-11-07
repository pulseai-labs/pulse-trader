//! Shared position sizing + exchange constraints (FR-5, NFR-3, BACKLOG-5).
//!
//! This is the `pulse-broker` money-math home, realized as a **module** this
//! slice (the crate split is later). It promotes VS-1.2.1's inline
//! `position_size` (now [`risk_capped_qty`], its pre-quantization core moved here
//! verbatim) into a single, shared, pure sizer that applies **exchange
//! constraints** (lot step / min qty /
//! min notional / exchange max-leverage). There is exactly **one** sizing
//! function, so simulation and (future v3) live execution call the same code and
//! cannot diverge — NFR-3 (the sizing identity) is enforced **by construction**,
//! and property-tested for the identity + determinism (NFR-2).
//!
//! Two functions, one arithmetic path:
//!
//! - [`risk_capped_qty`] — the **pre-quantization core**, moved here verbatim
//!   from VS-1.2.1's `position_size` (with its tests). Zero-stop refusal
//!   (`BacktestError::NoStopLoss`, G5 / #20), `risk_qty =
//!   equity·risk_per_trade_pct / |entry − stop|`, leverage cap
//!   `qty·entry ≤ equity·max_leverage`.
//! - [`compute_position_size`] — built **on top** of `risk_capped_qty`: it calls
//!   the core with the **effective cap** `min(strategy_max_leverage,
//!   filters.max_leverage)`, then floors the result to `filters.lot_step` and
//!   applies the sub-minimum skip checks ([`SymbolFilters`] / [`SizingOutcome`] /
//!   [`SkipReason`]).
//!
//! `Decimal`-only money-math (NFR-2): no `f64` arithmetic anywhere in this module.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::backtest::BacktestError;

/// Compute the risk-based, leverage-capped position size (G5).
///
/// This is the **pre-quantization core** — VS-1.2.1's `position_size` logic,
/// moved here verbatim so there is one arithmetic path (`rust_decimal` is
/// order-sensitive: `a*b/c ≠ a/c*b`, so the byte-identical move is what keeps
/// the golden-fixture refreeze (2.04) attributable to lot-step flooring alone).
///
/// `equity` is the (constant) account equity; `risk_per_trade_pct` is a decimal
/// fraction (`0.01` = 1%, matching the DSL `RiskParams` convention);
/// `entry_price` / `stop_price` are the trade geometry; `max_leverage` is a
/// plain multiplier cap (`3` = 3×).
///
/// Returns the base-asset quantity. The risk-derived size is reduced to the
/// leverage cap when it would exceed `equity × max_leverage / entry_price`; it
/// is never increased.
///
/// # Errors
///
/// Returns [`BacktestError::NoStopLoss`] when `entry_price == stop_price` (zero
/// stop distance — no risk denominator, G5 / #20).
pub fn risk_capped_qty(
    equity: Decimal,
    risk_per_trade_pct: Decimal,
    entry_price: Decimal,
    stop_price: Decimal,
    max_leverage: Decimal,
) -> Result<Decimal, BacktestError> {
    let stop_distance = (entry_price - stop_price).abs();
    if stop_distance.is_zero() {
        return Err(BacktestError::NoStopLoss);
    }

    let risk_qty = equity * risk_per_trade_pct / stop_distance;

    // Leverage cap: qty * entry <= equity * max_leverage.
    let max_notional = equity * max_leverage;
    let max_qty = max_notional / entry_price;

    Ok(risk_qty.min(max_qty))
}

/// Exchange-imposed sizing constraints for a symbol (C2).
///
/// Pure, `Decimal`-only, serde-able value type. Carries the four filters
/// [`compute_position_size`] applies: the lot-step granularity (`qty` floors to a
/// multiple of it), the minimum order quantity, the minimum order notional, and
/// the exchange's hard max-leverage cap. **Price tick-size rounding is out of
/// scope** (prices come from candles; a future realism item).
///
/// A `0` filter is the **disabled** sentinel: `lot_step == 0` ⇒ no flooring,
/// `min_qty == 0` ⇒ never skip on sub-lot, `min_notional == 0` ⇒ never skip on
/// sub-notional. See [`SymbolFilters::unconstrained`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolFilters {
    /// Quantity granularity: `qty` is floored **down** to a multiple of this
    /// (`LOT_SIZE.stepSize`). `0` ⇒ no flooring (the raw `qty` is kept exact).
    pub lot_step: Decimal,
    /// Minimum order quantity (`LOT_SIZE.minQty`). A floored `qty` below this is
    /// [`Skipped(SubLot)`](SkipReason::SubLot). `0` ⇒ never skip on sub-lot.
    pub min_qty: Decimal,
    /// Minimum order notional (`MIN_NOTIONAL`). A `qty·entry` below this is
    /// [`Skipped(SubNotional)`](SkipReason::SubNotional). `0` ⇒ never skip on
    /// sub-notional.
    pub min_notional: Decimal,
    /// Exchange hard max-leverage cap. Folded into the effective cap as
    /// `min(strategy_max_leverage, filters.max_leverage)`.
    pub max_leverage: Decimal,
}

impl SymbolFilters {
    /// The "no exchange constraints" filter: reproduces the raw
    /// [`risk_capped_qty`] result exactly.
    ///
    /// `lot_step == 0` ⇒ no flooring, `min_qty == 0` / `min_notional == 0` ⇒
    /// never-skip, and a very large `max_leverage` ⇒ never-caps below the
    /// strategy cap. VS-1.2.1's engine unit tests use this so their sizing
    /// assertions stay **byte-identical** through R1 (2.04 depends on this).
    #[must_use]
    pub fn unconstrained() -> Self {
        Self {
            lot_step: Decimal::ZERO,
            min_qty: Decimal::ZERO,
            min_notional: Decimal::ZERO,
            // Large enough that `min(strategy_max_leverage, this)` is always the
            // strategy cap for any sane strategy leverage.
            max_leverage: Decimal::new(1_000_000_000, 0),
        }
    }
}

/// Why a candidate entry was suppressed by [`compute_position_size`] (C4).
///
/// Each skip is labelled by its **true cause**: the leverage cap is the cause
/// only when it zeroed the **pre-quantization** size (`core == 0`); a positive
/// `core` that floors to zero (or below `min_qty`) is a sub-lot quantization
/// skip; a sized-but-too-small order is sub-notional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkipReason {
    /// The (lot-step-floored) quantity is below one tradeable lot — either
    /// floored to `0` because the risk budget was less than `filters.lot_step`,
    /// or below `filters.min_qty` (`LOT_SIZE.minQty`). The leverage cap did NOT
    /// bind (`core > 0`); the size is simply too small to round to a whole lot.
    SubLot,
    /// The order notional (`qty · entry`) is below `filters.min_notional`.
    SubNotional,
    /// The leverage cap (or a zero risk budget) drove the **pre-quantization**
    /// size to `0` — the effective cap left no notional room for any position at
    /// all (distinct from a positive size that merely floors below one lot).
    LeverageCapZero,
    /// A structure-stop entry whose levels cannot be placed at the fill: the next bar opened
    /// at or past the stop level, or a short's `TakeProfit` resolves to a non-positive
    /// price. The entry is skipped, not filled (ADR-0033).
    StopBeyondFill,
}

/// The outcome of [`compute_position_size`] (C4).
///
/// Either a positive base-asset quantity ([`Sized`](SizingOutcome::Sized)) or a
/// suppressed entry with its reason ([`Skipped`](SizingOutcome::Skipped)). The
/// engine (2.04) fills on `Sized` and records the reason on `Skipped`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizingOutcome {
    /// A sizeable position: the (lot-step-floored) base-asset quantity.
    Sized(Decimal),
    /// The entry was suppressed; the variant carries why.
    Skipped(SkipReason),
}

/// Compute an **exchange-constrained** position size (C1).
///
/// Builds on [`risk_capped_qty`] (the single arithmetic path): it caps with the
/// **effective leverage** `min(strategy_max_leverage, filters.max_leverage)`,
/// then floors the result to `filters.lot_step` and applies the sub-minimum skip
/// checks. NFR-3: this is the **one** sizer sim and (future v3) live execution
/// share, so they cannot diverge.
///
/// Steps, in order (C1):
/// 1. `cap = min(strategy_max_leverage, filters.max_leverage)`.
/// 2. `core = risk_capped_qty(equity, risk_per_trade_pct, entry, stop, cap)`.
/// 3. If `core == 0` (the cap left no room **before** flooring) →
///    [`LeverageCapZero`](SkipReason::LeverageCapZero).
/// 4. **Floor** `core` to `filters.lot_step` (`(q / step).floor() · step`,
///    always **down**); `lot_step == 0` ⇒ no flooring.
/// 5. Skip checks by true cause: floored `qty == 0` (positive `core` below one
///    lot) OR `qty < filters.min_qty` → [`SubLot`](SkipReason::SubLot);
///    `qty·entry < filters.min_notional` → [`SubNotional`](SkipReason::SubNotional);
///    else [`Sized(qty)`](SizingOutcome::Sized).
///
/// `strategy_max_leverage` is passed **positionally** (the caller's
/// `RiskParams.max_leverage`) — the cleaner Rust shape for a five-filter +
/// six-arg pure function; 2.04 adapts. (C1 left this choice to the implementer.)
///
/// # Errors
///
/// Returns [`BacktestError::NoStopLoss`] when `entry_price == stop_price` (the
/// zero-stop refusal propagates from [`risk_capped_qty`], G5 / #20).
pub fn compute_position_size(
    equity: Decimal,
    risk_per_trade_pct: Decimal,
    entry_price: Decimal,
    stop_price: Decimal,
    strategy_max_leverage: Decimal,
    filters: &SymbolFilters,
) -> Result<SizingOutcome, BacktestError> {
    // 1. Effective leverage cap = the tighter of strategy + exchange.
    let cap = strategy_max_leverage.min(filters.max_leverage);

    // 2. The single arithmetic path (zero-stop refusal propagates).
    let core = risk_capped_qty(equity, risk_per_trade_pct, entry_price, stop_price, cap)?;

    // 3. Skip checks, labelled by TRUE cause. The leverage cap is the cause ONLY
    //    when it drove the PRE-quantization size to zero (`core == 0`, e.g. a
    //    zero/near-zero effective cap leaves no notional room). A positive `core`
    //    that later floors to zero is a sub-lot quantization skip, NOT a cap skip
    //    — so the cap check must read `core`, before flooring.
    if core.is_zero() {
        return Ok(SizingOutcome::Skipped(SkipReason::LeverageCapZero));
    }

    // 4. Floor to the lot step (always DOWN). `lot_step == 0` ⇒ no flooring.
    let qty = if filters.lot_step.is_zero() {
        core
    } else {
        (core / filters.lot_step).floor() * filters.lot_step
    };

    // 5. A positive `core` that floored to zero is below one lot — a sub-lot
    //    (quantization) skip, same class as `qty < min_qty`.
    if qty.is_zero() || (!filters.min_qty.is_zero() && qty < filters.min_qty) {
        return Ok(SizingOutcome::Skipped(SkipReason::SubLot));
    }
    if !filters.min_notional.is_zero() && qty * entry_price < filters.min_notional {
        return Ok(SizingOutcome::Skipped(SkipReason::SubNotional));
    }

    Ok(SizingOutcome::Sized(qty))
}

/// Bounded O(1) per-reason tally of entries [`compute_position_size`] suppressed
/// over a backtest run (audit C4).
///
/// One `usize` counter per [`SkipReason`] — **not** an unbounded `Vec` of skip
/// events: the engine (`crate::adapters::backtest`) holds one of these on its
/// loop state and calls [`record`](SkippedEntryCounts::record) each time a
/// candidate entry returns [`SizingOutcome::Skipped`], then surfaces it on the
/// run result (2.04 wires, 2.05 renders). `Decimal`-free (pure counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SkippedEntryCounts {
    /// Entries skipped because the floored qty was below `filters.min_qty`
    /// ([`SkipReason::SubLot`]).
    pub sub_lot: usize,
    /// Entries skipped because the order notional was below
    /// `filters.min_notional` ([`SkipReason::SubNotional`]).
    pub sub_notional: usize,
    /// Entries skipped because the leverage cap floored the qty to `0`
    /// ([`SkipReason::LeverageCapZero`]).
    pub leverage_capped: usize,
    /// Entries skipped because the fill price had already reached the structure stop, or a
    /// short's target price resolved non-positive ([`SkipReason::StopBeyondFill`], ADR-0033). A zero count is never serialized, so every
    /// result that predates the field serializes byte-identically.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stop_beyond_fill: usize,
}

/// `skip_serializing_if` for a count: serialize the field only when it is non-zero.
// serde's `skip_serializing_if` calls the predicate with a reference, whatever the type.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl SkippedEntryCounts {
    /// A fresh, all-zero tally.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Increment the cell matching `reason` (the engine calls this on each
    /// [`SizingOutcome::Skipped`]).
    pub fn record(&mut self, reason: SkipReason) {
        let cell = match reason {
            SkipReason::SubLot => &mut self.sub_lot,
            SkipReason::SubNotional => &mut self.sub_notional,
            SkipReason::LeverageCapZero => &mut self.leverage_capped,
            SkipReason::StopBeyondFill => &mut self.stop_beyond_fill,
        };
        *cell = cell.saturating_add(1);
    }

    /// Total entries suppressed across all reasons.
    #[must_use]
    pub fn total(&self) -> usize {
        self.sub_lot
            .saturating_add(self.sub_notional)
            .saturating_add(self.leverage_capped)
            .saturating_add(self.stop_beyond_fill)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        SizingOutcome, SkipReason, SkippedEntryCounts, SymbolFilters, compute_position_size,
        risk_capped_qty,
    };
    use crate::domain::backtest::BacktestError;
    use rust_decimal::Decimal;

    fn dec(n: i64, scale: u32) -> Decimal {
        Decimal::new(n, scale)
    }

    // --- risk_capped_qty: VS-1.2.1 `position_size` tests, moved verbatim. ---

    #[test]
    fn risk_based_size_when_under_leverage_cap() {
        // equity 10_000, risk 1% (0.01), entry 100, stop 95 → dist 5.
        // qty = 10_000 * 0.01 / 5 = 100 / 5 = 20.
        // notional = 20 * 100 = 2_000 <= 10_000 * 10 = 100_000 → uncapped.
        let qty = risk_capped_qty(
            dec(10_000, 0),
            dec(1, 2), // 0.01
            dec(100, 0),
            dec(95, 0),
            dec(10, 0), // 10x cap
        )
        .unwrap();
        assert_eq!(qty, dec(20, 0));
    }

    #[test]
    fn leverage_cap_binds_for_a_tight_stop() {
        // A very tight stop drives the risk-size huge; the leverage cap binds.
        // equity 10_000, risk 1% (0.01), entry 100, stop 99.9 → dist 0.1.
        // risk_qty = 10_000 * 0.01 / 0.1 = 100 / 0.1 = 1_000.
        // max_qty = (10_000 * 3) / 100 = 30_000 / 100 = 300 < 1_000 → capped 300.
        let qty = risk_capped_qty(
            dec(10_000, 0),
            dec(1, 2), // 0.01
            dec(100, 0),
            dec(999, 1), // 99.9
            dec(3, 0),   // 3x cap
        )
        .unwrap();
        assert_eq!(qty, dec(300, 0));
    }

    #[test]
    fn cap_exactly_at_boundary_keeps_risk_size() {
        // Construct equality: risk_qty == max_qty.
        // equity 10_000, risk 1%, entry 100, stop 96.666... is messy; instead
        // pick dist so risk_qty = max_qty. max_qty (3x) = 300 → need risk_qty 300
        // → dist = 10_000*0.01/300 = 100/300. Use a cleaner pair: equity 9_000,
        // risk 1%, entry 100, 3x → max_qty = 270; dist so 9_000*0.01/dist = 270
        // → dist = 90/270 = 1/3 → stop 100 - 1/3. Keep it exact via a 3-scale.
        // Simpler exactness: equity 1_000, risk 10% (0.1), entry 10, stop 9 →
        // dist 1, risk_qty = 1_000*0.1/1 = 100; max_qty (1x) = 1_000*1/10 = 100.
        // Equal → result 100 (cap does not reduce below the risk size).
        let qty = risk_capped_qty(
            dec(1_000, 0),
            dec(1, 1), // 0.1
            dec(10, 0),
            dec(9, 0),
            dec(1, 0), // 1x cap
        )
        .unwrap();
        assert_eq!(qty, dec(100, 0));
    }

    #[test]
    fn zero_stop_distance_errors_nostoploss() {
        // entry == stop → NoStopLoss (G5 / #20), no divide-by-zero.
        let err = risk_capped_qty(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(100, 0),
            dec(10, 0),
        )
        .unwrap_err();
        assert_eq!(err, BacktestError::NoStopLoss);
    }

    // --- compute_position_size + SymbolFilters: new behavior (C1/C2/C4). ---

    #[test]
    fn unconstrained_reproduces_raw_risk_capped_qty() {
        // unconstrained() must yield exactly the raw core (byte-identical) so the
        // engine's VS-1.2.1 sizing assertions survive into R1 (2.04 depends).
        let filters = SymbolFilters::unconstrained();
        let core = risk_capped_qty(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(95, 0),
            dec(10, 0),
        )
        .unwrap();
        let outcome = compute_position_size(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(95, 0),
            dec(10, 0),
            &filters,
        )
        .unwrap();
        assert_eq!(outcome, SizingOutcome::Sized(core));
        assert_eq!(outcome, SizingOutcome::Sized(dec(20, 0)));
    }

    #[test]
    fn lot_step_floors_quantity_down() {
        // Raw qty = 20 (as above). lot_step 0.3 → floor(20/0.3)*0.3 =
        // floor(66.66..)*0.3 = 66*0.3 = 19.8 (always down, never up).
        let filters = SymbolFilters {
            lot_step: dec(3, 1), // 0.3
            min_qty: Decimal::ZERO,
            min_notional: Decimal::ZERO,
            max_leverage: dec(125, 0),
        };
        let outcome = compute_position_size(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(95, 0),
            dec(10, 0),
            &filters,
        )
        .unwrap();
        assert_eq!(outcome, SizingOutcome::Sized(dec(198, 1))); // 19.8
    }

    #[test]
    fn sub_lot_skips_with_sublot_reason() {
        // Raw qty = 20; floor to lot_step 0.001 keeps 20; but min_qty 50 > 20.
        let filters = SymbolFilters {
            lot_step: dec(1, 3), // 0.001
            min_qty: dec(50, 0),
            min_notional: Decimal::ZERO,
            max_leverage: dec(125, 0),
        };
        let outcome = compute_position_size(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(95, 0),
            dec(10, 0),
            &filters,
        )
        .unwrap();
        assert_eq!(outcome, SizingOutcome::Skipped(SkipReason::SubLot));
    }

    #[test]
    fn sub_notional_skips_with_subnotional_reason() {
        // Raw qty = 20, entry 100 → notional 2_000; min_notional 5_000 > 2_000.
        // min_qty 0 so it is not a sub-lot skip; the notional check fires.
        let filters = SymbolFilters {
            lot_step: dec(1, 3),
            min_qty: Decimal::ZERO,
            min_notional: dec(5_000, 0),
            max_leverage: dec(125, 0),
        };
        let outcome = compute_position_size(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(95, 0),
            dec(10, 0),
            &filters,
        )
        .unwrap();
        assert_eq!(outcome, SizingOutcome::Skipped(SkipReason::SubNotional));
    }

    #[test]
    fn core_below_one_lot_floors_to_zero_and_skips_with_sublot_reason() {
        // A POSITIVE core that floors to 0 (the risk budget is less than one lot)
        // is a SubLot quantization skip — the leverage cap did NOT bind, so it
        // must NOT be labelled LeverageCapZero (PR #54 Codex P2: "report zero-lot
        // skips as sub-lot").
        // equity 10, risk 1%, entry 100, stop 50 → dist 50.
        // risk_qty = 10*0.01/50 = 0.002. cap 1x → max_qty = 10*1/100 = 0.1, so
        // core = min(0.002, 0.1) = 0.002 (> 0: risk_qty won, cap did NOT bind).
        // lot_step 1 → floor(0.002/1)*1 = 0 → SubLot (below one lot).
        let filters = SymbolFilters {
            lot_step: dec(1, 0),
            min_qty: dec(5, 0),
            min_notional: dec(1_000, 0),
            max_leverage: dec(125, 0),
        };
        let outcome = compute_position_size(
            dec(10, 0),
            dec(1, 2),
            dec(100, 0),
            dec(50, 0),
            dec(1, 0), // 1x strategy cap
            &filters,
        )
        .unwrap();
        assert_eq!(outcome, SizingOutcome::Skipped(SkipReason::SubLot));
    }

    #[test]
    fn zero_effective_cap_skips_with_leverage_cap_zero_reason() {
        // A genuine LeverageCapZero: a zero effective cap zeroes the
        // PRE-quantization size (core == 0) — no room for ANY position, distinct
        // from a positive core that merely floors below one lot. Exchange cap 0
        // ⇒ effective cap min(10, 0) = 0 ⇒ max_qty = equity*0/entry = 0 ⇒
        // core = min(risk_qty, 0) = 0 → LeverageCapZero.
        let filters = SymbolFilters {
            lot_step: dec(1, 3),
            min_qty: dec(1, 3),
            min_notional: dec(100, 0),
            max_leverage: dec(0, 0), // exchange cap 0 wins over the strategy cap
        };
        let outcome = compute_position_size(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(95, 0),
            dec(10, 0), // 10x strategy cap, but the 0 exchange cap binds
            &filters,
        )
        .unwrap();
        assert_eq!(outcome, SizingOutcome::Skipped(SkipReason::LeverageCapZero));
    }

    #[test]
    fn effective_cap_takes_the_tighter_of_strategy_and_exchange() {
        // Tight stop drives risk_qty huge; exchange max_leverage 2 is tighter
        // than the strategy's 10 → cap binds at 2x.
        // equity 10_000, risk 1% , entry 100, stop 99.9 → dist 0.1.
        // risk_qty = 10_000*0.01/0.1 = 1_000.
        // effective cap = min(10, 2) = 2 → max_qty = 10_000*2/100 = 200.
        // core = min(1_000, 200) = 200 (no flooring, no skip).
        let filters = SymbolFilters {
            lot_step: Decimal::ZERO,
            min_qty: Decimal::ZERO,
            min_notional: Decimal::ZERO,
            max_leverage: dec(2, 0), // exchange cap tighter than strategy 10
        };
        let outcome = compute_position_size(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(999, 1),
            dec(10, 0), // strategy cap 10x (looser)
            &filters,
        )
        .unwrap();
        assert_eq!(outcome, SizingOutcome::Sized(dec(200, 0)));
    }

    #[test]
    fn compute_propagates_no_stop_loss() {
        let err = compute_position_size(
            dec(10_000, 0),
            dec(1, 2),
            dec(100, 0),
            dec(100, 0), // entry == stop
            dec(10, 0),
            &SymbolFilters::unconstrained(),
        )
        .unwrap_err();
        assert_eq!(err, BacktestError::NoStopLoss);
    }

    // --- SkippedEntryCounts: bounded O(1) per-reason tally (audit C4, 2.04). ---

    #[test]
    fn skipped_entry_counts_record_increments_matching_cell() {
        let mut counts = SkippedEntryCounts::new();
        assert_eq!(counts, SkippedEntryCounts::default());
        counts.record(SkipReason::SubLot);
        counts.record(SkipReason::SubLot);
        counts.record(SkipReason::SubNotional);
        counts.record(SkipReason::LeverageCapZero);
        assert_eq!(counts.sub_lot, 2);
        assert_eq!(counts.sub_notional, 1);
        assert_eq!(counts.leverage_capped, 1);
    }

    #[test]
    fn skipped_entry_counts_total_sums_all_reasons() {
        let mut counts = SkippedEntryCounts::new();
        assert_eq!(counts.total(), 0);
        counts.record(SkipReason::SubLot);
        counts.record(SkipReason::SubNotional);
        counts.record(SkipReason::LeverageCapZero);
        assert_eq!(counts.total(), 3);
    }

    #[test]
    fn skipped_entry_counts_serde_round_trips() {
        let mut counts = SkippedEntryCounts::new();
        counts.record(SkipReason::SubNotional);
        let json = serde_json::to_string(&counts).expect("serialize SkippedEntryCounts");
        let back: SkippedEntryCounts =
            serde_json::from_str(&json).expect("deserialize SkippedEntryCounts");
        assert_eq!(counts, back);
    }

    /// r6.s2.w4 (ADR-0033): a structure stop the fill had already reached is its own
    /// reason, and its count is part of the total.
    #[test]
    fn skipped_entry_counts_record_stop_beyond_fill() {
        let mut counts = SkippedEntryCounts::new();
        counts.record(SkipReason::StopBeyondFill);
        assert_eq!(counts.stop_beyond_fill, 1);
        assert_eq!(counts.sub_lot, 0);
        assert_eq!(counts.sub_notional, 0);
        assert_eq!(counts.leverage_capped, 0);
        assert_eq!(counts.total(), 1);
    }

    /// A zero `stop_beyond_fill` never appears in a serialized tally, so every serialization
    /// that predates the field stays byte-identical; a non-zero count does appear, and both
    /// read back (a missing key reads as zero).
    #[test]
    fn stop_beyond_fill_serializes_only_when_non_zero() {
        let mut counts = SkippedEntryCounts::new();
        let json = serde_json::to_string(&counts).expect("serialize a zero tally");
        assert!(
            !json.contains("stop_beyond_fill"),
            "a zero count has no key: {json}"
        );
        assert_eq!(
            json,
            r#"{"sub_lot":0,"sub_notional":0,"leverage_capped":0}"#
        );
        let back: SkippedEntryCounts = serde_json::from_str(&json).expect("read a pre-field tally");
        assert_eq!(back, counts);

        counts.record(SkipReason::StopBeyondFill);
        let json = serde_json::to_string(&counts).expect("serialize a non-zero tally");
        assert!(
            json.contains(r#""stop_beyond_fill":1"#),
            "a count of 1 has its key: {json}"
        );
        let back: SkippedEntryCounts = serde_json::from_str(&json).expect("read it back");
        assert_eq!(back, counts);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod prop_tests {
    use super::{SizingOutcome, SymbolFilters, compute_position_size, risk_capped_qty};
    use proptest::prelude::*;
    use rust_decimal::Decimal;

    /// A positive `Decimal` from an integer mantissa + small scale (NOT `f64`)
    /// so the arithmetic stays exact (matching the `condition.rs` precedent).
    fn arb_pos_decimal(max_mantissa: i64) -> impl Strategy<Value = Decimal> {
        (1i64..=max_mantissa, 0u32..=4u32).prop_map(|(m, s)| Decimal::new(m, s))
    }

    /// Valid sizing geometry: positive equity / entry / stop with `entry != stop`
    /// (so the stop distance is non-zero), a small positive risk fraction, and a
    /// positive leverage. Built so the **uncapped** region is reachable.
    fn arb_valid_inputs() -> impl Strategy<Value = (Decimal, Decimal, Decimal, Decimal, Decimal)> {
        (
            arb_pos_decimal(1_000_000),                       // equity
            (1i64..=50i64).prop_map(|m| Decimal::new(m, 2)),  // risk 0.01..=0.50
            arb_pos_decimal(100_000),                         // entry
            arb_pos_decimal(100_000),                         // stop
            (1i64..=125i64).prop_map(|m| Decimal::new(m, 0)), // leverage 1..=125
        )
            .prop_filter(
                "entry != stop (non-zero stop distance)",
                |(_, _, e, s, _)| e != s,
            )
    }

    proptest! {
        /// Identity (NFR-3), pre-quantization — **formula** leg: in the uncapped
        /// region (cap does not bind), `risk_capped_qty` returns *exactly* the
        /// risk-budget formula `equity · risk / |entry − stop|` (the function and
        /// the asserted formula are the same `Decimal` op, so this is exact for
        /// every draw — it proves no cap/flooring perturbs the uncapped core).
        #[test]
        fn prop_risk_capped_qty_identity_when_uncapped(
            (equity, risk, entry, stop) in (
                arb_pos_decimal(1_000_000),
                (1i64..=50i64).prop_map(|m| Decimal::new(m, 2)),
                arb_pos_decimal(100_000),
                arb_pos_decimal(100_000),
            ).prop_filter("entry != stop", |(_, _, e, s)| e != s)
        ) {
            let stop_dist = (entry - stop).abs();
            let risk_qty = equity * risk / stop_dist;
            // Leverage large enough that the cap never binds for any draw.
            let lev = Decimal::new(1_000_000_000, 0);
            let qty = risk_capped_qty(equity, risk, entry, stop, lev).unwrap();
            prop_assert_eq!(qty, risk_qty);
        }

        /// Identity (NFR-3), pre-quantization — **round-trip** leg: when the risk
        /// budget divides the stop distance **exactly** (an exact `Decimal`
        /// quotient), `qty · |entry − stop| == equity · risk_per_trade_pct` holds
        /// to the bit. (The general round-trip is only inexact because finite
        /// `Decimal` division truncates `equity·risk / dist`; restricting to exact
        /// quotients isolates the spec's identity from `rust_decimal` precision.)
        #[test]
        fn prop_risk_capped_qty_round_trip_when_division_exact(
            qty_units in 1i64..=10_000i64,   // the (integer) uncapped qty
            stop_dist_units in 1i64..=200i64, // the (integer) stop distance
            entry_units in 1i64..=100_000i64,
        ) {
            // Construct so equity·risk == qty_units·stop_dist EXACTLY, hence
            // risk_qty = (qty_units·stop_dist) / stop_dist = qty_units (an exact
            // integer Decimal), and the round-trip is bit-exact. risk = 0.01
            // (a clean 2-scale fraction); equity = budget / 0.01 = budget·100.
            let risk = Decimal::new(1, 2); // 0.01
            let stop_dist = Decimal::new(stop_dist_units, 0);
            let budget = Decimal::new(qty_units, 0) * stop_dist; // equity·risk
            let equity = budget * Decimal::new(100, 0);
            let entry = Decimal::new(entry_units, 0) + stop_dist; // entry > stop ≥ 1
            let stop = entry - stop_dist;
            let lev = Decimal::new(1_000_000_000, 0); // uncapped
            let qty = risk_capped_qty(equity, risk, entry, stop, lev).unwrap();
            prop_assert_eq!(qty, Decimal::new(qty_units, 0));
            prop_assert_eq!(qty * stop_dist, equity * risk);
        }

        /// Identity bound (NFR-3) for the constrained sizer: a `Sized(qty)` from
        /// `compute_position_size` never risks MORE than `equity · risk` — the
        /// effective cap + lot-step flooring only ever **reduce** the size.
        #[test]
        fn prop_compute_position_size_never_over_risks(
            (equity, risk, entry, stop, lev) in arb_valid_inputs(),
            lot_m in 0i64..=1000i64,
        ) {
            let filters = SymbolFilters {
                lot_step: Decimal::new(lot_m, 3), // 0..=1.0
                min_qty: Decimal::ZERO,
                min_notional: Decimal::ZERO,
                max_leverage: Decimal::new(125, 0),
            };
            let stop_dist = (entry - stop).abs();
            let budget = equity * risk;
            // Pre-quantization, cap-applied size (effective cap = min(strategy
            // leverage, exchange max_leverage) — mirrors compute_position_size).
            let pre_quant =
                risk_capped_qty(equity, risk, entry, stop, lev.min(filters.max_leverage))
                    .unwrap();
            if let SizingOutcome::Sized(qty) = compute_position_size(
                equity, risk, entry, stop, lev, &filters,
            ).unwrap() {
                // Cap + lot-step flooring only ever REDUCE the size vs. the
                // pre-quantization risk_capped_qty — monotone, exact, no rounding
                // slack. This is the bulletproof "quantization never sizes up" guard.
                prop_assert!(qty <= pre_quant);
                // After real lot-step flooring (lot_step > 0), realized risk is
                // strictly within budget — the slice's post-flooring `≤` demo
                // criterion. With lot_step == 0 there is NO flooring, so qty is the
                // pre-quantization size, which equals equity·risk / |entry−stop| only
                // up to `rust_decimal` division rounding (round-half-even can land a
                // sub-ulp ABOVE budget; the identity proptests cover the `==` leg), so
                // the strict budget bound is asserted only on the flooring path.
                if filters.lot_step > Decimal::ZERO {
                    prop_assert!(qty * stop_dist <= budget);
                }
            }
        }

        /// Determinism (NFR-2 / NFR-3): identical inputs yield byte-identical
        /// `Decimal` outputs across 128 repetitions (no hidden float / hash / time
        /// state leaks into the size).
        #[test]
        fn prop_compute_position_size_is_deterministic(
            (equity, risk, entry, stop, lev) in arb_valid_inputs(),
            lot_m in 0i64..=1000i64,
        ) {
            let filters = SymbolFilters {
                lot_step: Decimal::new(lot_m, 3),
                min_qty: Decimal::ZERO,
                min_notional: Decimal::ZERO,
                max_leverage: Decimal::new(125, 0),
            };
            let first = compute_position_size(equity, risk, entry, stop, lev, &filters).unwrap();
            for _ in 0..128 {
                let again =
                    compute_position_size(equity, risk, entry, stop, lev, &filters).unwrap();
                prop_assert_eq!(again, first);
            }
        }
    }
}
