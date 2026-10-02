//! r3.s4.w2 — AC-9: the recorded bars (`paper_recorded_bars`, audit #2 as
//! corrected).
//!
//! A paper session's consumed candles live in `paper_bar` — one row per bar
//! per timeframe, the warm-up flagged `lead_in` so `count_from_ms` is just
//! the first non-lead-in `open_time`. This suite proves the round trip: bars
//! appended over SEVERAL `append_bar` calls read back in order;
//! `materialise` turns the rows into a content-addressed `CandleSeries` that
//! the REAL engine (`run_backtest`, snapshots pinned, never HEAD) runs
//! identically to the same candles built in memory; two materialisations
//! agree on the `data_version`; the row count is exactly one per appended
//! bar (lead-in included); and BTCUSDT HEAD stays byte-absent throughout.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BacktestConfig, BinanceAdapter, Candle, CandleSeries, CandleStore, CompiledStrategy,
    ExchangeAdapter, Migrator, Pair, PaperSessionRepository, SnapshotSelection,
    SqlitePaperSessionRepo, StrategyDsl, Timeframe, compile, fixture_strategy_dsl, validate,
};
use rust_decimal::Decimal;
use support::mcp::migrated_db;
use tempfile::TempDir;

const PAIR: &str = "BTCUSDT";

// ---------------------------------------------------------------------------
// World + candles
// ---------------------------------------------------------------------------

struct World {
    _tmp: TempDir,
    candles_tmp: TempDir,
    db: pulse::Db,
    paper: SqlitePaperSessionRepo<pulse::FakeClock>,
    store: CandleStore,
}

async fn world() -> World {
    let tmp = TempDir::new().unwrap();
    let (_path, db) = migrated_db(&tmp).await;
    let pool = db.pool().clone();
    // The session's row (a certified BTCUSDT M15 session; the FKs + CHECK need
    // the strategy/version/run seeds).
    sqlx::query("INSERT INTO strategy (id, name, created_at) VALUES ('st-1', 'recorded', '2026-01-01T00:00:00.000Z')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO strategy_version (id, strategy_id, dsl_schema_version, dsl, dsl_original, \
         version_hash, created_by, creating_llm_call_ids, created_at) \
         VALUES ('ver-1', 'st-1', '1.2.0', '{}', '{}', 'hash', 'human', '[]', '2026-01-01T00:00:00.000Z')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO walk_forward_run (id, seq, strategy_version_id, created_at, scheme, rule, k, \
         span_from_ms, span_to_ms, from_defaulted, engine_fingerprint, folds_holding, \
         folds_required, pooled_n, pooled_mean_r, pooled_lower_bound, pass) \
         VALUES ('run-1', 1, 'ver-1', '2026-01-01T00:00:00.000Z', 'rolling-oos/v1', 'wf-v1', 6, \
                 0, 1, 0, 'fp', 6, 4, 100, '0.5', 0.1, 1)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO paper_session \
         (id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
          htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
          engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
          override_at, certified_data_versions, fixture, min_trades, promoted_by) \
         VALUES ('sess-1', 1, 'ver-1', '2026-01-01T00:00:00.000Z', 'BTCUSDT', '15m', NULL, 0, \
                 '10000', '4', '1', 'fp', 'certified', 'run-1', NULL, NULL, \
                 '[{\"timeframe\":\"15m\",\"data_version\":\"a\"}]', 1, 20, 'operator-token')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let candles_tmp = TempDir::new().unwrap();
    let store = CandleStore::with_base_dir(candles_tmp.path().to_path_buf());
    World {
        _tmp: tmp,
        candles_tmp,
        db,
        paper: SqlitePaperSessionRepo::with_clock(pool, pulse::FakeClock::at(1_767_225_600_000)),
        store,
    }
}

impl World {
    /// The candle store's base dir (where HEAD would live, if it ever did).
    fn store_base(&self) -> &Path {
        self.candles_tmp.path()
    }
}

