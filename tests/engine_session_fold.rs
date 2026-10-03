//! r3.s4.w1 d56 — the fold-and-step equivalence ledger for `EngineSession`.
//!
//! r3.s4.w1 turns `run_backtest`'s whole-series event loop into a stepwise
//! session (`EngineSession::new` → `step` per bar → `finish`) so the paper
//! session (w3) can drive the exact production event loop bar by bar. This file
//! is the behavioural ledger for that flip (spec approach step 8):
//!
//! 1. **Fold ⇒ step**: `run_backtest` (the fold over the session) and a
//!    hand-driven `EngineSession` fed the same candles bar by bar must produce
//!    byte-identical results — full-result JSON, `result_content_hash`,
//!    `money_math_hash`, serialized trade log, `open_position`, `equity_curve`,
//!    `summary`, `skipped_entries`.
//! 2. **Frozen pins**: on the committed 1-month fixture the shared result must
//!    reproduce `tests/backtest_fixture.rs`'s `GOLDEN_TRADE_COUNT` /
//!    `GOLDEN_NET_PNL`, and the committed 1.0.0 document's run must reproduce
//!    `tests/dsl_schema_1_2.rs`'s `FROZEN_COMMITTED_1_0_0` result content hash
//!    and trade log byte-for-byte — for BOTH drivers, so no shared drift can
//!    pass. The frozen trade log is the extracted
//!    `tests/fixtures/frozen/dsl-1-0-0-committed-trade-log.frozen.json`.
//! 3. **Engine-level scenarios** (HTF+AtrStop, H4+D1, trailing/time stops,
//!    lead-in windows, funding-across-position) fold and step identically.
//! 4. **Step refusals** — out-of-order / duplicate / gapped primary, funding
//!    order, out-of-order / duplicate / not-yet-closed HTF **and** D1 — each
//!    fires the typed `BacktestError` variant and leaves the session's full
//!    state untouched (proven by finishing the corrected remainder to the same
//!    result a clean session produces).

#![allow(clippy::expect_used, clippy::unwrap_used)]

use rust_decimal::Decimal;

use pulse::{
    BacktestConfig, BacktestError, BacktestResult, BinanceAdapter, Candle, CandleSeries,
    CandleStore, CompiledStrategy, Condition, DataVersion, Direction, EngineSession,
    ExchangeAdapter, ExitRule, IndicatorSpec, Migrator, Pair, PriceField, RiskParams,
    SchemaVersion, Series, SeriesEnd, SessionTimeframes, StrategyDsl, SweepableValue,
    SymbolFilters, Timeframe, ValueSource, compile, run_backtest, validate,
};

// ---------------------------------------------------------------------------
// Frozen expectations (the same bytes the source tests pin)
// ---------------------------------------------------------------------------

/// `tests/backtest_fixture.rs`'s frozen golden (`backtest_fixture.rs:70,86`).
const GOLDEN_TRADE_COUNT: usize = 6;
/// `tests/backtest_fixture.rs`'s frozen golden net P&L string.
const GOLDEN_NET_PNL: &str = "142.29083294950040454";

/// `tests/dsl_schema_1_2.rs`'s `FROZEN_COMMITTED_1_0_0` result content hash
/// (dsl_schema_1_2.rs:257-258) — captured from the BASE engine.
const FROZEN_COMMITTED_1_0_0_HASH: &str =
    "b8e91b89b7727eb97c8200ee04373ffc3ef3568b51ab145bbb2e652cb2bb9228";

/// `tests/dsl_schema_1_2.rs`'s `FROZEN_COMMITTED_1_0_0` trade log
/// (dsl_schema_1_2.rs:259-260), extracted verbatim into a fixture file so the
/// 4.6 KB of frozen bytes stay out of this source file.
const FROZEN_COMMITTED_1_0_0_TRADE_LOG: &str =
    include_str!("fixtures/frozen/dsl-1-0-0-committed-trade-log.frozen.json");

// ---------------------------------------------------------------------------
// Decimals + hand-built candle builders (the pinned tests' shapes, with a raw
// unstamped sibling per the memory-bank fixture lesson)
// ---------------------------------------------------------------------------

fn dec_s(s: &str) -> Decimal {
    s.parse::<Decimal>().expect("decimal literal")
}

