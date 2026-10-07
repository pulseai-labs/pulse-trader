//! Offline smoke/integration test for `pulse backtest` (VS-1.2.1 work-1.04).
//!
//! **Fixture independence (R3 parallelism, spec §3 + handoff §8):** this test
//! does NOT consume 1.05's golden `rsi-oversold-long.json` (it does not exist in
//! this worktree). Instead it **synthesizes a minimal valid DSL into a tempdir**
//! and runs the CLI against the committed `tests/fixtures/btcusdt-1m-store/`
//! candle fixture — keeping 1.04 ∥ 1.05 file-disjoint.
//!
//! The DSL is the demo-1 "RSI Oversold" shape (long; RSI(14) < 30; 5% stop = 1R;
//! 2R take-profit; 1% risk/trade) — a complete document that passes `validate`
//! (≥1 exit, a `StopLoss` to anchor the `TakeProfit`) so the engine never returns
//! `NoStopLoss`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::process::Command;

use pulse::{CandleSeriesRepository, CandleStore, Pair, Timeframe};

/// A minimal, valid DSL document (serde JSON) — long RSI-oversold with a 5% stop
/// (1R), a 2R take-profit, and 1% risk per trade. Decimals serialize as strings
/// (`rust_decimal` `serde-with-str`); `schema_version` is the CURRENT `"1.0.0"`.
const MINIMAL_DSL: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold (smoke)",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": {
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }
}"#;

/// Write the minimal DSL into a fresh tempdir and return (dir, dsl-path-string).
/// The `TempDir` is returned so the caller keeps it alive for the run's duration.
fn write_minimal_dsl() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("create tempdir for DSL");
    let path = dir.path().join("strategy.json");
    std::fs::write(&path, MINIMAL_DSL).expect("write minimal DSL");
    let path_str = path.to_str().expect("DSL path is utf8").to_owned();
    (dir, path_str)
}

/// AC-5/AC-6 + the user demo criterion: a full `pulse backtest` run over the
/// fixture store succeeds, and the human-readable trade log carries the
/// fee/funding/slippage cost columns + the cost-breakdown footer.
#[test]
fn backtest_cli_runs_over_fixture_and_shows_costs() {
    let (_dir, dsl_path) = write_minimal_dsl();

    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            &dsl_path,
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--store",
            "tests/fixtures/btcusdt-1m-store",
        ])
        .output()
        .expect("run pulse backtest");

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}\nstdout={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");

    // Header row names the cost columns the demo criterion reads ("confirm
    // fees/funding/slippage are deducted").
    assert!(
        stdout.contains("fees") && stdout.contains("funding") && stdout.contains("slippage"),
        "header must name fee/funding/slippage columns; stdout was:\n{stdout}"
    );
    // The cost-breakdown footer totals + net P&L.
    assert!(
        stdout.contains("net_pnl"),
        "footer must report net P&L; stdout was:\n{stdout}"
    );
    assert!(
        stdout.contains("trades="),
        "footer must report the trade count; stdout was:\n{stdout}"
    );
}

/// The VS-1.2.2 work-2.05 user demo criterion (the slice's headline payoff): the
/// human `pulse backtest` readout carries per-trade `mfe_r`/`mae_r`/`regime`
/// columns AND a summary regime-breakdown block with at least one **non-`unknown`**
/// regime, plus an observable `skipped_entries=` line.
#[test]
fn backtest_cli_renders_regime_breakdown_and_mfe_mae() {
    let (_dir, dsl_path) = write_minimal_dsl();

    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            &dsl_path,
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--store",
            "tests/fixtures/btcusdt-1m-store",
        ])
        .output()
        .expect("run pulse backtest");

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");

    // Per-trade excursion + entry-regime columns (the trade-log readout).
    assert!(
        stdout.contains("mfe_r") && stdout.contains("mae_r") && stdout.contains("regime"),
        "trade-log header must name mfe_r/mae_r/regime; stdout was:\n{stdout}"
    );

    // The summary regime-breakdown block (one `regime=<label>` line per regime).
    assert!(
        stdout.contains("regime="),
        "summary must show a regime breakdown line; stdout was:\n{stdout}"
    );

    // At least one NON-`unknown` regime must appear (the EMA200/ADX warms at M15
    // bar ~200 of ~2976, so the bulk of the run is in a determined regime).
    let has_non_unknown = stdout.contains("regime=trending_up")
        || stdout.contains("regime=trending_down")
        || stdout.contains("regime=ranging");
    assert!(
        has_non_unknown,
        "at least one non-`unknown` regime must appear in the breakdown; stdout was:\n{stdout}"
    );

    // The skipped-entries observability line always prints (even at zero).
    assert!(
        stdout.contains("skipped_entries="),
        "summary must show a skipped_entries line; stdout was:\n{stdout}"
    );
}

