//! DSL schema **1.2.0** — the r3.s2.w1 contract suite (spec §AC-1).
//!
//! One suite, five cases:
//!
//! 1. **Pre-1.2.0 documents load to CURRENT.** Every committed and byte-pinned
//!    1.0.0 / 1.1.0 document loads, `from` stays the authored version,
//!    `dsl_original` stays byte-verbatim, a migration step is recorded, and the
//!    persisted `version_hash` recomputes UNCHANGED from the schema version the
//!    row was written under (CURRENT at write time — ADR-0010's hash inputs),
//!    provably NOT from the authored version string.
//! 2. **1.2.0 is served; 1.3.0 and 2.0.0 are not.** The served schema's
//!    `schema_version` enum lists exactly `["1.0.0", "1.1.0", "1.2.0"]`; a
//!    same-major future minor and a new major are refused `FutureVersion` on
//!    both the migrator load path and the MCP write path.
//! 3. **Re-runs are frozen at the pre-bump values.** For each fixture document,
//!    a backtest on the committed 1-month fixture produces the SAME
//!    `result_content_hash` and the SAME full trade log the pre-bump engine
//!    produced (see the recipe below). The 1.1.0 → 1.2.0 step is an identity
//!    migration and MACD's new `output` field defaults to the historical line —
//!    so nothing observable may move.
//! 4. **MACD `output`.** `IndicatorSpec::Macd` carries an optional
//!    `output: "line" | "signal" | "histogram"` (default `line`, written
//!    always). A rule condition comparing the MACD line output to the MACD
//!    signal output (`macd.line > macd.signal`) compiles to two DISTINCT
//!    `IndicatorSpec::Macd` values and, through the engine's `from_specs`, two
//!    engine slots. Render spellings: `macd(12,26,9)`, `macd(12,26,9).signal`,
//!    `macd(12,26,9).histogram`.
//! 5. **Unknown fields are refused at every write, tolerated on read.** A
//!    document carrying `entry.typo_field` plus a top-level `name_of_thing` is
//!    refused with two `UnknownField` errors at those paths on the MCP write
//!    path and on both repository write preludes — and nothing is persisted —
//!    while the SAME document inserted directly as `dsl_original` (as a pre-r3
//!    database would hold it) loads, lists and backtests without error.
//!
//! ## Frozen pre-bump values — regeneration recipe
//!
//! The `FROZEN_*` constants were captured from the BASE engine (worktree
//! `/home/dev/projects/pluse-trader/pulse-trader/.worktrees/r3.s2.w1` at commit
//! `2adcf7fc072a20a9a290ba38a930e87f1c563f3e`, BEFORE any r3.s2.w1 source edit)
//! with:
//!
//! ```text
//! TMPDIR="$HOME/.cache/pulse-scratch/r3.s2.w1/tmp" \
//! CARGO_TARGET_DIR=/home/dev/projects/pluse-trader/pulse-trader/target \
//! cargo nextest run --test dsl_schema_1_2 emit_frozen_prebump -- --ignored --nocapture
//! ```
//!
//! and by copying each emitted `FROZEN[label] hash=… trades=…` line into the
//! constants. A silent engine-behaviour drift across the 1.1.0 → 1.2.0 bump
//! fails `pre_bump_runs_are_reproduced_exactly_after_the_bump` loudly; a
//! deliberate engine change is a reviewed re-capture diff.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use pulse::{
    BacktestConfig, BinanceAdapter, CandleSeries, CandleStore, Condition, CreatedBy, Db, Direction,
    ExchangeAdapter, ExitRule, IndicatorEngine, IndicatorSpec, MIGRATOR, MacdOutput, Migrator,
    NewVersion, Pair, RiskParams, SchemaVersion, SeriesEnd, SqliteStrategyRepo, StrategyDsl,
    StrategyRepository, SubmitRequest, SubmitTarget, SweepableValue, Timeframe, ValidationCode,
    ValueSource, compile, render, run_backtest, submit_agent_version, validate, version_hash,
};
use sqlx::SqlitePool;

fn dec(mantissa: i64, scale: u32) -> rust_decimal::Decimal {
    rust_decimal::Decimal::new(mantissa, scale)
}

fn v(major: u16, minor: u16, patch: u16) -> SchemaVersion {
    SchemaVersion {
        major,
        minor,
        patch,
    }
}

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Byte-for-byte copy of the committed 1.0.0 fixture
/// `tests/fixtures/strategies/rsi-oversold-long.json`, pinned inline so the
/// migration assertions hold even if the fixture file is ever re-authored.
const INLINE_1_0_0: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold Long",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "spec": { "indicator": "Rsi", "period": 14 }
    },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#;

/// A byte-pinned 1.1.0 document — the 1.0.0 grammar plus the explicit
/// `series: "primary"` operand field 1.1.0 introduced (r2.s2 b13).
const INLINE_1_1_0: &str = r#"{
  "schema_version": "1.1.0",
  "name": "RSI Oversold (pinned 1.1.0)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "series": "primary",
      "spec": { "indicator": "Rsi", "period": 14 }
    },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#;

/// A byte-pinned 1.0.0 document that RUNS MACD(12,26,9): the frozen re-run
/// case (3) must include a MACD-carrying document so the identity claim
/// "the bump cannot move a pre-r3 run" is proven on the very indicator this
/// item extends, and so case (4)'s no-`output` document has a frozen twin to
/// match.
const INLINE_MACD_1_0_0: &str = r#"{
  "schema_version": "1.0.0",
  "name": "MACD Line Long (pinned 1.0.0)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "spec": { "indicator": "Macd", "fast": 12, "slow": 26, "signal": 9 }
    },
    "op": "Gt",
    "rhs": { "type": "Constant", "value": "0" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#;

/// The byte-pinned documents with their AUTHORED schema versions.
fn pinned_docs() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("inline 1.0.0 (fixture twin)", "1.0.0", INLINE_1_0_0),
        ("inline 1.1.0", "1.1.0", INLINE_1_1_0),
        ("inline macd 1.0.0", "1.0.0", INLINE_MACD_1_0_0),
    ]
}

/// The committed 1.0.0 fixture document bytes.
fn committed_fixture_doc() -> String {
    std::fs::read_to_string(manifest("tests/fixtures/strategies/rsi-oversold-long.json"))
        .expect("read committed fixture strategy json")
}

