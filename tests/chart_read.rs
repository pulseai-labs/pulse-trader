//! r5.s2.w1 (PDR-0036, d104) — the chart read contract.
//!
//! The window and the default window, the 20,000-bar cap, the holdout cut and
//! its count, the run layer (its stored trades, and one overlay per S/R, pivot,
//! candle-pattern and EMA operand of its version's rule), the refusals, and the
//! app-scope route over the spawned server.
//!
//! **Every overlay is checked against an INDEPENDENT engine pass** over the same
//! cut series for the same spec (SPINE G2): this suite loads the fixture, applies
//! the holdout cut itself, steps its own `IndicatorEngine` and derives its own
//! events, so the read's own answer is never the thing it is compared against.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::path::Path;

use pulse::{
    BacktestInputs, BacktestResult, BacktestRunId, BacktestRunRepository, BusError, BusErrorCode,
    Candle, CandlePatternName, CandleSeries, CandleSeriesRepository, CandleStore, CandleWindow,
    ChartDataDto, ChartDataRequest, ChartOverlayDto, CompiledValue, DataVersion, DesktopState,
    Direction, EngineFingerprint, EquityCurve, EvalContext, ExitReason, FakeClock, Fill,
    FundingConfig, GetBacktestRunRequest, IndicatorEngine, IndicatorSpec, NewVersion,
    OpenFreezeRequest, Pair, Regime, RegimeBreakdown, Series, SkippedEntryCounts,
    SnapshotSelection, SqliteBacktestRunRepo, SqliteCertificationFreezeRepo, StrategyId,
    StrategyRepository, SummaryStats, SweepableValue, Timeframe, Trade, TradeSource, VersionId,
    chart_data_core, get_backtest_run_core,
};
use rust_decimal::Decimal;
use support::server::{ServerOptions, TestServer, spawn_server};

/// The holdout start every freeze case uses — inside the fixture's January 2025
/// range (`tests/holdout_guard.rs`'s own constant).
const HOLDOUT_MS: i64 = 1_736_985_600_000;

/// The instant the test freeze opens at (deterministic, injected).
const OPENED_MS: i64 = 1_760_000_000_000;

/// The fixture's pair.
fn pair() -> Pair {
    Pair::new("BTCUSDT")
}

/// A store over the server's own (copied) fixture data dir.
fn store(dir: &Path) -> CandleStore {
    CandleStore::with_base_dir(dir.to_path_buf())
}

/// The version the fixture's HEAD names for one timeframe.
fn head(store: &CandleStore, timeframe: Timeframe) -> DataVersion {
    store
        .read_head(&pair(), timeframe)
        .expect("the fixture HEAD reads")
        .expect("the fixture has a HEAD")
}