fn dec_m(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

/// One M15 candle with `open_time = t` and the boundary-stamp policy the
/// engine's funding-order precondition expects: a zero-rate stamp on every
/// 8h boundary (`open_time % 28_800_000 == 0`).
fn m15_at(t: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    Candle {
        open_time: t,
        close_time: t + Timeframe::M15.duration_ms() - 1,
        open: dec_m(open, 0),
        high: dec_m(high, 0),
        low: dec_m(low, 0),
        close: dec_m(close, 0),
        volume: dec_m(1, 0),
        funding_rate: if t % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

/// [`m15_at`] at absolute M15 index `i` (`open_time = i × 15m`).
fn m15(i: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    m15_at(i * Timeframe::M15.duration_ms(), open, high, low, close)
}

/// One H4 candle at absolute index `j`, same boundary-stamp policy.
fn h4(j: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    let t = j * Timeframe::H4.duration_ms();
    Candle {
        open_time: t,
        close_time: t + Timeframe::H4.duration_ms() - 1,
        open: dec_m(open, 0),
        high: dec_m(high, 0),
        low: dec_m(low, 0),
        close: dec_m(close, 0),
        volume: dec_m(1, 0),
        funding_rate: if t % 28_800_000 == 0 {
            Some(Decimal::ZERO)
        } else {
            None
        },
    }
}

/// One D1 candle for day `j` (flat OHLC), always stamped (every D1 open is an
/// 8h boundary).
fn d1(j: i64, close: i64) -> Candle {
    let t = j * Timeframe::D1.duration_ms();
    Candle {
        open_time: t,
        close_time: t + Timeframe::D1.duration_ms() - 1,
        open: dec_m(close, 0),
        high: dec_m(close, 0),
        low: dec_m(close, 0),
        close: dec_m(close, 0),
        volume: dec_m(1, 0),
        funding_rate: Some(Decimal::ZERO),
    }
}

/// An M15 candle with NO funding stamp and caller-supplied OHLC — the raw
/// sibling for controlled funding scenarios.
fn raw_m15_at(
    t: i64,
    open: i64,
    high: i64,
    low: i64,
    close: i64,
    stamp: Option<Decimal>,
) -> Candle {
    Candle {
        open_time: t,
        close_time: t + Timeframe::M15.duration_ms() - 1,
        open: dec_m(open, 0),
        high: dec_m(high, 0),
        low: dec_m(low, 0),
        close: dec_m(close, 0),
        volume: dec_m(1, 0),
        funding_rate: stamp,
    }
}

fn series(timeframe: Timeframe, candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe,
        version: DataVersion::new("v-engine-session-fold"),
        candles,
    }
}

// ---------------------------------------------------------------------------
// Strategy construction (the pinned tests' shapes)
// ---------------------------------------------------------------------------

fn fixed_u32(v: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(v)
}

fn fixed_dec(mantissa: i64, scale: u32) -> SweepableValue<Decimal> {
    SweepableValue::Fixed(dec_m(mantissa, scale))
}

fn constant(mantissa: i64, scale: u32) -> ValueSource {
    ValueSource::Constant {
        value: dec_m(mantissa, scale),
    }
}

fn primary_price(field: PriceField) -> ValueSource {
    ValueSource::Price {
        series: Series::Primary,
        field,
    }
}

fn d1_price(field: PriceField) -> ValueSource {
    ValueSource::Price {
        series: Series::D1,
        field,
    }
}

fn htf_price(field: PriceField) -> ValueSource {
    ValueSource::Price {
        series: Series::Htf,
        field,
    }
}

fn htf_ema(period: u32) -> ValueSource {
    ValueSource::Indicator {
        series: Series::Htf,
        spec: IndicatorSpec::Ema {
            period: fixed_u32(period),
        },
    }
}

fn d1_ema(period: u32) -> ValueSource {
    ValueSource::Indicator {
        series: Series::D1,
        spec: IndicatorSpec::Ema {
            period: fixed_u32(period),
        },
    }
}

fn compare(lhs: ValueSource, op: pulse::Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

/// `primary.close > 0` — always true from the first evaluated bar on, so
/// entries fire deterministically with no indicators to warm.
fn price_entry() -> Condition {
    compare(
        primary_price(PriceField::Close),
        pulse::Comparator::Gt,
        constant(0, 0),
    )
}

fn stop_loss(distance_m: i64, distance_scale: u32) -> ExitRule {
    ExitRule::StopLoss {
        distance_pct: fixed_dec(distance_m, distance_scale),
    }
}

fn trail(trail_m: i64, trail_scale: u32) -> ExitRule {
    ExitRule::TrailingStop {
        trail_pct: fixed_dec(trail_m, trail_scale),
    }
}

fn time_stop(max_bars: u32) -> ExitRule {
    ExitRule::TimeStop {
        max_bars: fixed_u32(max_bars),
    }
}

fn take_profit(target_m: i64, target_scale: u32) -> ExitRule {
    ExitRule::TakeProfit {
        target_r: fixed_dec(target_m, target_scale),
    }
}

fn atr_stop(period: u32, multiple_m: i64, multiple_scale: u32) -> ExitRule {
    ExitRule::AtrStop {
        period: fixed_u32(period),
        multiple: fixed_dec(multiple_m, multiple_scale),
    }
}

fn dsl(entry: Condition, exits: Vec<ExitRule>, direction: Direction) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "engine-session-fold fixture".to_owned(),
        direction,
        entry,
        filters: vec![],
        exits,
        risk: RiskParams {
            risk_per_trade_pct: fixed_dec(1, 2),
            max_leverage: fixed_dec(3, 0),
        },
    }
}

fn compiled(entry: Condition, exits: Vec<ExitRule>, direction: Direction) -> CompiledStrategy {
    compile(&validate(&dsl(entry, exits, direction)).expect("fixture validates"))
        .expect("fixture compiles")
}

/// Zero-fee, zero-slippage config so fill prices equal raw candle opens —
/// hand-checkable geometry, exactly the pinned tests' convention.
fn zero_cost() -> BacktestConfig {
    BacktestConfig {
        starting_equity: dec_m(10_000, 0),
        taker_fee_bps: dec_m(0, 0),
        slippage_bps: dec_m(0, 0),
    }
}

// ---------------------------------------------------------------------------
// The two drivers
// ---------------------------------------------------------------------------

/// The step driver: an `EngineSession` hand-fed every primary candle in order
/// (the argument set mirrors `run_backtest`'s — hence the same allow).
/// with the HTF/D1 candles whose `close_time <= primary.close_time` drained in
/// ahead of each step (the same closed-bar rule `run_backtest`'s cursors use),
/// then finished with the caller's `series_end`. This is the control arm every
/// fold result in this file is compared against.
#[allow(clippy::too_many_arguments)]
fn step_run(
    compiled_strategy: &CompiledStrategy,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    d1_series: Option<&CandleSeries>,
    config: &BacktestConfig,
    filters: &SymbolFilters,
    series_end: SeriesEnd,
    count_from_ms: Option<i64>,
) -> Result<BacktestResult, BacktestError> {
    let mut session = EngineSession::new(
        compiled_strategy,
        &primary.pair,
        SessionTimeframes {
            primary: primary.timeframe,
            htf: htf.map(|s| s.timeframe),
            d1: d1_series.map(|s| s.timeframe),
        },
        *config,
        filters.clone(),
        count_from_ms,
    )
    .expect("session constructs over the fixture");

    let mut htf_iter = htf.map(|s| s.candles.iter());
    let mut d1_iter = d1_series.map(|s| s.candles.iter());

    for bar in &primary.candles {
        let mut closed_htf: Vec<Candle> = Vec::new();
        if let Some(it) = htf_iter.as_mut() {
            while let Some(c) = it.clone().next() {
                if c.close_time <= bar.close_time {
                    closed_htf.push(c.clone());
                    it.next();
                } else {
                    break;
                }
            }
        }
        let mut closed_d1: Vec<Candle> = Vec::new();
        if let Some(it) = d1_iter.as_mut() {
            while let Some(c) = it.clone().next() {
                if c.close_time <= bar.close_time {
                    closed_d1.push(c.clone());
                    it.next();
                } else {
                    break;
                }
            }
        }
        session
            .step(bar, &closed_htf, &closed_d1)
            .expect("step accepts a well-formed series");
    }

    session.finish(series_end, primary.candles.last())
}

/// The fold-vs-step equality ledger: every named surface of the spec's d56
/// list must be byte-identical between the two drivers.
fn assert_results_identical(fold: &BacktestResult, step: &BacktestResult, ctx: &str) {
    let fold_full = serde_json::to_string(fold).expect("fold result serializes");
    let step_full = serde_json::to_string(step).expect("step result serializes");
    assert_eq!(fold_full, step_full, "{ctx}: full-result JSON diverges");

    assert_eq!(
        fold.result_content_hash(),
        step.result_content_hash(),
        "{ctx}: result_content_hash diverges"
    );
    assert_eq!(
        fold.money_math_hash(),
        step.money_math_hash(),
        "{ctx}: money_math_hash diverges"
    );
    assert_eq!(
        serde_json::to_string(&fold.trades).expect("fold trades serialize"),
        serde_json::to_string(&step.trades).expect("step trades serialize"),
        "{ctx}: serialized trade log diverges"
    );
    assert_eq!(
        serde_json::to_string(&fold.open_position).expect("fold open position serializes"),
        serde_json::to_string(&step.open_position).expect("step open position serializes"),
        "{ctx}: open_position diverges"
    );
    assert_eq!(
        serde_json::to_string(&fold.equity_curve).expect("fold equity curve serializes"),
        serde_json::to_string(&step.equity_curve).expect("step equity curve serializes"),
        "{ctx}: equity_curve diverges"
    );
    assert_eq!(
        serde_json::to_string(&fold.summary).expect("fold summary serializes"),
        serde_json::to_string(&step.summary).expect("step summary serializes"),
        "{ctx}: summary diverges"
    );
    assert_eq!(
        serde_json::to_string(&fold.skipped_entries).expect("fold skipped entries serialize"),
        serde_json::to_string(&step.skipped_entries).expect("step skipped entries serialize"),
        "{ctx}: skipped_entries diverges"
    );
}

// ---------------------------------------------------------------------------
// The committed 1-month fixture (the backtest_fixture.rs / dsl_schema_1_2.rs
// load recipe, verbatim)
// ---------------------------------------------------------------------------

fn load_fixture_primary() -> CandleSeries {
    let base =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
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

fn committed_document() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/strategies/rsi-oversold-long.json"),
    )
    .expect("read committed fixture strategy json")
}

fn btcusdt_filters() -> pulse::SymbolFilters {
    BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port")
}

/// Compile a DSL document through the G7 path (migrate → validate → compile).
fn compile_document(doc: &str) -> CompiledStrategy {
    let loaded = Migrator::v1().load(doc).expect("load (migrate) document");
    let validated = validate(&loaded.dsl).expect("document validates");
    compile(&validated).expect("document compiles")
}

// ---------------------------------------------------------------------------
// d56 (i) — the committed fixture: fold and step both hit the frozen pins
// ---------------------------------------------------------------------------

