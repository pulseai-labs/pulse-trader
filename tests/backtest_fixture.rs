//! End-to-end golden backtest over the 1-month BTCUSDT fixture (VS-1.2.1
//! work-1.05; FR-5 / FR-6, BACKLOG-4).
//!
//! This is the slice's **regression anchor**: it loads the canonical
//! known-strategy DSL (`tests/fixtures/strategies/rsi-oversold-long.json` — the
//! demo-1/demo-2 "RSI(14) < 30, 5% stop, 2R take-profit, 1% risk" long), drives
//! `run_backtest` (the library API, NOT the CLI — keeps this independent of
//! work-1.04) over the real, gap-free 1-month M15 candle store
//! (`tests/fixtures/btcusdt-1m-store/`), and asserts the **frozen golden** trade
//! count + net P&L plus the non-vacuity invariants.
//!
//! ## Golden regeneration (deliberate engine change ⇒ reviewed golden diff)
//!
//! The golden constants below were captured from the first calibrated run. To
//! regenerate after an *intentional* engine change, run the print probe and copy
//! the emitted `GOLDEN_TRADE_COUNT` / `GOLDEN_NET_PNL` lines into the constants:
//!
//! ```text
//! cargo test --test backtest_fixture print_golden_for_regeneration -- --ignored --nocapture
//! ```
//!
//! A silent drift fails `golden_backtest_reproduces_frozen_trade_count_and_pnl`
//! loudly; a deliberate change is a one-line, reviewed golden diff.
//!
//! ## Regime breakdown is DELIBERATELY NOT a frozen golden constant (audit C1+C2)
//!
//! Unlike `GOLDEN_NET_PNL`, the per-regime breakdown is **not** frozen as an exact
//! constant. It is derived from EMA50/200 + ADX(14) thresholding, and those
//! indicators round through `f64` at the convert seam (the only float allowed,
//! VS-1.1.3), so the breakdown inherits the deferred **#29** cross-arch
//! determinism caveat: deterministic on the v1 pinned toolchain, but NOT
//! byte-portable across architectures. Freezing it as a constant would make this
//! test brittle on other targets for no regression-detection gain. Instead,
//! `golden_regime_breakdown_is_non_vacuous` asserts the structural invariant — the
//! run produces at least one non-`Unknown` regime — so a degenerate "everything is
//! Unknown" breakdown still fails loudly. **Do not "fix" this into a frozen
//! constant.** (2.04 reports the actual per-regime spread in the report.md before
//! the slice ships; it is observed, not pinned.)
//!
//! ## Lot-step quantization (2.04 refreeze)
//!
//! 2.04 wired the engine onto the exchange-constrained `compute_position_size`,
//! which floors the sized qty DOWN to the BTCUSDT `lot_step = 0.001`. That shrinks
//! every position slightly, so the refrozen `GOLDEN_NET_PNL` is slightly SMALLER
//! in magnitude than the VS-1.2.1 value (same sign), explainable purely by the
//! qty-quantization fraction. The constant-independent
//! `golden_entry_qty_is_lot_aligned` test proves the floor actually applied,
//! independent of whatever `GOLDEN_NET_PNL` is set to.
//!
//! ## Calibration (C2 — done before freezing)
//!
//! A param grid was swept on this exact fixture; the canonical demo params
//! (RSI period 14, oversold 30, 5% stop, 2R TP) yield 6 completed trades — all 6
//! holding across an 8h funding boundary — so the auto-demo is non-vacuous
//! (>= 3 trades, >= 1 funding crossing, total funding != 0). No strategy swap was
//! needed; the canonical RSI-oversold-long strategy satisfies C2 directly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    BacktestConfig, BacktestResult, BinanceAdapter, CandleSeries, CandleStore, ExchangeAdapter,
    Migrator, Pair, Regime, Timeframe, Trade, compile, run_backtest, validate,
};
use rust_decimal::Decimal;

/// Frozen golden: the exact number of completed trades the canonical strategy
/// produces on the 1-month BTCUSDT fixture. See the module header for
/// regeneration. (RSI(14) < 30 long, 5% stop, 2R take-profit, 1% risk.)
const GOLDEN_TRADE_COUNT: usize = 6;

