//! r6.s3.w4 — `loss_context`: a stored run's trades, losses and losing streaks by
//! context at entry (PDR-0046 decision 4; STR-0003: "They describe; they do not
//! decide").
//!
//! The world: a migrated temp `pulse.db`, a copy of the SOLUSDT fixture store
//! (`tests/fixtures/solusdt-cipher-store`, 15m + 4h) and one REAL run of Cipher v1
//! or v2 saved by the application use case the product calls, so every run records
//! the fixture's real snapshot identities. The read rebuilds each trade's context
//! from those recorded snapshots and writes nothing.
//!
//! 1. `v1_and_v2_loss_context_over_solusdt` — both documents, every grouping sums
//!    to the run, v2's trend at the ARM bar is with the side or unknown, goldens;
//! 2. `loss_context_over_mcp` — the tool returns the report, and refuses an
//!    unknown run id;
//! 3. `holdout_guarded` — the tool refuses exactly as `get_run` does;
//! 4. `no_hash_moves` — the read leaves the run's `result_content_hash` as it was.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::path::PathBuf;

use pulse::{
    BacktestConfig, BacktestRequest, BacktestRunId, BacktestRunRepository, BinanceAdapter,
    CandleSeries, CandleSeriesRepository, CandleStore, CreatedBy, Db, FakeClock, OpenFreezeRequest,
    Pair, PersistedRun, SqliteBacktestRunRepo, SqliteCertificationFreezeRepo, SqliteStrategyRepo,
    StrategyId, StrategyRepository, Timeframe, Trade, TrendSide, VersionId, loss_context_for_run,
    run_version_backtest, swing_trend_at,
};
use serde_json::{Value, json};
use support::mcp::{call, call_err, copy_tree, manifest, migrated_db, spawn_client};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// The world
// ---------------------------------------------------------------------------

fn v1_json() -> String {
    std::fs::read_to_string(manifest("examples/strategies/cipher-v1.json"))
        .expect("read the checked-in v1 document")
}

fn v2_json() -> String {
    std::fs::read_to_string(manifest("examples/strategies/cipher-v2.json"))
        .expect("read the checked-in v2 document")
}

/// A migrated temp DB and a copy of the SOLUSDT fixture store.
struct World {
    _tmp: TempDir,
    db: Db,
    db_path: PathBuf,
    store_dir: PathBuf,
}

async fn world() -> World {
    let tmp = TempDir::new().unwrap();
    let (db_path, db) = migrated_db(&tmp).await;
    let store_dir = tmp.path().join("store");
    copy_tree(&manifest("tests/fixtures/solusdt-cipher-store"), &store_dir);
    World {
        _tmp: tmp,
        db,
        db_path,
        store_dir,
    }
}

impl World {
    fn store(&self) -> CandleStore {
        CandleStore::with_base_dir(self.store_dir.clone())
    }

    fn runs(&self) -> SqliteBacktestRunRepo<pulse::SystemClock> {
        SqliteBacktestRunRepo::new(self.db.pool().clone())
    }