/// The run's version's rule: S/R, pivot and candle-pattern operands plus an EMA
/// line **on both series**, and one RSI the tab does not draw (Q4).
///
/// The primary series carries `support`/`pivot_high`/`cdl`/`ema` + `rsi`; the HTF
/// carries `support`/`pivot_low`/`cdl`/`ema`.
const CHART_DSL: &str = r#"{
  "schema_version": "1.4.0",
  "name": "r5.s2.w1 chart read",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "spec": {
        "indicator": "Support", "left": 2, "right": 2,
        "tolerance_pct": "0.001", "min_touches": 2, "lookback": 100
      }
    },
    "op": "Lt",
    "rhs": { "type": "Price", "field": "Close" }
  },
  "filters": [
    {
      "type": "Compare",
      "lhs": { "type": "Indicator", "spec": { "indicator": "PivotHigh", "left": 1, "right": 1 } },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "0" }
    },
    {
      "type": "Compare",
      "lhs": { "type": "Indicator", "spec": { "indicator": "CandlePattern", "pattern": "Engulfing" } },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "0" }
    },
    {
      "type": "Compare",
      "lhs": { "type": "Indicator", "spec": { "indicator": "Ema", "period": 50 } },
      "op": "Gt",
      "rhs": { "type": "Price", "field": "Close" }
    },
    {
      "type": "Compare",
      "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "50" }
    },
    {
      "type": "Compare",
      "lhs": {
        "type": "Indicator",
        "series": "htf",
        "spec": {
          "indicator": "Support", "left": 3, "right": 3,
          "tolerance_pct": "0.005", "min_touches": 2, "lookback": 50
        }
      },
      "op": "Lt",
      "rhs": { "type": "Price", "series": "htf", "field": "Close" }
    },
    {
      "type": "Compare",
      "lhs": { "type": "Indicator", "series": "htf", "spec": { "indicator": "PivotLow", "left": 2, "right": 2 } },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "0" }
    },
    {
      "type": "Compare",
      "lhs": { "type": "Indicator", "series": "htf", "spec": { "indicator": "CandlePattern", "pattern": "Hammer" } },
      "op": "Gt",
      "rhs": { "type": "Constant", "value": "0" }
    },
    {
      "type": "Compare",
      "lhs": { "type": "Indicator", "series": "htf", "spec": { "indicator": "Ema", "period": 20 } },
      "op": "Lt",
      "rhs": { "type": "Price", "series": "htf", "field": "Close" }
    }
  ],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.02" },
    { "type": "TakeProfit", "target_r": "1" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

fn fixed(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
}

fn dec(text: &str) -> Decimal {
    text.parse().expect("an exact decimal")
}

/// The primary series' rule specs, in the DSL's reference order.
fn primary_specs() -> Vec<IndicatorSpec> {
    vec![
        IndicatorSpec::Support {
            left: fixed(2),
            right: fixed(2),
            tolerance_pct: SweepableValue::Fixed(dec("0.001")),
            min_touches: fixed(2),
            lookback: fixed(100),
        },
        IndicatorSpec::PivotHigh {
            left: fixed(1),
            right: fixed(1),
        },
        IndicatorSpec::CandlePattern {
            pattern: CandlePatternName::Engulfing,
        },
        IndicatorSpec::Ema { period: fixed(50) },
    ]
}

/// The HTF series' rule specs, in the DSL's reference order.
fn htf_specs() -> Vec<IndicatorSpec> {
    vec![
        IndicatorSpec::Support {
            left: fixed(3),
            right: fixed(3),
            tolerance_pct: SweepableValue::Fixed(dec("0.005")),
            min_touches: fixed(2),
            lookback: fixed(50),
        },
        IndicatorSpec::PivotLow {
            left: fixed(2),
            right: fixed(2),
        },
        IndicatorSpec::CandlePattern {
            pattern: CandlePatternName::Hammer,
        },
        IndicatorSpec::Ema { period: fixed(20) },
    ]
}

/// Seed one strategy plus the chart-read version, and return the version id.
async fn seed_version(state: &DesktopState) -> VersionId {
    let repo = state.strategy_repo();
    let strategy = repo
        .create_strategy("chart read", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create the strategy");
    repo.create_version(NewVersion {
        strategy_id: StrategyId::new(strategy.id.as_str().to_owned()),
        parent_version_id: None,
        dsl_json: CHART_DSL.to_owned(),
        created_by: pulse::CreatedBy::Human,
        creating_llm_call_ids: vec![],
    })
    .await
    .expect("create the version")
    .id
}

/// One closed long trade entering at `entry_ms`, `r` R above a 90 000 entry with
/// its stop 1 000 below (the `seeded_trade_at_r` shape, on the caller's bar).
fn trade_at(entry_ms: i64, r: &str) -> Trade {
    let entry = Decimal::new(90_000, 0);
    let stop = Decimal::new(89_000, 0);
    let r = dec(r);
    let exit = entry + r * (entry - stop);
    let qty = Decimal::ONE;
    let exit_ms = entry_ms + 900_000;
    let fees = Decimal::new(2, 2);
    Trade {
        direction: Direction::Long,
        qty,
        entry_price: entry,
        exit_price: exit,
        entry_signal_time: entry_ms,
        entry_fill_time: entry_ms,
        exit_signal_time: exit_ms,
        exit_fill_time: exit_ms,
        fills: vec![
            Fill {
                price: entry,
                qty,
                time_ms: entry_ms,
                fee: Decimal::new(1, 2),
            },
            Fill {
                price: exit,
                qty,
                time_ms: exit_ms,
                fee: Decimal::new(1, 2),
            },
        ],
        fees_total: fees,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        realized_pnl: (exit - entry) * qty - fees,
        realized_r: r,
        mfe_r: if r > Decimal::ZERO { r } else { Decimal::ZERO },
        mae_r: if r < Decimal::ZERO { r } else { Decimal::ZERO },
        exit_reason: if r > Decimal::ZERO {
            ExitReason::TakeProfit
        } else {
            ExitReason::StopLoss
        },
        source: TradeSource::Backtest,
        regime: Regime::TrendingUp,
        stop_price: Some(stop),
    }
}

/// Save one run of `trades` against `version`, recording the fixture's REAL
/// snapshot identities so the read resolves exactly the data the run consumed.
async fn seed_run(server: &TestServer, version: &VersionId, trades: Vec<Trade>) -> String {
    let store = store(&server.data_dir);
    seed_run_on(
        server,
        version,
        SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: head(&store, Timeframe::M15),
        },
        Some(SnapshotSelection {
            timeframe: Timeframe::H4,
            data_version: head(&store, Timeframe::H4),
        }),
        None,
        trades,
    )
    .await
}

/// The same, over snapshots and a window the caller chooses — the shape a windowed
/// backtest or a walk-forward fold persists (`inputs.window`).
async fn seed_run_on(
    server: &TestServer,
    version: &VersionId,
    primary: SnapshotSelection,
    htf: Option<SnapshotSelection>,
    window: Option<CandleWindow>,
    trades: Vec<Trade>,
) -> String {
    let inputs = BacktestInputs {
        pair: pair(),
        primary,
        htf,
        d1: None,
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: FundingConfig::SnapshotRates,
        symbol_filters: None,
        window,
        lead_in_from_ms: None,
    };
    let net_pnl = trades.iter().map(|trade| trade.realized_pnl).sum();
    let fees_total = trades.iter().map(|trade| trade.fees_total).sum();
    let starting_equity = Decimal::new(10_000, 0);
    let equity_curve = EquityCurve::default();
    let summary =
        SummaryStats::from_trades(&trades, net_pnl, fees_total, Decimal::ZERO, &equity_curve);
    let result = BacktestResult {
        trades,
        net_pnl,
        fees_total,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        summary: summary.clone(),
        regime_breakdown: RegimeBreakdown::new(),
        skipped_entries: SkippedEntryCounts::new(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::current(),
        equity_curve,
    };
    SqliteBacktestRunRepo::new(server.db.pool().clone())
        .save_run(version, &inputs, &result, &summary, starting_equity)
        .await
        .expect("the seeded run persists")
        .as_str()
        .to_owned()
}

/// The chart read for one request.
async fn read(state: &DesktopState, request: ChartDataRequest) -> Result<ChartDataDto, BusError> {
    chart_data_core(state, request).await
}

/// A request naming only a pair and timeframe.
fn request(pair_text: &str, timeframe: &str) -> ChartDataRequest {
    ChartDataRequest {
        pair: pair_text.to_owned(),
        timeframe: timeframe.to_owned(),
        from_ms: None,
        to_ms: None,
        run_id: None,
    }
}

/// Open one freeze through the store, with the injected clock.
async fn open_freeze(server: &TestServer, holdout_start_ms: i64) {
    SqliteCertificationFreezeRepo::with_deps(server.db.pool().clone(), FakeClock::at(OPENED_MS))
        .open(&OpenFreezeRequest {
            holdout_start_ms,
            h: 12,
            alpha: "0.0463".to_owned(),
            holdout_test: "C1: z = z(1 - 0.05/H)".to_owned(),
        })
        .await
        .expect("open the freeze");
}

/// The independent pass (SPINE G3/G2): load the pinned series, cut it at the
/// holdout start BEFORE any step, then step the whole cut series and read one
/// value per bar per spec — the `export_indicators` loop, written here.
fn independent_pass(
    store: &CandleStore,
    timeframe: Timeframe,
    version: &DataVersion,
    holdout: Option<i64>,
    specs: &[IndicatorSpec],
) -> (CandleSeries, Vec<Vec<Option<Decimal>>>) {
    let mut series = store
        .load_version(&pair(), timeframe, version)
        .expect("the pinned snapshot reads")
        .series;
    if let Some(holdout_start_ms) = holdout {
        series
            .candles
            .retain(|candle| candle.open_time < holdout_start_ms);
    }
    let mut engine = IndicatorEngine::from_specs(specs).expect("the specs build an engine");
    let mut rows = Vec::with_capacity(series.candles.len());
    for candle in &series.candles {
        engine.step(candle);
        rows.push(
            specs
                .iter()
                .map(|spec| {
                    engine.current(&CompiledValue::Indicator {
                        series: Series::Primary,
                        spec: spec.clone(),
                        lag: 0,
                    })
                })
                .collect::<Vec<_>>(),
        );
    }
    (series, rows)
}

/// The window's bar indices in the cut series: `from <= open_time <= to`.
fn window_indices(series: &CandleSeries, from: i64, to: i64) -> Vec<usize> {
    (0..series.candles.len())
        .filter(|&index| (from..=to).contains(&series.candles[index].open_time))
        .collect()
}

/// The overlay with `label`, or a panic naming what the read returned instead.
fn overlay<'a>(overlays: &'a [ChartOverlayDto], label: &str) -> &'a ChartOverlayDto {
    overlays
        .iter()
        .find(|overlay| match overlay {
            ChartOverlayDto::Level { label: carried, .. }
            | ChartOverlayDto::Line { label: carried, .. }
            | ChartOverlayDto::Pivot { label: carried, .. }
            | ChartOverlayDto::Pattern { label: carried, .. } => carried == label,
        })
        .unwrap_or_else(|| panic!("no overlay labelled {label}: {overlays:?}"))
}

