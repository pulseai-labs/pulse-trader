//! r2.s2.w2 — the HTF operand + ATR-stop engine semantics (the `d20` target).
//!
//! Schema 1.1.0 declared two things this suite pins as *behaviour*, not grammar:
//! an `series: "htf"` operand reads the **last closed** higher-timeframe bar, and
//! an `AtrStop` exit resolves to a per-trade stop frozen at the signal bar. Every
//! case is hand-built M15 + H4 candle pairs driven through the real
//! [`run_backtest`] loop, plus one application-ring refusal and one
//! persist-and-reload hash oracle.
//!
//! What each lettered case proves:
//!
//! - **(a)** an `htf` price operand reads the paired *closed* H4 bar — the
//!   still-forming bar's close is never visible, so a comparison that would flip
//!   on it cannot fire early.
//! - **(b)** `previous()` on an `htf` operand is H4-bar-relative — an H4 cross
//!   fires once per *closed* H4 bar (and stays "just crossed" for the whole
//!   primary window it spans), not once per M15 bar.
//! - **(c)** the ATR stop is `entry − multiple × ATR(period)` from the **signal**
//!   bar (Long) and `+` for Short, filled at the next bar's open — and frozen:
//!   a later-bar ATR update does not move it.
//! - **(d)** an unwarm ATR at signal time produces no entry — the first signal
//!   cannot precede the bar where `Atr(period)` first exists.
//! - **(e)** `needs_htf()` on a request with no H4 snapshot → `HtfRequired`
//!   through the application ring, before any candle work.
//! - **(f)** two cold runs of an HTF+ATR strategy over the fixture persist an
//!   identical `result_content_hash` (the repeat-twice oracle).
//! - **(g)** every trade's `stop_price` equals the geometric stop, and a
//!   windowed run's open-position mark is unaffected.
//! - **(h)** a non-higher `inputs.htf` selection is refused with the typed
//!   field-pathed error — at the request boundary before any candle I/O, and
//!   again inside `run_backtest` as engine-level defence (round-1 fix F1/F6).
//! - **(i)** the HTF engine steps EVERY closed H4 candle exactly once — H4
//!   lead-in history closed before the first primary bar warms the indicator,
//!   and no closed H4 bar is ever fed twice (round-1 fix F2).
//! - **(j)** when the strategy needs HTF, entries and signal exits wait for a
//!   paired closed H4 bar — a `Not(...)` over an absent `Htf` operand reads
//!   `true`, so the gate must not be vacuous for a Price-leaf-only strategy
//!   (round-1 fix F3).
//! - **(k)** an ATR-derived stop resolving to an untradeable level — a
//!   non-positive price, or `stop == entry` on a flat series (zero distance;
//!   case (m)) — refuses with the typed `ImpossibleStop`, not the generic
//!   `NoStopLoss`, not a silent skip (round-1 fix F4; the zero-distance leg
//!   is r2.s2 round-3).
//! - **(l)** a supplied HTF series for a DIFFERENT pair is refused with the
//!   typed `HtfPairMismatch` before alignment — `Series::Htf` operands must
//!   never read another symbol's bars — while a matching pair still runs
//!   (round-2 fix G1).
//! - **(m)** a flat series drives the frozen ATR to exactly `0`, so the stop
//!   resolves to `stop == entry` — positive, which the arm-local check missed
//!   — and the hoisted geometry guard refuses `ImpossibleStop`, never the
//!   generic `NoStopLoss` (round-3 fix, same class as F4).
//! - **(n)** an HTF series whose coverage ends more than one HTF interval
//!   before the primary's end is the typed `HtfCoverageShort` refusal — on the
//!   versioned/application path AND the `pulse backtest --dsl` CLI path —
//!   because `align` pairs forward-only and would otherwise read the frozen
//!   final HTF bar for the rest of the run; the one-interval-short live shape
//!   and the empty (windowed) HTF slice still run (r2.s2 round-5). The check
//!   applies only when the strategy consumes the HTF series — a primary-only
//!   strategy handed the default-resolved, stale-but-unused H4 snapshot still
//!   runs (r2.s2 round-6 gate).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    BacktestAppError, BacktestConfig, BacktestError, BacktestRequest, BacktestResult,
    BinanceAdapter, Candle, CandleSeries, CandleSeriesRepository, CandleStore, Comparator,
    CompiledStrategy, Condition, CreatedBy, DataVersion, Db, Direction, ExitReason, ExitRule,
    IndicatorSpec, MIGRATOR, NewVersion, Pair, PriceField, RiskParams, SchemaVersion, Series,
    SeriesEnd, SqliteBacktestRunRepo, SqliteStrategyRepo, StrategyDsl, StrategyRepository,
    SweepableValue, SymbolFilters, Timeframe, ValueSource, atr_stop_price, compile, run_backtest,
    run_version_backtest, stop_price, validate,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

// ---------------------------------------------------------------------------
// Hand-built candle series
// ---------------------------------------------------------------------------

/// One H4 candle: `open_time = j × 4h`, `close_time = open + 4h − 1`.
fn h4(j: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
    let open_time = j * Timeframe::H4.duration_ms();
    Candle {
        open_time,
        close_time: open_time + Timeframe::H4.duration_ms() - 1,
        open: dec(open, 0),
        high: dec(high, 0),
        low: dec(low, 0),
        close: dec(close, 0),
        volume: dec(1, 0),
        funding_rate: None,
    }
}

fn series(timeframe: Timeframe, candles: Vec<Candle>) -> CandleSeries {
    CandleSeries {
        pair: Pair::new("BTCUSDT"),
        timeframe,
        version: DataVersion::new("v-htf-atr"),
        candles,
    }
}

/// A flat M15 series whose every bar has true range exactly `1.0` — `open =
/// close = 100`, `high = 100.5`, `low = 99.5` — so Wilder ATR(any period) reads
/// exactly `1.0` once warm (TR = max(1.0, |0.5|, |−0.5|) = 1.0 on every bar
/// after the first). `n` bars.
fn flat_m15(n: i64) -> Vec<Candle> {
    flat_m15_from(0, n)
}

