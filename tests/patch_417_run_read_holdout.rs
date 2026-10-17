//! #417 / PDR-0038 option A — the app's run read while a freeze is open.
//!
//! `get_backtest_run_core` (`src/tauri/walk_forward.rs`) answered a persisted run
//! with every stored trade and every stored total, passing the open freeze's
//! holdout start only as metadata. The Chart tab hid a trade that reaches the
//! holdout — in the CLIENT, after the rows had arrived. The Lab showed them.
//!
//! This suite is the reproduction `PATCH_SKILL` §3 asks for: it seeds a stored run
//! and an open freeze and drives the REAL command core over the committed BTCUSDT
//! M15/H4 fixture, a migrated temp database and the real repositories — no
//! permanent double anywhere on the product path.
//!
//! What it pins:
//!
//! 1. **A trade is listed only when the Chart tab's own rule holds** — entry
//!    before H, and exit before H (`src/tauri/chart.rs`'s `trade_clears_holdout`,
//!    which is `ui/src/chart/layers.ts`'s `beforeFence` spelled server-side). A
//!    STRADDLING trade and a trade INSIDE the holdout are not sent, server side.
//! 2. **The run's stored totals are withheld** when the run's counted window
//!    reaches H — the same window decision agent reads use
//!    (`guard_persisted_window`), never a recomputation over the remaining trades.
//! 3. **The read says what it withheld** — the count, that the totals are
//!    withheld, and H (on the existing `holdoutStartMs`).
//! 4. **Nothing else moves**: a run wholly before H, and any run while no freeze
//!    is open, read exactly as before.
//! 5. **The fresh-run arm and the read agree at the FINAL-BAR boundary**
//!    (`#417`'s fix round, the OMP review's blocking finding 1): a snapshot whose
//!    last bar OPENS before H but CLOSES after it passes the window guard's
//!    clamp decision, so the run reaches `SeriesEnd::SnapshotEnd` and its
//!    end-of-data close fills at that bar's `close_time` — on or after H. The
//!    fresh-run arm must not answer with that trade and the full totals while the
//!    Lab's later read of the same run withholds both.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use pulse::{
    BacktestInputs, BacktestOutcome, BacktestResult, BacktestRunId, BacktestRunRepository,
    CandleSeriesRepository, CandleStore, CandleWindow, CreatedBy, DesktopState, Direction,
    EngineFingerprint, EquityCurve, ExitReason, FakeClock, Fill, FundingConfig, NewVersion,
    OpenFreezeRequest, Pair, Regime, RegimeBreakdown, SkippedEntryCounts, SnapshotSelection,
    SqliteBacktestRunRepo, SqliteCertificationFreezeRepo, StrategyId, StrategyRepository,
    SummaryStats, Timeframe, Trade, TradeSource, VersionId, get_backtest_run_core,
    project_histogram, run_read_fence,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

mod support;

/// The committed candle fixture every arm runs over.
const FIXTURE_STORE: &str = "tests/fixtures/btcusdt-1m-store";

/// The golden strategy — RSI(14) on M15 (the `tauri_backtest`/`tauri_walk_forward`
/// fixture, so this suite drives the same version shape).
const GOLDEN_STRATEGY: &str = "tests/fixtures/strategies/rsi-oversold-long.json";

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// A temp environment: a db path and a WRITABLE copy of the candle fixture.
struct Env {
    _tmp: TempDir,
    db_path: PathBuf,
    store_root: PathBuf,
}

fn env() -> Env {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("pulse.db");
    let store_root = tmp.path().join("store");
    copy_tree(&manifest(FIXTURE_STORE), &store_root);
    Env {
        _tmp: tmp,
        db_path,
        store_root,
    }
}

impl Env {
    fn store(&self) -> CandleStore {
        CandleStore::with_base_dir(self.store_root.clone())
    }

    async fn cold_state(&self) -> DesktopState {
        DesktopState::open_with_store(&self.db_path, self.store())
            .await
            .expect("open desktop state over the temp db + fixture store")
    }
}

fn pair() -> Pair {
    Pair::new("BTCUSDT")
}

/// The M15 snapshot identity a run records — the fixture's own HEAD.
fn m15_head(store: &CandleStore) -> pulse::DataVersion {
    store
        .read_head(&pair(), Timeframe::M15)
        .expect("the fixture HEAD reads")
        .expect("the fixture has an M15 HEAD")
}