/// The per-bar values one level/line overlay carries.
fn values(overlay: &ChartOverlayDto) -> &[Option<String>] {
    match overlay {
        ChartOverlayDto::Level { values, .. } | ChartOverlayDto::Line { values, .. } => values,
        other => panic!("expected a level or line overlay, got {other:?}"),
    }
}

/// The expected per-bar strings for one spec slot over a window.
fn expected_values(
    rows: &[Vec<Option<Decimal>>],
    window: &[usize],
    slot: usize,
) -> Vec<Option<String>> {
    window
        .iter()
        .map(|&index| rows[index][slot].map(|value| value.normalize().to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// The window and the default window
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_returns_its_candles_and_the_head_version() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let store = store(&server.data_dir);
    let series = store
        .load_version(&pair(), Timeframe::M15, &head(&store, Timeframe::M15))
        .expect("the fixture reads")
        .series;
    let from = series.candles[100].open_time;
    let to = series.candles[199].close_time;

    let dto = read(
        &state,
        ChartDataRequest {
            from_ms: Some(from.to_string()),
            to_ms: Some(to.to_string()),
            ..request("BTCUSDT", "15m")
        },
    )
    .await
    .expect("the window reads");

    assert_eq!(dto.pair, "BTCUSDT");
    assert_eq!(dto.timeframe, "15m");
    assert_eq!(
        dto.data_version,
        head(&store, Timeframe::M15).as_str(),
        "the read names the snapshot it read"
    );
    assert_eq!(dto.candles.len(), 100, "the window holds its own bars");
    assert_eq!(dto.candles[0].t, from.to_string());
    assert_eq!(dto.candles[99].t, series.candles[199].open_time.to_string());
    assert_eq!(
        dto.candles[0].o,
        series.candles[100].open.normalize().to_string()
    );
    assert_eq!(
        dto.candles[0].c,
        series.candles[100].close.normalize().to_string()
    );
    assert!(dto.holdout.is_none(), "no freeze is open");
    assert!(dto.run.is_none(), "no run was named");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_window_returns_the_latest_bars_of_the_head_snapshot() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let store = store(&server.data_dir);
    let series = store
        .load_version(&pair(), Timeframe::M15, &head(&store, Timeframe::M15))
        .expect("the fixture reads")
        .series;

    let dto = read(&state, request("BTCUSDT", "15m"))
        .await
        .expect("the default read works");

    assert_eq!(
        dto.candles.len(),
        series.candles.len(),
        "the fixture is under the cap, so the default read returns it whole"
    );
    assert_eq!(
        dto.candles.last().expect("bars").t,
        series.candles.last().expect("bars").open_time.to_string()
    );
    assert_eq!(
        dto.candles.first().expect("bars").t,
        series.candles.first().expect("bars").open_time.to_string()
    );
}

// ---------------------------------------------------------------------------
// A windowed run's default window is the window it consumed
// ---------------------------------------------------------------------------

/// A run that recorded `inputs.window` — a windowed backtest or a walk-forward
/// fold — must default to THAT window, not to the whole pinned snapshot: a small
/// window inside an over-cap snapshot is a valid request, and its candles are the
/// only ones the run consumed.
///
/// On the base commit the read defaults to the pinned snapshot's whole span, so
/// this small window is refused with the cap (the false 422) — and the HTF leg
/// answers with bars from outside the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_windowed_runs_default_window_is_the_window_it_consumed() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    // A synthetic snapshot longer than the cap, so only the RUN's window can
    // answer: the whole-snapshot default would be refused.
    let m15 = write_series(
        &server.data_dir,
        &pair(),
        Timeframe::M15,
        i64::from(pulse::CHART_BAR_CAP) + 100,
    );
    let h4 = write_series(&server.data_dir, &pair(), Timeframe::H4, 3_000);
    let store = store(&server.data_dir);
    let series = store
        .load_version(&pair(), Timeframe::M15, &m15)
        .expect("the synthetic M15 reads")
        .series;
    let held = u32::try_from(series.candles.len()).expect("a count");
    assert!(
        held > pulse::CHART_BAR_CAP,
        "the snapshot must be over the cap for this test to bite"
    );
    let step = Timeframe::M15.duration_ms();
    let window = CandleWindow::new(
        series.candles[10_000].open_time,
        series.candles[12_000].open_time,
    )
    .expect("a window");
    let run_id = seed_run_on(
        &server,
        &version,
        SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: m15.clone(),
        },
        Some(SnapshotSelection {
            timeframe: Timeframe::H4,
            data_version: h4.clone(),
        }),
        Some(window.clone()),
        vec![],
    )
    .await;

    let dto = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id.clone()),
            ..request("BTCUSDT", "15m")
        },
    )
    .await
    .expect("the run's own window is under the cap, so it must answer");
    let consumed: Vec<i64> = series
        .candles
        .iter()
        .filter(|candle| candle.open_time >= window.from_ms && candle.open_time < window.to_ms)
        .map(|candle| candle.open_time)
        .collect();
    assert_eq!(
        dto.candles.len(),
        consumed.len(),
        "the default window holds exactly the bars the run consumed"
    );
    assert_eq!(dto.candles[0].t, consumed[0].to_string());
    assert_eq!(
        dto.candles[dto.candles.len() - 1].t,
        consumed[consumed.len() - 1].to_string()
    );

    // The ends are the RUN DTO's own span — the same numbers the Lab shows for
    // this run, and the reason the tab's own fit decision agrees with the read.
    let run_dto = get_backtest_run_core(
        &state,
        GetBacktestRunRequest {
            run_id: run_id.clone(),
        },
    )
    .await
    .expect("the run reads back");
    assert_eq!(dto.candles[0].t, run_dto.first_open_time_ms);
    assert_eq!(
        dto.candles[dto.candles.len() - 1]
            .t
            .parse::<i64>()
            .expect("a time")
            + step
            - 1,
        run_dto.last_close_time_ms.parse::<i64>().expect("a time"),
        "the defaulted window ends where the run's own span ends"
    );

    // The HTF leg defaults to the SAME run window: the H4 bars the run consumed,
    // never bars from the rest of the snapshot.
    let htf = store
        .load_version(&pair(), Timeframe::H4, &h4)
        .expect("the synthetic H4 reads")
        .series;
    let htf_consumed: Vec<i64> = htf
        .candles
        .iter()
        .filter(|candle| candle.open_time >= window.from_ms && candle.open_time < window.to_ms)
        .map(|candle| candle.open_time)
        .collect();
    assert!(
        !htf_consumed.is_empty(),
        "the window must hold H4 bars for this test to bite"
    );
    let htf_dto = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id),
            ..request("BTCUSDT", "4h")
        },
    )
    .await
    .expect("the HTF leg answers");
    assert_eq!(
        htf_dto.candles.len(),
        htf_consumed.len(),
        "the HTF leg defaults to the run's own window"
    );
    assert_eq!(htf_dto.candles[0].t, htf_consumed[0].to_string());
    assert_eq!(
        htf_dto.candles[htf_dto.candles.len() - 1].t,
        htf_consumed[htf_consumed.len() - 1].to_string()
    );
}