/// [`flat_m15`] whose first bar is absolute M15 index `start` — so the series
/// can begin mid-stream while the H4 fixture keeps its own `h4(j)` indexing.
fn flat_m15_from(start: i64, n: i64) -> Vec<Candle> {
    (start..start + n)
        .map(|i| {
            let open_time = i * Timeframe::M15.duration_ms();
            Candle {
                open_time,
                close_time: open_time + Timeframe::M15.duration_ms() - 1,
                open: dec(100, 0),
                high: dec(1005, 1), // 100.5
                low: dec(995, 1),   // 99.5
                close: dec(100, 0),
                volume: dec(1, 0),
                funding_rate: None,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Strategy builders
// ---------------------------------------------------------------------------

fn fixed_u32(v: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(v)
}

fn fixed_dec(mantissa: i64, scale: u32) -> SweepableValue<Decimal> {
    SweepableValue::Fixed(dec(mantissa, scale))
}

fn constant(mantissa: i64, scale: u32) -> ValueSource {
    ValueSource::Constant {
        value: dec(mantissa, scale),
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

fn primary_price(field: PriceField) -> ValueSource {
    ValueSource::Price {
        series: Series::Primary,
        field,
    }
}

fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

fn stop_loss() -> ExitRule {
    ExitRule::StopLoss {
        distance_pct: fixed_dec(5, 2),
    }
}

fn atr_stop(period: u32, multiple_mantissa: i64, multiple_scale: u32) -> ExitRule {
    ExitRule::AtrStop {
        period: fixed_u32(period),
        multiple: fixed_dec(multiple_mantissa, multiple_scale),
    }
}

fn dsl(entry: Condition, exits: Vec<ExitRule>, direction: Direction) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "htf-atr fixture".to_owned(),
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

fn compiled(dsl: &StrategyDsl) -> CompiledStrategy {
    compile(&validate(dsl).expect("fixture validates")).expect("fixture compiles")
}

/// Zero-slippage config so fill prices equal raw candle opens — makes the ATR
/// geometry exact and hand-checkable.
fn zero_slippage() -> BacktestConfig {
    BacktestConfig {
        starting_equity: dec(10_000, 0),
        taker_fee_bps: dec(0, 0),
        slippage_bps: dec(0, 0),
    }
}

fn run(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    series_end: SeriesEnd,
) -> BacktestResult {
    run_backtest(
        compiled,
        primary,
        htf,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        series_end,
    )
    .expect("backtest runs")
}

// ---------------------------------------------------------------------------
// (a) an `htf` operand reads the paired CLOSED H4 bar — never the forming one
// ---------------------------------------------------------------------------

/// `htf.close > 100` fires only when the paired closed H4 close clears 100.
///
/// H4 closes are `[50, 150, …]`; H4 bar 1 (close 150) *closes* at M15 index 31 —
/// so the earliest lawful signal is `M15[31].close_time`, filling at `M15[32].open`.
/// A no-look-ahead violation (reading H4[1] while it was still *forming*, i.e.
/// pairing it to the M15 bars whose `open_times` fall inside its 4h span) would
/// fire at M15[16]. Asserting the signal lands exactly at M15[31] proves the
/// closed-bar pairing.
#[test]
fn htf_operand_reads_only_the_closed_h4_bar() {
    // 48 M15 bars span H4 bars 0..2 fully and into H4[3]. Flat price so the only
    // trigger is the paired H4 close.
    let primary = series(Timeframe::M15, flat_m15(48));
    let htf = series(
        Timeframe::H4,
        vec![
            h4(0, 100, 101, 99, 50),  // closes 50 — below the 100 threshold
            h4(1, 100, 160, 90, 150), // closes 150 — above; still FORMING across M15 16..=30
            h4(2, 100, 160, 90, 150),
            h4(3, 100, 160, 90, 150),
        ],
    );
    let strategy = dsl(
        compare(
            htf_price(PriceField::Close),
            Comparator::Gt,
            constant(100, 0),
        ),
        vec![stop_loss()],
        Direction::Long,
    );
    let result = run(
        &compiled(&strategy),
        &primary,
        Some(&htf),
        SeriesEnd::SnapshotEnd,
    );

    assert_eq!(
        result.trades.len(),
        1,
        "exactly one entry, at the first bar paired with the closed H4[1]"
    );
    let trade = &result.trades[0];
    // M15[31].close_time == 31·900_000 + 899_999 = 28_799_999 == H4[1].close_time.
    assert_eq!(
        trade.entry_signal_time, primary.candles[31].close_time,
        "signal must fire on the first M15 bar paired with the CLOSED H4[1] (idx 31), \
         not the first whose open_time falls inside H4[1] (idx 16) — the forming bar"
    );
    assert_eq!(
        trade.entry_fill_time, primary.candles[32].open_time,
        "fill at the next bar's open"
    );
    assert_eq!(trade.direction, Direction::Long);
    assert_eq!(trade.exit_reason, ExitReason::EndOfData);
}

// ---------------------------------------------------------------------------
// (b) `previous()` on an `htf` operand is H4-bar-relative
// ---------------------------------------------------------------------------

/// `CrossesAbove(htf.close, 150)` is an H4-to-H4 event. H4 closes are
/// `[100, 100, 200, 200]`; the H4[2] bar (close 200) first pairs at M15[47],
/// where `previous()` reads `H4[1]=100` and `current()` reads `H4[2]=200` — the cross
/// fires. Because `previous` moves only on a *newly closed* H4 bar, the cross
/// stays true across the whole M15 window that H4[2] spans (47..=62): after the
/// first position stops out at M15[49], a second entry fires at M15[50].
///
/// A previous-that-moved-per-M15-bar would see `prev == cur == 200` for every
/// bar after the first and produce only ONE trade — so two trades at the exact
/// pinned times pin the H4-relative rule.
#[test]
fn previous_on_htf_is_relative_to_the_prior_h4_bar() {
    // Flat 200 through the entry region, one dip to 189 at M15[49] to stop the
    // first position (5% stop below a 200 entry → 190).
    let mut candles = flat_m15(64);
    for c in &mut candles {
        c.open = dec(200, 0);
        c.high = dec(201, 0);
        c.low = dec(199, 0);
        c.close = dec(200, 0);
    }
    candles[49].low = dec(189, 0); // intra-bar breach of the 190 stop
    let primary = series(Timeframe::M15, candles);
    let htf = series(
        Timeframe::H4,
        vec![
            h4(0, 100, 100, 100, 100),
            h4(1, 100, 100, 100, 100),
            h4(2, 200, 200, 200, 200), // the cross: 100 -> 200
            h4(3, 200, 200, 200, 200),
        ],
    );
    let strategy = dsl(
        Condition::CrossesAbove {
            lhs: htf_price(PriceField::Close),
            rhs: constant(150, 0),
        },
        vec![stop_loss()],
        Direction::Long,
    );
    let result = run(
        &compiled(&strategy),
        &primary,
        Some(&htf),
        SeriesEnd::SnapshotEnd,
    );

    assert_eq!(
        result.trades.len(),
        2,
        "the H4 cross stays 'just crossed' for H4[2]'s whole M15 window, so a \
         second entry follows the first's stop-out"
    );
    let first = &result.trades[0];
    assert_eq!(first.entry_signal_time, primary.candles[47].close_time);
    assert_eq!(first.entry_fill_time, primary.candles[48].open_time);
    assert_eq!(first.exit_reason, ExitReason::StopLoss);
    // The re-entry fires on the SAME bar as the stop-out — the close resolves
    // before the eval, so bar 49's cross (still "just crossed" inside H4[2]'s
    // window) signals again and fills at bar 50.
    let second = &result.trades[1];
    assert_eq!(second.entry_signal_time, primary.candles[49].close_time);
    assert_eq!(second.entry_fill_time, primary.candles[50].open_time);
}

// ---------------------------------------------------------------------------
// (c) ATR stop frozen at the signal bar, direction-relative
// ---------------------------------------------------------------------------

/// Every TR is `1.0` through the warmup so `Atr(14)` is `1.0` at the signal bar
/// (index 14). The **fill** bar (index 15) is given a `TR = 6` spike so its
/// ATR would update — but the recorded stop uses the *signal-bar* `1.0`, frozen.
/// Asserting `stop_price == entry − 2·1.0` (not `entry − 2·ATR(15)`) proves the
/// stop is pinned to the signal bar, not the fill bar.
#[test]
fn atr_stop_is_frozen_at_the_signal_bar_long() {
    // 21 bars: the stop-out lands on the LAST bar so the always-true entry's
    // re-signal at that bar never fills (no bar 21 exists) — one trade total.
    let mut candles = flat_m15(21);
    // TR = 6 spike at the FILL bar (still above the 98 stop, so no intra-bar
    // breach there) — enough to move ATR(15) off 1.0 and prove the freeze.
    candles[15].high = dec(105, 0);
    candles[15].low = dec(99, 0);
    // The last bar dips through the 98 stop so the trade closes by StopLoss.
    candles[20].low = dec(97, 0);
    let primary = series(Timeframe::M15, candles);
    let strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![atr_stop(14, 2, 0)],
        Direction::Long,
    );
    let result = run(&compiled(&strategy), &primary, None, SeriesEnd::SnapshotEnd);

    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    let entry = trade.entry_price; // = candles[15].open = 100 (zero slippage)
    assert_eq!(entry, dec(100, 0));
    // Frozen at the SIGNAL bar's ATR(14)=1.0: stop = 100 − 2·1.0 = 98. The
    // fill-bar ATR would be ≈1.36 (the TR spike) — 98 proves the freeze.
    assert_eq!(
        trade.stop_price,
        Some(atr_stop_price(entry, dec(1, 0), dec(2, 0), Direction::Long)),
        "stop must equal entry − 2 × ATR(signal-bar); the fill-bar ATR spike must not move it"
    );
    assert_eq!(trade.stop_price, Some(dec(98, 0)));
    assert_eq!(trade.exit_reason, ExitReason::StopLoss);
    assert_eq!(trade.exit_price, dec(98, 0));
    // realized_r = (exit − entry) / |entry − stop| = (98 − 100)/2 = −1.
    assert_eq!(trade.realized_r, dec(-1, 0));
}

/// The short mirror: stop sits *above* entry at `entry + 2·ATR(signal)`.
#[test]
fn atr_stop_is_frozen_at_the_signal_bar_short() {
    // Same last-bar trick: the stop-out bar is the series end. The fill-bar
    // spike goes DOWN not up — a short stops on the high, so high=101 stays
    // under the 102 stop while TR = max(6, 1, 5) = 6 still moves ATR(15).
    let mut candles = flat_m15(21);
    candles[15].high = dec(101, 0);
    candles[15].low = dec(95, 0);
    candles[20].high = dec(103, 0); // breaches the 102 stop
    let primary = series(Timeframe::M15, candles);
    let strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![atr_stop(14, 2, 0)],
        Direction::Short,
    );
    let result = run(&compiled(&strategy), &primary, None, SeriesEnd::SnapshotEnd);

    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    let entry = trade.entry_price;
    assert_eq!(entry, dec(100, 0));
    assert_eq!(
        trade.stop_price,
        Some(atr_stop_price(
            entry,
            dec(1, 0),
            dec(2, 0),
            Direction::Short
        )),
        "short stop = entry + 2 × ATR(signal-bar)"
    );
    assert_eq!(trade.stop_price, Some(dec(102, 0)));
    assert_eq!(trade.exit_reason, ExitReason::StopLoss);
    assert_eq!(trade.exit_price, dec(102, 0));
    assert_eq!(trade.realized_r, dec(-1, 0));
}

// ---------------------------------------------------------------------------
// (d) an unwarm ATR at signal time produces no entry
// ---------------------------------------------------------------------------

/// `AtrStop(5)` registers `Atr(5)` on the primary series; `is_warm` (and the
/// extra `Atr`-at-signal gate) holds every entry until `Atr(5)` first exists at
/// index 5. An always-true entry (`close > 0`) would otherwise fire at index 1 —
/// asserting the signal lands at index 5 proves the unwarm-ATR suppression.
#[test]
fn unwarm_atr_produces_no_entry() {
    let primary = series(Timeframe::M15, flat_m15(40));
    let strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![atr_stop(5, 2, 0)],
        Direction::Long,
    );
    let result = run(&compiled(&strategy), &primary, None, SeriesEnd::SnapshotEnd);

    assert_eq!(result.trades.len(), 1);
    // The earliest possible signal is the first index where ATR(5) is Some —
    // index 5 (TRs at bars 1..=5 seed it). No earlier entry is possible.
    assert_eq!(
        result.trades[0].entry_signal_time, primary.candles[5].close_time,
        "an unwarm ATR at signal time produces no entry — the first signal is at the first warm bar"
    );
    assert_eq!(
        result.trades[0].entry_fill_time,
        primary.candles[6].open_time
    );
}

// ---------------------------------------------------------------------------
// (e) needs_htf() with no H4 snapshot → HtfRequired, before any candle work
// ---------------------------------------------------------------------------

/// A strategy with an `htf` operand needs a higher-timeframe snapshot. Run it
/// through `run_version_backtest` with `htf_timeframe: None` and the application
/// ring must refuse with `BacktestAppError::HtfRequired` naming `inputs.htf` —
/// before the engine touches a candle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn htf_strategy_without_htf_snapshot_is_refused_with_htf_required() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("htf-need", None, &[])
        .await
        .expect("create strategy");
    // A 1.1.0 document whose entry reads the H4 close.
    let htf_dsl = r#"{
      "schema_version": "1.1.0",
      "name": "htf filter",
      "direction": "long",
      "entry": {
        "type": "Compare",
        "lhs": { "type": "Price", "series": "htf", "field": "Close" },
        "op": "Gt",
        "rhs": { "type": "Constant", "value": "0" }
      },
      "filters": [],
      "exits": [ { "type": "StopLoss", "distance_pct": "0.05" } ],
      "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
    }"#;
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: htf_dsl.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let store = CandleStore::with_base_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store"),
    );
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let err = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: version.id.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None, // the strategy needs it; the request omits it
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect_err("an htf operand with no H4 snapshot must refuse");

    match err {
        BacktestAppError::HtfRequired { field } => {
            assert_eq!(field, "inputs.htf");
        }
        other => panic!("expected BacktestAppError::HtfRequired, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// (h) a non-higher `inputs.htf` selection is refused before any candle I/O
// ---------------------------------------------------------------------------

/// Round-1 fixes F1+F6: `--tf M15 --htf M15` and `--tf H4 --htf M15` are
/// refused with `BacktestAppError::HtfNotHigher` naming `inputs.htf`, compared
/// by `Timeframe::duration_ms` — not an M15/H4 special case.
///
/// Both refused requests run against a pair with NO snapshot (`NOPEUSDT`), so
/// landing on `HtfNotHigher` rather than `SnapshotMissing`/`PreSaveRead`
/// proves the guard fires before any candle I/O — a missing/corrupt primary
/// snapshot can no longer mask the field error. The valid M15→H4 pair still
/// runs end-to-end over the fixture.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_higher_htf_selection_is_refused_at_the_request_boundary() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("htf-need", None, &[])
        .await
        .expect("create strategy");
    let htf_dsl = r#"{
      "schema_version": "1.1.0",
      "name": "htf filter",
      "direction": "long",
      "entry": {
        "type": "Compare",
        "lhs": { "type": "Price", "series": "htf", "field": "Close" },
        "op": "Gt",
        "rhs": { "type": "Constant", "value": "0" }
      },
      "filters": [],
      "exits": [ { "type": "StopLoss", "distance_pct": "0.05" } ],
      "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
    }"#;
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: htf_dsl.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let store = CandleStore::with_base_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store"),
    );
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());

    let request = |pair: &str, primary: Timeframe, htf: Option<Timeframe>| BacktestRequest {
        version_id: version.id.clone(),
        pair: Pair::new(pair),
        primary_timeframe: primary,
        htf_timeframe: htf,
        config: BacktestConfig::default(),
        snapshots: None,
        window: None,
    };

    // Equal timeframe: `M15 --htf M15`.
    let err = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &request("NOPEUSDT", Timeframe::M15, Some(Timeframe::M15)),
    )
    .await
    .expect_err("an equal-timeframe htf selection must refuse");
    match err {
        BacktestAppError::HtfNotHigher {
            field,
            primary,
            htf,
        } => {
            assert_eq!(field, "inputs.htf");
            assert_eq!((primary, htf), (Timeframe::M15, Timeframe::M15));
        }
        other => panic!("expected BacktestAppError::HtfNotHigher, got {other:?}"),
    }

    // Lower timeframe: `H4 --htf M15`.
    let err = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &request("NOPEUSDT", Timeframe::H4, Some(Timeframe::M15)),
    )
    .await
    .expect_err("a lower-timeframe htf selection must refuse");
    match err {
        BacktestAppError::HtfNotHigher {
            field,
            primary,
            htf,
        } => {
            assert_eq!(field, "inputs.htf");
            assert_eq!((primary, htf), (Timeframe::H4, Timeframe::M15));
        }
        other => panic!("expected BacktestAppError::HtfNotHigher, got {other:?}"),
    }

    // The valid M15→H4 pair still runs end-to-end.
    let outcome = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &request("BTCUSDT", Timeframe::M15, Some(Timeframe::H4)),
    )
    .await
    .expect("a strictly-higher htf selection runs");
    assert!(
        !outcome.trades.is_empty(),
        "the valid pair must still produce a real run, not just a non-error"
    );
}