/// One 15-minute candle on the real fixture grid: `open_time` is the bar's
/// open, funding rides every 8-hour boundary (the engine's cadence check).
fn candle(open_time: i64, open: i64, close: i64) -> Candle {
    Candle {
        open_time,
        close_time: open_time + 899_999,
        open: Decimal::from(open),
        high: Decimal::from(open.max(close) + 20),
        low: Decimal::from(open.min(close) - 20),
        close: Decimal::from(close),
        volume: Decimal::from(100),
        funding_rate: (open_time % 28_800_000 == 0).then(|| Decimal::new(1, 5)),
    }
}

/// The recorded bars: TWO lead-in bars (23:30, 23:45 on 2025-01-01) and THREE
/// counted bars (00:00 — an 8-hour boundary, so it carries a funding stamp —
/// 00:15, 00:30). The prices dip below the fixture strategy's entry
/// threshold (`60_000 − 300 = 59_700`) and revert, so the run trades.
fn recorded_candles() -> Vec<Candle> {
    let day = 1_735_689_600_000_i64; // 2025-01-01T00:00:00Z
    let m15 = 900_000_i64;
    vec![
        candle(day - 2 * m15, 60_100, 60_050),
        candle(day - m15, 60_050, 60_000),
        candle(day, 59_500, 59_600), // the dip: below the entry threshold
        candle(day + m15, 59_600, 59_850),
        candle(day + 2 * m15, 59_850, 60_050), // the reversion past the exit
    ]
}

/// The FIRST COUNTED bar's `open_time` — what `count_from_ms` must be.
fn first_counted_open_time() -> i64 {
    1_735_689_600_000
}

fn compiled() -> CompiledStrategy {
    let dsl: StrategyDsl = fixture_strategy_dsl();
    let json = serde_json::to_string(&dsl).unwrap();
    let loaded = Migrator::v1().load(&json).unwrap();
    let validated = validate(&loaded.dsl).unwrap();
    compile(&validated).unwrap()
}

async fn append_recorded_bars(world: &World) {
    let session_id = pulse::PaperSessionId::new("sess-1".to_owned());
    let all = recorded_candles();
    // THREE append_bar calls — the bars land over several batches, lead-in
    // flagged on the first two.
    world
        .paper
        .append_bar(
            &session_id,
            &[
                (Timeframe::M15, all[0].clone(), true),
                (Timeframe::M15, all[1].clone(), true),
            ],
            &[],
        )
        .await
        .unwrap();
    world
        .paper
        .append_bar(
            &session_id,
            &[
                (Timeframe::M15, all[2].clone(), false),
                (Timeframe::M15, all[3].clone(), false),
            ],
            &[],
        )
        .await
        .unwrap();
    world
        .paper
        .append_bar(&session_id, &[(Timeframe::M15, all[4].clone(), false)], &[])
        .await
        .unwrap();
}

/// HEAD for the pair, per timeframe, as bytes (`None` = absent).
fn head_bytes(candles: &Path, tf: &str) -> Option<Vec<u8>> {
    std::fs::read(candles.join(format!("{PAIR}/{tf}/HEAD"))).ok()
}

use std::path::Path;

// ---------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------