/// A windowed run's withheld count is the cut's count INSIDE THE RUN'S WINDOW —
/// not inside the whole pinned snapshot. Fails on the base commit, where the
/// defaulted window is the whole snapshot: 200 candles come back instead of the
/// window's 100, and the count is 100 instead of 50.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_windowed_runs_withheld_count_counts_only_its_own_window() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let m15 = write_series(&server.data_dir, &pair(), Timeframe::M15, 300);
    let store = store(&server.data_dir);
    let series = store
        .load_version(&pair(), Timeframe::M15, &m15)
        .expect("the synthetic M15 reads")
        .series;
    let window = CandleWindow::new(series.candles[100].open_time, series.candles[250].open_time)
        .expect("a window");
    let holdout_start = series.candles[200].open_time;
    open_freeze(&server, holdout_start).await;
    let run_id = seed_run_on(
        &server,
        &version,
        SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: m15,
        },
        None,
        Some(window.clone()),
        vec![],
    )
    .await;

    let dto = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id),
            ..request("BTCUSDT", "15m")
        },
    )
    .await
    .expect("the run's own window reads");
    let kept = series
        .candles
        .iter()
        .filter(|candle| {
            candle.open_time >= window.from_ms
                && candle.open_time < window.to_ms
                && candle.open_time < holdout_start
        })
        .count();
    let withheld = series
        .candles
        .iter()
        .filter(|candle| {
            candle.open_time >= window.from_ms
                && candle.open_time < window.to_ms
                && candle.open_time >= holdout_start
        })
        .count();
    assert!(
        kept > 0 && withheld > 0,
        "the window must straddle the holdout start to test anything"
    );
    assert_eq!(
        dto.candles.len(),
        kept,
        "the default window is the run's, cut at the holdout"
    );
    assert_eq!(
        dto.candles[0].t,
        series.candles[100].open_time.to_string(),
        "the window starts at the run's first consumed bar"
    );
    let holdout = dto.holdout.expect("the read says a freeze cut it");
    assert_eq!(holdout.holdout_start_ms, holdout_start.to_string());
    assert_eq!(
        holdout.bars_withheld,
        u32::try_from(withheld).expect("a count"),
        "barsWithheld counts the cut's candles inside the RUN's window"
    );
}