/// The canonical strategy over the committed 1-month fixture: the fold result
/// and the hand-driven step result are identical AND both reproduce
/// `GOLDEN_TRADE_COUNT` / `GOLDEN_NET_PNL` exactly.
#[test]
fn golden_fixture_fold_and_step_reproduce_the_frozen_golden() {
    let primary = load_fixture_primary();
    let compiled_strategy = compile_document(&committed_document());
    let filters = btcusdt_filters();
    let config = BacktestConfig::default();

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        None,
        None,
        &config,
        &filters,
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs over the fixture");
    let step = step_run(
        &compiled_strategy,
        &primary,
        None,
        None,
        &config,
        &filters,
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run over the fixture");

    let golden_net = dec_s(GOLDEN_NET_PNL);
    assert_eq!(
        fold.trades.len(),
        GOLDEN_TRADE_COUNT,
        "fold golden trade count drifted"
    );
    assert_eq!(fold.net_pnl, golden_net, "fold golden net P&L drifted");
    assert_eq!(
        step.trades.len(),
        GOLDEN_TRADE_COUNT,
        "step golden trade count drifted"
    );
    assert_eq!(step.net_pnl, golden_net, "step golden net P&L drifted");

    assert_results_identical(&fold, &step, "golden fixture");
}

/// The committed 1.0.0 document's run: fold and step are identical AND both
/// reproduce `FROZEN_COMMITTED_1_0_0`'s result content hash and trade log
/// byte-for-byte — the fold cannot drift where the base engine did not.
#[test]
fn committed_document_fold_and_step_match_the_frozen_pre_bump_run() {
    let primary = load_fixture_primary();
    let doc = committed_document();
    let compiled_strategy = compile_document(&doc);
    let filters = btcusdt_filters();
    let config = BacktestConfig::default();

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        None,
        None,
        &config,
        &filters,
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs over the fixture");
    let step = step_run(
        &compiled_strategy,
        &primary,
        None,
        None,
        &config,
        &filters,
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run over the fixture");

    for (label, result) in [("fold", &fold), ("step", &step)] {
        assert_eq!(
            result.result_content_hash(),
            FROZEN_COMMITTED_1_0_0_HASH,
            "{label}: frozen result_content_hash drifted"
        );
        assert_eq!(
            serde_json::to_string(&result.trades).expect("trade log serializes"),
            FROZEN_COMMITTED_1_0_0_TRADE_LOG,
            "{label}: frozen trade log drifted"
        );
    }

    assert_results_identical(&fold, &step, "committed 1.0.0 document");
}

// ---------------------------------------------------------------------------
// The manual step driver — for hand-scheduled HTF/D1 batches, refusal
// injections and split runs. The twin arm of every refusal/split test steps
// the IDENTICAL accepted feed without the injection, so any state the refused
// step leaked would diverge the final results.
// ---------------------------------------------------------------------------

struct Manual {
    session: EngineSession,
    htf: Vec<Candle>,
    d1: Vec<Candle>,
    hi: usize,
    di: usize,
}

impl Manual {
    fn new(
        compiled_strategy: &CompiledStrategy,
        htf: Vec<Candle>,
        d1: Vec<Candle>,
        count_from_ms: Option<i64>,
    ) -> Self {
        let session = EngineSession::new(
            compiled_strategy,
            &Pair::new("BTCUSDT"),
            SessionTimeframes {
                primary: Timeframe::M15,
                htf: if htf.is_empty() {
                    None
                } else {
                    Some(Timeframe::H4)
                },
                d1: if d1.is_empty() {
                    None
                } else {
                    Some(Timeframe::D1)
                },
            },
            zero_cost(),
            SymbolFilters::unconstrained(),
            count_from_ms,
        )
        .expect("session constructs");
        Self {
            session,
            htf,
            d1,
            hi: 0,
            di: 0,
        }
    }

    /// Hand `primary` with the not-yet-handed higher candles that closed at or
    /// before its close (the fold's cursor rule).
    fn step_drain(&mut self, primary: &Candle) -> Result<(), BacktestError> {
        let mut closed_htf: Vec<Candle> = Vec::new();
        while self
            .htf
            .get(self.hi)
            .is_some_and(|c| c.close_time <= primary.close_time)
        {
            closed_htf.push(self.htf[self.hi].clone());
            self.hi += 1;
        }
        let mut closed_d1: Vec<Candle> = Vec::new();
        while self
            .d1
            .get(self.di)
            .is_some_and(|c| c.close_time <= primary.close_time)
        {
            closed_d1.push(self.d1[self.di].clone());
            self.di += 1;
        }
        self.session.step(primary, &closed_htf, &closed_d1)
    }

    /// Hand `primary` with an EXPLICIT higher batch (the refusal-injection
    /// and hand-schedule seam).
    fn step_raw(
        &mut self,
        primary: &Candle,
        closed_htf: &[Candle],
        closed_d1: &[Candle],
    ) -> Result<(), BacktestError> {
        self.session.step(primary, closed_htf, closed_d1)
    }

    fn finish(self, series_end: SeriesEnd, last: Option<&Candle>) -> BacktestResult {
        self.session
            .finish(series_end, last)
            .expect("session finishes")
    }
}

// ---------------------------------------------------------------------------
// d56 (ii) — HTF + AtrStop: an Htf-EMA entry with an ATR stop folds and steps
// identically over a hand-built M15/H4 fixture, and the trade is real.
// ---------------------------------------------------------------------------

/// 96 flat-then-rising M15 bars; H4 closes `[50, 150, 90, 150, 90, 150]`.
/// Flat bars carry a true range of exactly 2.0 (`high = 101`, `low = 99`) so
/// the primary ATR(3) reads exactly 2.0 once warm — hand-checkable ATR-stop
/// geometry. `htf_ema(2) > 99` fires at the first warm paired H4 bar.
fn htf_fixture() -> (Vec<Candle>, Vec<Candle>) {
    let primary: Vec<Candle> = (0..96)
        .map(|i| {
            if i < 32 {
                m15(i, 100, 101, 99, 100)
            } else {
                let c = 100 + 2 * (i - 31);
                m15(i, c, c + 1, c - 1, c)
            }
        })
        .collect();
    let htf_closes = [50, 150, 90, 150, 90, 150];
    let htf_candles: Vec<Candle> = htf_closes
        .iter()
        .enumerate()
        .map(|(j, close)| {
            h4(
                i64::try_from(j).expect("six h4 bars"),
                *close,
                *close,
                *close,
                *close,
            )
        })
        .collect();
    (primary, htf_candles)
}

#[test]
fn htf_atr_strategy_folds_and_steps_identically() {
    let (primary, htf_candles) = htf_fixture();
    let primary = series(Timeframe::M15, primary);
    let htf = series(Timeframe::H4, htf_candles);
    let compiled_strategy = compiled(
        compare(htf_ema(2), pulse::Comparator::Gt, constant(99, 0)),
        vec![
            stop_loss(5, 2),
            ExitRule::SignalExit {
                condition: compare(htf_ema(2), pulse::Comparator::Lt, constant(99, 0)),
            },
        ],
        Direction::Long,
    );

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        Some(&htf),
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs");
    let step = step_run(
        &compiled_strategy,
        &primary,
        Some(&htf),
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run runs");

    // H4 closes `[50, 150, 90, 150, 90, 150]` drive `htf_ema(2)` through the
    // entry threshold — the EMA(2) engine is not warm until the THIRD paired
    // H4 value, so the first entry signal lands at M15 63 (EMA ≈ 133 > 99)
    // and fills at 64; no later pairing reads < 99, so the trade rides to
    // the data's end.
    assert_eq!(
        fold.trades.len(),
        1,
        "fixture must produce exactly one trade"
    );
    assert_eq!(
        step.trades.len(),
        1,
        "step must produce the same single trade"
    );
    assert_eq!(fold.trades[0].exit_reason, pulse::ExitReason::EndOfData);
    assert_eq!(step.trades[0].exit_reason, pulse::ExitReason::EndOfData);
    assert_results_identical(&fold, &step, "htf + atr fixture");
}

/// The ATR-stop variant of the same fixture: the stop derives from the signal
/// bar's primary ATR(3) — exactly 1.0 over the flat prefix — so the geometry
/// is hand-checkable and both drivers must agree to the last Decimal.
#[test]
fn htf_atr_stop_folds_and_steps_identically() {
    let (primary, htf_candles) = htf_fixture();
    let primary = series(Timeframe::M15, primary);
    let htf = series(Timeframe::H4, htf_candles);
    // A Price-leaf HTF entry needs no HTF indicator warm, so the first signal
    // lands at M15 31 (H4[1] closes 150 > 100) and fills at 32's open (102).
    let compiled_strategy = compiled(
        compare(
            htf_price(PriceField::Close),
            pulse::Comparator::Gt,
            constant(100, 0),
        ),
        vec![atr_stop(3, 1, 0), take_profit(10, 0)],
        Direction::Long,
    );

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        Some(&htf),
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs");
    let step = step_run(
        &compiled_strategy,
        &primary,
        Some(&htf),
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run runs");

    // Trade 1: fill 102 with the signal bar's flat-prefix ATR(3) = exactly
    // 2.0 (every flat bar's true range is 2.0): stop 100, TP 122 — hit on
    // M15 41. The paired H4[1] (close 150) still satisfies the entry, so the
    // run re-enters at 42 for a second TP, and again at 64 after H4[3]
    // closes — three take-profit trades in total.
    assert_eq!(
        fold.trades.len(),
        3,
        "fixture must produce exactly three trades"
    );
    assert_eq!(
        step.trades.len(),
        3,
        "step must produce the same three trades"
    );
    assert_eq!(fold.trades[0].exit_reason, pulse::ExitReason::TakeProfit);
    assert_eq!(step.trades[0].exit_reason, pulse::ExitReason::TakeProfit);
    assert_eq!(fold.trades[1].exit_reason, pulse::ExitReason::TakeProfit);
    assert_eq!(step.trades[1].exit_reason, pulse::ExitReason::TakeProfit);
    assert_eq!(fold.trades[2].exit_reason, pulse::ExitReason::TakeProfit);
    assert_eq!(step.trades[2].exit_reason, pulse::ExitReason::TakeProfit);
    // The ATR stop was frozen from the flat prefix: fill 102, ATR 2.0, stop
    // 100 — recorded verbatim on the trade by BOTH drivers.
    assert_eq!(fold.trades[0].stop_price, Some(dec_s("100")));
    assert_eq!(step.trades[0].stop_price, Some(dec_s("100")));
    assert_results_identical(&fold, &step, "htf + atr-stop fixture");
}

// ---------------------------------------------------------------------------
// d56 (iii) — H4 + D1: a daily-gated strategy folds and steps identically
// with all three series supplied (the H4 series is consumed by nothing —
// exactly the default resolver's shape).
// ---------------------------------------------------------------------------

#[test]
fn d1_gated_strategy_folds_and_steps_identically() {
    let primary: Vec<Candle> = (0..500)
        .map(|i| {
            if i <= 95 {
                m15(i, 100, 100, 100, 100)
            } else if i <= 100 {
                let c = 100 - (i - 95);
                m15(i, c, c + 1, c - 1, c)
            } else {
                m15(i, 95, 95, 95, 95)
            }
        })
        .collect();
    let htf_candles: Vec<Candle> = (0..125)
        .map(|j| h4(j, 100 + j, 100 + j, 100 + j, 100 + j))
        .collect();
    let d1_closes = [100, 101, 102, 103, 90, 91];
    let d1_candles: Vec<Candle> = d1_closes
        .iter()
        .enumerate()
        .map(|(j, close)| d1(i64::try_from(j).expect("six days"), *close))
        .collect();
    let primary = series(Timeframe::M15, primary);
    let htf = series(Timeframe::H4, htf_candles);
    let d1_series = series(Timeframe::D1, d1_candles);
    let compiled_strategy = compiled(
        compare(
            d1_price(PriceField::Close),
            pulse::Comparator::Lt,
            constant(101, 0),
        ),
        vec![stop_loss(5, 2)],
        Direction::Long,
    );

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        Some(&htf),
        Some(&d1_series),
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs");
    let step = step_run(
        &compiled_strategy,
        &primary,
        Some(&htf),
        Some(&d1_series),
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run runs");

    // Day 0 (close 100) closes on M15 95; the entry fires there and fills at
    // 96; the decline breaches the 5% stop at M15 100. Days 1–3 close at
    // 101/102/103 (never < 101), day 4 (90) closes at M15 479 and re-enters
    // for a second trade that rides to the data's end.
    assert_eq!(
        fold.trades.len(),
        2,
        "fixture must produce exactly two trades"
    );
    assert_eq!(
        step.trades.len(),
        2,
        "step must produce the same two trades"
    );
    assert_eq!(fold.trades[0].exit_reason, pulse::ExitReason::StopLoss);
    assert_eq!(step.trades[0].exit_reason, pulse::ExitReason::StopLoss);
    assert_eq!(fold.trades[1].exit_reason, pulse::ExitReason::EndOfData);
    assert_eq!(step.trades[1].exit_reason, pulse::ExitReason::EndOfData);
    assert_results_identical(&fold, &step, "h4 + d1 fixture");
}

// ---------------------------------------------------------------------------
// d56 (iv) — trailing- and time-stop exits: each label must actually occur
// (non-vacuous) and both drivers must agree.
// ---------------------------------------------------------------------------

#[test]
fn trailing_stop_exit_folds_and_steps_identically() {
    let primary: Vec<Candle> = (0..30)
        .map(|i| match i {
            3..=7 => {
                let c = 100 + 2 * (i - 2);
                m15(i, c, c + 1, c - 1, c)
            }
            8 => m15(i, 108, 109, 107, 106),
            _ => m15(i, 100, 100, 100, 100),
        })
        .collect();
    let primary = series(Timeframe::M15, primary);
    let compiled_strategy = compiled(price_entry(), vec![trail(2, 2)], Direction::Long);

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs");
    let step = step_run(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run runs");

    // Trade 1 exits at M15 8's open (108) through the trail tightened to
    // 111×0.98 = 108.78; the entry re-fires on the exit bar's close, and the
    // flat remainder rides that second position to the data's end.
    assert_eq!(fold.trades.len(), 2);
    assert_eq!(step.trades.len(), 2);
    // A real TrailingStop label on the first trade, on both drivers.
    assert_eq!(fold.trades[0].exit_reason, pulse::ExitReason::TrailingStop);
    assert_eq!(step.trades[0].exit_reason, pulse::ExitReason::TrailingStop);
    assert_results_identical(&fold, &step, "trailing stop");
}

#[test]
fn time_stop_exit_folds_and_steps_identically() {
    let primary: Vec<Candle> = (0..20).map(|i| m15(i, 100, 100, 100, 100)).collect();
    let primary = series(Timeframe::M15, primary);
    let compiled_strategy = compiled(
        price_entry(),
        vec![stop_loss(5, 2), time_stop(3)],
        Direction::Long,
    );

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs");
    let step = step_run(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run runs");

    // Flat 100 forever: each fill re-enters on its exit bar's close, so the
    // run is a chain of 3-bar time stops (5 trades; the last holds to the
    // data's end) — every completed exit carries the TimeStop label.
    assert_eq!(fold.trades.len(), 5);
    assert_eq!(step.trades.len(), 5);
    assert_eq!(fold.trades[0].exit_reason, pulse::ExitReason::TimeStop);
    assert_eq!(step.trades[0].exit_reason, pulse::ExitReason::TimeStop);
    assert_eq!(
        fold.trades[0].exit_fill_time,
        5 * Timeframe::M15.duration_ms()
    );
    assert_results_identical(&fold, &step, "time stop");
}

// ---------------------------------------------------------------------------
// d56 (v) — lead-in window ending at a WindowEdge with a position open: the
// run-start time is the first counted open and the open position is marked,
// on both drivers (and via the w3 accessor mid-run).
// ---------------------------------------------------------------------------

#[test]
fn lead_in_window_edge_open_position_folds_and_steps_identically() {
    let primary: Vec<Candle> = (0..40).map(|i| m15(i, 100, 100, 100, 100)).collect();
    let primary = series(Timeframe::M15, primary);
    let compiled_strategy = compiled(price_entry(), vec![stop_loss(50, 2)], Direction::Long);
    let count_from = 20 * Timeframe::M15.duration_ms();

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::WindowEdge,
        Some(count_from),
    )
    .expect("fold runs");
    let step = step_run(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::WindowEdge,
        Some(count_from),
    )
    .expect("step run runs");

    assert!(
        fold.open_position.is_some(),
        "a flat run at a window edge still holds the entry"
    );
    assert!(
        step.open_position.is_some(),
        "step must hold the same open position"
    );
    assert!(
        fold.trades.is_empty(),
        "a window edge never books the open position as a trade"
    );
    assert_eq!(
        fold.open_position.as_ref().map(|m| m.mark_time),
        primary.candles.last().map(|c| c.close_time),
        "the mark time is the last in-window close"
    );
    // The leading equity point sits on the first COUNTED open (M15 20).
    let first_counted = 20 * Timeframe::M15.duration_ms();
    assert_eq!(
        fold.equity_curve.0.first().map(|p| p.time_ms),
        Some(first_counted),
        "run start is the first counted open, not the snapshot's first bar"
    );
    assert_results_identical(&fold, &step, "lead-in window edge");

    // The w3 accessor path: mid-run the open position mark reads the live
    // position, and finishing the remainder reproduces the same result.
    let mut manual = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), Some(count_from));
    for bar in &primary.candles[..26] {
        manual.step_drain(bar).expect("mid-run steps accept");
    }
    let mark_at_26 = manual
        .session
        .open_position_mark(&primary.candles[25])
        .expect("position is open mid-run");
    assert_eq!(mark_at_26.mark_time, primary.candles[25].close_time);
    assert_eq!(manual.session.bars_stepped(), 26);
    let remainder = primary.candles[26..].to_vec();
    for bar in &remainder {
        manual.step_drain(bar).expect("remainder steps accept");
    }
    let resumed = manual.finish(SeriesEnd::WindowEdge, primary.candles.last());
    assert_eq!(
        serde_json::to_string(&resumed).expect("resumed serializes"),
        serde_json::to_string(&step).expect("step serializes"),
        "the split manual run reproduces the uninterrupted step result"
    );
}

// ---------------------------------------------------------------------------
// d56 (vi) — funding stamps crossing an open position: the held trade accrues
// real funding and both drivers agree to the last Decimal.
// ---------------------------------------------------------------------------

#[test]
fn funding_across_open_position_folds_and_steps_identically() {
    let m15_ms = Timeframe::M15.duration_ms();
    let primary: Vec<Candle> = (0..200)
        .map(|i| {
            let t = i * m15_ms;
            let stamp = if t % 28_800_000 == 0 {
                Some(dec_m(1, 3))
            } else {
                None
            };
            raw_m15_at(t, 100, 100, 100, 100, stamp)
        })
        .collect();
    let primary = series(Timeframe::M15, primary);
    let compiled_strategy = compiled(price_entry(), vec![stop_loss(50, 2)], Direction::Long);

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs");
    let step = step_run(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run runs");

    assert_eq!(fold.trades.len(), 1);
    assert_eq!(step.trades.len(), 1);
    // Filled at M15 2, force-closed at 199: the hold crosses the 32/64/96/
    // 128/160/192 boundaries — six stamped events, non-zero funding.
    assert_eq!(fold.trades[0].exit_reason, pulse::ExitReason::EndOfData);
    assert_ne!(
        fold.funding_total,
        Decimal::ZERO,
        "funding must actually accrue"
    );
    assert_eq!(
        fold.funding_total, step.funding_total,
        "funding totals must agree"
    );
    assert_results_identical(&fold, &step, "funding across position");
}

// ---------------------------------------------------------------------------
// d56 (vii) — split run: first half stepped, accessors read, remainder
// stepped, finished — equals both uninterrupted arms.
// ---------------------------------------------------------------------------

#[test]
fn split_run_matches_the_uninterrupted_arms() {
    let primary: Vec<Candle> = (0..60)
        .map(|i| {
            if i < 30 {
                m15(i, 100, 100, 100, 100)
            } else {
                let c = 100 + 2 * (i - 29);
                m15(i, c, c + 1, c - 1, c)
            }
        })
        .collect();
    let primary = series(Timeframe::M15, primary);
    let compiled_strategy = compiled(
        compare(
            primary_price(PriceField::Close),
            pulse::Comparator::Gt,
            constant(105, 0),
        ),
        vec![stop_loss(2, 2), take_profit(2, 0)],
        Direction::Long,
    );

    let fold = run_backtest(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("fold runs");
    let step = step_run(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("step run runs");

    let mut manual = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    for bar in &primary.candles[..31] {
        manual.step_drain(bar).expect("first-half steps accept");
    }
    assert_eq!(
        manual.session.bars_stepped(),
        31,
        "the first half was stepped"
    );
    assert!(
        manual.session.closed_trades().is_empty(),
        "no trade closes inside the flat prefix"
    );
    assert!(
        manual
            .session
            .open_position_mark(&primary.candles[30])
            .is_none(),
        "the entry has not filled inside the flat prefix"
    );
    for bar in &primary.candles[31..] {
        manual.step_drain(bar).expect("remainder steps accept");
    }
    let resumed = manual.finish(SeriesEnd::SnapshotEnd, primary.candles.last());

    // The rising tail keeps re-arming the `close > 105` entry, so the run is
    // a chain of take-profit trades — the ledger point is that BOTH arms and
    // the split manual run agree on every one of them.
    assert!(
        !fold.trades.is_empty(),
        "the rising tail must produce at least one trade (non-vacuous)"
    );
    assert_eq!(fold.trades[0].exit_reason, pulse::ExitReason::TakeProfit);
    assert_eq!(fold.trades.len(), step.trades.len());
    assert_eq!(fold.trades.len(), resumed.trades.len());
    assert_results_identical(&fold, &step, "split run fold-vs-step");
    assert_results_identical(&fold, &resumed, "split run fold-vs-resumed");
}

// ---------------------------------------------------------------------------
// d56 (viii) — step refusals. Every case asserts the typed variant, then
// proves FULL state unchanged: the refused session continues on the accepted
// feed and its finished result must equal a twin session stepped over the
// same accepted feed without the injection.
// ---------------------------------------------------------------------------

/// The mid-position primary fixture for the primary-feed refusals: flat 100
/// through M15 14, rising from 15 — the entry fills at 2, the take-profit
/// closes the trade at 20, so the injected refusals land MID-POSITION.
fn refusal_primary_fixture() -> Vec<Candle> {
    (0..30)
        .map(|i| {
            if i < 15 {
                m15(i, 100, 100, 100, 100)
            } else {
                let c = 100 + 2 * (i - 14);
                m15(i, c, c + 1, c - 1, c)
            }
        })
        .collect()
}

fn trading_strategy() -> CompiledStrategy {
    compiled(
        price_entry(),
        vec![stop_loss(5, 2), take_profit(2, 0)],
        Direction::Long,
    )
}

#[test]
fn step_refuses_an_out_of_order_primary_and_continues_unchanged() {
    let candles = refusal_primary_fixture();
    let compiled_strategy = trading_strategy();
    let m15_ms = Timeframe::M15.duration_ms();

    let mut refused = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    for bar in &candles[..10] {
        refused.step_drain(bar).expect("valid prefix accepts");
    }
    let err = refused
        .step_raw(&candles[5], &[], &[])
        .expect_err("an out-of-order primary must refuse");
    assert_eq!(
        err,
        BacktestError::SeriesUnsorted {
            series: pulse::SeriesRole::Primary,
            at: 5 * m15_ms
        },
        "the refusal names the out-of-order candle"
    );
    for bar in &candles[10..] {
        refused
            .step_drain(bar)
            .expect("the accepted remainder continues");
    }
    let refused_result = refused.finish(SeriesEnd::SnapshotEnd, candles.last());

    let mut twin = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    for bar in &candles {
        twin.step_drain(bar).expect("twin accepts");
    }
    let twin_result = twin.finish(SeriesEnd::SnapshotEnd, candles.last());

    assert!(
        !twin_result.trades.is_empty(),
        "the fixture trades (non-vacuous proof)"
    );
    assert_eq!(
        twin_result.trades[0].exit_reason,
        pulse::ExitReason::TakeProfit
    );
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "the refused step must leave the session's FULL state untouched"
    );
}

#[test]
fn step_refuses_a_duplicate_primary_and_continues_unchanged() {
    let candles = refusal_primary_fixture();
    let compiled_strategy = trading_strategy();
    let m15_ms = Timeframe::M15.duration_ms();

    let mut refused = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    for bar in &candles[..11] {
        refused.step_drain(bar).expect("valid prefix accepts");
    }
    let err = refused
        .step_raw(&candles[10], &[], &[])
        .expect_err("a repeated open_time must refuse");
    assert_eq!(
        err,
        BacktestError::SeriesUnsorted {
            series: pulse::SeriesRole::Primary,
            at: 10 * m15_ms
        },
        "the refusal names the duplicated open_time"
    );
    for bar in &candles[11..] {
        refused
            .step_drain(bar)
            .expect("the accepted remainder continues");
    }
    let refused_result = refused.finish(SeriesEnd::SnapshotEnd, candles.last());

    let mut twin = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    for bar in &candles {
        twin.step_drain(bar).expect("twin accepts");
    }
    let twin_result = twin.finish(SeriesEnd::SnapshotEnd, candles.last());

    assert!(!twin_result.trades.is_empty());
    assert_eq!(
        twin_result.trades[0].exit_reason,
        pulse::ExitReason::TakeProfit
    );
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "the refused step must leave the session's FULL state untouched"
    );
}

#[test]
fn step_refuses_a_gapped_primary_and_backfills_unchanged() {
    let candles = refusal_primary_fixture();
    let compiled_strategy = trading_strategy();
    let m15_ms = Timeframe::M15.duration_ms();

    let mut refused = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    for bar in &candles[..10] {
        refused.step_drain(bar).expect("valid prefix accepts");
    }
    let err = refused
        .step_raw(&candles[11], &[], &[])
        .expect_err("skipping M15 10 must refuse as a gap");
    assert_eq!(
        err,
        BacktestError::SeriesGap {
            series: pulse::SeriesRole::Primary,
            expected: 10 * m15_ms,
            found: 11 * m15_ms,
        },
        "the refusal names the expected and found open_times"
    );
    // The backfill PROVES the refused step never advanced the last-accepted
    // open: M15 10 is still lawful, and the run continues to the identical
    // result.
    for bar in &candles[10..] {
        refused
            .step_drain(bar)
            .expect("the backfilled remainder continues");
    }
    let refused_result = refused.finish(SeriesEnd::SnapshotEnd, candles.last());

    let mut twin = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    for bar in &candles {
        twin.step_drain(bar).expect("twin accepts");
    }
    let twin_result = twin.finish(SeriesEnd::SnapshotEnd, candles.last());

    assert!(!twin_result.trades.is_empty());
    assert_eq!(
        twin_result.trades[0].exit_reason,
        pulse::ExitReason::TakeProfit
    );
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "the refused step must leave the session's FULL state untouched"
    );
}

/// The funding refusal's accepted feed: M15 0 stamped (rate 1/3), M15 32
/// handed UNSTAMPED (the caller's corruption), M15 33 first arrives UNSTAMPED
/// (refuses — 8h30m past the anchor with no stamp), then M15 33 arrives WITH
/// the stamp (the caller's correction) and the run continues to M15 60. The
/// entry fills at 2 and holds to the end, so the corrected M15 33 stamp lands
/// inside the trade's funding window: any corruption of the funding index or
/// anchor would move `funding_total` and diverge the twin.
#[test]
fn step_refuses_a_funding_gap_and_the_correction_continues_unchanged() {
    let m15_ms = Timeframe::M15.duration_ms();
    let stamp = dec_m(1, 3);
    let candle_at = |i: i64, stamped: bool| {
        let t = i * m15_ms;
        raw_m15_at(
            t,
            100,
            100,
            100,
            100,
            if stamped { Some(stamp) } else { None },
        )
    };
    let compiled_strategy = compiled(price_entry(), vec![stop_loss(50, 2)], Direction::Long);

    let mut refused = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    refused
        .step_raw(&candle_at(0, true), &[], &[])
        .expect("M15 0 accepts");
    for i in 1..=32 {
        refused
            .step_raw(&candle_at(i, false), &[], &[])
            .expect("prefix accepts");
    }
    let err = refused
        .step_raw(&candle_at(33, false), &[], &[])
        .expect_err("8h30m past the anchor with no stamp must refuse");
    assert_eq!(
        err,
        BacktestError::FundingGap {
            from: 0,
            to: 33 * m15_ms + m15_ms - 1
        },
        "the refusal names the uncovered segment's anchors"
    );
    let err_again = refused
        .step_raw(&candle_at(33, false), &[], &[])
        .expect_err("the same bad candle must refuse again");
    assert_eq!(
        err, err_again,
        "the refusal is idempotent — nothing advanced"
    );
    refused
        .step_raw(&candle_at(33, true), &[], &[])
        .expect("the corrected stamp accepts");
    for i in 34..=60 {
        refused
            .step_raw(&candle_at(i, false), &[], &[])
            .expect("the run continues");
    }
    let last = candle_at(60, false);
    let refused_result = refused.finish(SeriesEnd::SnapshotEnd, Some(&last));

    // The twin steps the IDENTICAL accepted feed without the injection.
    let mut twin = Manual::new(&compiled_strategy, Vec::new(), Vec::new(), None);
    twin.step_raw(&candle_at(0, true), &[], &[])
        .expect("twin M15 0 accepts");
    for i in 1..=32 {
        twin.step_raw(&candle_at(i, false), &[], &[])
            .expect("twin prefix accepts");
    }
    twin.step_raw(&candle_at(33, true), &[], &[])
        .expect("twin corrected M15 33 accepts");
    for i in 34..=60 {
        twin.step_raw(&candle_at(i, false), &[], &[])
            .expect("twin run continues");
    }
    let twin_result = twin.finish(SeriesEnd::SnapshotEnd, Some(&last));

    assert_eq!(
        twin_result.trades.len(),
        1,
        "the fixture holds a trade (non-vacuous)"
    );
    assert_ne!(
        twin_result.funding_total,
        Decimal::ZERO,
        "the corrected stamp must sit inside the funding window"
    );
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "the funding refusal must leave the funding index, anchor and span state untouched"
    );
}

/// The HTF refusal fixture: 96 flat M15 bars; the hand schedule pairs H4[0]
/// at M15 15, H4[1] at 31, H4[2] at 47, H4[3] at 63, H4[4] at 79, H4[5] at
/// 95 (each exactly when its `close_time` allows). `htf_ema(2)` drives both
/// the entry and the signal exit, so a leaked double-step at the refusal
/// point would shift the engine ring and diverge the twin's trade.
fn htf_refusal_fixture() -> (Vec<Candle>, Vec<Candle>, Vec<(usize, usize)>) {
    let primary: Vec<Candle> = (0..96).map(|i| m15(i, 100, 100, 100, 100)).collect();
    let htf_closes = [50, 150, 90, 150, 90, 150];
    let htf_candles: Vec<Candle> = htf_closes
        .iter()
        .enumerate()
        .map(|(j, close)| {
            h4(
                i64::try_from(j).expect("six h4 bars"),
                *close,
                *close,
                *close,
                *close,
            )
        })
        .collect();
    let schedule: Vec<(usize, usize)> = (0..6).map(|k| (15 + 16 * k, k)).collect();
    (primary, htf_candles, schedule)
}

fn htf_signal_strategy() -> CompiledStrategy {
    compiled(
        compare(htf_ema(2), pulse::Comparator::Gt, constant(99, 0)),
        vec![
            stop_loss(5, 2),
            ExitRule::SignalExit {
                condition: compare(htf_ema(2), pulse::Comparator::Lt, constant(99, 0)),
            },
        ],
        Direction::Long,
    )
}

/// Run the HTF hand schedule, optionally injecting a batch (with its EXPECTED
/// refusal) at a step. EVERY primary bar is stepped in order; the scheduled
/// bar hands its H4 candle (exactly when the fold's cursor would drain it),
/// all other bars hand an empty batch. Both arms of every HTF refusal test
/// use this same schedule, so the accepted feeds are identical.
fn run_htf_schedule(
    compiled_strategy: &CompiledStrategy,
    primary: &[Candle],
    htf_candles: &[Candle],
    schedule: &[(usize, usize)],
    inject: Option<(usize, Vec<Candle>, BacktestError)>,
) -> BacktestResult {
    let mut manual = Manual::new(compiled_strategy, htf_candles.to_vec(), Vec::new(), None);
    let mut injector = inject;
    for (bar_idx, bar) in primary.iter().enumerate() {
        if let Some((at, ref batch, ref expected)) = injector
            && at == bar_idx
        {
            let err = manual
                .step_raw(bar, batch, &[])
                .expect_err("the injected batch must refuse");
            assert_eq!(
                &err, expected,
                "the injected batch refuses with the typed variant"
            );
            injector = None;
        }
        let scheduled = schedule
            .iter()
            .find(|(idx, _)| idx == &bar_idx)
            .map(|(_, k)| *k);
        let batch: Vec<Candle> = match scheduled {
            Some(k) => htf_candles[k..=k].to_vec(),
            None => Vec::new(),
        };
        manual
            .step_raw(bar, &batch, &[])
            .expect("scheduled batch accepts");
    }
    let last = primary.last().expect("non-empty primary");
    manual.finish(SeriesEnd::SnapshotEnd, Some(last))
}

#[test]
fn step_refuses_an_out_of_order_htf_batch_and_continues_unchanged() {
    let (primary, htf_candles, schedule) = htf_refusal_fixture();
    let compiled_strategy = htf_signal_strategy();
    let h4_ms = Timeframe::H4.duration_ms();

    let refused_result = run_htf_schedule(
        &compiled_strategy,
        &primary,
        &htf_candles,
        &schedule,
        Some((
            31,
            vec![htf_candles[2].clone(), htf_candles[1].clone()],
            BacktestError::SeriesUnsorted {
                series: pulse::SeriesRole::Htf,
                at: h4_ms,
            },
        )),
    );
    let twin_result = run_htf_schedule(&compiled_strategy, &primary, &htf_candles, &schedule, None);

    assert_eq!(
        twin_result.trades.len(),
        1,
        "the fixture trades (non-vacuous)"
    );
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "an out-of-order HTF batch must leave the engine ring and pairing untouched"
    );
}

#[test]
fn step_refuses_a_duplicate_htf_batch_and_continues_unchanged() {
    let (primary, htf_candles, schedule) = htf_refusal_fixture();
    let compiled_strategy = htf_signal_strategy();
    let h4_ms = Timeframe::H4.duration_ms();

    let refused_result = run_htf_schedule(
        &compiled_strategy,
        &primary,
        &htf_candles,
        &schedule,
        Some((
            31,
            vec![htf_candles[1].clone(), htf_candles[1].clone()],
            BacktestError::SeriesUnsorted {
                series: pulse::SeriesRole::Htf,
                at: h4_ms,
            },
        )),
    );
    let twin_result = run_htf_schedule(&compiled_strategy, &primary, &htf_candles, &schedule, None);

    assert_eq!(twin_result.trades.len(), 1);
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "a duplicate HTF candle must be refused before it can double-step the engine"
    );
}

#[test]
fn step_refuses_a_not_yet_closed_htf_candle_and_continues_unchanged() {
    let (primary, htf_candles, schedule) = htf_refusal_fixture();
    let compiled_strategy = htf_signal_strategy();
    let m15_ms = Timeframe::M15.duration_ms();
    let h4_ms = Timeframe::H4.duration_ms();

    let mut refused = Manual::new(&compiled_strategy, htf_candles.clone(), Vec::new(), None);
    for (bar_idx, bar) in primary.iter().enumerate() {
        if bar_idx == 31 {
            // H4[2] only CLOSES at M15 47 — handing it at 31 must refuse.
            // H4[1] rides ahead of it so the batch has no gap (a gap refuses
            // first, as the fold's series validation does).
            let err = refused
                .step_raw(bar, &[htf_candles[1].clone(), htf_candles[2].clone()], &[])
                .expect_err("a still-forming H4 candle must refuse");
            assert_eq!(
                err,
                BacktestError::HigherBarNotClosed {
                    series: pulse::SeriesRole::Htf,
                    close_time: 3 * h4_ms - 1,
                    primary_close: 31 * m15_ms + m15_ms - 1,
                }
            );
        }
        let scheduled = schedule
            .iter()
            .find(|(idx, _)| idx == &bar_idx)
            .map(|(_, k)| *k);
        let batch: Vec<Candle> = match scheduled {
            Some(k) => htf_candles[k..=k].to_vec(),
            None => Vec::new(),
        };
        refused
            .step_raw(bar, &batch, &[])
            .expect("scheduled batch accepts");
    }
    assert_eq!(
        refused.session.bars_stepped(),
        96,
        "all 96 bars were stepped across the refusal"
    );
    let refused_result = refused.finish(SeriesEnd::SnapshotEnd, primary.last());
    let twin_result = run_htf_schedule(&compiled_strategy, &primary, &htf_candles, &schedule, None);

    assert_eq!(twin_result.trades.len(), 1);
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "the not-closed refusal must leave pairing and engine state untouched"
    );
}