/// Engine-level defence for the same rule: callers that skip the request ring
/// (the coach accept path replays persisted inputs through `prepare_backtest`)
/// get the typed [`BacktestError::HtfNotHigher`] from `run_backtest` itself.
#[test]
fn engine_refuses_a_non_higher_htf_series() {
    let strategy = dsl(
        compare(htf_price(PriceField::Close), Comparator::Gt, constant(0, 0)),
        vec![stop_loss()],
        Direction::Long,
    );
    let compiled = compiled(&strategy);

    // Equal timeframe: an "htf" series built from M15 bars.
    let primary = series(Timeframe::M15, flat_m15(40));
    let equal = series(Timeframe::M15, flat_m15(40));
    let err = run_backtest(
        &compiled,
        &primary,
        Some(&equal),
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
    )
    .expect_err("an equal-timeframe htf series must refuse");
    assert!(
        matches!(
            err,
            BacktestError::HtfNotHigher {
                primary: Timeframe::M15,
                htf: Timeframe::M15
            }
        ),
        "expected HtfNotHigher(M15, M15), got {err:?}"
    );

    // Lower timeframe: M15 bars supplied as the "higher" series for an H4
    // primary.
    let h4_primary = series(
        Timeframe::H4,
        vec![
            h4(0, 100, 101, 99, 100),
            h4(1, 100, 101, 99, 100),
            h4(2, 100, 101, 99, 100),
        ],
    );
    let lower = series(Timeframe::M15, flat_m15(48));
    let err = run_backtest(
        &compiled,
        &h4_primary,
        Some(&lower),
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
    )
    .expect_err("a lower-timeframe htf series must refuse");
    assert!(
        matches!(
            err,
            BacktestError::HtfNotHigher {
                primary: Timeframe::H4,
                htf: Timeframe::M15
            }
        ),
        "expected HtfNotHigher(H4, M15), got {err:?}"
    );
}