// ---------------------------------------------------------------------------
// The cap (Q3/G4) and the holdout cut (Q5)
// ---------------------------------------------------------------------------

/// Write one synthetic, gap-free series into `dir` and point HEAD at it.
fn write_series(dir: &Path, pair: &Pair, timeframe: Timeframe, count: i64) -> DataVersion {
    let step = timeframe.duration_ms();
    let candles: Vec<Candle> = (0..count)
        .map(|index| {
            let price = Decimal::from(100 + index);
            Candle {
                open_time: index * step,
                close_time: index * step + step - 1,
                open: price,
                high: price,
                low: price,
                close: price,
                volume: Decimal::ONE,
                funding_rate: None,
            }
        })
        .collect();
    let mut series = CandleSeries {
        pair: pair.clone(),
        timeframe,
        version: DataVersion::new("placeholder"),
        candles,
    };
    series.version = CandleStore::content_version(pair, timeframe, &series.candles);
    let store = CandleStore::with_base_dir(dir.to_path_buf());
    store
        .write_snapshot(&series)
        .expect("the synthetic series writes");
    store
        .write_head(pair, timeframe, &series.version)
        .expect("HEAD moves to it");
    series.version
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_over_the_cap_is_refused_naming_the_cap_and_the_count() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let over = 20_001_i64;
    let pair_text = "ETHUSDT";
    let eth = Pair::new(pair_text);
    let version = write_series(&server.data_dir, &eth, Timeframe::M15, over);
    let store = store(&server.data_dir);
    let series = store
        .load_version(&eth, Timeframe::M15, &version)
        .expect("the synthetic series reads")
        .series;

    let error = read(
        &state,
        ChartDataRequest {
            from_ms: Some(series.candles[0].open_time.to_string()),
            to_ms: Some(
                series.candles[series.candles.len() - 1]
                    .open_time
                    .to_string(),
            ),
            ..request(pair_text, "15m")
        },
    )
    .await
    .expect_err("a window over the cap is refused");

    assert_eq!(error.code, BusErrorCode::Validation);
    assert!(
        error.message.contains("20000"),
        "the refusal names the cap: {}",
        error.message
    );
    assert!(
        error.message.contains("20001"),
        "the refusal names the count: {}",
        error.message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_window_over_a_long_series_keeps_the_latest_bars_up_to_the_cap() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let pair_text = "SOLUSDT";
    let sol = Pair::new(pair_text);
    let long = 25_000_i64;
    let version = write_series(&server.data_dir, &sol, Timeframe::M15, long);
    let store = store(&server.data_dir);
    let series = store
        .load_version(&sol, Timeframe::M15, &version)
        .expect("the synthetic series reads")
        .series;

    let dto = read(&state, request(pair_text, "15m"))
        .await
        .expect("the default read does not refuse; it takes the latest bars");
    let cap = usize::try_from(pulse::CHART_BAR_CAP).unwrap();

    assert_eq!(
        dto.candles.len(),
        cap,
        "the default read is capped, not refused"
    );
    assert_eq!(
        dto.candles[0].t,
        series.candles[series.candles.len() - cap]
            .open_time
            .to_string(),
        "the LATEST bars, not the first"
    );
    assert_eq!(
        dto.candles[cap - 1].t,
        series.candles[series.candles.len() - 1]
            .open_time
            .to_string()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_freeze_cuts_the_candles_and_counts_only_its_window() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    open_freeze(&server, HOLDOUT_MS).await;
    let store = store(&server.data_dir);
    let series = store
        .load_version(&pair(), Timeframe::M15, &head(&store, Timeframe::M15))
        .expect("the fixture reads")
        .series;
    let step = Timeframe::M15.duration_ms();
    let from = HOLDOUT_MS - 3 * step;
    let to = HOLDOUT_MS + 5 * step;

    let dto = read(
        &state,
        ChartDataRequest {
            from_ms: Some(from.to_string()),
            to_ms: Some(to.to_string()),
            ..request("BTCUSDT", "15m")
        },
    )
    .await
    .expect("the frozen window reads");

    // The independent truth: the cut removes every bar opening at/after the
    // holdout start, so the returned candles are exactly the window's bars before
    // it and the count is exactly the window's bars at/after it.
    let kept = series
        .candles
        .iter()
        .filter(|candle| candle.open_time >= from && candle.open_time < HOLDOUT_MS)
        .count();
    let withheld = series
        .candles
        .iter()
        .filter(|candle| candle.open_time >= HOLDOUT_MS && candle.open_time <= to)
        .count();
    assert!(
        kept > 0 && withheld > 0,
        "the window must straddle the holdout start to test anything"
    );

    assert_eq!(dto.candles.len(), kept);
    assert!(
        dto.candles
            .iter()
            .all(|candle| candle.t.parse::<i64>().unwrap() < HOLDOUT_MS),
        "no candle at or after the holdout start may cross"
    );
    let holdout = dto.holdout.expect("the read says a freeze cut it");
    assert_eq!(holdout.holdout_start_ms, HOLDOUT_MS.to_string());
    assert_eq!(
        holdout.bars_withheld,
        u32::try_from(withheld).unwrap(),
        "barsWithheld counts only the candles the cut removed inside the window"
    );
}

// ---------------------------------------------------------------------------
// The run layer — its stored trades
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_read_returns_the_runs_stored_trades() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let store = store(&server.data_dir);
    let series = store
        .load_version(&pair(), Timeframe::M15, &head(&store, Timeframe::M15))
        .expect("the fixture reads")
        .series;
    let first = series.candles[0].open_time;
    let inside_a = first + 10 * Timeframe::M15.duration_ms();
    let inside_b = first + 20 * Timeframe::M15.duration_ms();
    let outside = 1_700_000_000_000;
    let run_id = seed_run(
        &server,
        &version,
        vec![
            trade_at(inside_a, "2"),
            trade_at(inside_b, "-1"),
            trade_at(outside, "1"),
        ],
    )
    .await;

    let dto = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id.clone()),
            ..request("BTCUSDT", "15m")
        },
    )
    .await
    .expect("the run reads");
    let run = dto.run.expect("the read carries the run layer");
    assert_eq!(run.run_id, run_id);
    assert_eq!(run.version_id, version.as_str());
    assert_eq!(run.series, "primary");

    let stored = SqliteBacktestRunRepo::new(server.db.pool().clone())
        .get_trades(&BacktestRunId::new(&run_id))
        .await
        .expect("the run's trades read back");
    let from = series.candles[0].open_time;
    let to = series.candles[series.candles.len() - 1].close_time;
    let in_window: Vec<&Trade> = stored
        .iter()
        .filter(|trade| {
            (from..=to).contains(&trade.entry_fill_time)
                || (from..=to).contains(&trade.exit_fill_time)
        })
        .collect();
    assert_eq!(
        in_window.len(),
        2,
        "the trade outside the window is not drawn"
    );
    assert_eq!(run.trades.len(), in_window.len());

    for (drawn, trade) in run.trades.iter().zip(&in_window) {
        assert_eq!(drawn.side, "long");
        assert_eq!(drawn.entry_time, trade.entry_fill_time.to_string());
        assert_eq!(drawn.entry_price, trade.entry_price.normalize().to_string());
        assert_eq!(
            drawn.exit_time.as_deref(),
            Some(trade.exit_fill_time.to_string().as_str())
        );
        assert_eq!(
            drawn.exit_price.as_deref(),
            Some(trade.exit_price.normalize().to_string().as_str())
        );
        assert_eq!(
            drawn.realized_r,
            trade.realized_r.normalize().to_string(),
            "the stored R, unchanged"
        );
    }
    match drawn_reason(&run.trades[0]) {
        Some("take_profit") => {}
        other => panic!("the first trade's stored reason is take_profit, got {other:?}"),
    }
    match drawn_reason(&run.trades[1]) {
        Some("stop_loss") => {}
        other => panic!("the second trade's stored reason is stop_loss, got {other:?}"),
    }
}