/// The fixture's M15 series, so H and the trade times are real candle instants.
fn m15_series(store: &CandleStore) -> Vec<pulse::Candle> {
    store
        .load_version(&pair(), Timeframe::M15, &m15_head(store))
        .expect("the fixture reads")
        .series
        .candles
}

/// Seed one strategy plus a real, compilable version from the golden DSL, on any
/// state — the temp-env state and the spawned server's test-side state alike.
async fn seed_version(state: &DesktopState) -> VersionId {
    let repo = state.strategy_repo();
    let strategy = repo
        .create_strategy("patch 417", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create the strategy");
    let dsl = std::fs::read_to_string(manifest(GOLDEN_STRATEGY)).expect("read the golden strategy");
    repo.create_version(NewVersion {
        strategy_id: StrategyId::new(strategy.id.as_str().to_owned()),
        parent_version_id: None,
        dsl_json: dsl,
        created_by: CreatedBy::Human,
        creating_llm_call_ids: vec![],
    })
    .await
    .expect("create the version")
    .id
}

/// One closed long trade entering at `entry_ms` and exiting one M15 bar later,
/// `r` R above a 90 000 entry with its stop 1 000 below (the `chart_read` shape).
///
/// **The SIGNAL times deliberately differ from the FILL times** (`#417`): an exit
/// signals on the bar it is decided and fills at the NEXT bar's open, which is
/// the engine's own rule. The fence is a fill-time rule, so a fence wrongly
/// spelled on signal times would let the straddling trade through — with equal
/// times these arms would pass either way and would pin nothing.
fn trade_at(entry_ms: i64, r: &str) -> Trade {
    let entry = Decimal::new(90_000, 0);
    let stop = Decimal::new(89_000, 0);
    let r = r.parse::<Decimal>().expect("a decimal R");
    let exit = entry + r * (entry - stop);
    let qty = Decimal::ONE;
    let exit_signal_ms = entry_ms + Timeframe::M15.duration_ms();
    let exit_fill_ms = exit_signal_ms + Timeframe::M15.duration_ms();
    let fees = Decimal::new(2, 2);
    Trade {
        direction: Direction::Long,
        qty,
        entry_price: entry,
        exit_price: exit,
        entry_signal_time: entry_ms - Timeframe::M15.duration_ms(),
        entry_fill_time: entry_ms,
        exit_signal_time: exit_signal_ms,
        exit_fill_time: exit_fill_ms,
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
                time_ms: exit_fill_ms,
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

/// Save one run of `trades` against `version` on `state`'s pool — over the
/// fixture's REAL M15 snapshot identity, with no recorded window (the Lab's own
/// whole-snapshot shape, so the run's counted window is the whole snapshot and
/// reaches any H inside it).
async fn seed_run(
    state: &DesktopState,
    store: &CandleStore,
    version: &VersionId,
    trades: Vec<Trade>,
) -> BacktestRunId {
    seed_run_on(state, store, version, None, trades).await
}

/// The same, over a caller-chosen recorded window — the shape a windowed
/// backtest or a walk-forward fold persists (`inputs.window`).
async fn seed_run_on(
    state: &DesktopState,
    store: &CandleStore,
    version: &VersionId,
    window: Option<CandleWindow>,
    trades: Vec<Trade>,
) -> BacktestRunId {
    let inputs = BacktestInputs {
        pair: pair(),
        primary: SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: m15_head(store),
        },
        htf: None,
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
    SqliteBacktestRunRepo::new(state.db().pool().clone())
        .save_run(version, &inputs, &result, &summary, starting_equity)
        .await
        .expect("the seeded run persists")
}

/// Open one freeze on `state`'s pool, with the injected clock.
async fn open_freeze_on(state: &DesktopState, holdout_start_ms: i64) {
    SqliteCertificationFreezeRepo::with_deps(
        state.db().pool().clone(),
        FakeClock::at(1_760_000_000_000),
    )
    .open(&OpenFreezeRequest {
        holdout_start_ms,
        h: 12,
        alpha: "0.0463".to_owned(),
        holdout_test: "C1: z = z(1 - 0.05/H)".to_owned(),
    })
    .await
    .expect("open the freeze");
}

/// Every fact a withheld read must state, in one place: the count, the flag, H,
/// and the ABSENCE of each outcome — not a recomputation over the trades that
/// crossed. Shared by the core arm and the route arm so the two cannot drift.
#[track_caller]
fn assert_withheld(dto: &pulse::BacktestRunDto, holdout_start: i64, expected_count: u32) {
    assert!(
        dto.totals_withheld,
        "the run's counted window reaches H, so its totals are withheld"
    );
    assert_eq!(
        dto.withheld_trade_count, expected_count,
        "the read withheld exactly the trades that reach the holdout"
    );
    assert_eq!(
        dto.holdout_start_ms.as_deref(),
        Some(holdout_start.to_string().as_str()),
        "the read names the open freeze's holdout start"
    );
    assert!(
        dto.net_pnl.is_none()
            && dto.fees_total.is_none()
            && dto.funding_total.is_none()
            && dto.slippage_total.is_none()
            && dto.expectancy.is_none()
            && dto.win_rate.is_none()
            && dto.profit_factor.is_none()
            && dto.gross_profit.is_none()
            && dto.gross_loss.is_none()
            && dto.avg_win.is_none()
            && dto.avg_loss.is_none()
            && dto.max_drawdown.is_none(),
        "every headline aggregate is withheld"
    );
    assert!(
        dto.trade_count.is_none()
            && dto.win_count.is_none()
            && dto.loss_count.is_none()
            && dto.max_win_streak.is_none()
            && dto.max_loss_streak.is_none()
            && dto.skipped_sub_lot.is_none()
            && dto.skipped_sub_notional.is_none()
            && dto.skipped_leverage_capped.is_none(),
        "every counted aggregate is withheld"
    );
    assert!(
        dto.sharpe.is_none() && dto.sortino.is_none(),
        "the risk ratios are withheld"
    );
    assert!(
        dto.equity.is_empty() && dto.regimes.is_empty(),
        "no series is sent: the equity curve and the regimes are empty"
    );
    assert!(
        dto.mfe.is_none() && dto.mae.is_none(),
        "neither excursion histogram is sent"
    );
}

async fn read_run(state: &DesktopState, run_id: &BacktestRunId) -> pulse::BacktestRunDto {
    get_backtest_run_core(
        state,
        pulse::GetBacktestRunRequest {
            run_id: run_id.as_str().to_owned(),
        },
    )
    .await
    .expect("the run reads back")
}

// ---------------------------------------------------------------------------
// 1. the reported defect: an open freeze's holdout trades must not be sent
// ---------------------------------------------------------------------------

/// While a freeze is open, the app's run read lists a trade only when the Chart
/// tab's own rule holds — entry before H, exit before H. The straddling trade and
/// the trade inside the holdout are WITHHELD BY THE SERVER, not filtered by a
/// client: nothing in the answer names them.
///
/// This is the defect's reproduction. On the base commit all three trades come
/// back in full, so the assertions below fail on the FIRST one: the read listed a
/// trade whose exit landed inside the withheld period.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_freeze_keeps_holdout_trades_out_of_the_apps_run_read() {
    let env = env();
    let state = env.cold_state().await;
    let version = seed_version(&state).await;
    let series = m15_series(&env.store());
    let step = Timeframe::M15.duration_ms();
    // H: a real candle open inside the fixture's range.
    let holdout_start = series[200].open_time;

    // (a) wholly before the fence, (b) straddling it, (c) inside the holdout.
    // `trade_at` fills its exit one bar after its own signal, so (a) clears H on
    // its FILL time with two bars to spare — and its SIGNAL time is earlier still,
    // which is what makes this arm pin the fill-time rule (a signal-time fence
    // would pass (b) through).
    let before = trade_at(holdout_start - 3 * step, "2");
    let straddling = trade_at(holdout_start - step / 2, "-1");
    let inside = trade_at(holdout_start + 2 * step, "2");
    assert_eq!(
        before.exit_fill_time,
        holdout_start - step,
        "the pre-fence trade must FILL its exit before H"
    );
    assert!(
        before.exit_signal_time < holdout_start,
        "the pre-fence trade's signal is before H too"
    );
    assert!(
        straddling.entry_fill_time < holdout_start && straddling.exit_fill_time > holdout_start,
        "the straddling trade must enter before H and exit after it"
    );
    assert!(
        inside.entry_fill_time > holdout_start,
        "the inside trade must open after H"
    );

    let run_id = seed_run(
        &state,
        &env.store(),
        &version,
        vec![before.clone(), straddling.clone(), inside.clone()],
    )
    .await;
    open_freeze_on(&state, holdout_start).await;

    let dto = read_run(&state, &run_id).await;

    // (a) is listed, exactly as stored.
    assert_eq!(
        dto.trades.len(),
        1,
        "only the trade that ends before the holdout may cross; got {:?}",
        dto.trades
            .iter()
            .map(|t| (t.entry_signal_time.clone(), t.exit_signal_time.clone()))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        dto.trades[0].entry_fill_time,
        before.entry_fill_time.to_string()
    );
    assert_eq!(
        dto.trades[0].exit_fill_time,
        before.exit_fill_time.to_string()
    );

    // (b) and (c) are withheld WHOLE: no time they own appears anywhere.
    let withheld_times = [
        straddling.entry_fill_time.to_string(),
        straddling.exit_fill_time.to_string(),
        inside.entry_fill_time.to_string(),
        inside.exit_fill_time.to_string(),
    ];
    let answer = serde_json::to_string(&dto).expect("the DTO serializes");
    for time in &withheld_times {
        assert!(
            !answer.contains(time.as_str()),
            "a holdout trade's time `{time}` reached the app's run read"
        );
    }
    // The fence is the point, not a truncation: every listed FILL is before H.
    for trade in &dto.trades {
        assert!(
            trade.entry_fill_time.parse::<i64>().expect("a time") < holdout_start
                && trade.exit_fill_time.parse::<i64>().expect("a time") < holdout_start,
            "a listed trade reaches the holdout"
        );
    }
    // The read still names H, so a client knows where its fence is.
    assert_eq!(
        dto.holdout_start_ms.as_deref(),
        Some(holdout_start.to_string().as_str()),
        "the read names the open freeze's holdout start"
    );
    // The read says what it withheld (PDR-0038 step 3), and says it the same way
    // it will over the route: one helper, one set of assertions.
    assert_withheld(&dto, holdout_start, 2);
}

// ---------------------------------------------------------------------------
// 2. the fresh-run arm and the read agree at the final-bar boundary
// ---------------------------------------------------------------------------

/// The blocking finding from this patch's own review (finding 1), pinned.
///
/// The window guard clamps only when a bar OPENS at or after H. When H lands
/// strictly INSIDE the pinned snapshot's final bar, the run passes the guard, runs
/// to `SeriesEnd::SnapshotEnd`, and any position open at the end is booked at that
/// bar's `close_time` — which is at or after H. `run_read_fence` takes the window
/// over `[first_open, last_close]` and the trade test over FILL times, so BOTH the
/// fresh-run arm and the Lab's later read withhold that trade and the totals.
///
/// This arm builds the outcome in that exact shape and drives the one function
/// both arms call, so a regression in either shows up here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_final_bar_boundary_is_withheld_by_the_same_decision_both_arms_call() {
    let env = env();
    let state = env.cold_state().await;
    let version = seed_version(&state).await;
    let series = m15_series(&env.store());
    let last = series.last().expect("the fixture has candles");
    // H strictly inside the final bar: after its open, before its close.
    let holdout_start = last.open_time + 1;
    assert!(
        holdout_start > last.open_time && holdout_start < last.close_time,
        "H must sit inside the final bar for this boundary to exist"
    );

    // The end-of-data close: a trade that opens before H and FILLS at the final
    // bar's close, on or after H.
    let mut final_bar_trade = trade_at(last.open_time - Timeframe::M15.duration_ms(), "1");
    final_bar_trade.exit_fill_time = last.close_time;
    final_bar_trade.exit_signal_time = last.close_time;
    assert!(
        final_bar_trade.entry_fill_time < holdout_start
            && final_bar_trade.exit_fill_time >= holdout_start,
        "the final-bar trade must enter before H and fill at or after it"
    );

    // The stored run, read back through the app's own read.
    let run_id = seed_run(
        &state,
        &env.store(),
        &version,
        vec![final_bar_trade.clone()],
    )
    .await;
    open_freeze_on(&state, holdout_start).await;
    let read = read_run(&state, &run_id).await;

    assert_eq!(
        read.trades.len(),
        0,
        "the final-bar trade reaches H and must not be sent: {:?}",
        read.trades
    );
    assert_withheld(&read, holdout_start, 1);

    // The fresh-run arm's own decision, on the same outcome shape: the run the
    // engine just produced has `window: None` over this snapshot, so the fence it
    // takes must withhold here too.
    let stored = SqliteBacktestRunRepo::new(state.db().pool().clone())
        .get_run(&run_id)
        .await
        .expect("read the seeded run")
        .expect("the seeded run exists");
    let trades = SqliteBacktestRunRepo::new(state.db().pool().clone())
        .get_trades(&run_id)
        .await
        .expect("read the seeded trades");
    let inputs = stored
        .inputs
        .clone()
        .expect("the seeded run carries inputs");
    assert!(
        inputs.window.is_none(),
        "an unwindowed run is the shape whose end-of-data close reaches H"
    );
    let primary = env
        .store()
        .load_version(
            &inputs.pair,
            inputs.primary.timeframe,
            &inputs.primary.data_version,
        )
        .expect("reload the primary snapshot")
        .series;
    let outcome = BacktestOutcome {
        run: stored,
        inputs,
        trades,
        primary,
        htf: None,
        d1: None,
        fingerprint_warning: None,
        mfe: project_histogram(std::iter::empty()),
        mae: project_histogram(std::iter::empty()),
    };
    let freeze = pulse::HoldoutFreeze {
        holdout_start_ms: holdout_start,
    };
    let fence =
        run_read_fence(&outcome, Some(freeze)).expect("a freeze is open, so the fence exists");
    assert!(
        fence.withhold_totals,
        "the fresh-run arm must withhold: its window reaches H through the final bar"
    );
    assert!(
        !pulse::trade_clears_fence(&final_bar_trade, Some(fence)),
        "the fresh-run arm must not send the final-bar trade"
    );

    // And the two arms agree, which is the whole point: the fresh-run projection
    // and the stored read withhold the same things for the same run.
    let fresh = pulse::backtest_run_dto(&outcome, true, Some(freeze), Some(fence))
        .expect("the projection succeeds");
    assert!(
        fresh.totals_withheld && fresh.trades.is_empty(),
        "the fresh-run DTO withholds the totals and the trade: {fresh:?}"
    );
    assert_eq!(fresh.withheld_trade_count, 1);
    assert_eq!(read.withheld_trade_count, fresh.withheld_trade_count);
    assert_eq!(read.totals_withheld, fresh.totals_withheld);
}

// ---------------------------------------------------------------------------
// 3. the route edge carries the same fence
// ---------------------------------------------------------------------------

/// The HTTP edge is not a second implementation: `POST /api/v1/get-backtest-run`
/// answers the core's own document. This arm drives the route over a spawned
/// server and asserts the WITHHELD shape crosses the wire — a client cannot get
/// the rows back by asking the route instead of the bus.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_answers_the_same_withheld_read() {
    use support::server::{ServerOptions, spawn_server};

    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let store = CandleStore::with_base_dir(server.data_dir.clone());
    let series = m15_series(&store);
    let step = Timeframe::M15.duration_ms();
    let holdout_start = series[200].open_time;
    let trades = vec![
        // The exit FILLS a bar after it signals, so the safe trade needs three
        // bars of clearance to end before H.
        trade_at(holdout_start - 3 * step, "2"),
        trade_at(holdout_start + 2 * step, "2"),
    ];
    let run_id = seed_run(&state, &store, &version, trades).await;

    // Open the freeze on the SAME pool the server reads.
    open_freeze_on(&state, holdout_start).await;

    let response = reqwest::Client::new()
        .post(format!("{}/api/v1/get-backtest-run", server.base))
        .header("Authorization", format!("Bearer {}", server.app_token))
        .json(&serde_json::json!({ "runId": run_id.as_str() }))
        .send()
        .await
        .expect("the route answers");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = response.json().await.expect("a JSON body");

    assert_eq!(body["withheldTradeCount"], serde_json::json!(1), "{body}");
    assert_eq!(body["totalsWithheld"], serde_json::json!(true), "{body}");
    assert_eq!(
        body["holdoutStartMs"],
        serde_json::json!(holdout_start.to_string()),
        "{body}"
    );
    assert_eq!(
        body["trades"].as_array().expect("trades").len(),
        1,
        "only the trade before the holdout crosses the wire: {body}"
    );
    assert!(body["netPnl"].is_null(), "the totals are withheld: {body}");
    assert!(
        body["equity"].as_array().expect("equity").is_empty(),
        "no curve crosses: {body}"
    );
    // The holdout trade's own time is nowhere in the served document.
    let text = body.to_string();
    assert!(
        !text.contains(&(holdout_start + 2 * step).to_string()),
        "a holdout trade's time reached the route's answer"
    );

    // The route's own document is the core's OWN document, field for field —
    // the same proof `server_routes.rs` makes for the unfenced read.
    let core = read_run(&state, &run_id).await;
    assert_eq!(
        body,
        serde_json::to_value(&core).expect("the DTO serializes"),
        "the route equals the core's own result on the same state"
    );
    assert_withheld(&core, holdout_start, 1);
}