/// r2.s2 round-2 fix G1: a supplied HTF series for a DIFFERENT pair is refused
/// with the typed [`BacktestError::HtfPairMismatch`] before any alignment —
/// `CandleSeries::pair` is public and `run_backtest` takes the two series
/// independently, so without the check a direct caller produces mixed-symbol
/// signals with nothing red. The request path loads both series by the one
/// request pair, so this engine check is the whole seam; a matching pair
/// still runs.
#[test]
fn engine_refuses_a_mismatched_pair_htf_series() {
    let strategy = dsl(
        compare(htf_price(PriceField::Close), Comparator::Gt, constant(0, 0)),
        vec![stop_loss()],
        Direction::Long,
    );
    let compiled = compiled(&strategy);
    let primary = series(Timeframe::M15, flat_m15(40));

    // Same timeframe ordering (H4 > M15) but a different symbol: the pair
    // check must fire, not the cadence check.
    let foreign = CandleSeries {
        pair: Pair::new("ETHUSDT"),
        ..series(
            Timeframe::H4,
            vec![
                h4(0, 100, 101, 99, 100),
                h4(1, 100, 101, 99, 100),
                h4(2, 100, 101, 99, 100),
            ],
        )
    };
    let err = run_backtest(
        &compiled,
        &primary,
        Some(&foreign),
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
    )
    .expect_err("a different-pair htf series must refuse");
    match err {
        BacktestError::HtfPairMismatch { primary, htf } => {
            assert_eq!(primary, Pair::new("BTCUSDT"));
            assert_eq!(htf, Pair::new("ETHUSDT"));
        }
        other => panic!("expected HtfPairMismatch, got {other:?}"),
    }

    // The same H4 candles under the matching pair still run to completion.
    let matching = series(
        Timeframe::H4,
        vec![
            h4(0, 100, 101, 99, 100),
            h4(1, 100, 101, 99, 100),
            h4(2, 100, 101, 99, 100),
        ],
    );
    run_backtest(
        &compiled,
        &primary,
        Some(&matching),
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
    )
    .expect("a same-pair htf series still runs");
}