/// The stored exit reason a chart trade carries.
fn drawn_reason(trade: &pulse::ChartTradeDto) -> Option<&str> {
    trade.exit_reason.as_deref()
}

/// While a freeze is open, EVERY trade surface of the run layer is fenced: a
/// trade whose entry OR exit is at or after the holdout start carries an outcome
/// the cut withheld — its exit price, its reason and its realized R included — so
/// the read must not return it at all.
///
/// On the base commit all five trades come back: the fence is applied to the
/// candles only, and the withheld period's outcomes cross beside the note that
/// says the bars were withheld.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_freeze_withholds_every_trade_that_reaches_the_holdout() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let store = store(&server.data_dir);
    let series = store
        .load_version(&pair(), Timeframe::M15, &head(&store, Timeframe::M15))
        .expect("the fixture reads")
        .series;
    let step = Timeframe::M15.duration_ms();
    let holdout_start = series.candles[200].open_time;
    // Every boundary in one case: wholly before, exiting exactly ON the boundary,
    // entering exactly ON it, straddling it, and wholly after.
    let before = trade_at(holdout_start - 8 * step, "2");
    let exit_on = trade_at(holdout_start - step, "1");
    let entry_on = trade_at(holdout_start, "1");
    let straddling = trade_at(holdout_start - step / 2, "-1");
    let after = trade_at(holdout_start + 4 * step, "2");
    assert_eq!(
        exit_on.exit_fill_time, holdout_start,
        "the exit-at-boundary trade must exit exactly at the holdout start"
    );
    let run_id = seed_run(
        &server,
        &version,
        vec![before.clone(), exit_on, entry_on, straddling, after],
    )
    .await;
    open_freeze(&server, holdout_start).await;

    let dto = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id),
            from_ms: Some((holdout_start - 12 * step).to_string()),
            to_ms: Some((holdout_start + 8 * step).to_string()),
            ..request("BTCUSDT", "15m")
        },
    )
    .await
    .expect("the fenced window reads");
    let run = dto.run.expect("the read carries the run layer");
    assert_eq!(
        run.trades.len(),
        1,
        "only the trade that ends before the holdout may cross"
    );
    assert_eq!(run.trades[0].entry_time, before.entry_fill_time.to_string());
    assert_eq!(
        run.trades[0].exit_time,
        Some(before.exit_fill_time.to_string())
    );
    assert_eq!(
        run.trades[0].realized_r,
        before.realized_r.normalize().to_string()
    );
    assert!(
        dto.holdout.is_some(),
        "the read still says a freeze cut its candles"
    );
    // The fence is the point, not a truncation: nothing in the response carries a
    // time at or after the holdout start.
    for trade in &run.trades {
        assert!(
            trade.entry_time.parse::<i64>().expect("a time") < holdout_start
                && trade
                    .exit_time
                    .as_deref()
                    .expect("a closed trade")
                    .parse::<i64>()
                    .expect("a time")
                    < holdout_start,
            "no returned trade may carry a time at or after the holdout start"
        );
    }
}