// ---------------------------------------------------------------------------
// 4. nothing else moves
// ---------------------------------------------------------------------------

/// A run READ WHILE NO FREEZE IS OPEN is unchanged: the same run read twice
/// answers the same document, every field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_read_with_no_freeze_open_is_unchanged() {
    let env = env();
    let state = env.cold_state().await;
    let version = seed_version(&state).await;
    let series = m15_series(&env.store());
    let step = Timeframe::M15.duration_ms();
    let holdout_start = series[200].open_time;
    let trades = vec![
        trade_at(holdout_start - 4 * step, "2"),
        trade_at(holdout_start + 2 * step, "1"),
    ];
    let run_id = seed_run(&state, &env.store(), &version, trades).await;

    let first = read_run(&state, &run_id).await;
    let second = read_run(&state, &run_id).await;

    assert_eq!(
        first, second,
        "two reads with no freeze open answer the same document"
    );
    assert_eq!(first.trades.len(), 2, "every stored trade is listed");
    assert!(
        first.holdout_start_ms.is_none(),
        "no freeze is open, so the boundary is null"
    );
    assert!(
        !first.totals_withheld,
        "no freeze is open, so no total is withheld"
    );
    assert_eq!(
        first.withheld_trade_count, 0,
        "no freeze is open, so no trade is withheld"
    );
    assert!(
        first.net_pnl.is_some(),
        "the stored net P&L crosses as before"
    );
}