/// Frozen golden: the exact net P&L (quote currency, already net of
/// fees/funding/slippage) across the run. Exact `Decimal` — a silent engine
/// drift changes this and fails the golden assertion. See the module header for
/// regeneration.
///
/// **2.04 refreeze (lot-step quantization).** Was `145.38478212503902969051241815`
/// in VS-1.2.1 (raw `risk_capped_qty`, no exchange flooring). 2.04 wired the
/// engine onto `compute_position_size`, which floors each entry qty DOWN to the
/// BTCUSDT `lot_step = 0.001`; that shrinks every position slightly, so the net
/// drops to the value below — **slightly smaller in magnitude, same (positive)
/// sign** (a 2.13% / 3.09-quote reduction, well within the per-trade sub-lot
/// flooring bound across 6 trades). Trade count is unchanged at 6 (no entry
/// skipped). The constant-independent `golden_entry_qty_is_lot_aligned` guard
/// proves the floor applied regardless of this value.
const GOLDEN_NET_PNL: &str = "142.29083294950040454";

/// The RSI lookback period the canonical fixture uses. The streaming indicator
/// engine warms RSI(period) at candle index `period + 1` (the first index where
/// `is_warm()` becomes true; matches the CLI `rsi:14_first_row=15` readout), so
/// the first entry CANNOT fill before then (G6 real-data readiness gate).
const RSI_PERIOD: usize = 14;

/// The BTCUSDT `LOT_SIZE.stepSize` the golden quantizes to (the pinned
/// `BinanceAdapter` filter). Every entry qty must be a positive integer multiple
/// of this after 2.04's exchange-constrained sizing.
const LOT_STEP: &str = "0.001";

/// Load the primary M15 candle series from the committed offline fixture store
/// (no network / LLM / SQLite): `with_base_dir` -> `read_head` -> `read_snapshot`.
fn load_primary() -> CandleSeries {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("BTCUSDT");
    let head = store
        .read_head(&pair, Timeframe::M15)
        .expect("read M15 HEAD")
        .expect("M15 HEAD present in fixture store");
    store
        .read_snapshot(&pair, Timeframe::M15, &head)
        .expect("read M15 snapshot")
}

/// Compile the canonical known-strategy DSL fixture through the full read-path
/// the engine consumes: `Migrator::v1().load` (FR-4 version-safe) -> `validate`
/// (FR-3) -> `compile` -> the executable evaluator tree.
fn run_golden(primary: &CandleSeries) -> BacktestResult {
    let json = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/strategies/rsi-oversold-long.json"),
    )
    .expect("read strategy fixture json");
    let loaded = Migrator::v1().load(&json).expect("load (migrate) strategy");
    let validated = validate(&loaded.dsl).expect("strategy validates");
    let compiled = compile(&validated).expect("strategy compiles");
    // Resolve the REAL BTCUSDT USD-M filters through the `BinanceAdapter`
    // exchange-metadata port (NFR-3 end-to-end): the golden quantizes the sized
    // qty to `lot_step = 0.001` — this is what the 2.04 refreeze captures, vs. the
    // engine unit tests' `SymbolFilters::unconstrained()`.
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port");
    // Single-TF M15: no HTF feed (htf = None). Default cost model: 4bps taker
    // fee, 1bp slippage, 10_000 starting equity.
    run_backtest(
        &compiled,
        primary,
        None,
        &BacktestConfig::default(),
        &filters,
    )
    .expect("backtest runs over the fixture")
}

/// Whether a trade's hold crossed at least one 8h funding boundary: a fixture
/// candle that carries a `funding_rate` and whose `open_time` lies strictly
/// after the entry fill and at or before the exit fill (the exact window the
/// engine accrues funding over).
fn crosses_funding_boundary(primary: &CandleSeries, trade: &Trade) -> bool {
    primary.candles.iter().any(|c| {
        c.funding_rate.is_some()
            && c.open_time > trade.entry_fill_time
            && c.open_time <= trade.exit_fill_time
    })
}

/// The candle index at which the first trade's entry filled.
fn first_entry_fill_index(primary: &CandleSeries, result: &BacktestResult) -> usize {
    primary
        .candles
        .iter()
        .position(|c| c.open_time == result.trades[0].entry_fill_time)
        .expect("first entry fill resolves to a candle index")
}