// ---------------------------------------------------------------------------
// The overlays — every value equals an independent engine pass (G2)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_primary_overlay_equals_an_independent_engine_pass() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let store = store(&server.data_dir);
    let m15 = head(&store, Timeframe::M15);
    let primary = store
        .load_version(&pair(), Timeframe::M15, &m15)
        .expect("the fixture reads")
        .series;
    let from = primary.candles[0].open_time;
    let to = primary.candles[primary.candles.len() - 1].close_time;
    let run_id = seed_run(
        &server,
        &version,
        vec![trade_at(from + 10 * Timeframe::M15.duration_ms(), "2")],
    )
    .await;

    let dto = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id),
            ..request("BTCUSDT", "15m")
        },
    )
    .await
    .expect("the run reads");
    let run = dto.run.expect("the run layer");

    let specs = primary_specs();
    let (cut, rows) = independent_pass(&store, Timeframe::M15, &m15, None, &specs);
    let window = window_indices(&cut, from, to);
    assert!(!window.is_empty(), "the run's window holds bars");

    // The level, as it stood at each bar (G3), and the EMA line.
    let level = overlay(&run.overlays, "support:2-2-0.001-2-100");
    assert!(matches!(level, ChartOverlayDto::Level { .. }), "{level:?}");
    assert_eq!(*values(level), expected_values(&rows, &window, 0));

    let line = overlay(&run.overlays, "ema:50");
    assert!(matches!(line, ChartOverlayDto::Line { .. }), "{line:?}");
    assert_eq!(*values(line), expected_values(&rows, &window, 3));

    // The pivot's events are the bars where the engine's held value changed.
    let ChartOverlayDto::Pivot { events, .. } = overlay(&run.overlays, "pivot_high:1-1").clone()
    else {
        panic!("pivot_high is drawn as a pivot")
    };
    let expected: Vec<(i64, i64, Decimal)> = window
        .iter()
        .filter_map(|&index| {
            let value = rows[index][1]?;
            let previous = index.checked_sub(1).and_then(|earlier| rows[earlier][1]);
            if previous == Some(value) {
                return None;
            }
            Some((
                cut.candles[index].open_time,
                cut.candles[index - 1].open_time,
                value,
            ))
        })
        .collect();
    assert!(!expected.is_empty(), "the fixture confirms pivots");
    let drawn: Vec<(i64, i64, Decimal)> = events
        .iter()
        .map(|event| {
            (
                event.confirm_time.parse().unwrap(),
                event.peak_time.parse().unwrap(),
                dec(&event.price),
            )
        })
        .collect();
    assert_eq!(
        drawn, expected,
        "one event per confirmation, at its own bar"
    );

    // The pattern's events are the window's non-zero bars, value and all.
    let ChartOverlayDto::Pattern { events, .. } = overlay(&run.overlays, "cdl:engulfing").clone()
    else {
        panic!("cdl:engulfing is drawn as a pattern")
    };
    let expected: Vec<(i64, Decimal)> = window
        .iter()
        .filter_map(|&index| {
            let value = rows[index][2]?;
            if value.is_zero() {
                return None;
            }
            Some((cut.candles[index].open_time, value))
        })
        .collect();
    assert!(!expected.is_empty(), "the fixture fires engulfing patterns");
    let drawn: Vec<(i64, Decimal)> = events
        .iter()
        .map(|event| (event.time.parse().unwrap(), dec(&event.value)))
        .collect();
    assert_eq!(drawn, expected);
    assert!(
        drawn.iter().all(|(_, value)| !value.is_zero()),
        "no zero-valued pattern event"
    );

    // An RSI is a spec the tab does not draw (Q4).
    assert!(
        run.not_drawn.iter().any(|label| label == "rsi:14"),
        "notDrawn lists the rule's other specs: {:?}",
        run.not_drawn
    );
    assert!(
        !run.not_drawn
            .iter()
            .any(|label| drawn_labels().contains(&label.as_str())),
        "notDrawn holds only the specs that are not drawn: {:?}",
        run.not_drawn
    );
}

/// The labels the primary read draws.
fn drawn_labels() -> [&'static str; 4] {
    [
        "support:2-2-0.001-2-100",
        "pivot_high:1-1",
        "cdl:engulfing",
        "ema:50",
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_htf_read_computes_its_own_series_overlays() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let store = store(&server.data_dir);
    let h4 = head(&store, Timeframe::H4);
    let primary = store
        .load_version(&pair(), Timeframe::M15, &head(&store, Timeframe::M15))
        .expect("the fixture reads")
        .series;
    let from = primary.candles[0].open_time;
    let to = primary.candles[primary.candles.len() - 1].close_time;
    let run_id = seed_run(
        &server,
        &version,
        vec![trade_at(from + 10 * Timeframe::M15.duration_ms(), "2")],
    )
    .await;

    let dto = read(
        &state,
        ChartDataRequest {
            timeframe: "4h".to_owned(),
            run_id: Some(run_id),
            ..request("BTCUSDT", "4h")
        },
    )
    .await
    .expect("the HTF read works");
    assert_eq!(dto.timeframe, "4h");
    assert_eq!(dto.data_version, h4.as_str());
    let run = dto.run.expect("the run layer");
    assert_eq!(run.series, "htf", "the HTF read draws the HTF series");

    let specs = htf_specs();
    let (cut, rows) = independent_pass(&store, Timeframe::H4, &h4, None, &specs);
    let window = window_indices(&cut, from, to);
    assert!(!window.is_empty(), "the H4 window holds bars");

    assert_eq!(
        *values(overlay(&run.overlays, "support:3-3-0.005-2-50")),
        expected_values(&rows, &window, 0)
    );
    assert_eq!(
        *values(overlay(&run.overlays, "ema:20")),
        expected_values(&rows, &window, 3)
    );

    // The HTF pivot spans two bars on each side, so its peak sits two H4 bars
    // before its confirmation bar.
    let ChartOverlayDto::Pivot { events, .. } = overlay(&run.overlays, "pivot_low:2-2").clone()
    else {
        panic!("pivot_low is drawn as a pivot")
    };
    assert!(!events.is_empty(), "the fixture confirms H4 pivots");
    for event in &events {
        let confirm: i64 = event.confirm_time.parse().unwrap();
        let peak: i64 = event.peak_time.parse().unwrap();
        let index = cut
            .candles
            .iter()
            .position(|candle| candle.open_time == confirm)
            .expect("the confirmation bar is in the cut series");
        assert_eq!(
            peak,
            cut.candles[index - 2].open_time,
            "the peak bar sits `right` bars before the confirmation"
        );
    }

    let ChartOverlayDto::Pattern { events, .. } = overlay(&run.overlays, "cdl:hammer").clone()
    else {
        panic!("cdl:hammer is drawn as a pattern")
    };
    for event in &events {
        assert_ne!(dec(&event.value), Decimal::ZERO);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_htf_read_carries_the_runs_trades_too() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let store = store(&server.data_dir);
    let primary = store
        .load_version(&pair(), Timeframe::M15, &head(&store, Timeframe::M15))
        .expect("the fixture reads")
        .series;
    let entry = primary.candles[10].open_time;
    let run_id = seed_run(&server, &version, vec![trade_at(entry, "2")]).await;

    let dto = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id.clone()),
            ..request("BTCUSDT", "4h")
        },
    )
    .await
    .expect("the HTF read works");
    let run = dto.run.expect("the run layer");
    assert_eq!(
        run.trades.len(),
        1,
        "the trades ride the run layer on BOTH timeframes (Q2)"
    );
    assert_eq!(run.trades[0].entry_time, entry.to_string());
}