/// The D1 refusal fixture: 500 flat M15 bars; D1 closes `[100, 90, 105, 95,
/// 105]` paired at M15 95/191/287/383/479. `d1_ema(2)` drives the entry
/// (> 90 fires at M15 191 — the clean EMA there is exactly 95) and the signal
/// exit (< 90), so a leaked double-step of D1[1] drops that EMA to exactly 90
/// and the entry timing provably shifts (entry at 288 instead of 192).
fn d1_refusal_fixture() -> (Vec<Candle>, Vec<Candle>, Vec<(usize, usize)>) {
    let primary: Vec<Candle> = (0..500).map(|i| m15(i, 100, 100, 100, 100)).collect();
    let d1_closes = [100, 90, 105, 95, 105];
    let d1_candles: Vec<Candle> = d1_closes
        .iter()
        .enumerate()
        .map(|(j, close)| d1(i64::try_from(j).expect("five days"), *close))
        .collect();
    let schedule: Vec<(usize, usize)> =
        [(95usize, 0usize), (191, 1), (287, 2), (383, 3), (479, 4)].to_vec();
    (primary, d1_candles, schedule)
}

fn d1_signal_strategy() -> CompiledStrategy {
    compiled(
        compare(d1_ema(2), pulse::Comparator::Gt, constant(90, 0)),
        vec![
            stop_loss(50, 2),
            ExitRule::SignalExit {
                condition: compare(d1_ema(2), pulse::Comparator::Lt, constant(90, 0)),
            },
        ],
        Direction::Long,
    )
}