// ---------------------------------------------------------------------------
// (f) two cold runs of an HTF+ATR strategy persist identical hashes
// ---------------------------------------------------------------------------

/// The repeat-twice oracle: run the same HTF+ATR strategy over the committed
/// fixture twice on independent engines, persist both, and the reloaded
/// `result_content_hash` must be identical. This is the end-to-end determinism
/// guard for the whole new path (dual-series eval + frozen ATR stop + recorded
/// stop column + conditional hash feed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_cold_htf_atr_runs_persist_identical_hashes() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("htf-atr", None, &[])
        .await
        .expect("create strategy");
    // Entry on the H4 close crossing the primary close, an ATR stop — every new
    // surface in one strategy.
    let htf_atr_dsl = r#"{
      "schema_version": "1.1.0",
      "name": "htf + atr",
      "direction": "long",
      "entry": {
        "type": "Compare",
        "lhs": { "type": "Price", "series": "htf", "field": "Close" },
        "op": "Gt",
        "rhs": { "type": "Constant", "value": "0" }
      },
      "filters": [],
      "exits": [
        { "type": "AtrStop", "period": 14, "multiple": "2.0" },
        { "type": "TakeProfit", "target_r": "2.0" }
      ],
      "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
    }"#;
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: htf_atr_dsl.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let store = CandleStore::with_base_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store"),
    );
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());

    let request = BacktestRequest {
        version_id: version.id.clone(),
        pair: Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: Some(Timeframe::H4),
        config: BacktestConfig::default(),
        snapshots: None,
        window: None,
    };

    // Two cold end-to-end runs (fresh engine + fresh store handle each time).
    let run_one =
        run_version_backtest(&strategies, &store, &BinanceAdapter::new(), &runs, &request)
            .await
            .expect("run one");
    let store_two = CandleStore::with_base_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store"),
    );
    let run_two = run_version_backtest(
        &strategies,
        &store_two,
        &BinanceAdapter::new(),
        &runs,
        &request,
    )
    .await
    .expect("run two");

    assert_eq!(
        run_one.run.result_content_hash, run_two.run.result_content_hash,
        "two cold runs must persist identical result_content_hash"
    );
    assert!(
        !run_one.trades.is_empty(),
        "the HTF+ATR strategy must produce trades for the hash to be meaningful"
    );
    assert!(
        run_one.trades.iter().all(|t| t.stop_price.is_some()),
        "every trade must record its stop"
    );
}

// ---------------------------------------------------------------------------
// (g) every trade's stop_price equals the geometric stop; open-position mark ok
// ---------------------------------------------------------------------------

/// `stop_price` is recorded for BOTH stop families: a pct-stop run's trades all
/// carry `Some(stop_price(entry, pct, direction))`, and an ATR-stop run's carry
/// `Some(atr_stop_price(entry, atr_at_signal, multiple, direction))` — the ATR
/// recomputed by an independent oracle over the primary series. A windowed run
/// that ends mid-position still carries its `OpenPositionMark` (unchanged by the
/// new column).
#[test]
fn every_trade_records_the_geometric_stop() {
    // ATR-stop run over the flat-TR series (every ATR = 1.0 after warmup).
    let mut candles = flat_m15(60);
    // give the second trade's exit something to hit
    candles[45].low = dec(97, 0);
    let primary = series(Timeframe::M15, candles);
    let atr_strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![atr_stop(14, 2, 0)],
        Direction::Long,
    );
    let result = run(
        &compiled(&atr_strategy),
        &primary,
        None,
        SeriesEnd::SnapshotEnd,
    );
    assert!(!result.trades.is_empty());
    for (i, trade) in result.trades.iter().enumerate() {
        // The signal bar is the candle whose close_time equals entry_signal_time;
        // its ATR(14) is recomputed by an independent Wilder oracle below.
        let atr_at_signal = oracle_atr(&primary, 14, trade.entry_signal_time);
        let expected = atr_stop_price(trade.entry_price, atr_at_signal, dec(2, 0), Direction::Long);
        assert_eq!(
            trade.stop_price,
            Some(expected),
            "trade {i}: stop_price must equal entry − 2 × ATR(signal bar)"
        );
    }

    // A pct-stop run records the same field through the other family.
    let pct_strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![ExitRule::StopLoss {
            distance_pct: fixed_dec(5, 2),
        }],
        Direction::Long,
    );
    let pct_result = run(
        &compiled(&pct_strategy),
        &primary,
        None,
        SeriesEnd::SnapshotEnd,
    );
    for (i, trade) in pct_result.trades.iter().enumerate() {
        assert_eq!(
            trade.stop_price,
            Some(stop_price(trade.entry_price, dec(5, 2), Direction::Long)),
            "trade {i}: pct stop must record entry·(1−pct)"
        );
    }
}

/// A windowed run that ends mid-position keeps its `OpenPositionMark` — the new
/// `stop_price` column must not disturb the r2.s1 mark semantics.
#[test]
fn windowed_run_open_position_mark_is_unaffected_by_the_stop_column() {
    // 60 flat-TR bars; a window that ends before the last bar makes the last
    // bar a WindowEdge — the still-open ATR position is left as a mark, not a
    // closed trade.
    let primary = series(Timeframe::M15, flat_m15(60));
    let strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![atr_stop(14, 2, 0)],
        Direction::Long,
    );
    let compiled = compiled(&strategy);
    // Window the primary series to [0, candle[40].open_time) so the engine sees
    // a WindowEdge and leaves the still-open position as a mark.
    let to_ms = primary.candles[40].open_time;
    let windowed = primary.windowed(&pulse::CandleWindow::new(0, to_ms).expect("window"));
    let result = run(&compiled, &windowed, None, SeriesEnd::WindowEdge);

    assert!(
        result.open_position.is_some(),
        "a windowed run ending mid-position must carry an OpenPositionMark"
    );
    let mark = result.open_position.expect("mark present");
    assert_eq!(mark.direction, Direction::Long);
    assert_eq!(mark.entry_price, dec(100, 0));
}

// ---------------------------------------------------------------------------
// (i) the HTF engine steps EVERY closed H4 candle exactly once (F2)
// ---------------------------------------------------------------------------

/// The six-H4 fixture shared by both F2 pins: `h4[0..=3]` all close before the
/// shifted primary series' first bar (`close_time <= primary[0].close_time`),
/// `h4[4]` pairs at primary index 15 and `h4[5]` at index 31.
fn htf_lead_in_series() -> CandleSeries {
    series(
        Timeframe::H4,
        vec![
            h4(0, 100, 105, 95, 100),
            h4(1, 100, 115, 95, 110),
            h4(2, 100, 125, 95, 120),
            h4(3, 100, 135, 95, 130),
            h4(4, 100, 145, 95, 140),
            h4(5, 100, 155, 95, 150),
        ],
    )
}