// ---------------------------------------------------------------------------
// The refusals (Q1/G4) and the route's scope (the risk gate)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pair_or_timeframe_the_run_does_not_use_is_refused_naming_the_field() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let run_id = seed_run(&server, &version, vec![]).await;

    let wrong_pair = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id.clone()),
            ..request("ETHUSDT", "15m")
        },
    )
    .await
    .expect_err("a pair the run did not trade is refused");
    assert_eq!(wrong_pair.code, BusErrorCode::Validation);
    assert!(
        wrong_pair.message.contains("pair"),
        "the refusal names the field: {}",
        wrong_pair.message
    );

    let wrong_tf = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id),
            ..request("BTCUSDT", "1d")
        },
    )
    .await
    .expect_err("a timeframe the run does not have is refused");
    assert_eq!(wrong_tf.code, BusErrorCode::Validation);
    assert!(
        wrong_tf.message.contains("timeframe"),
        "the refusal names the field: {}",
        wrong_tf.message
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_run_id_is_a_not_found_refusal() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;

    let error = read(
        &state,
        ChartDataRequest {
            run_id: Some("run-names-nothing".to_owned()),
            ..request("BTCUSDT", "15m")
        },
    )
    .await
    .expect_err("an unknown run is refused");
    assert_eq!(error.code, BusErrorCode::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_is_app_scope_and_carries_the_window() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let run_id = seed_run(&server, &version, vec![]).await;
    let client = reqwest::Client::new();

    let body: serde_json::Value = client
        .post(format!("{}/api/v1/chart", server.base))
        .bearer_auth(&server.app_token)
        .json(&serde_json::json!({ "pair": "BTCUSDT", "timeframe": "15m", "runId": run_id }))
        .send()
        .await
        .expect("POST the app route")
        .json()
        .await
        .expect("the route's JSON");
    assert!(body["dataVersion"].is_string(), "{body}");
    assert!(body["candles"].is_array(), "{body}");
    assert_eq!(body["run"]["runId"], run_id);
    assert!(body["run"]["overlays"].is_array());
    assert!(body["run"]["notDrawn"].is_array());

    let unknown = client
        .post(format!("{}/api/v1/chart", server.base))
        .bearer_auth(&server.app_token)
        .json(&serde_json::json!({
            "pair": "BTCUSDT", "timeframe": "15m", "runId": "run-names-nothing"
        }))
        .send()
        .await
        .expect("POST an unknown run");
    assert_eq!(unknown.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let body: serde_json::Value = unknown.json().await.expect("the refusal's JSON");
    assert_eq!(body["code"], "not_found");

    let refused = client
        .post(format!("{}/api/v1/chart", server.base))
        .bearer_auth(&server.agent_token)
        .json(&serde_json::json!({ "pair": "BTCUSDT", "timeframe": "15m" }))
        .send()
        .await
        .expect("POST the app route with an agent token");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an agent token cannot read the chart"
    );
    let body: serde_json::Value = refused.json().await.expect("the refusal's JSON");
    assert_eq!(body["code"], "scope_refused");
}

/// Print probe for the item report's ONE trimmed HTF response (the
/// `streak_card.rs` `print_anchor_golden` precedent). Run:
///
/// ```sh
/// cargo test --test chart_read -- --ignored --nocapture print_htf_response
/// ```
///
/// and paste the printed body — 3 candles, at most 2 events per overlay — into
/// the item's report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "print probe for the item report's trimmed HTF response"]
async fn print_htf_response() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let store = store(&server.data_dir);
    let primary = store
        .load_version(&pair(), Timeframe::M15, &head(&store, Timeframe::M15))
        .expect("the fixture reads")
        .series;
    let entry = primary.candles[10].open_time;
    let run_id = seed_run(&server, &version, vec![trade_at(entry, "2")]).await;

    let dto = read(
        &state,
        ChartDataRequest {
            run_id: Some(run_id),
            ..request("BTCUSDT", "4h")
        },
    )
    .await
    .expect("the HTF read works");
    println!("{}", trimmed_response(&dto));
}

/// The response trimmed to 3 candles and at most 2 events per overlay.
fn trimmed_response(dto: &ChartDataDto) -> String {
    let mut value = serde_json::to_value(dto).expect("the DTO serializes");
    let take = |items: &serde_json::Value, count: usize| {
        serde_json::Value::Array(
            items
                .as_array()
                .map(|array| array.iter().take(count).cloned().collect())
                .unwrap_or_default(),
        )
    };
    value["candles"] = take(&value["candles"], 3);
    if let Some(overlays) = value["run"]["overlays"].as_array_mut() {
        for overlay in overlays {
            if overlay.get("values").is_some() {
                overlay["values"] = take(&overlay["values"], 3);
            }
            if overlay.get("events").is_some() {
                overlay["events"] = take(&overlay["events"], 2);
            }
        }
    }
    serde_json::to_string_pretty(&value).expect("the trimmed body renders")
}