/// The D1 mirror of [`run_htf_schedule`]: every primary bar stepped, the
/// scheduled bar handing its daily candle.
fn run_d1_schedule(
    compiled_strategy: &CompiledStrategy,
    primary: &[Candle],
    d1_candles: &[Candle],
    schedule: &[(usize, usize)],
    inject: Option<(usize, Vec<Candle>, BacktestError)>,
) -> BacktestResult {
    let mut manual = Manual::new(compiled_strategy, Vec::new(), d1_candles.to_vec(), None);
    let mut injector = inject;
    for (bar_idx, bar) in primary.iter().enumerate() {
        if let Some((at, ref batch, ref expected)) = injector
            && at == bar_idx
        {
            let err = manual
                .step_raw(bar, &[], batch)
                .expect_err("the injected D1 batch must refuse");
            assert_eq!(
                &err, expected,
                "the injected batch refuses with the typed variant"
            );
            injector = None;
        }
        let scheduled = schedule
            .iter()
            .find(|(idx, _)| idx == &bar_idx)
            .map(|(_, k)| *k);
        let batch: Vec<Candle> = match scheduled {
            Some(k) => d1_candles[k..=k].to_vec(),
            None => Vec::new(),
        };
        manual
            .step_raw(bar, &[], &batch)
            .expect("scheduled D1 batch accepts");
    }
    manual.finish(SeriesEnd::SnapshotEnd, primary.last())
}