/// Independent seeded-EMA oracle — `ema[0] = close[0]`, then
/// `ema[t] = k·close[t] + (1−k)·ema[t−1]` with `k = 2/(period+1)`, matching the
/// adapter's ta-rs recursion (NOT the engine under test).
fn oracle_ema(closes: &[f64], period: u32) -> Decimal {
    use pulse::f64_to_decimal_rounded;
    let k = 2.0 / (f64::from(period) + 1.0);
    let mut ema = closes[0];
    for &close in &closes[1..] {
        ema = k * close + (1.0 - k) * ema;
    }
    f64_to_decimal_rounded(ema).expect("oracle ema to Decimal")
}

/// An H4 snapshot normally carries lead-in history: bars already closed when
/// the run's first primary bar lands. `align` pairs that first bar with the
/// LAST closed H4 — so an engine stepped only on the pairing would see a
/// single H4 candle where four had closed, and an H4 indicator would still be
/// in warmup when the data says it should be warm. This entry is true exactly
/// when the H4 EMA(3) equals the value full-history stepping produces —
/// `121.25` after `h4[0..=3]` — so the trade exists only if the lead-in bars
/// were actually fed.
#[test]
fn htf_lead_in_history_warms_the_indicator_at_the_first_paired_bar() {
    // 40 M15 bars starting at absolute M15 index 64 (open_time = 4 × 4h): H4
    // bars 0..=3 closed before primary[0], h4[4] pairs at index 15, h4[5] at 31.
    let primary = series(Timeframe::M15, flat_m15_from(64, 40));
    let htf = htf_lead_in_series();
    let expected = oracle_ema(&[100.0, 110.0, 120.0, 130.0], 3);
    assert_eq!(expected, dec(12125, 2), "hand-check: EMA(3) = 121.25");
    let strategy = dsl(
        compare(
            htf_ema(3),
            Comparator::Eq,
            ValueSource::Constant { value: expected },
        ),
        vec![stop_loss()],
        Direction::Long,
    );
    let result = run(
        &compiled(&strategy),
        &primary,
        Some(&htf),
        SeriesEnd::SnapshotEnd,
    );

    assert_eq!(
        result.trades.len(),
        1,
        "an H4 EMA warm at the first paired bar fires one entry"
    );
    // The earliest lawful signal bar is index 1 (`bar.index > 0`).
    assert_eq!(
        result.trades[0].entry_signal_time,
        primary.candles[1].close_time
    );
    assert_eq!(
        result.trades[0].entry_fill_time,
        primary.candles[2].open_time
    );
}

/// The no-double-step half of F2: the H4 EMA(3) after `h4[5]` closes is
/// `140.3125` ONLY when every one of `h4[0..=5]` was fed exactly once — a
/// re-stepped or skipped candle anywhere in the prefix perturbs the recursive
/// state and the equality never fires. `h4[5]` first pairs at primary[31].
#[test]
fn htf_engine_never_steps_a_closed_candle_twice() {
    let primary = series(Timeframe::M15, flat_m15_from(64, 40));
    let htf = htf_lead_in_series();
    let expected = oracle_ema(&[100.0, 110.0, 120.0, 130.0, 140.0, 150.0], 3);
    assert_eq!(expected, dec(1_403_125, 4), "hand-check: EMA(3) = 140.3125");
    let strategy = dsl(
        compare(
            htf_ema(3),
            Comparator::Eq,
            ValueSource::Constant { value: expected },
        ),
        vec![stop_loss()],
        Direction::Long,
    );
    let result = run(
        &compiled(&strategy),
        &primary,
        Some(&htf),
        SeriesEnd::SnapshotEnd,
    );

    assert_eq!(
        result.trades.len(),
        1,
        "the EMA equals 140.3125 only while h4[5] is the paired bar"
    );
    assert_eq!(
        result.trades[0].entry_signal_time, primary.candles[31].close_time,
        "h4[5] (close_time 86_399_999) first pairs at the M15 bar closing then"
    );
    assert_eq!(
        result.trades[0].entry_fill_time,
        primary.candles[32].open_time
    );
}

// ---------------------------------------------------------------------------
// (j) entries and signal exits wait for a paired closed H4 bar (F3)
// ---------------------------------------------------------------------------

/// `Not(htf.close < 100)` — true whenever an H4 bar IS paired (the fixture's
/// closes are 150), but ALSO `true` before any H4 bar exists: the absent `Htf`
/// leaf evaluates to `false` and `Not` flips it. Only the paired-bar gate
/// keeps this from firing in the void.
fn not_over_absent_htf_price() -> Condition {
    Condition::Not {
        condition: Box::new(compare(
            htf_price(PriceField::Close),
            Comparator::Lt,
            constant(100, 0),
        )),
    }
}

/// The two-H4 fixture for F3: `h4[0]` (close 150) closes at `14_399_999` and
/// first pairs at primary[15]; `h4[1]` closes inside the run.
fn two_h4_closing_150() -> CandleSeries {
    series(
        Timeframe::H4,
        vec![h4(0, 100, 160, 90, 150), h4(1, 100, 160, 90, 150)],
    )
}

/// Entry `Not(htf.close < 100)` on a Price-leaf-only HTF strategy: the HTF
/// engine registers NO indicators, so `is_warm` is vacuous and the bug fired
/// the entry at bar 1 — before the first H4 close pairs at bar 15. With the
/// gate the signal must land exactly on `primary[15].close_time`.
#[test]
fn not_over_htf_price_entry_waits_for_the_first_closed_h4_bar() {
    let primary = series(Timeframe::M15, flat_m15(40));
    let htf = two_h4_closing_150();
    let strategy = dsl(
        not_over_absent_htf_price(),
        vec![stop_loss()],
        Direction::Long,
    );
    let result = run(
        &compiled(&strategy),
        &primary,
        Some(&htf),
        SeriesEnd::SnapshotEnd,
    );

    assert_eq!(
        result.trades.len(),
        1,
        "one entry, gated until the first pair"
    );
    let trade = &result.trades[0];
    assert_eq!(
        trade.entry_signal_time, primary.candles[15].close_time,
        "the entry must wait for the bar paired with the closed h4[0] — \
         firing before 15 means `Not` over absent HTF data read true"
    );
    assert_eq!(trade.entry_fill_time, primary.candles[16].open_time);
}