/// Bars appended over several `append_bar` calls read back in order; the row
/// count is exactly one per appended bar, lead-in included; and BTCUSDT HEAD
/// stays byte-absent before and after.
#[tokio::test]
async fn recorded_bars_read_back_in_order_one_row_per_bar() {
    let world = world().await;
    let session_id = pulse::PaperSessionId::new("sess-1".to_owned());

    assert!(head_bytes(world.store_base(), "M15").is_none());
    append_recorded_bars(&world).await;

    let bars = world
        .paper
        .bars(&session_id, Timeframe::M15)
        .await
        .expect("the bars read back");
    let all = recorded_candles();
    assert_eq!(
        bars, all,
        "the read-back is the recorded sequence, in order"
    );

    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM paper_bar WHERE session_id = 'sess-1'")
            .fetch_one(world.db.pool())
            .await
            .unwrap();
    assert_eq!(
        usize::try_from(rows).unwrap(),
        all.len(),
        "one row per bar, lead-in included"
    );

    // The stored `lead_in` flags read back with the rows: the first two bars
    // are warm-up, the rest counted — and the first non-lead-in `open_time`
    // IS the `count_from_ms` the shadow runs with. w3 reads the column; no
    // marker event to find.
    let day = 1_735_689_600_000_i64;
    let m15 = 900_000_i64;
    let flags: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT open_time, lead_in FROM paper_bar \
             WHERE session_id = 'sess-1' ORDER BY open_time",
    )
    .fetch_all(world.db.pool())
    .await
    .unwrap();
    assert_eq!(
        flags,
        vec![
            (day - 2 * m15, 1),
            (day - m15, 1),
            (day, 0),
            (day + m15, 0),
            (day + 2 * m15, 0),
        ],
        "the warm-up bars are flagged, the counted bars are not"
    );
    let derived_count_from = flags
        .iter()
        .find(|(_, lead_in)| *lead_in == 0)
        .map(|(open_time, _)| *open_time)
        .expect("a counted bar exists");
    assert_eq!(
        derived_count_from,
        first_counted_open_time(),
        "the column derives count_from_ms"
    );

    // HEAD never appears — materialisation writes snapshots, never HEAD.
    assert!(head_bytes(world.store_base(), "M15").is_none());
    assert!(head_bytes(world.store_base(), "H4").is_none());
}

/// The materialised series runs the REAL engine with the snapshots pinned,
/// `count_from_ms` = the first non-lead-in `open_time`, and its result equals a
/// run over the same candles built in memory. Two materialisations give the
/// same `data_version`.
#[tokio::test]
async fn materialised_series_run_the_engine_like_the_in_memory_series() {
    let world = world().await;
    let session_id = pulse::PaperSessionId::new("sess-1".to_owned());
    append_recorded_bars(&world).await;

    // Materialise twice — same rows, same data_version (content-addressed).
    let first = world
        .paper
        .materialise(&session_id, &world.store)
        .await
        .expect("the first materialisation");
    let second = world
        .paper
        .materialise(&session_id, &world.store)
        .await
        .expect("the second materialisation");
    assert_eq!(first, second, "two materialisations of the same rows agree");
    assert_eq!(first.len(), 1, "only the recorded timeframe materialises");
    let SnapshotSelection {
        timeframe,
        data_version,
    } = &first[0];
    assert_eq!(*timeframe, Timeframe::M15);

    // The pinned read: the materialised snapshot loads by ITS version, never
    // HEAD.
    let pinned = world
        .store
        .read_snapshot(&Pair::new(PAIR), Timeframe::M15, data_version)
        .expect("the materialised snapshot reads back");
    assert_eq!(pinned.candles, recorded_candles());

    // The real engine over the pinned series, counting from the first
    // non-lead-in open_time.
    let config = BacktestConfig::default();
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new(PAIR))
        .unwrap();
    let counted = run_engine(&pinned, &config, &filters);

    // The same candles built in memory give the SAME result.
    let in_memory = CandleSeries {
        pair: Pair::new(PAIR),
        timeframe: Timeframe::M15,
        version: data_version.clone(),
        candles: recorded_candles(),
    };
    let counted_in_memory = run_engine(&in_memory, &config, &filters);
    assert_eq!(
        counted, counted_in_memory,
        "the materialised run equals the in-memory run"
    );
    assert!(
        !counted.trades.is_empty(),
        "the dip-and-reversion candles trade at least once"
    );

    // HEAD is STILL absent after the materialisations.
    assert!(head_bytes(world.store_base(), "M15").is_none());
    assert!(head_bytes(world.store_base(), "H4").is_none());
}

/// A higher-timeframe candle on the same day grid (4h open/close, funding on
/// the 8h boundary) — the shape an HTF session records.
fn h4_candle(open_time: i64, open: i64, close: i64) -> Candle {
    Candle {
        open_time,
        close_time: open_time + 4 * 3_600_000 - 1,
        open: Decimal::from(open),
        high: Decimal::from(open.max(close) + 20),
        low: Decimal::from(open.min(close) - 20),
        close: Decimal::from(close),
        volume: Decimal::from(100),
        funding_rate: (open_time % 28_800_000 == 0).then(|| Decimal::new(1, 5)),
    }
}