/// Step every primary candle through a [`Manual`] driver (the fold's cursor
/// rule) and return the first refusal.
fn first_step_refusal(
    compiled_strategy: &CompiledStrategy,
    primary: &[Candle],
    htf: Vec<Candle>,
    d1_candles: Vec<Candle>,
) -> Option<BacktestError> {
    let mut manual = Manual::new(compiled_strategy, htf, d1_candles, None);
    primary.iter().find_map(|bar| manual.step_drain(bar).err())
}

/// Round 1 (droid `session.rs:404`): a higher series with a missing candle is
/// refused by `step` exactly as `run_backtest` refuses it — the same
/// `SeriesGap`, the same expected and found instants — for HTF and for D1.
#[test]
fn step_refuses_a_gapped_higher_series_exactly_as_the_fold_does() {
    // HTF: the htf fixture with its third H4 candle removed.
    let (primary_candles, mut htf_candles) = htf_fixture();
    let missing = htf_candles.remove(2);
    let compiled_strategy = compiled(
        compare(htf_ema(2), pulse::Comparator::Gt, constant(99, 0)),
        vec![stop_loss(5, 2)],
        Direction::Long,
    );
    let fold = run_backtest(
        &compiled_strategy,
        &series(Timeframe::M15, primary_candles.clone()),
        Some(&series(Timeframe::H4, htf_candles.clone())),
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect_err("the fold refuses a gapped HTF series");
    let expected = BacktestError::SeriesGap {
        series: pulse::SeriesRole::Htf,
        expected: missing.open_time,
        found: missing.open_time + Timeframe::H4.duration_ms(),
    };
    assert_eq!(fold, expected);
    assert_eq!(
        first_step_refusal(
            &compiled_strategy,
            &primary_candles,
            htf_candles,
            Vec::new()
        ),
        Some(expected),
        "step refuses the HTF gap the fold refuses"
    );

    // D1: three days of M15 bars with day 1 missing from the daily series.
    let primary_candles: Vec<Candle> = (0..288).map(|i| m15(i, 100, 100, 100, 100)).collect();
    let d1_candles = vec![d1(0, 100), d1(2, 100)];
    let compiled_strategy = compiled(
        compare(
            d1_price(PriceField::Close),
            pulse::Comparator::Lt,
            constant(101, 0),
        ),
        vec![stop_loss(5, 2)],
        Direction::Long,
    );
    let fold = run_backtest(
        &compiled_strategy,
        &series(Timeframe::M15, primary_candles.clone()),
        None,
        Some(&series(Timeframe::D1, d1_candles.clone())),
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect_err("the fold refuses a gapped D1 series");
    let expected = BacktestError::SeriesGap {
        series: pulse::SeriesRole::D1,
        expected: Timeframe::D1.duration_ms(),
        found: 2 * Timeframe::D1.duration_ms(),
    };
    assert_eq!(fold, expected);
    assert_eq!(
        first_step_refusal(&compiled_strategy, &primary_candles, Vec::new(), d1_candles),
        Some(expected),
        "step refuses the D1 gap the fold refuses"
    );
}

#[test]
fn step_refuses_an_out_of_order_d1_batch_and_continues_unchanged() {
    let (primary, d1_candles, schedule) = d1_refusal_fixture();
    let compiled_strategy = d1_signal_strategy();
    let d1_ms = Timeframe::D1.duration_ms();

    let refused_result = run_d1_schedule(
        &compiled_strategy,
        &primary,
        &d1_candles,
        &schedule,
        Some((
            191,
            vec![d1_candles[2].clone(), d1_candles[1].clone()],
            BacktestError::SeriesUnsorted {
                series: pulse::SeriesRole::D1,
                at: d1_ms,
            },
        )),
    );
    let twin_result = run_d1_schedule(&compiled_strategy, &primary, &d1_candles, &schedule, None);

    assert_eq!(
        twin_result.trades.len(),
        1,
        "the fixture trades (non-vacuous)"
    );
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "an out-of-order D1 batch must leave the daily engine and pairing untouched"
    );
}

#[test]
fn step_refuses_a_duplicate_d1_batch_and_continues_unchanged() {
    let (primary, d1_candles, schedule) = d1_refusal_fixture();
    let compiled_strategy = d1_signal_strategy();
    let d1_ms = Timeframe::D1.duration_ms();

    let refused_result = run_d1_schedule(
        &compiled_strategy,
        &primary,
        &d1_candles,
        &schedule,
        Some((
            191,
            vec![d1_candles[1].clone(), d1_candles[1].clone()],
            BacktestError::SeriesUnsorted {
                series: pulse::SeriesRole::D1,
                at: d1_ms,
            },
        )),
    );
    let twin_result = run_d1_schedule(&compiled_strategy, &primary, &d1_candles, &schedule, None);

    assert!(!twin_result.trades.is_empty());
    assert_eq!(
        twin_result.trades[0].exit_reason,
        pulse::ExitReason::EndOfData
    );
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "a duplicate D1 candle must be refused before it can double-step the engine"
    );
}