    /// Store `dsl_json` as a one-version strategy.
    async fn version(&self, name: &str, dsl_json: String) -> VersionId {
        let strategies = SqliteStrategyRepo::new(self.db.pool().clone());
        let strategy = strategies
            .create_strategy(name, Some("alice"), &["sol".to_owned()])
            .await
            .expect("create the strategy");
        strategies
            .create_version(pulse::NewVersion {
                strategy_id: StrategyId::new(strategy.id.as_str().to_owned()),
                parent_version_id: None,
                dsl_json,
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await
            .expect("create the version")
            .id
    }

    /// One REAL unwindowed SOLUSDT run (15m primary, 4h htf) of `version`.
    async fn run(&self, version: &VersionId) -> BacktestRunId {
        let strategies = SqliteStrategyRepo::new(self.db.pool().clone());
        let outcome = run_version_backtest(
            &strategies,
            &self.store(),
            &BinanceAdapter::new(),
            &self.runs(),
            &BacktestRequest {
                version_id: version.clone(),
                pair: Pair::new("SOLUSDT"),
                primary_timeframe: Timeframe::M15,
                htf_timeframe: Some(Timeframe::H4),
                config: BacktestConfig::default(),
                snapshots: None,
                window: None,
            },
            None,
        )
        .await
        .expect("the run completes over the SOLUSDT fixture");
        outcome.run.id
    }

    /// The persisted run and its trades, read back as the tool reads them.
    async fn persisted(&self, id: &BacktestRunId) -> (PersistedRun, Vec<Trade>) {
        let runs = self.runs();
        let run = runs
            .get_run(id)
            .await
            .expect("read the run")
            .expect("the run exists");
        let trades = runs.get_trades(id).await.expect("read the trades");
        (run, trades)
    }
}

/// The 4h HEAD of the fixture store.
fn h4_series(store: &CandleStore) -> CandleSeries {
    store
        .load_head(&Pair::new("SOLUSDT"), Timeframe::H4)
        .expect("read the 4h HEAD")
        .expect("the fixture has a 4h snapshot")
        .series
}

/// A loss is `realized_pnl < 0` (the `SummaryStats` convention).
fn loss_count(trades: &[Trade]) -> usize {
    trades
        .iter()
        .filter(|trade| trade.realized_pnl < rust_decimal::Decimal::ZERO)
        .count()
}

/// Every grouping's cells sum to the report's trade and loss counts.
fn assert_groupings_sum(report: &Value, trades: usize, losses: usize) {
    assert_eq!(report["trades"], trades, "the report's trade count");
    assert_eq!(report["losses"], losses, "the report's loss count");
    for key in ["by_trend", "by_regime", "by_volatility", "by_hour"] {
        let cells = report[key].as_array().unwrap_or_else(|| panic!("{key}"));
        let sum = |field: &str| -> u64 {
            cells
                .iter()
                .map(|pair| pair[1][field].as_u64().expect(field))
                .sum()
        };
        assert_eq!(sum("trades"), trades as u64, "{key}: trades sum");
        assert_eq!(sum("losses"), losses as u64, "{key}: losses sum");
    }
}

/// Golden for the Cipher v1 report over the SOLUSDT fixture.
const V1_GOLDEN: &str = r#"
{
  "trades": 27,
  "losses": 22,
  "volatility_cuts": ["0.0034556103095326180786780121","0.0047596054496578690127077224"],
  "by_trend": [
    ["with",{"longest_losing_streak":2,"losing_streaks":1,"losses":6,"net_pnl":"5.7547092842593697","net_r":"0.0575672260593578474203088543","trades":8}],
    ["ranging",{"longest_losing_streak":5,"losing_streaks":2,"losses":9,"net_pnl":"-420.4828608674817128","net_r":"-4.2048986064454142118518612827","trades":11}],
    ["against",{"longest_losing_streak":7,"losing_streaks":2,"losses":7,"net_pnl":"-474.0449244782362716","net_r":"-4.7406109171093935474651090286","trades":8}],
    ["unknown",{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}]
  ],
  "by_regime": [
    ["trending_up",{"longest_losing_streak":5,"losing_streaks":1,"losses":4,"net_pnl":"417.3396876024126588","net_r":"4.1736208054045071815608733970","trades":7}],
    ["trending_down",{"longest_losing_streak":5,"losing_streaks":2,"losses":7,"net_pnl":"-665.9893848107431438","net_r":"-6.6600865813781308520941632664","trades":7}],
    ["ranging",{"longest_losing_streak":7,"losing_streaks":2,"losses":11,"net_pnl":"-640.1233788531281297","net_r":"-6.4014765215218262413633715876","trades":13}],
    ["unknown",{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}]
  ],
  "by_volatility": [
    ["low",{"longest_losing_streak":5,"losing_streaks":2,"losses":8,"net_pnl":"-628.2519290317972555","net_r":"-6.2826690738450624755636451662","trades":9}],
    ["mid",{"longest_losing_streak":7,"losing_streaks":1,"losses":6,"net_pnl":"313.5658899573079308","net_r":"3.1359181516243925581938174353","trades":9}],
    ["high",{"longest_losing_streak":5,"losing_streaks":2,"losses":8,"net_pnl":"-574.0870369869692900","net_r":"-5.7411913752747799945268337261","trades":9}],
    ["unknown",{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}]
  ],
  "by_hour": [
    [0,{"longest_losing_streak":0,"losing_streaks":0,"losses":4,"net_pnl":"-427.873931037751156","net_r":"-4.2789727818059125154323684650","trades":4}],
    [1,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [2,{"longest_losing_streak":7,"losing_streaks":1,"losses":2,"net_pnl":"72.0513785495805715","net_r":"0.7207020605940678880493543472","trades":3}],
    [3,{"longest_losing_streak":5,"losing_streaks":2,"losses":2,"net_pnl":"-213.9252258562140696","net_r":"-2.1393673878604862587439436607","trades":2}],
    [4,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [5,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"173.5984146476148672","net_r":"1.7361480994475348507577158661","trades":2}],
    [6,{"longest_losing_streak":2,"losing_streaks":1,"losses":1,"net_pnl":"-105.8383791864408","net_r":"-1.058574352184990933327853947","trades":1}],
    [7,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-106.7712312875202888","net_r":"-1.0677426299323401348928540937","trades":1}],
    [8,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [9,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-107.377984068984798","net_r":"-1.0739076191583504886695541514","trades":1}],
    [10,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [11,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [12,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-107.976223064318364","net_r":"-1.0798081272397470891061742045","trades":1}],
    [13,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-123.887613060738624","net_r":"-1.2388978499733736932088903837","trades":1}],
    [14,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [15,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [16,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-117.8454837900988692","net_r":"-1.1784686865579119086460032626","trades":1}],
    [17,{"longest_losing_streak":0,"losing_streaks":0,"losses":2,"net_pnl":"-121.2039252114686982","net_r":"-1.2120858287028004790790560446","trades":2}],
    [18,{"longest_losing_streak":0,"losing_streaks":0,"losses":2,"net_pnl":"58.5931595931566642","net_r":"0.5860552928531925493873094549","trades":3}],
    [19,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [20,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-111.0312286668301872","net_r":"-1.1103197631174587997205737985","trades":1}],
    [21,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"290.9882964298712864","net_r":"2.9100114052181118189322193027","trades":1}],
    [22,{"longest_losing_streak":2,"losing_streaks":1,"losses":1,"net_pnl":"-114.5348777840323452","net_r":"-1.1453606915386286293534139454","trades":1}],
    [23,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"174.2617777327161962","net_r":"1.7426465624636439111574255292","trades":2}]
  ]
}
"#;

/// Golden for the Cipher v2 report over the SOLUSDT fixture.
const V2_GOLDEN: &str = r#"
{
  "trades": 9,
  "losses": 6,
  "volatility_cuts": ["0.0036444085529854760623991393","0.0049018287644787644787644788"],
  "by_trend": [
    ["with",{"longest_losing_streak":3,"losing_streaks":2,"losses":6,"net_pnl":"5.7547092842593697","net_r":"0.0575672260593578474203088543","trades":8}],
    ["ranging",{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"287.4906667181560932","net_r":"2.8750953706160968747790598739","trades":1}],
    ["against",{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    ["unknown",{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}]
  ],
  "by_regime": [
    ["trending_up",{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"459.7827709372236906","net_r":"4.5980474384258348615469523930","trades":3}],
    ["trending_down",{"longest_losing_streak":2,"losing_streaks":1,"losses":2,"net_pnl":"-129.819924379947004","net_r":"-1.2982118661955092478313501705","trades":2}],
    ["ranging",{"longest_losing_streak":3,"losing_streaks":1,"losses":3,"net_pnl":"-36.7174705548612237","net_r":"-0.3671729755548708915162334943","trades":4}],
    ["unknown",{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}]
  ],
  "by_volatility": [
    ["low",{"longest_losing_streak":2,"losing_streaks":1,"losses":2,"net_pnl":"40.5248721308173109","net_r":"0.4052383676895432201549581094","trades":3}],
    ["mid",{"longest_losing_streak":0,"losing_streaks":0,"losses":2,"net_pnl":"182.099445006713046","net_r":"1.8212229736722643603137784904","trades":3}],
    ["high",{"longest_losing_streak":3,"losing_streaks":1,"losses":2,"net_pnl":"70.6210588648851060","net_r":"0.7062012553136471417306321284","trades":3}],
    ["unknown",{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}]
  ],
  "by_hour": [
    [0,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-108.945187087499796","net_r":"-1.0894699706754087788362063174","trades":1}],
    [1,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [2,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"180.1521372984097635","net_r":"1.8017211397475788415321942512","trades":2}],
    [3,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [4,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [5,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"287.4906667181560932","net_r":"2.8750953706160968747790598739","trades":1}],
    [6,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-105.8383791864408","net_r":"-1.058574352184990933327853947","trades":1}],
    [7,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [8,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [9,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [10,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [11,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [12,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [13,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [14,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [15,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [16,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [17,{"longest_losing_streak":0,"losing_streaks":0,"losses":1,"net_pnl":"-4.573062704952","net_r":"-0.0457345797977827627686205663","trades":1}],
    [18,{"longest_losing_streak":2,"losing_streaks":1,"losses":1,"net_pnl":"-125.246861674995004","net_r":"-1.2524772863977264850627296042","trades":1}],
    [19,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [20,{"longest_losing_streak":3,"losing_streaks":1,"losses":1,"net_pnl":"-111.0312286668301872","net_r":"-1.1103197631174587997205737985","trades":1}],
    [21,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [22,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"0","net_r":"0","trades":0}],
    [23,{"longest_losing_streak":0,"losing_streaks":0,"losses":0,"net_pnl":"281.2372913065673934","net_r":"2.8124220384851467656040988365","trades":1}]
  ]
}
"#;

// ---------------------------------------------------------------------------
// 1. v1 and v2 over SOLUSDT
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_and_v2_loss_context_over_solusdt() {
    let world = world().await;
    let store = world.store();
    let htf = h4_series(&store);

    let mut reports: Vec<Value> = Vec::new();
    for (label, dsl) in [("v1", v1_json()), ("v2", v2_json())] {
        let version = world.version(&format!("cipher {label}"), dsl).await;
        let id = world.run(&version).await;
        let (run, trades) = world.persisted(&id).await;
        assert!(!trades.is_empty(), "{label}: the run has trades");

        let read = loss_context_for_run(&store, None, &run, &trades)
            .unwrap_or_else(|error| panic!("{label}: {error}"));
        assert_eq!(read.run_id, id.as_str(), "{label}: the read names its run");
        assert_eq!(read.htf_timeframe.as_deref(), Some("4h"));
        assert_eq!(read.unmatched_signal_bars, 0, "{label}: every bar matches");

        let report = serde_json::to_value(&read.report).unwrap();
        let losses = loss_count(&trades);
        assert_groupings_sum(&report, trades.len(), losses);
        assert_eq!(report["by_hour"].as_array().unwrap().len(), 24);

        if label == "v2" {
            // v2 arms only with the trend: read at the ARM bar, a v2 trade's side
            // is with the 4h swings (or the swings are not yet known).
            let mut off_trend_at_entry = 0_usize;
            for trade in &trades {
                let arm = trade.setup.expect("a v2 trade carries its setup").arm_time;
                let at_arm = swing_trend_at(&htf, arm).relative_to(trade.direction);
                assert!(
                    matches!(at_arm, TrendSide::With | TrendSide::Unknown),
                    "v2 trade armed at {arm} reads {at_arm:?} at its arm bar ({:?})",
                    trade.direction
                );
                let at_entry =
                    swing_trend_at(&htf, trade.entry_signal_time).relative_to(trade.direction);
                off_trend_at_entry +=
                    usize::from(matches!(at_entry, TrendSide::Ranging | TrendSide::Against));
            }
            eprintln!(
                "v2: {off_trend_at_entry} of {} trades read ranging or against at the entry bar",
                trades.len()
            );
        }
        reports.push(report);
    }

    let goldens = [V1_GOLDEN, V2_GOLDEN];
    for ((label, report), golden) in ["v1", "v2"].iter().zip(&reports).zip(goldens) {
        let golden: Value = serde_json::from_str(golden).expect("the golden is JSON");
        assert_eq!(
            report,
            &golden,
            "{label}: the report drifted from its golden; observed:\n{}",
            serde_json::to_string(report).unwrap()
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Over MCP
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loss_context_over_mcp() {
    let world = world().await;
    let version = world.version("cipher v1", v1_json()).await;
    let id = world.run(&version).await;
    let (run, trades) = world.persisted(&id).await;
    let expected = loss_context_for_run(&world.store(), None, &run, &trades).unwrap();

    let client = spawn_client(&world.db_path, &world.store_dir).await;
    let out = call(&client, "loss_context", json!({"run_id": id.as_str()})).await;
    assert_eq!(out["run_id"], id.as_str());
    assert_eq!(out["htf_timeframe"], "4h");
    for grouping in ["by_trend", "by_regime", "by_volatility", "by_hour"] {
        assert!(
            out["report"][grouping].is_array(),
            "the report carries {grouping}: {out}"
        );
    }
    assert_eq!(
        out["report"],
        serde_json::to_value(&expected.report).unwrap(),
        "the tool returns the use case's report"
    );
    assert_groupings_sum(&out["report"], trades.len(), loss_count(&trades));

    let err = call_err(&client, "loss_context", json!({"run_id": "no-such-run"})).await;
    assert_eq!(err["field"], "run_id");
    assert_eq!(err["message"], "no such backtest run");
    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// 3. The holdout fence
// ---------------------------------------------------------------------------

/// A freeze whose holdout starts inside the fixture's range.
async fn open_freeze_inside(world: &World, at_ms: i64) {
    SqliteCertificationFreezeRepo::with_deps(
        world.db.pool().clone(),
        FakeClock::at(1_760_000_000_000),
    )
    .open(&OpenFreezeRequest {
        holdout_start_ms: at_ms,
        h: 12,
        alpha: "0.0463".to_owned(),
        holdout_test: "C1: z = z(1 - 0.05/H)".to_owned(),
    })
    .await
    .expect("open the freeze");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn holdout_guarded() {
    let world = world().await;
    let version = world.version("cipher v1", v1_json()).await;
    let id = world.run(&version).await;
    let (run, _) = world.persisted(&id).await;

    // The run is unwindowed, so it reaches every candle of its snapshot: a holdout
    // that starts at the snapshot's middle bar lies inside it.
    let primary = world
        .store()
        .load_head(&Pair::new("SOLUSDT"), Timeframe::M15)
        .unwrap()
        .unwrap()
        .series;
    let middle = primary.candles[primary.candles.len() / 2].open_time;
    assert!(run.inputs.as_ref().unwrap().window.is_none());

    let client = spawn_client(&world.db_path, &world.store_dir).await;
    let before = call(&client, "loss_context", json!({"run_id": id.as_str()})).await;

    open_freeze_inside(&world, middle).await;
    let refused = call_err(&client, "loss_context", json!({"run_id": id.as_str()})).await;
    let get_run = call_err(&client, "get_run", json!({"run_id": id.as_str()})).await;
    assert_eq!(refused["field"], "run_id");
    assert!(refused["message"].as_str().unwrap().contains("SOLUSDT"));
    assert_eq!(
        refused, get_run,
        "loss_context refuses exactly as get_run does"
    );

    // The fence lifts when the freeze closes, and the read is what it was.
    SqliteCertificationFreezeRepo::with_deps(
        world.db.pool().clone(),
        FakeClock::at(1_760_000_600_000),
    )
    .close()
    .await
    .unwrap();
    let after = call(&client, "loss_context", json!({"run_id": id.as_str()})).await;
    assert_eq!(after, before);
    client.cancel().await.unwrap();
}

// ---------------------------------------------------------------------------
// 4. The read writes nothing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_hash_moves() {
    let world = world().await;
    let version = world.version("cipher v2", v2_json()).await;
    let id = world.run(&version).await;
    let (run, trades) = world.persisted(&id).await;
    let hash_before = run.result_content_hash.clone();
    let head_before = world
        .store()
        .load_head(&Pair::new("SOLUSDT"), Timeframe::M15)
        .unwrap()
        .unwrap()
        .series
        .version;

    let first = loss_context_for_run(&world.store(), None, &run, &trades).unwrap();
    let second = loss_context_for_run(&world.store(), None, &run, &trades).unwrap();
    assert_eq!(first, second, "the read is deterministic");

    // Re-read through the repository, which re-derives the hash from the stored
    // rows and refuses on a mismatch (D4's tamper guard).
    let (again, again_trades) = world.persisted(&id).await;
    assert_eq!(again.result_content_hash, hash_before);
    assert_eq!(again_trades, trades);
    let head_after = world
        .store()
        .load_head(&Pair::new("SOLUSDT"), Timeframe::M15)
        .unwrap()
        .unwrap()
        .series
        .version;
    assert_eq!(head_after, head_before, "the read wrote no snapshot");
}