/// `--json` emits a structured object carrying the trades + run-level cost
/// totals (the same `BacktestResult` surface the demo reads, machine-parseable).
#[test]
fn backtest_cli_json_emits_structured_result() {
    let (_dir, dsl_path) = write_minimal_dsl();

    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            &dsl_path,
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--store",
            "tests/fixtures/btcusdt-1m-store",
            "--json",
        ])
        .output()
        .expect("run pulse backtest --json");

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("stdout is JSON");
    assert!(
        parsed.get("trades").is_some(),
        "JSON must carry the trade log; got:\n{stdout}"
    );
    assert!(
        parsed.get("net_pnl").is_some(),
        "JSON must carry net P&L; got:\n{stdout}"
    );
    assert!(
        parsed.get("fees_total").is_some()
            && parsed.get("funding_total").is_some()
            && parsed.get("slippage_total").is_some(),
        "JSON must carry the cost totals; got:\n{stdout}"
    );
    // VS-1.2.2 work-2.05: the JSON path already serializes the regime breakdown +
    // skipped-entries aggregate (serde rides along — no extra render code), but the
    // e2e asserts they are present so a regression in the result shape is caught.
    assert!(
        parsed.get("regime_breakdown").is_some(),
        "JSON must carry the regime breakdown; got:\n{stdout}"
    );
    assert!(
        parsed.get("skipped_entries").is_some(),
        "JSON must carry the skipped-entries aggregate; got:\n{stdout}"
    );
}

/// A bad `--dsl` path surfaces a clear error and a non-zero exit (no panic) —
/// the spec §3 error-surfacing contract via `anyhow`.
#[test]
fn backtest_cli_missing_dsl_errors_non_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            "/nonexistent/strategy.json",
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--store",
            "tests/fixtures/btcusdt-1m-store",
        ])
        .output()
        .expect("run pulse backtest with bad dsl");

    assert!(
        !output.status.success(),
        "a missing --dsl file must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("pulse:"),
        "the error must be surfaced via the binary shim (no panic); stderr was:\n{stderr}"
    );
}

/// r3.s2 round-1 fix (C1): `--tf 1d` is refused by the request-shape guard
/// BEFORE any candle I/O. D1 is the fixed *signal-only* third series, read
/// through `series: "d1"` operands only, and a D1 primary would accrue at most
/// one of each day's three 8-hourly funding events (`stamp_funding` keeps the
/// last) — every funding total, and so net `PnL` and R, would be understated.
/// The refusal names the member, so the user sees why rather than a
/// missing-snapshot error.
#[test]
fn d1_primary_timeframe_is_refused_before_any_store_read() {
    let (_dir, dsl_path) = write_minimal_dsl();
    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            &dsl_path,
            "--pair",
            "BTCUSDT",
            "--tf",
            "1d",
            "--store",
            "tests/fixtures/btcusdt-1m-store",
        ])
        .output()
        .expect("run pulse backtest with a D1 primary");

    assert!(
        !output.status.success(),
        "a D1 primary timeframe must exit non-zero; stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must not be D1"),
        "the refusal must name the D1 primary; stderr was:\n{stderr}"
    );
}

/// r4.s1.w2 (#52): `pulse backtest` on a pair the exchange adapter does not
/// know exits non-zero with a message naming the pair — the refusal comes from
/// the adapter port, offline — while the three new pairs (ETHUSDT, SOLUSDT,
/// XRPUSDT) run the same fixture snapshot exactly as BTCUSDT does.
#[test]
fn backtest_cli_unknown_pair_is_refused_while_the_new_pairs_run() {
    // A temp store seeded per pair from the committed BTCUSDT fixture — the
    // store accepts any pair, so the three new pairs get real HEAD snapshots,
    // and DOGEUSDT is seeded too so ITS refusal is the adapter's unknown symbol
    // rather than a missing snapshot.
    let fixture = CandleStore::with_base_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store"),
    );
    let m15 = fixture
        .load_head(&Pair::new("BTCUSDT"), Timeframe::M15)
        .expect("load the fixture HEAD")
        .expect("the fixture has a BTCUSDT M15 HEAD")
        .series
        .candles;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let store_dir = tmp.path().join("store");
    let store = CandleStore::with_base_dir(store_dir.clone());
    for pair in ["ETHUSDT", "SOLUSDT", "XRPUSDT", "DOGEUSDT"] {
        store
            .commit(&Pair::new(pair), Timeframe::M15, m15.clone())
            .unwrap_or_else(|e| panic!("commit {pair}: {e}"));
    }
    let store_arg = store_dir.to_str().expect("store path is utf8");

    let (_dir, dsl_path) = write_minimal_dsl();

    // The three new pairs take the ordinary path: exit 0 over their snapshots.
    for pair in ["ETHUSDT", "SOLUSDT", "XRPUSDT"] {
        let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
            .args([
                "backtest", "--dsl", &dsl_path, "--pair", pair, "--tf", "M15", "--store", store_arg,
            ])
            .output()
            .expect("run pulse backtest");
        assert!(
            output.status.success(),
            "{pair} must run like BTCUSDT does; status={:?}\nstderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // The unknown pair is refused, naming the pair.
    let output = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest", "--dsl", &dsl_path, "--pair", "DOGEUSDT", "--tf", "M15", "--store",
            store_arg,
        ])
        .output()
        .expect("run pulse backtest on an unknown pair");
    assert!(
        !output.status.success(),
        "an unknown pair must exit non-zero; stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("DOGEUSDT"),
        "the refusal must name the pair; stderr was:\n{stderr}"
    );
}