#[test]
fn step_refuses_a_not_yet_closed_d1_candle_and_continues_unchanged() {
    let (primary, d1_candles, schedule) = d1_refusal_fixture();
    let compiled_strategy = d1_signal_strategy();
    let m15_ms = Timeframe::M15.duration_ms();
    let d1_ms = Timeframe::D1.duration_ms();

    let mut refused = Manual::new(&compiled_strategy, Vec::new(), d1_candles.clone(), None);
    for (bar_idx, bar) in primary.iter().enumerate() {
        if bar_idx == 190 {
            // D1[1] only CLOSES at M15 191's close_time — handing it at 190
            // must refuse.
            let err = refused
                .step_raw(bar, &[], &[d1_candles[1].clone()])
                .expect_err("a still-forming daily candle must refuse");
            assert_eq!(
                err,
                BacktestError::HigherBarNotClosed {
                    series: pulse::SeriesRole::D1,
                    close_time: 2 * d1_ms - 1,
                    primary_close: 190 * m15_ms + m15_ms - 1,
                }
            );
        }
        let scheduled = schedule
            .iter()
            .find(|(idx, _)| idx == &bar_idx)
            .map(|(_, k)| *k);
        let batch: Vec<Candle> = match scheduled {
            Some(k) => d1_candles[k..=k].to_vec(),
            None => Vec::new(),
        };
        refused
            .step_raw(bar, &[], &batch)
            .expect("scheduled batch accepts");
    }
    let refused_result = refused.finish(SeriesEnd::SnapshotEnd, primary.last());
    let twin_result = run_d1_schedule(&compiled_strategy, &primary, &d1_candles, &schedule, None);

    assert!(!twin_result.trades.is_empty());
    assert_eq!(
        twin_result.trades[0].exit_reason,
        pulse::ExitReason::EndOfData
    );
    assert_eq!(
        serde_json::to_string(&refused_result).expect("refused serializes"),
        serde_json::to_string(&twin_result).expect("twin serializes"),
        "the D1 not-closed refusal must leave pairing and engine state untouched"
    );
}