/// The slice regression anchor: the canonical strategy on the 1-month fixture
/// reproduces the **frozen golden** trade count and net P&L exactly. A silent
/// engine drift fails this loudly; a deliberate change is a reviewed golden diff
/// (see the module-header regeneration note).
#[test]
fn golden_backtest_reproduces_frozen_trade_count_and_pnl() {
    let primary = load_primary();
    let result = run_golden(&primary);

    assert_eq!(
        result.trades.len(),
        GOLDEN_TRADE_COUNT,
        "golden trade_count drifted: expected {GOLDEN_TRADE_COUNT}, got {} \
         (regenerate the golden only on a deliberate engine change)",
        result.trades.len()
    );
    let expected_net_pnl = Decimal::from_str_exact(GOLDEN_NET_PNL).expect("GOLDEN_NET_PNL parses");
    assert_eq!(
        result.net_pnl, expected_net_pnl,
        "golden net P&L drifted: expected {expected_net_pnl}, got {}",
        result.net_pnl
    );
}

/// FR-6 / C5 MFE/MAE demo invariant: every completed golden trade satisfies
/// `mfe_r >= 0 ∧ mae_r <= 0` (the slice's MFE/MAE auto-demo criterion). Holds by
/// the init-0 running sample regardless of price path. This reads the *new* fields
/// only — it does NOT touch `GOLDEN_NET_PNL` / `GOLDEN_TRADE_COUNT` (those refreeze
/// in 2.04, not here).
#[test]
fn golden_trades_have_nonneg_mfe_nonpos_mae() {
    let primary = load_primary();
    let result = run_golden(&primary);

    assert!(
        !result.trades.is_empty(),
        "the golden run must produce trades for the MFE/MAE invariant to be meaningful"
    );
    for (i, trade) in result.trades.iter().enumerate() {
        assert!(
            trade.mfe_r >= Decimal::ZERO,
            "trade {i}: mfe_r must be >= 0, got {}",
            trade.mfe_r
        );
        assert!(
            trade.mae_r <= Decimal::ZERO,
            "trade {i}: mae_r must be <= 0, got {}",
            trade.mae_r
        );
    }
}

/// C2 non-vacuity: the auto-demo would be meaningless if the chosen params
/// produced 0 (or only intra-bar) trades, or never held across a funding
/// boundary. Assert >= 3 completed trades, >= 1 trade crossing an 8h funding
/// boundary, and total funding != 0 — so a degenerate "0 trades / no funding"
/// golden fails loudly rather than passing as "expected".
#[test]
fn golden_run_is_non_vacuous_at_least_three_trades_and_funding_applied() {
    let primary = load_primary();
    let result = run_golden(&primary);

    assert!(
        result.trades.len() >= 3,
        "expected at least 3 completed trades (non-vacuous demo), got {}",
        result.trades.len()
    );

    let funding_crossings = result
        .trades
        .iter()
        .filter(|t| crosses_funding_boundary(&primary, t))
        .count();
    assert!(
        funding_crossings >= 1,
        "expected at least 1 trade whose hold crosses an 8h funding boundary, got {funding_crossings}"
    );

    assert_ne!(
        result.funding_total,
        Decimal::ZERO,
        "total funding must be non-zero (funding is exercised end-to-end)"
    );
}

/// G6 real-data readiness: the first trade's entry index must be at or beyond the
/// RSI warmup (no pre-warmup / bar-0 entry on real data). RSI(period) is not
/// `is_warm()` until candle index `period + 1`, and the loop additionally
/// requires `bar.index > 0`, so the earliest possible fill is `period + 1`.
#[test]
fn first_entry_respects_rsi_warmup() {
    let primary = load_primary();
    let result = run_golden(&primary);

    let first_idx = first_entry_fill_index(&primary, &result);
    // The earliest possible fill is `RSI_PERIOD + 1` (warmup); `> RSI_PERIOD` is
    // the same bound for `usize` and is clippy-`int_plus_one`-clean.
    assert!(
        first_idx > RSI_PERIOD,
        "first entry filled at candle index {first_idx}, before the RSI({RSI_PERIOD}) warmup \
         (must be > {RSI_PERIOD}, i.e. >= {})",
        RSI_PERIOD + 1
    );
}