/// A daily candle — the fixed `d1` series a `uses_d1` session records.
fn d1_candle(open_time: i64) -> Candle {
    Candle {
        open_time,
        close_time: open_time + 86_400_000 - 1,
        open: Decimal::from(60_000),
        high: Decimal::from(60_100),
        low: Decimal::from(59_900),
        close: Decimal::from(60_050),
        volume: Decimal::from(100),
        funding_rate: None,
    }
}

/// Materialise covers EVERY recorded timeframe — M15, the HTF H4 rows and
/// the D1 rows a multi-timeframe session records — each version stable
/// across repeat materialisations, each snapshot reading back by ITS version
/// with the recorded candles, and HEAD absent for all three timeframes
/// before and after (D1 included: nothing in the item ever writes HEAD).
#[tokio::test]
async fn materialise_covers_every_recorded_timeframe_and_never_touches_head() {
    let world = world().await;
    let session_id = pulse::PaperSessionId::new("sess-1".to_owned());
    let day = 1_735_689_600_000_i64;
    let h4 = vec![
        h4_candle(day, 60_100, 60_050),
        h4_candle(day + 4 * 3_600_000, 60_050, 59_500),
    ];
    let d1 = vec![d1_candle(day)];

    append_recorded_bars(&world).await;
    // The HTF and D1 rows ride their own bar-only appends.
    world
        .paper
        .append_bar(
            &session_id,
            &[
                (Timeframe::H4, h4[0].clone(), true),
                (Timeframe::H4, h4[1].clone(), false),
            ],
            &[],
        )
        .await
        .unwrap();
    world
        .paper
        .append_bar(&session_id, &[(Timeframe::D1, d1[0].clone(), false)], &[])
        .await
        .unwrap();

    for timeframe in ["M15", "H4", "D1"] {
        assert!(
            head_bytes(world.store_base(), timeframe).is_none(),
            "{timeframe} HEAD absent before materialising"
        );
    }

    let first = world
        .paper
        .materialise(&session_id, &world.store)
        .await
        .expect("the first materialisation");
    let second = world
        .paper
        .materialise(&session_id, &world.store)
        .await
        .expect("the second materialisation");
    assert_eq!(
        first, second,
        "same rows, same versions — content-addressed"
    );
    let timeframes: Vec<Timeframe> = first.iter().map(|s| s.timeframe).collect();
    assert_eq!(
        timeframes,
        vec![Timeframe::M15, Timeframe::H4, Timeframe::D1],
        "every recorded timeframe materialises, in the fixed order"
    );

    // Each snapshot reads back by ITS version with exactly the recorded
    // candles for that timeframe.
    for selection in &first {
        let series = world
            .store
            .read_snapshot(
                &Pair::new(PAIR),
                selection.timeframe,
                &selection.data_version,
            )
            .expect("the materialised snapshot reads back");
        let expected = match selection.timeframe {
            Timeframe::M15 => recorded_candles(),
            Timeframe::H4 => h4.clone(),
            Timeframe::D1 => d1.clone(),
        };
        assert_eq!(
            series.candles, expected,
            "{:?} snapshot is the recorded candles",
            selection.timeframe
        );
    }

    // HEAD is STILL absent everywhere — the D1 materialisation included.
    for timeframe in ["M15", "H4", "D1"] {
        assert!(
            head_bytes(world.store_base(), timeframe).is_none(),
            "{timeframe} HEAD absent after materialising"
        );
    }
}

fn run_engine(
    series: &CandleSeries,
    config: &BacktestConfig,
    filters: &pulse::SymbolFilters,
) -> pulse::BacktestResult {
    pulse::run_backtest(
        &compiled(),
        series,
        None,
        None,
        config,
        filters,
        pulse::SeriesEnd::SnapshotEnd,
        Some(first_counted_open_time()),
    )
    .expect("the engine runs over the recorded candles")
}