/// A run whose counted window ENDS BEFORE H reads exactly as it does with no
/// freeze open — the fence owes an honest read of a run that never reached it.
/// The one field that moves is `holdoutStartMs`, which the ruling keeps as the
/// fence THIS read saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_wholly_before_the_holdout_reads_exactly_as_before() {
    let env = env();
    let state = env.cold_state().await;
    let version = seed_version(&state).await;
    let series = m15_series(&env.store());
    let holdout_start = series[200].open_time;
    // The run's recorded window ends a bar before H: its counted window cannot
    // reach the holdout, whatever the snapshot holds past it.
    let window = CandleWindow::new(
        series[100].open_time,
        holdout_start - Timeframe::M15.duration_ms(),
    )
    .expect("a legal window");
    let trades = vec![
        trade_at(series[150].open_time, "2"),
        trade_at(series[160].open_time, "-1"),
    ];
    let run_id = seed_run_on(&state, &env.store(), &version, Some(window), trades).await;

    let plain = read_run(&state, &run_id).await;
    open_freeze_on(&state, holdout_start).await;
    let fenced = read_run(&state, &run_id).await;

    assert!(
        plain.holdout_start_ms.is_none(),
        "the first read saw no freeze"
    );
    assert_eq!(
        fenced.holdout_start_ms.as_deref(),
        Some(holdout_start.to_string().as_str()),
        "the fenced read names the boundary it saw"
    );

    // Everything else is byte-identical: compare the two documents with only the
    // boundary field normalized.
    let mut a = serde_json::to_value(&plain).expect("serialize");
    let mut b = serde_json::to_value(&fenced).expect("serialize");
    a["holdoutStartMs"] = serde_json::Value::Null;
    b["holdoutStartMs"] = serde_json::Value::Null;
    assert_eq!(
        a, b,
        "a run wholly before the holdout reads exactly as it did with no freeze"
    );
}