// ---------------------------------------------------------------------------
// d56 (ix) — the close-halt sizing-atomicity regression: a sizing/geometry
// refusal raised by the counted fill (ImpossibleStop and its siblings) leaves
// the session's full state untouched and the identical retry repeats the same
// typed error. The hidden-field proof lives in session.rs's `state_digest`
// unit test; this one pins the public surface and the fold counterpart.
// ---------------------------------------------------------------------------

/// The probe fixture: ATR(5)×2, `close > 0`, positive-OHLC candles (true
/// range exactly 1.0), funding stamp only on bar 0.
fn sizing_refusal_fixture() -> (CompiledStrategy, Vec<Candle>) {
    let dsl = dsl(
        compare(
            primary_price(PriceField::Close),
            pulse::Comparator::Gt,
            constant(0, 0),
        ),
        vec![atr_stop(5, 2, 0)],
        Direction::Long,
    );
    let compiled_strategy =
        compile(&validate(&dsl).expect("fixture validates")).expect("fixture compiles");
    let m15_ms = Timeframe::M15.duration_ms();
    let candles: Vec<Candle> = (0..15)
        .map(|i| {
            let t = i * m15_ms;
            Candle {
                open_time: t,
                close_time: (i + 1) * m15_ms - 1,
                open: dec_s("1"),
                high: dec_s("1.5"),
                low: dec_s("0.5"),
                close: dec_s("1"),
                volume: dec_s("1"),
                funding_rate: if i == 0 { Some(Decimal::ZERO) } else { None },
            }
        })
        .collect();
    (compiled_strategy, candles)
}

#[test]
fn sizing_refusal_is_atomic_state_and_retry_repeats_the_typed_error() {
    let (compiled_strategy, candles) = sizing_refusal_fixture();
    let mut session = EngineSession::new(
        &compiled_strategy,
        &Pair::new("BTCUSDT"),
        SessionTimeframes {
            primary: Timeframe::M15,
            htf: None,
            d1: None,
        },
        zero_cost(),
        SymbolFilters::unconstrained(),
        None,
    )
    .expect("session constructs");
    for bar in &candles[..7] {
        session.step(bar, &[], &[]).expect("warm-up steps accept");
    }
    let before_trades = session.closed_trades().to_vec();
    let before_bars = session.bars_stepped();
    let before_mark = session.open_position_mark(&candles[7]);

    let first = session
        .step(&candles[7], &[], &[])
        .expect_err("the negative-stop ATR fill refuses");
    assert!(
        matches!(first, BacktestError::ImpossibleStop(_)),
        "the refusal is the typed ImpossibleStop: {first:?}"
    );
    assert_eq!(
        session.bars_stepped(),
        before_bars,
        "a refused step never counts"
    );
    assert_eq!(
        session.closed_trades(),
        before_trades,
        "no trade may appear"
    );
    assert_eq!(
        session.open_position_mark(&candles[7]),
        before_mark,
        "no position may appear or move"
    );

    let retry = session
        .step(&candles[7], &[], &[])
        .expect_err("the identical retry must refuse again");
    assert_eq!(
        format!("{first:?}"),
        format!("{retry:?}"),
        "the identical retry returns the same typed error"
    );
    assert_eq!(session.bars_stepped(), before_bars);

    // The refusal is persistent by design — the pending entry stays exactly
    // as it was, so the identical bar keeps repeating the same refusal until
    // the caller discards the session (bar 8 without an accepted bar 7 is a
    // different, legitimate gap refusal — atomicity means the refused bar is
    // never silently consumed). No phantom trade or position may surface, and
    // the finish path stays clean.
    let again = session
        .step(&candles[7], &[], &[])
        .expect_err("the stuck pending keeps refusing the identical bar");
    assert_eq!(format!("{again:?}"), format!("{first:?}"));
    assert_eq!(session.bars_stepped(), before_bars);
    assert!(session.closed_trades().is_empty());
    assert!(
        session
            .open_position_mark(candles.last().expect("non-empty"))
            .is_none()
    );
    let result = session
        .finish(SeriesEnd::SnapshotEnd, candles.last())
        .expect("finish over a never-filled run is clean");
    assert!(result.trades.is_empty());
    assert!(result.open_position.is_none());

    // The fold counterpart: the whole-run path over the same series raises
    // the same typed refusal (the fill is the same fallible route) — the two
    // drivers cannot diverge on the error either.
    let primary = series(Timeframe::M15, candles);
    let fold_err = run_backtest(
        &compiled_strategy,
        &primary,
        None,
        None,
        &zero_cost(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect_err("the fold refuses the same fill");
    assert!(
        matches!(fold_err, BacktestError::ImpossibleStop(_)),
        "the fold's refusal is the same typed error: {fold_err:?}"
    );
}