/// Every committed fixture document under `tests/fixtures/strategies/`:
/// `(relative file name, bytes)`.
fn committed_fixture_docs() -> Vec<(String, String)> {
    let dir = manifest("tests/fixtures/strategies");
    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir).expect("read fixtures dir");
    for entry in entries {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "json") {
            let bytes = std::fs::read_to_string(&path).expect("read fixture json");
            out.push((
                path.file_name()
                    .expect("file name")
                    .to_string_lossy()
                    .into_owned(),
                bytes,
            ));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// The authored `schema_version` string of a document (read without
/// deserializing the whole DSL).
fn authored_version_of(doc: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(doc).expect("doc parses as JSON");
    value["schema_version"]
        .as_str()
        .expect("schema_version is a string")
        .to_owned()
}

/// The primary M15 candle series from the committed offline fixture store —
/// the same load recipe `tests/backtest_fixture.rs` uses.
fn load_primary() -> CandleSeries {
    let base = manifest("tests/fixtures/btcusdt-1m-store");
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

/// One frozen re-run: load → validate → compile → backtest over the committed
/// 1-month fixture, returning `(result_content_hash, full trade log json)`.
/// The recipe is exactly `tests/run_comparability.rs`'s fixture run.
fn frozen_run(primary: &CandleSeries, doc: &str) -> (String, String) {
    let loaded = Migrator::v1().load(doc).expect("load (migrate) document");
    let validated = validate(&loaded.dsl).expect("document validates");
    let compiled = compile(&validated).expect("document compiles");
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port");
    let result = run_backtest(
        &compiled,
        primary,
        None,
        &BacktestConfig::default(),
        &filters,
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("backtest runs over the fixture");
    (
        result.result_content_hash(),
        serde_json::to_string(&result.trades).expect("trade log serializes"),
    )
}

/// The frozen pre-bump `(result_content_hash, trade log json)` per document —
/// captured from the BASE engine; see the module-header recipe.
const FROZEN_COMMITTED_1_0_0: (&str, &str) = (
    "b8e91b89b7727eb97c8200ee04373ffc3ef3568b51ab145bbb2e652cb2bb9228",
    // The full pre-bump trade log (serde_json of `result.trades`).
    r#"[{"direction":"long","qty":"0.021","entry_price":"93306.42971","exit_price":"102626.8089737319","entry_signal_time":1735722899999,"entry_fill_time":1735722900000,"exit_signal_time":1736209800000,"exit_fill_time":1736209800000,"fills":[{"price":"93306.42971","qty":"0.021","time_ms":1735722900000,"fee":"0.783774009564"},{"price":"102626.8089737319","qty":"0.021","time_ms":1736209800000,"fee":"0.86206519537934796"}],"fees_total":"1.64583920494334796","funding_total":"-2.9794973030073069","slippage_total":"0.4114617626301","realized_pnl":"191.10262803041924514","realized_r":"1.99780","mfe_r":"2.0267992933367163985123828612","mae_r":"-0.1093021588297572424604563728","exit_reason":"take_profit","source":"backtest","regime":"unknown","stop_price":"88641.1082245"},{"direction":"long","qty":"0.019","entry_price":"101409.93998","exit_price":"96329.8090367019","entry_signal_time":1736244899999,"entry_fill_time":1736244900000,"exit_signal_time":1736278200000,"exit_fill_time":1736278200000,"fills":[{"price":"101409.93998","qty":"0.019","time_ms":1736244900000,"fee":"0.770715543848"},{"price":"96329.8090367019","qty":"0.019","time_ms":1736278200000,"fee":"0.73210654867893444"}],"fees_total":"1.50282209252693444","funding_total":"-0.1595188496879398","slippage_total":"0.3757045616639","realized_pnl":"-98.18482886487877424","realized_r":"-1.00190","mfe_r":"0.0263406171084098101445301733","mae_r":"-1.0085875173594595396387098818","exit_reason":"stop_loss","source":"backtest","regime":"ranging","stop_price":"96339.4429810"},{"direction":"long","qty":"0.020","entry_price":"96691.76821","exit_price":"91847.99408152005","entry_signal_time":1736279099999,"entry_fill_time":1736279100000,"exit_signal_time":1736430300000,"exit_fill_time":1736430300000,"fills":[{"price":"96691.76821","qty":"0.020","time_ms":1736279100000,"fee":"0.77353414568"},{"price":"91847.99408152005","qty":"0.020","time_ms":1736430300000,"fee":"0.7347839526521604"}],"fees_total":"1.5083180983321604","funding_total":"-0.9637075153954280","slippage_total":"0.37707855959900","realized_pnl":"-99.3475081833265884","realized_r":"-1.0019","mfe_r":"0.1138528750047356280526954281","mae_r":"-1.0118272321539955836536252748","exit_reason":"stop_loss","source":"backtest","regime":"trending_down","stop_price":"91857.1797995"},{"direction":"long","qty":"0.021","entry_price":"91935.99268","exit_price":"101119.4789888052","entry_signal_time":1736431199999,"entry_fill_time":1736431200000,"exit_signal_time":1737077400000,"exit_fill_time":1737077400000,"fills":[{"price":"91935.99268","qty":"0.021","time_ms":1736431200000,"fee":"0.772262338512"},{"price":"101119.4789888052","qty":"0.021","time_ms":1737077400000,"fee":"0.84940362350596368"}],"fees_total":"1.62166596201796368","funding_total":"-2.2665899635327200","slippage_total":"0.4054184230908","realized_pnl":"188.96495655935851632","realized_r":"1.99780","mfe_r":"2.1166916321591405695800227259","mae_r":"-0.6585000263250543062499730437","exit_reason":"take_profit","source":"backtest","regime":"trending_down","stop_price":"87339.1930460"},{"direction":"long","qty":"0.019","entry_price":"104053.00426","exit_price":"98840.4690115953","entry_signal_time":1737158399999,"entry_fill_time":1737158400000,"exit_signal_time":1737960300000,"exit_fill_time":1737960300000,"fills":[{"price":"104053.00426","qty":"0.019","time_ms":1737158400000,"fee":"0.790802832376"},{"price":"98840.4690115953","qty":"0.019","time_ms":1737960300000,"fee":"0.75118756448812428"}],"fees_total":"1.54199039686412428","funding_total":"-4.7952306748199700","slippage_total":"0.3854966126893","realized_pnl":"-105.37539079137339428","realized_r":"-1.00190","mfe_r":"1.1430704538121905832611084285","mae_r":"-1.0365110163518886561747794364","exit_reason":"stop_loss","source":"backtest","regime":"trending_up","stop_price":"98850.3540470"},{"direction":"long","qty":"0.020","entry_price":"98927.8918","exit_price":"102369.46203","entry_signal_time":1737961199999,"entry_fill_time":1737961200000,"exit_signal_time":1738367999999,"exit_fill_time":1738367999999,"fills":[{"price":"98927.8918","qty":"0.020","time_ms":1737961200000,"fee":"0.7914231344"},{"price":"102369.46203","qty":"0.020","time_ms":1738367999999,"fee":"0.81895569624"}],"fees_total":"1.61037883064","funding_total":"-2.090049570058600","slippage_total":"0.40259540","realized_pnl":"65.130976199301400","realized_r":"0.6957734906466489564877192703","mfe_r":"1.5207254623816819272398565356","mae_r":"-0.2522831078868699797765224387","exit_reason":"end_of_data","source":"backtest","regime":"trending_down","stop_price":"93981.497210"}]"#,
);
const FROZEN_INLINE_1_1_0: (&str, &str) = (
    "b8e91b89b7727eb97c8200ee04373ffc3ef3568b51ab145bbb2e652cb2bb9228",
    // The full pre-bump trade log (serde_json of `result.trades`).
    r#"[{"direction":"long","qty":"0.021","entry_price":"93306.42971","exit_price":"102626.8089737319","entry_signal_time":1735722899999,"entry_fill_time":1735722900000,"exit_signal_time":1736209800000,"exit_fill_time":1736209800000,"fills":[{"price":"93306.42971","qty":"0.021","time_ms":1735722900000,"fee":"0.783774009564"},{"price":"102626.8089737319","qty":"0.021","time_ms":1736209800000,"fee":"0.86206519537934796"}],"fees_total":"1.64583920494334796","funding_total":"-2.9794973030073069","slippage_total":"0.4114617626301","realized_pnl":"191.10262803041924514","realized_r":"1.99780","mfe_r":"2.0267992933367163985123828612","mae_r":"-0.1093021588297572424604563728","exit_reason":"take_profit","source":"backtest","regime":"unknown","stop_price":"88641.1082245"},{"direction":"long","qty":"0.019","entry_price":"101409.93998","exit_price":"96329.8090367019","entry_signal_time":1736244899999,"entry_fill_time":1736244900000,"exit_signal_time":1736278200000,"exit_fill_time":1736278200000,"fills":[{"price":"101409.93998","qty":"0.019","time_ms":1736244900000,"fee":"0.770715543848"},{"price":"96329.8090367019","qty":"0.019","time_ms":1736278200000,"fee":"0.73210654867893444"}],"fees_total":"1.50282209252693444","funding_total":"-0.1595188496879398","slippage_total":"0.3757045616639","realized_pnl":"-98.18482886487877424","realized_r":"-1.00190","mfe_r":"0.0263406171084098101445301733","mae_r":"-1.0085875173594595396387098818","exit_reason":"stop_loss","source":"backtest","regime":"ranging","stop_price":"96339.4429810"},{"direction":"long","qty":"0.020","entry_price":"96691.76821","exit_price":"91847.99408152005","entry_signal_time":1736279099999,"entry_fill_time":1736279100000,"exit_signal_time":1736430300000,"exit_fill_time":1736430300000,"fills":[{"price":"96691.76821","qty":"0.020","time_ms":1736279100000,"fee":"0.77353414568"},{"price":"91847.99408152005","qty":"0.020","time_ms":1736430300000,"fee":"0.7347839526521604"}],"fees_total":"1.5083180983321604","funding_total":"-0.9637075153954280","slippage_total":"0.37707855959900","realized_pnl":"-99.3475081833265884","realized_r":"-1.0019","mfe_r":"0.1138528750047356280526954281","mae_r":"-1.0118272321539955836536252748","exit_reason":"stop_loss","source":"backtest","regime":"trending_down","stop_price":"91857.1797995"},{"direction":"long","qty":"0.021","entry_price":"91935.99268","exit_price":"101119.4789888052","entry_signal_time":1736431199999,"entry_fill_time":1736431200000,"exit_signal_time":1737077400000,"exit_fill_time":1737077400000,"fills":[{"price":"91935.99268","qty":"0.021","time_ms":1736431200000,"fee":"0.772262338512"},{"price":"101119.4789888052","qty":"0.021","time_ms":1737077400000,"fee":"0.84940362350596368"}],"fees_total":"1.62166596201796368","funding_total":"-2.2665899635327200","slippage_total":"0.4054184230908","realized_pnl":"188.96495655935851632","realized_r":"1.99780","mfe_r":"2.1166916321591405695800227259","mae_r":"-0.6585000263250543062499730437","exit_reason":"take_profit","source":"backtest","regime":"trending_down","stop_price":"87339.1930460"},{"direction":"long","qty":"0.019","entry_price":"104053.00426","exit_price":"98840.4690115953","entry_signal_time":1737158399999,"entry_fill_time":1737158400000,"exit_signal_time":1737960300000,"exit_fill_time":1737960300000,"fills":[{"price":"104053.00426","qty":"0.019","time_ms":1737158400000,"fee":"0.790802832376"},{"price":"98840.4690115953","qty":"0.019","time_ms":1737960300000,"fee":"0.75118756448812428"}],"fees_total":"1.54199039686412428","funding_total":"-4.7952306748199700","slippage_total":"0.3854966126893","realized_pnl":"-105.37539079137339428","realized_r":"-1.00190","mfe_r":"1.1430704538121905832611084285","mae_r":"-1.0365110163518886561747794364","exit_reason":"stop_loss","source":"backtest","regime":"trending_up","stop_price":"98850.3540470"},{"direction":"long","qty":"0.020","entry_price":"98927.8918","exit_price":"102369.46203","entry_signal_time":1737961199999,"entry_fill_time":1737961200000,"exit_signal_time":1738367999999,"exit_fill_time":1738367999999,"fills":[{"price":"98927.8918","qty":"0.020","time_ms":1737961200000,"fee":"0.7914231344"},{"price":"102369.46203","qty":"0.020","time_ms":1738367999999,"fee":"0.81895569624"}],"fees_total":"1.61037883064","funding_total":"-2.090049570058600","slippage_total":"0.40259540","realized_pnl":"65.130976199301400","realized_r":"0.6957734906466489564877192703","mfe_r":"1.5207254623816819272398565356","mae_r":"-0.2522831078868699797765224387","exit_reason":"end_of_data","source":"backtest","regime":"trending_down","stop_price":"93981.497210"}]"#,
);
const FROZEN_INLINE_MACD_1_0_0: (&str, &str) = (
    "bd31da3b1d9e9802c0c82085eabe00436da03002c970494c6ed0bccaf360092f",
    // The full pre-bump trade log (serde_json of `result.trades`).
    r#"[{"direction":"long","qty":"0.021","entry_price":"93764.77554","exit_price":"89067.6291093237","entry_signal_time":1735735499999,"entry_fill_time":1735735500000,"exit_signal_time":1736778600000,"exit_fill_time":1736778600000,"fills":[{"price":"93764.77554","qty":"0.021","time_ms":1735735500000,"fee":"0.787624114536"},{"price":"89067.6291093237","qty":"0.021","time_ms":1736778600000,"fee":"0.74816808451831908"}],"fees_total":"1.53579219905431908","funding_total":"-5.2963586675944686","slippage_total":"0.3839470672023","realized_pnl":"-105.47222591085108768","realized_r":"-1.00190","mfe_r":"1.9191267527029372548530499047","mae_r":"-1.0357355439790988273718636153","exit_reason":"stop_loss","source":"backtest","regime":"unknown","stop_price":"89076.5367630"},{"direction":"long","qty":"0.021","entry_price":"92137.2128","exit_price":"101340.798986592","entry_signal_time":1736796599999,"entry_fill_time":1736796600000,"exit_signal_time":1737077400000,"exit_fill_time":1737077400000,"fills":[{"price":"92137.2128","qty":"0.021","time_ms":1736796600000,"fee":"0.77395258752"},{"price":"101340.798986592","qty":"0.021","time_ms":1737077400000,"fee":"0.8512627114873728"}],"fees_total":"1.6252152990073728","funding_total":"-1.288921290441120","slippage_total":"0.406305761568","realized_pnl":"190.3611733289835072","realized_r":"1.99780","mfe_r":"2.0683905906040170557449291542","mae_r":"-0.0581551778826980101573031304","exit_reason":"take_profit","source":"backtest","regime":"trending_down","stop_price":"87530.352160"},{"direction":"long","qty":"0.019","entry_price":"101626.56164","exit_price":"102369.46203","entry_signal_time":1737078299999,"entry_fill_time":1737078300000,"exit_signal_time":1738367999999,"exit_fill_time":1738367999999,"fills":[{"price":"101626.56164","qty":"0.019","time_ms":1737078300000,"fee":"0.772361868464"},{"price":"102369.46203","qty":"0.019","time_ms":1738367999999,"fee":"0.778007911428"}],"fees_total":"1.550369779892","funding_total":"-7.1001295663224360","slippage_total":"0.38759259","realized_pnl":"5.4646080637855640","realized_r":"0.1462020121534045830973367958","mfe_r":"1.647883825817488318598200682","mae_r":"-0.7766791626740703730847977944","exit_reason":"end_of_data","source":"backtest","regime":"ranging","stop_price":"96545.2335580"}]"#,
);

// ---------------------------------------------------------------------------
// Case 1 — pre-1.2.0 documents load to CURRENT.
// ---------------------------------------------------------------------------

#[test]
fn every_pre_1_2_0_document_loads_to_current() {
    // The committed fixture files (currently the one 1.0.0 document).
    for (name, doc) in committed_fixture_docs() {
        let authored = authored_version_of(&doc);
        assert!(
            authored == "1.0.0" || authored == "1.1.0",
            "{name}: fixture is pre-1.2.0"
        );
        let loaded = Migrator::v1().load(&doc).expect("load (migrate) document");
        let authored_version = authored
            .parse::<SchemaVersion>()
            .expect("authored version parses");
        assert_eq!(
            loaded.from, authored_version,
            "{name}: `from` stays the authored version"
        );
        assert_eq!(
            loaded.dsl_original, doc,
            "{name}: dsl_original stays byte-verbatim"
        );
        assert!(loaded.migrated, "{name}: a migration step was applied");
        assert_eq!(
            loaded.dsl.schema_version,
            SchemaVersion::CURRENT,
            "{name}: loads to CURRENT"
        );
    }
    // The byte-pinned inline documents.
    for (label, authored_version, doc) in pinned_docs() {
        let loaded = Migrator::v1().load(doc).expect("load (migrate) document");
        let authored_version = authored_version
            .parse::<SchemaVersion>()
            .expect("authored version parses");
        assert_eq!(
            loaded.from, authored_version,
            "{label}: `from` stays the authored version"
        );
        assert_eq!(
            loaded.dsl_original, doc,
            "{label}: dsl_original byte-verbatim"
        );
        assert!(loaded.migrated, "{label}: a migration step was applied");
        assert_eq!(
            loaded.dsl.schema_version,
            v(1, 2, 0),
            "{label}: loads to CURRENT == 1.2.0"
        );
    }
}

async fn repo() -> (SqliteStrategyRepo<pulse::SystemClock>, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let pool = db.pool().clone();
    (SqliteStrategyRepo::new(pool), tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_pre_1_2_0_versions_hash_from_the_written_under_version() {
    let (repo, _tmp) = repo().await;

    let mut cases: Vec<(String, String)> = committed_fixture_docs()
        .into_iter()
        .map(|(name, doc)| (format!("fixture {name}"), doc))
        .collect();
    cases.extend(
        pinned_docs()
            .into_iter()
            .map(|(label, _v, doc)| (label.to_owned(), doc.to_owned())),
    );

    for (label, doc) in cases {
        let s = repo
            .create_strategy(
                &format!("MigrateMe {label}"),
                Some("alice"),
                &["btc".to_owned()],
            )
            .await
            .expect("create strategy");
        let created = repo
            .create_version(NewVersion {
                strategy_id: s.id.clone(),
                parent_version_id: None,
                dsl_json: doc.clone(),
                created_by: CreatedBy::Human,
                creating_llm_call_ids: vec![],
            })
            .await
            .expect("create version");

        let fetched = repo
            .get_version(&created.id)
            .await
            .expect("get version")
            .expect("version exists");
        assert_eq!(
            fetched.dsl_schema_version,
            SchemaVersion::CURRENT,
            "{label}: persisted version reads back at CURRENT"
        );
        assert_eq!(
            fetched.dsl_original, doc,
            "{label}: dsl_original byte-verbatim"
        );

        // The re-derived hash keys on the schema version the row was WRITTEN
        // under (CURRENT at write time — ADR-0010's hash inputs), and stays
        // unchanged across the bump.
        let written_under = fetched.dsl_schema_version.to_string();
        let from_write_version = version_hash(s.id.as_str(), None, &written_under, &doc);
        assert_eq!(
            fetched.version_hash, from_write_version,
            "{label}: version_hash recomputes unchanged from the version it was written under"
        );
        // …and provably NOT from the authored version string wherever the two
        // actually differ (post-bump that is every migrated document; pre-bump
        // it is the 1.0.0 documents, since a 1.1.0 doc written pre-bump is
        // written under its own version).
        let authored = authored_version_of(&doc);
        if authored != written_under {
            let with_authored = version_hash(s.id.as_str(), None, &authored, &doc);
            assert_ne!(
                fetched.version_hash, with_authored,
                "{label}: the hash must NOT be re-derived from the authored version string"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Case 2 — 1.2.0 served; 1.3.0 / 2.0.0 refused.
// ---------------------------------------------------------------------------

#[test]
fn served_schema_version_enum_is_exactly_the_three_versions() {
    let schema =
        serde_json::to_value(schemars::schema_for!(StrategyDsl)).expect("schema serializes");
    assert_eq!(
        schema["properties"]["schema_version"]["enum"],
        serde_json::json!(["1.0.0", "1.1.0", "1.2.0"]),
        "the served schema_version enum lists exactly the three served versions"
    );
}

fn doc_at_version(template: &str, version: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(template).expect("template parses");
    value["schema_version"] = serde_json::json!(version);
    serde_json::to_string_pretty(&value).expect("doc serializes")
}

#[test]
fn future_minor_and_new_major_refuse_on_the_load_path() {
    for future in ["1.3.0", "2.0.0"] {
        let doc = doc_at_version(INLINE_1_1_0, future);
        let err = Migrator::v1()
            .load(&doc)
            .expect_err("a future version must refuse to load");
        assert!(
            matches!(err, pulse::LoadError::FutureVersion { .. }),
            "{future}: refused with FutureVersion, got {err:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Case 3 — re-runs are frozen at the pre-bump values.
// ---------------------------------------------------------------------------

#[test]
fn pre_bump_runs_are_reproduced_exactly_after_the_bump() {
    let primary = load_primary();

    let committed = committed_fixture_doc();
    let cases: Vec<(&str, String, (&str, &str))> = vec![
        ("committed fixture 1.0.0", committed, FROZEN_COMMITTED_1_0_0),
        ("inline 1.1.0", INLINE_1_1_0.to_owned(), FROZEN_INLINE_1_1_0),
        (
            "inline macd 1.0.0",
            INLINE_MACD_1_0_0.to_owned(),
            FROZEN_INLINE_MACD_1_0_0,
        ),
    ];

    for (label, doc, (frozen_hash, frozen_trades)) in cases {
        let (hash, trades) = frozen_run(&primary, &doc);
        assert!(
            !trades.is_empty() && trades != "[]",
            "{label}: the frozen run is non-vacuous"
        );
        assert_eq!(
            hash, frozen_hash,
            "{label}: result_content_hash must equal the pre-bump value"
        );
        assert_eq!(
            trades, frozen_trades,
            "{label}: the full trade log must equal the pre-bump log"
        );
    }
}

/// One-off capture helper (the `backtest_fixture.rs::print_golden_for_regeneration`
/// pattern): runs the frozen recipe against the CURRENT engine and PRINTS the
/// `FROZEN[...]` lines to copy into the constants above. Run it against the
/// BASE engine BEFORE any r3.s2.w1 source edit (see the module-header recipe).
#[test]
#[ignore = "one-off capture: prints the FROZEN constants for the current engine"]
fn emit_frozen_prebump() {
    let primary = load_primary();
    let committed = committed_fixture_doc();
    let cases: Vec<(&str, String)> = vec![
        ("committed_1_0_0", committed),
        ("inline_1_1_0", INLINE_1_1_0.to_owned()),
        ("inline_macd_1_0_0", INLINE_MACD_1_0_0.to_owned()),
    ];
    for (label, doc) in cases {
        let (hash, trades) = frozen_run(&primary, &doc);
        println!("FROZEN[{label}] hash={hash} trades={trades}");
        // Optional capture plumbing: when FROZEN_CAPTURE_DIR is set, write
        // `hash\ntrades` beside the run so regeneration is a mechanical splice
        // into the constants above (no hand transcription).
        if let Ok(dir) = std::env::var("FROZEN_CAPTURE_DIR") {
            let path = Path::new(&dir).join(format!("frozen_{label}.txt"));
            std::fs::write(&path, format!("{hash}\n{trades}")).expect("write frozen capture file");
        }
    }
}

// ---------------------------------------------------------------------------
// Case 4 — MACD `output`.
// ---------------------------------------------------------------------------

/// A 1.2.0 MACD(12,26,9) strategy document, optionally carrying the given
/// `output` selector. Semantically identical to `INLINE_MACD_1_0_0` (same
/// entry/exits/risk; only the name and version differ), so its runs must equal
/// that document's frozen run when `output` is absent.
fn macd_doc_at_current(output_field: Option<&str>) -> String {
    let output = output_field
        .map(|o| format!(r#", "output": "{o}""#))
        .unwrap_or_default();
    format!(
        r#"{{
  "schema_version": "1.2.0",
  "name": "MACD output case (1.2.0)",
  "direction": "long",
  "entry": {{
    "type": "Compare",
    "lhs": {{
      "type": "Indicator",
      "spec": {{ "indicator": "Macd", "fast": 12, "slow": 26, "signal": 9{output} }}
    }},
    "op": "Gt",
    "rhs": {{ "type": "Constant", "value": "0" }}
  }},
  "filters": [],
  "exits": [
    {{ "type": "StopLoss", "distance_pct": "0.05" }},
    {{ "type": "TakeProfit", "target_r": "2" }}
  ],
  "risk": {{
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }}
}}"#
    )
}

/// The entry condition's two `ValueSource`s (a `Compare` entry).
fn entry_operands(dsl: &StrategyDsl) -> (&ValueSource, &ValueSource) {
    match &dsl.entry {
        Condition::Compare { lhs, rhs, .. } => (lhs, rhs),
        other => panic!("expected a Compare entry, got {other:?}"),
    }
}

#[test]
fn macd_output_defaults_to_line_and_is_always_written() {
    // A document with NO `output` deserializes to the MACD line.
    let loaded = Migrator::v1()
        .load(&macd_doc_at_current(None))
        .expect("no-output document loads");
    let (lhs, _rhs) = entry_operands(&loaded.dsl);
    match lhs {
        ValueSource::Indicator { spec, .. } => match spec {
            IndicatorSpec::Macd { output, .. } => {
                assert_eq!(*output, MacdOutput::Line, "absent output defaults to line");
            }
            other => panic!("expected a Macd spec, got {other:?}"),
        },
        other => panic!("expected an Indicator operand, got {other:?}"),
    }

    // The typed value ALWAYS writes the field (no skip_serializing_if) — the
    // series precedent.
    let spec = IndicatorSpec::Macd {
        fast: SweepableValue::Fixed(12),
        slow: SweepableValue::Fixed(26),
        signal: SweepableValue::Fixed(9),
        output: MacdOutput::Line,
    };
    let json = serde_json::to_value(&spec).expect("spec serializes");
    assert_eq!(json["output"], "line", "the output field is always written");
}

#[test]
fn macd_signal_and_histogram_round_trip_through_load_validate_compile_and_render() {
    for (output, expected_render) in [
        ("signal", "macd(12,26,9).signal"),
        ("histogram", "macd(12,26,9).histogram"),
    ] {
        let doc = macd_doc_at_current(Some(output));
        let loaded = Migrator::v1().load(&doc).expect("document loads");
        let validated = validate(&loaded.dsl).expect("document validates");
        let compiled = compile(&validated).expect("document compiles");
        let specs = compiled.required_indicators();
        assert_eq!(specs.len(), 1, "{output}: one indicator in the tree");
        let expected = match output {
            "signal" => MacdOutput::Signal,
            _ => MacdOutput::Histogram,
        };
        match &specs[0] {
            IndicatorSpec::Macd { output: got, .. } => {
                assert_eq!(*got, expected, "{output}: round-trips through compile");
            }
            other => panic!("{output}: expected a Macd spec, got {other:?}"),
        }
        assert_eq!(
            render::indicator(&specs[0]),
            expected_render,
            "{output}: renders with the output suffix"
        );
    }

    // The line renders bare (the historical spelling).
    let line_spec = IndicatorSpec::Macd {
        fast: SweepableValue::Fixed(12),
        slow: SweepableValue::Fixed(26),
        signal: SweepableValue::Fixed(9),
        output: MacdOutput::Line,
    };
    assert_eq!(render::indicator(&line_spec), "macd(12,26,9)");
}

// ---------------------------------------------------------------------------
// Case 5 — value expressions (r3.s2.w3): `Arith` / `Lag` / `Rising`/`Falling`.
// ---------------------------------------------------------------------------

/// A 1.2.0 expression strategy document using ALL THREE constructs (spec §5):
/// entry `ema(10) > lag(ema(10), 3)`, the goal's volatility-normalised ratio
/// filter `atr(14) / close < 0.02`, and a `Falling (2 bars)` signal exit.
fn expression_doc_at_current() -> String {
    r#"{
  "schema_version": "1.2.0",
  "name": "value expressions (1.2.0)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "spec": { "indicator": "Ema", "period": 10 }
    },
    "op": "Gt",
    "rhs": {
      "type": "Lag",
      "value": {
        "type": "Indicator",
        "spec": { "indicator": "Ema", "period": 10 }
      },
      "bars": 3
    }
  },
  "filters": [
    {
      "type": "Compare",
      "lhs": {
        "type": "Arith",
        "op": "div",
        "lhs": {
          "type": "Indicator",
          "spec": { "indicator": "Atr", "period": 14 }
        },
        "rhs": { "type": "Price", "field": "Close" }
      },
      "op": "Lt",
      "rhs": { "type": "Constant", "value": "0.02" }
    }
  ],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    {
      "type": "SignalExit",
      "condition": {
        "type": "Falling",
        "value": { "type": "Price", "field": "Close" },
        "bars": 2
      }
    }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#
    .to_owned()
}

/// Spec §5: an `Arith`/`Lag`/`Falling` document round-trips through serde,
/// load, validate, compile and render — the render spellings match the pinned
/// vocabulary (see `tests/dsl_render.rs`).
#[test]
fn expression_document_round_trips_through_load_validate_compile_and_render() {
    let loaded = Migrator::v1()
        .load(&expression_doc_at_current())
        .expect("the expression document loads");
    let dsl = &loaded.dsl;

    // Render round-trip (implies the serde shape survived the load).
    assert_eq!(
        render::condition(&dsl.entry),
        "ema(10) > lag(ema(10), 3)",
        "the lagged entry renders in the pinned vocabulary"
    );
    let Condition::Compare { lhs: ratio, .. } = &dsl.filters[0] else {
        panic!("the filter is a Compare, got {:?}", dsl.filters[0]);
    };
    assert_eq!(
        render::value(ratio),
        "(atr(14) / close)",
        "the ratio filter renders infix, parenthesized"
    );
    let ExitRule::SignalExit { condition } = &dsl.exits[1] else {
        panic!("exits[1] is a SignalExit, got {:?}", dsl.exits[1]);
    };
    assert_eq!(
        render::condition(condition),
        "close falling (2 bars)",
        "the falling signal exit renders with its bar count"
    );

    // Validate + compile: the expression document is executable. The lag lives
    // on the leaves after compilation — no expression-level node exists.
    let validated = validate(dsl).expect("the expression document validates");
    let compiled = compile(&validated).expect("the expression document compiles");
    assert!(
        !compiled.needs_htf(),
        "every leaf sits on the primary series"
    );
    // The compiler folds the entry and its filters into one And tree.
    let pulse::CompiledCondition::And(conditions) = compiled.entry() else {
        panic!("the compiled entry is an And, got {:?}", compiled.entry());
    };
    assert_eq!(conditions.len(), 2, "entry + the ratio filter");
    let pulse::CompiledCondition::Compare { lhs, rhs, .. } = &conditions[0] else {
        panic!(
            "conditions[0] is the lagged entry compare, got {:?}",
            conditions[0]
        );
    };
    assert!(
        matches!(*lhs, pulse::CompiledValue::Indicator { lag: 0, .. }),
        "the today-side leaf carries lag 0, got {lhs:?}"
    );
    assert!(
        matches!(*rhs, pulse::CompiledValue::Indicator { lag: 3, .. }),
        "the lag pushed down to the leaf: lag 3 on the same spec, got {rhs:?}"
    );
    let pulse::CompiledCondition::Compare { lhs: ratio, .. } = &conditions[1] else {
        panic!("conditions[1] is the ratio filter, got {:?}", conditions[1]);
    };
    assert!(
        matches!(
            *ratio,
            pulse::CompiledValue::Arith {
                op: pulse::ArithOp::Div,
                ..
            }
        ),
        "the ratio filter compiles to an Arith tree, got {ratio:?}"
    );
}

/// Spec §5 + Q2: `bars` omitted on a `Rising` reads as 1, and writes ALWAYS
/// emit `bars` (the `series`/`output` precedent).
#[test]
fn expression_bars_default_to_one_and_are_always_written() {
    let doc = r#"{
  "schema_version": "1.2.0",
  "name": "rising default bars",
  "direction": "long",
  "entry": {
    "type": "Rising",
    "value": { "type": "Price", "field": "Close" }
  },
  "filters": [],
  "exits": [ { "type": "StopLoss", "distance_pct": "0.05" } ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;
    let loaded = Migrator::v1().load(doc).expect("bars-less Rising loads");
    let dsl = &loaded.dsl;
    let Condition::Rising { bars, .. } = &dsl.entry else {
        panic!("the entry is a Rising, got {:?}", dsl.entry);
    };
    assert_eq!(*bars, 1, "an omitted bars reads as the Q2 default 1");

    // The typed value ALWAYS writes the field.
    let json = serde_json::to_value(&dsl.entry).expect("entry serializes");
    assert_eq!(json["bars"], 1, "writes always emit bars");

    // …and the defaulted document validates, compiles, and renders `1 bar`.
    let validated = validate(dsl).expect("validates");
    compile(&validated).expect("compiles");
    assert_eq!(render::condition(&dsl.entry), "close rising (1 bar)");
}

/// `macd.line > macd.signal` — the rule condition comparing the MACD line
/// output to the MACD signal output over the same periods.
fn line_vs_signal_doc() -> String {
    r#"{
  "schema_version": "1.2.0",
  "name": "MACD line vs signal (1.2.0)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": {
      "type": "Indicator",
      "spec": { "indicator": "Macd", "fast": 12, "slow": 26, "signal": 9, "output": "line" }
    },
    "op": "Gt",
    "rhs": {
      "type": "Indicator",
      "spec": { "indicator": "Macd", "fast": 12, "slow": 26, "signal": 9, "output": "signal" }
    }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#
    .to_owned()
}

#[test]
fn macd_line_vs_signal_condition_compiles_to_two_distinct_slots() {
    let loaded = Migrator::v1()
        .load(&line_vs_signal_doc())
        .expect("line-vs-signal document loads");
    let validated = validate(&loaded.dsl).expect("document validates");
    let compiled = compile(&validated).expect("document compiles");

    // Two DISTINCT IndicatorSpec::Macd values in the compiled tree.
    let specs = compiled.required_indicators();
    assert_eq!(
        specs.len(),
        2,
        "the line and the signal are two distinct specs"
    );
    assert_ne!(specs[0], specs[1], "the two specs are distinct values");
    for spec in specs {
        assert!(
            matches!(spec, IndicatorSpec::Macd { .. }),
            "both specs are Macd specs"
        );
    }
    let outputs: Vec<MacdOutput> = specs
        .iter()
        .filter_map(|s| match s {
            IndicatorSpec::Macd { output, .. } => Some(*output),
            _ => None,
        })
        .collect();
    assert!(
        outputs.contains(&MacdOutput::Line) && outputs.contains(&MacdOutput::Signal),
        "one slot is the line and one is the signal, got {outputs:?}"
    );

    // Through the engine's from_specs: two slots.
    let engine = IndicatorEngine::from_specs(specs).expect("engine builds");
    assert_eq!(
        engine.indicator_count(),
        2,
        "from_specs keeps two distinct MACD slots"
    );
}

#[test]
fn no_output_document_runs_identically_to_the_frozen_macd_line_run() {
    // The 1.2.0-authored twin of the frozen 1.0.0 MACD document: same
    // entry/exits/risk, `output` omitted. Its run must equal (iii)'s frozen
    // MACD pair exactly.
    let primary = load_primary();
    let (hash, trades) = frozen_run(&primary, &macd_doc_at_current(None));
    let (frozen_hash, frozen_trades) = FROZEN_INLINE_MACD_1_0_0;
    assert_eq!(
        hash, frozen_hash,
        "the defaulted output does not move the run"
    );
    assert_eq!(trades, frozen_trades, "the trade log is identical");
}

// ---------------------------------------------------------------------------
// Case 5 — unknown fields are refused at every write, tolerated on read.
// ---------------------------------------------------------------------------

/// A valid 1.2.0 document carrying two unknown fields: one nested
/// (`entry.typo_field`) and one top-level (`name_of_thing`).
fn doc_with_unknown_fields() -> String {
    let mut value: serde_json::Value =
        serde_json::from_str(&macd_doc_at_current(None)).expect("base doc parses");
    value["entry"]["typo_field"] = serde_json::json!(true);
    value["name_of_thing"] = serde_json::json!("accidental");
    serde_json::to_string_pretty(&value).expect("doc serializes")
}

async fn repo_with_pool() -> (
    SqliteStrategyRepo<pulse::SystemClock>,
    SqlitePool,
    tempfile::TempDir,
) {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let pool = db.pool().clone();
    (SqliteStrategyRepo::new(pool.clone()), pool, tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_submit_refuses_unknown_fields_and_persists_nothing() {
    let (repo, pool, _tmp) = repo_with_pool().await;
    let request = SubmitRequest {
        target: SubmitTarget::Root {
            strategy_name: "Unknown Field Refuser".to_owned(),
        },
        dsl: serde_json::from_str(&doc_with_unknown_fields()).expect("doc parses"),
        hypothesis: "unknown fields must refuse the write".to_owned(),
        agent_name: "claude-code".to_owned(),
    };
    let err = submit_agent_version(&repo, request)
        .await
        .expect_err("unknown fields refuse the submit");
    match err {
        pulse::SubmitError::Validation(errors) => {
            let errs = errors.into_errors();
            assert_eq!(errs.len(), 2, "exactly the two unknown fields");
            let paths: Vec<&str> = errs.iter().map(|e| e.path.as_str()).collect();
            assert!(
                paths.contains(&"entry.typo_field"),
                "the nested unknown field is reported at its path, got {paths:?}"
            );
            assert!(
                paths.contains(&"name_of_thing"),
                "the top-level unknown field is reported at its path, got {paths:?}"
            );
            assert!(
                errs.iter().all(|e| e.code == ValidationCode::UnknownField),
                "both errors carry UnknownField"
            );
        }
        other => panic!("expected SubmitError::Validation, got {other:?}"),
    }
    // Nothing persisted — not even the Root strategy row (the check runs
    // BEFORE any write).
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .expect("count versions");
    assert_eq!(versions, 0, "no version row");
    let strategies: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy")
        .fetch_one(&pool)
        .await
        .expect("count strategies");
    assert_eq!(strategies, 0, "no strategy row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_version_refuses_unknown_fields() {
    let (repo, _pool, _tmp) = repo_with_pool().await;
    let s = repo
        .create_strategy("Unknown Field Repo", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let err = repo
        .create_version(NewVersion {
            strategy_id: s.id.clone(),
            parent_version_id: None,
            dsl_json: doc_with_unknown_fields(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect_err("the strict write prelude refuses unknown fields");
    let message = err.to_string();
    assert!(
        message.to_lowercase().contains("unknown"),
        "the refusal names the unknown field(s), got: {message}"
    );
    let versions = repo.list_versions(&s.id).await.expect("list versions");
    assert!(versions.is_empty(), "nothing was persisted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_fields_in_a_raw_row_load_list_and_backtest() {
    // The SAME document, inserted directly as `dsl_original` the way a pre-r3
    // database row would hold it (bypassing the strict write preludes): the
    // LENIENT read path loads, lists and backtests it without error.
    let (repo, pool, _tmp) = repo_with_pool().await;
    let s = repo
        .create_strategy("Raw Row Loader", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let doc = doc_with_unknown_fields();

    // Compute every row column exactly as the write path would (hash included,
    // so the read path's tamper defense passes).
    let loaded = Migrator::v1()
        .load(&doc)
        .expect("the migrator still loads it");
    let dsl_current = serde_json::to_string(&loaded.dsl).expect("canonical form serializes");
    let hash = version_hash(s.id.as_str(), None, "1.2.0", &doc);
    sqlx::query(
        "INSERT INTO strategy_version \
         (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
          version_hash, created_by, creating_llm_call_ids, created_at) \
         VALUES (?1, ?2, NULL, '1.2.0', ?3, ?4, ?5, '\"human\"', '[]', ?6)",
    )
    .bind("raw-row-version-0001")
    .bind(s.id.as_str())
    .bind(&dsl_current)
    .bind(&doc)
    .bind(&hash)
    .bind("2026-01-01T00:00:00Z")
    .execute(&pool)
    .await
    .expect("raw version row inserts");

    // list + get: the row reads back with the verbatim original.
    let versions = repo.list_versions(&s.id).await.expect("list versions");
    assert_eq!(versions.len(), 1, "the raw row lists");
    assert_eq!(versions[0].dsl_original, doc, "dsl_original stays verbatim");
    let fetched = repo
        .get_version(&versions[0].id)
        .await
        .expect("get version")
        .expect("version exists");
    assert_eq!(fetched.dsl_original, doc, "get_version reads the raw row");

    // And the document backtests over the committed fixture.
    let validated = validate(&fetched.dsl).expect("document validates");
    let compiled = compile(&validated).expect("document compiles");
    let primary = load_primary();
    let filters = BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port");
    let result = run_backtest(
        &compiled,
        &primary,
        None,
        &BacktestConfig::default(),
        &filters,
        SeriesEnd::SnapshotEnd,
        None,
    )
    .expect("the unknown-field document backtests");
    assert!(
        !result.trades.is_empty(),
        "the run is the same non-vacuous MACD run"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_document_with_defaulted_fields_omitted_passes_all_writes() {
    // `macd_doc_at_current(None)` omits every defaulted field (`series`,
    // `output`) — the strict check must accept it on both write paths.
    let (repo, _pool, _tmp) = repo_with_pool().await;
    let s = repo
        .create_strategy("Clean Defaulted", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let created = repo
        .create_version(NewVersion {
            strategy_id: s.id.clone(),
            parent_version_id: None,
            dsl_json: macd_doc_at_current(None),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("the clean doc passes the strict repo prelude");
    let fetched = repo
        .get_version(&created.id)
        .await
        .expect("get version")
        .expect("version exists");
    assert_eq!(
        fetched.dsl_schema_version,
        SchemaVersion::CURRENT,
        "the clean doc persisted at CURRENT"
    );

    let submitted = submit_agent_version(
        &repo,
        SubmitRequest {
            target: SubmitTarget::Root {
                strategy_name: "Clean Defaulted Root".to_owned(),
            },
            dsl: serde_json::from_str(&macd_doc_at_current(Some("signal"))).expect("doc parses"),
            hypothesis: "defaulted-field omission stays clean".to_owned(),
            agent_name: "claude-code".to_owned(),
        },
    )
    .await
    .expect("the clean doc passes the MCP submit prelude");
    assert!(
        !submitted.version.dsl_original.is_empty(),
        "the submit persisted a version"
    );
}

// Keep the unused-helper lints quiet until stage B lands (cases 4 and 5 reuse
// these).
#[allow(unused)]
fn _stage_b_placeholders() {
    let _ = dec(1, 2);
    let _: RiskParams = RiskParams {
        risk_per_trade_pct: SweepableValue::Fixed(dec(1, 2)),
        max_leverage: SweepableValue::Fixed(dec(3, 0)),
    };
    let _: Direction = Direction::Long;
    let _: Option<StrategyDsl> = None;
}