/// 2.04 lot-step structural guard (constant-independent): every entry qty the
/// golden produces is a **positive integer multiple of `lot_step` (0.001)**. This
/// proves the exchange-constrained sizer's flooring actually applied — regardless
/// of what `GOLDEN_NET_PNL` is frozen to (so it survives a refreeze and catches a
/// laundered regression where the floor silently stops running). `qty / lot_step`
/// must be a whole number (zero fractional part) and strictly positive.
#[test]
fn golden_entry_qty_is_lot_aligned() {
    let primary = load_primary();
    let result = run_golden(&primary);
    let lot_step = Decimal::from_str_exact(LOT_STEP).expect("LOT_STEP parses");

    assert!(
        !result.trades.is_empty(),
        "the golden run must produce trades for lot-alignment to be meaningful"
    );
    for (i, trade) in result.trades.iter().enumerate() {
        assert!(
            trade.qty > Decimal::ZERO,
            "trade {i}: entry qty must be strictly positive, got {}",
            trade.qty
        );
        let multiples = trade.qty / lot_step;
        assert_eq!(
            multiples.fract(),
            Decimal::ZERO,
            "trade {i}: entry qty {} is not a multiple of lot_step {lot_step} \
             (qty/lot_step = {multiples}); the exchange flooring did not apply",
            trade.qty
        );
    }
}

/// 2.04 regime non-vacuity (audit C1+C2): the golden run tags at least one trade
/// with a **non-`Unknown`** regime — so a degenerate "every trade is Unknown"
/// breakdown fails loudly rather than passing as "expected" (mirrors the
/// VS-1.2.1 C2 non-vacuity guard). The breakdown itself is DELIBERATELY NOT a
/// frozen constant (see the module header, #29 cross-arch caveat); this asserts
/// only the structural invariant.
#[test]
fn golden_regime_breakdown_is_non_vacuous() {
    let primary = load_primary();
    let result = run_golden(&primary);

    let non_unknown: usize = [Regime::TrendingUp, Regime::TrendingDown, Regime::Ranging]
        .iter()
        .map(|&r| result.regime_breakdown.cell(r).trade_count)
        .sum();

    assert!(
        non_unknown >= 1,
        "expected >= 1 trade in a non-Unknown regime (non-vacuous breakdown), got \
         {non_unknown}; the EMA200/ADX warm at ~bar 200 of the 31-day fixture, so \
         later entries should classify into a real regime"
    );
}

/// Regeneration probe (ignored by default): prints the golden trade count, exact
/// net P&L, funding-crossing count, and cost roll-ups. Run with `--ignored
/// --nocapture` after a deliberate engine change; copy the emitted constants up.
#[test]
#[ignore = "regeneration probe; run with --ignored --nocapture to refresh the golden"]
fn print_golden_for_regeneration() {
    let primary = load_primary();
    let result = run_golden(&primary);
    let funding_crossings = result
        .trades
        .iter()
        .filter(|t| crosses_funding_boundary(&primary, t))
        .count();

    eprintln!("GOLDEN_TRADE_COUNT = {}", result.trades.len());
    eprintln!("GOLDEN_NET_PNL = \"{}\"", result.net_pnl);
    eprintln!("funding_crossings = {funding_crossings}");
    eprintln!("funding_total = {}", result.funding_total);
    eprintln!("fees_total = {}", result.fees_total);
    eprintln!("slippage_total = {}", result.slippage_total);
    // Per-regime spread (counts + net P&L) — observed, NOT frozen (see header).
    for (label, regime) in [
        ("trending_up", Regime::TrendingUp),
        ("trending_down", Regime::TrendingDown),
        ("ranging", Regime::Ranging),
        ("unknown", Regime::Unknown),
    ] {
        let cell = result.regime_breakdown.cell(regime);
        eprintln!(
            "regime[{label}] = count {} net_pnl {}",
            cell.trade_count, cell.net_pnl
        );
    }
    // Per-trade entry qty (proves lot-step alignment in the probe output).
    for (i, t) in result.trades.iter().enumerate() {
        eprintln!("trade[{i}] qty = {} regime = {:?}", t.qty, t.regime);
    }
    eprintln!("skipped_entries.total = {}", result.skipped_entries.total());
    eprintln!(
        "first_entry_fill_index = {}",
        first_entry_fill_index(&primary, &result)
    );
    eprintln!(
        "fixture: {} M15 candles, {} funding-boundary candles",
        primary.candles.len(),
        primary
            .candles
            .iter()
            .filter(|c| c.funding_rate.is_some())
            .count()
    );
}