/// The signal-exit mirror: the strategy `needs_htf()` through the EXIT's `htf`
/// operand, so even the primary-only `close > 0` entry waits for the first
/// paired bar (signals at 15, fills at 16), and the always-true
/// `SignalExit(Not(htf.close < 100))` then fires at 16 and fills at 17.
/// Ungated, the exit would signal at bar 2 (fill at bar 3) and the still-true
/// conditions would spawn a re-entry train — the trade count alone catches it.
/// The series ends at bar 18 so the re-signal at 17 dies unfilled.
#[test]
fn not_over_htf_price_signal_exit_waits_for_the_first_closed_h4_bar() {
    let primary = series(Timeframe::M15, flat_m15(18));
    let htf = two_h4_closing_150();
    let strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![
            stop_loss(),
            ExitRule::SignalExit {
                condition: not_over_absent_htf_price(),
            },
        ],
        Direction::Long,
    );
    let result = run(
        &compiled(&strategy),
        &primary,
        Some(&htf),
        SeriesEnd::SnapshotEnd,
    );

    assert_eq!(result.trades.len(), 1);
    let trade = &result.trades[0];
    // The gated entry fires at the first paired bar (15) and fills at 16.
    assert_eq!(trade.entry_signal_time, primary.candles[15].close_time);
    assert_eq!(trade.entry_fill_time, primary.candles[16].open_time);
    assert_eq!(trade.exit_reason, ExitReason::Signal);
    assert_eq!(
        trade.exit_signal_time, primary.candles[16].close_time,
        "the signal exit evaluates from the first paired bar onward — \
         firing before the pair exists means `Not` over absent HTF data read true"
    );
    assert_eq!(trade.exit_fill_time, primary.candles[17].open_time);
}

// ---------------------------------------------------------------------------
// (k) a non-positive ATR-derived stop is a typed refusal (F4)
// ---------------------------------------------------------------------------

/// `multiple × ATR >= entry` on a long resolves to `entry − m·ATR <= 0` — a
/// stop level the market can never reach. Before this fix a zero stop fell out
/// as the generic `NoStopLoss` and a negative one was sized on its absolute
/// distance while never being fillable; the fill now refuses with
/// `BacktestError::ImpossibleStop`, mirroring the short-TP leg.
#[test]
fn atr_stop_resolving_non_positive_is_a_typed_refusal() {
    // Prices at ~1.0 with TR exactly 1.0 on every bar after the first
    // (high−low = 1.0, close = open = 1.0): ATR(5) reads 1.0 once warm, entry
    // fills at the open 1.0, and multiple 2.0 ⇒ stop = 1 − 2·1.0 = −1.
    let candles: Vec<Candle> = (0..10)
        .map(|i| {
            let open_time = i * Timeframe::M15.duration_ms();
            Candle {
                open_time,
                close_time: open_time + Timeframe::M15.duration_ms() - 1,
                open: dec(1, 0),
                high: dec(15, 1), // 1.5
                low: dec(5, 1),   // 0.5
                close: dec(1, 0),
                volume: dec(1, 0),
                funding_rate: None,
            }
        })
        .collect();
    let primary = series(Timeframe::M15, candles);
    let strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![atr_stop(5, 2, 0)],
        Direction::Long,
    );
    let err = run_backtest(
        &compiled(&strategy),
        &primary,
        None,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
    )
    .expect_err("a non-positive ATR stop must refuse");
    assert!(
        matches!(err, BacktestError::ImpossibleStop(_)),
        "expected ImpossibleStop, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// (m) a ZERO-distance ATR stop is the same typed refusal (round-3 fix)
// ---------------------------------------------------------------------------

/// The boundary F4's arm-local `stop <= 0` check missed: a flat series where
/// every bar has `high == low == prev_close` drives ATR to exactly `0`, so
/// `atr_stop_price` resolves `stop == entry` — a positive price that passed
/// the old check — and the sizer's zero-distance branch then failed the run
/// as the generic `NoStopLoss` ("the strategy has no stop loss") for a
/// strategy that DID declare an `AtrStop`. The hoisted guard in
/// `fill_pending_entry` refuses it as `ImpossibleStop`.
#[test]
fn atr_stop_resolving_to_zero_distance_is_a_typed_refusal() {
    // Every bar identical (`open = high = low = close = 100`): TR = 0 on every
    // bar, so ATR(5) reads exactly 0 once warm, and the stop resolves to
    // `entry − 2·0 = entry` — zero distance, positive price.
    let candles: Vec<Candle> = (0..10)
        .map(|i| {
            let open_time = i * Timeframe::M15.duration_ms();
            Candle {
                open_time,
                close_time: open_time + Timeframe::M15.duration_ms() - 1,
                open: dec(100, 0),
                high: dec(100, 0),
                low: dec(100, 0),
                close: dec(100, 0),
                volume: dec(1, 0),
                funding_rate: None,
            }
        })
        .collect();
    let primary = series(Timeframe::M15, candles);
    let strategy = dsl(
        compare(
            primary_price(PriceField::Close),
            Comparator::Gt,
            constant(0, 0),
        ),
        vec![atr_stop(5, 2, 0)],
        Direction::Long,
    );
    let err = run_backtest(
        &compiled(&strategy),
        &primary,
        None,
        &zero_slippage(),
        &SymbolFilters::unconstrained(),
        SeriesEnd::SnapshotEnd,
    )
    .expect_err("a zero-distance ATR stop must refuse");
    assert!(
        matches!(err, BacktestError::ImpossibleStop(_)),
        "expected ImpossibleStop — not the generic NoStopLoss — got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// (n) a stale HTF series is a typed refusal (r2.s2 round-5)
// ---------------------------------------------------------------------------

/// The htf-operand DSL both stale-coverage proofs run (schema 1.1.0 JSON —
/// `htf.close > 0` is the `Series::Htf` leaf that forces `needs_htf()`).
const HTF_OPERAND_DSL: &str = r#"{
  "schema_version": "1.1.0",
  "name": "htf filter",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "series": "htf", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "0" }
  },
  "filters": [],
  "exits": [ { "type": "StopLoss", "distance_pct": "0.05" } ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// Seed `store_dir` with a LONG M15 series and an H4 series that ends well
/// more than one H4 interval before it — 400 M15 bars span 25 H4 intervals,
/// so 10 H4 bars leave coverage ~15 intervals short.
fn seed_stale_htf_store(store_dir: PathBuf) {
    let store = CandleStore::with_base_dir(store_dir);
    let pair = Pair::new("BTCUSDT");
    store
        .commit(&pair, Timeframe::M15, flat_m15(400))
        .expect("commit m15 snapshot");
    store
        .commit(
            &pair,
            Timeframe::H4,
            (0..10).map(|j| h4(j, 100, 101, 99, 100)).collect(),
        )
        .expect("commit short h4 snapshot");
}

/// The versioned/application path: `run_version_backtest` loads both series
/// independently (no coverage invariant between snapshots), so the engine
/// seam is what refuses — `BacktestError::HtfCoverageShort` naming both ends
/// and the HTF interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn htf_coverage_ending_early_is_a_typed_refusal_on_the_versioned_path() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("htf-stale", None, &[])
        .await
        .expect("create strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: HTF_OPERAND_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let store_dir = tmp.path().join("store");
    seed_stale_htf_store(store_dir.clone());
    let store = CandleStore::with_base_dir(store_dir);
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let err = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: version.id.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect_err("an HTF series ending more than one interval early must refuse");

    match err {
        BacktestAppError::Engine(BacktestError::HtfCoverageShort {
            primary_end,
            htf_end,
            htf,
        }) => {
            assert_eq!(htf, Timeframe::H4);
            assert_eq!(primary_end, 400 * Timeframe::M15.duration_ms() - 1);
            assert_eq!(htf_end, 10 * Timeframe::H4.duration_ms() - 1);
        }
        other => panic!("expected Engine(HtfCoverageShort), got {other:?}"),
    }
}

/// The direct `pulse backtest --dsl` CLI path: `src/cli/backtest.rs` loads
/// both series then calls `run_backtest`, so the same engine seam refuses —
/// the run exits non-zero and the typed error's text reaches stderr.
#[test]
fn cli_backtest_refuses_a_stale_htf_series() {
    let tmp = TempDir::new().expect("tempdir");
    let store_dir = tmp.path().join("store");
    seed_stale_htf_store(store_dir.clone());
    let dsl_path = tmp.path().join("strategy.json");
    std::fs::write(&dsl_path, HTF_OPERAND_DSL).expect("write htf DSL");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            dsl_path.to_str().expect("dsl path is utf8"),
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--htf",
            "H4",
            "--store",
            store_dir.to_str().expect("store path is utf8"),
        ])
        .output()
        .expect("run pulse backtest");

    assert!(
        !output.status.success(),
        "a stale H4 series must exit non-zero; stdout was:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("higher-timeframe coverage ends") && stderr.contains("stale final bar"),
        "stderr must carry the HtfCoverageShort refusal; stderr was:\n{stderr}"
    );
}

/// One interval of slack is allowed — at most one not-yet-closed HTF bar may
/// be pending, the normal live shape. 400 M15 bars span exactly 25 H4
/// intervals, so a 24-bar H4 series ends exactly one interval before the
/// primary's end and still runs.
#[test]
fn htf_coverage_within_one_interval_still_runs() {
    let primary = series(Timeframe::M15, flat_m15(400));
    let htf = series(
        Timeframe::H4,
        (0..24).map(|j| h4(j, 100, 101, 99, 100)).collect(),
    );
    let strategy = dsl(
        compare(htf_price(PriceField::Close), Comparator::Gt, constant(0, 0)),
        vec![stop_loss()],
        Direction::Long,
    );
    let result = run(
        &compiled(&strategy),
        &primary,
        Some(&htf),
        SeriesEnd::SnapshotEnd,
    );
    assert_eq!(
        result.trades.len(),
        1,
        "the run must complete — htf.close > 0 fires once a closed H4 bar pairs"
    );
}

/// An empty HTF series is legal (r2.s1.w3): `align` yields `htf: None` for
/// every bar and the paired-bar gate closes entries outright — no refusal,
/// no trades.
#[test]
fn empty_htf_series_is_legal_and_runs() {
    let primary = series(Timeframe::M15, flat_m15(64));
    let htf = series(Timeframe::H4, vec![]);
    let strategy = dsl(
        compare(htf_price(PriceField::Close), Comparator::Gt, constant(0, 0)),
        vec![stop_loss()],
        Direction::Long,
    );
    let result = run(
        &compiled(&strategy),
        &primary,
        Some(&htf),
        SeriesEnd::SnapshotEnd,
    );
    assert!(
        result.trades.is_empty(),
        "every aligned bar has `htf: None`, so no entry may fire"
    );
}

/// A primary-only DSL — no `series` field anywhere (it defaults to
/// `"primary"`), so `needs_htf()` is false.
const PRIMARY_ONLY_DSL: &str = r#"{
  "schema_version": "1.1.0",
  "name": "primary only",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Price", "field": "Close" },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "0" }
  },
  "filters": [],
  "exits": [ { "type": "StopLoss", "distance_pct": "0.05" } ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

/// r2.s2 round-6: a PRIMARY-ONLY strategy handed a stale, unused H4 series
/// still runs — the coverage check is gated on `needs_htf`, so the lagging
/// H4 HEAD the default resolver supplies cannot refuse it (the regression the
/// ungated round-5 arm introduced). `htf_timeframe: Some(H4)` is what
/// `resolve_default_request`'s no-prior-run arm mints unconditionally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn primary_only_strategy_with_a_stale_unused_htf_series_still_runs() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("primary-only", None, &[])
        .await
        .expect("create strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: PRIMARY_ONLY_DSL.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let store_dir = tmp.path().join("store");
    seed_stale_htf_store(store_dir.clone());
    let store = CandleStore::with_base_dir(store_dir);
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let outcome = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: version.id.clone(),
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect("a primary-only strategy must run over a stale, unused H4 series");
    assert!(
        !outcome.trades.is_empty(),
        "the run must complete — close > 0 on flat 100 prices fires an entry"
    );
}

// ---------------------------------------------------------------------------
// Independent ATR oracle (Wilder) — NOT the adapter under test
// ---------------------------------------------------------------------------

/// Recompute `Atr(period)` for the primary candle whose `close_time` equals
/// `signal_time`, using a hand-rolled Wilder smoothing independent of the
/// `Atr` adapter — so `(g)` cross-checks the engine rather than mirroring it.
fn oracle_atr(primary: &CandleSeries, period: usize, signal_time: i64) -> Decimal {
    use pulse::{decimal_to_f64, f64_to_decimal_rounded};
    // True range per candle: max(high−low, |high−prev_close|, |low−prev_close|);
    // the first candle has no previous close so its TR is just high−low.
    let trs: Vec<f64> = {
        let mut out = Vec::with_capacity(primary.candles.len());
        let mut prev_close: Option<f64> = None;
        for c in &primary.candles {
            let high = decimal_to_f64(c.high).expect("high to f64");
            let low = decimal_to_f64(c.low).expect("low to f64");
            let tr = match prev_close {
                Some(pc) => (high - low).max((high - pc).abs()).max((low - pc).abs()),
                None => high - low,
            };
            out.push(tr);
            prev_close = Some(decimal_to_f64(c.close).expect("close to f64"));
        }
        out
    };
    // The signal bar index.
    let idx = primary
        .candles
        .iter()
        .position(|c| c.close_time == signal_time)
        .expect("signal time resolves to a candle");
    // Wilder seed: SMA of the first `period` TRs at index `period`; then smooth
    // `atr = (atr·(period−1) + tr)/period` to `idx`.
    let period_f = f64::from(u32::try_from(period).expect("period fits u32"));
    let mut atr = trs[1..=period].iter().sum::<f64>() / period_f;
    for tr in &trs[period + 1..=idx] {
        atr = (atr * (period_f - 1.0) + tr) / period_f;
    }
    f64_to_decimal_rounded(atr).expect("oracle atr to Decimal")
}
