//! r4.s1.w3 — the measurement harness behind ADR-0028's frozen tables.
//!
//! Every test here is `#[ignore]`d, so the default suite never runs them (AC-7
//! is untouched). Re-derive the ADR's numbers with:
//!
//! ```text
//! cargo nextest run --run-ignored ignored-only --test wf_v2_measurement
//! ```
//!
//! and, for the real-data rows, a release build and the data directory:
//!
//! ```text
//! PULSE_WF_V2_DATA_DIR=<data dir> PULSE_ALLOW_PLACEHOLDER_DIST=1 \
//!   cargo nextest run --release --run-ignored ignored-only --test wf_v2_measurement
//! ```
//!
//! Each test's doc names the ADR-0028 table it produces. The real-data tests
//! read their data directory from `PULSE_WF_V2_DATA_DIR` and SKIP with a
//! message when it is unset: no market data is committed, and prod's data
//! directory is never read.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::path::PathBuf;

use pulse::{
    Atr, BacktestConfig, BinanceAdapter, Candle, CandleStore, CandleWindow, CreatedBy, FoldVerdict,
    Indicator, Migrator, NewVersion, Pair, SnapshotPins, SqliteBacktestRunRepo, SqliteStrategyRepo,
    StrategyRepository, Timeframe, VerdictRule, fold_windows, run_walk_forward,
};
use rust_decimal::Decimal;
use support::mcp::{manifest, migrated_db};
use support::rng::{SplitMix64, normal_decimal};
use tempfile::TempDir;

/// 2021-01-01T00:00:00Z — the search span's start (SPINE grill Q1).
const SEARCH_FROM_MS: i64 = 1_609_459_200_000;
/// 2025-07-01T00:00:00Z — the holdout start, and the search span's exclusive
/// end, for all four pairs.
const SEARCH_TO_MS: i64 = 1_751_328_000_000;

/// The planted volatility the calibration and the power table share: σ = 1.2R.
const SIGMA_R: f64 = 1.2;
/// The planted edge the calibration and the power table share.
const PLANTED_MEAN_R: f64 = 0.25;
/// The calibration's fold shape: K = 6 folds of 100 trades.
const CALIBRATION_K: u8 = 6;
const FOLD_TRADES: usize = 100;

// ---------------------------------------------------------------------------
// The synthetic side: wf-v2's α and the C1 test's power
// ---------------------------------------------------------------------------

/// One seed's wf-v2 verdict at `mean_r`, on the same stream the calibration
/// test pins (`support::rng`).
fn wf_v2_passes(seed: u64, mean_r: f64) -> bool {
    let mut rng = SplitMix64::new(seed);
    let folds: Vec<Vec<Decimal>> = (0..CALIBRATION_K)
        .map(|_| {
            (0..FOLD_TRADES)
                .map(|_| normal_decimal(&mut rng, mean_r, SIGMA_R))
                .collect()
        })
        .collect();
    let fold_verdicts: Vec<FoldVerdict> = folds
        .iter()
        .map(|rs| VerdictRule::WfV2.assess_fold(rs))
        .collect();
    let pooled: Vec<Decimal> = folds.iter().flatten().copied().collect();
    VerdictRule::WfV2.assess_run(&fold_verdicts, &pooled).pass
}

/// One seed's C1 holdout verdict at `n` holdout trades of the planted edge.
fn holdout_passes(seed: u64, mean_r: f64, n: usize, h: u8) -> bool {
    let mut rng = SplitMix64::new(seed);
    let rs: Vec<Decimal> = (0..n)
        .map(|_| normal_decimal(&mut rng, mean_r, SIGMA_R))
        .collect();
    pulse::holdout_test(&rs, h).passes
}

/// ADR-0028 table: **α, synthetic** — wf-v2's zero-edge false-pass rate over the
/// fixed seed set 1..=200 (K = 6, 100 trades per fold, σ = 1.2R), and the Q2
/// bound `1 − (1 − α/2)^12`.
#[test]
#[ignore = "measurement harness — ADR-0028: alpha, synthetic"]
fn synthetic_alpha_over_200_seeds() {
    let passing = (1..=200_u64).filter(|s| wf_v2_passes(*s, 0.0)).count();
    let alpha = f64::from(u32::try_from(passing).expect("at most 200 seeds")) / 200.0;
    let q2_bound = 1.0 - (1.0 - alpha / 2.0).powi(12);
    println!(
        "ADR-0028 alpha, synthetic: {passing}/200 zero-edge seeds pass \
         (alpha = {alpha:.4}); Q2 bound 1-(1-alpha/2)^12 = {q2_bound:.4}"
    );
    assert_eq!(
        passing, PINNED_SYNTHETIC_ALPHA_PASSES,
        "the frozen alpha count (rule or generator change?)"
    );
}

/// The exact zero-edge count measured on 2026-10-08 (this item) — the pin the
/// ADR's α (0.05) and its Q2 bound (0.2620) are derived from.
const PINNED_SYNTHETIC_ALPHA_PASSES: usize = 10;

/// ADR-0028 table: **the C1 test's power** — the pass rate at H = 12 and H = 6
/// for the planted +0.25R edge, σ = 1.2R, at 100/150/160/200/250 holdout
/// trades, over the fixed seed set 1..=1000.
#[test]
#[ignore = "measurement harness — ADR-0028: C1 power table"]
fn holdout_power_table_over_1000_seeds() {
    let mut measured = Vec::new();
    for h in [12_u8, 6] {
        for n in [100_usize, 150, 160, 200, 250] {
            let passing = (1..=1000_u64)
                .filter(|s| holdout_passes(*s, PLANTED_MEAN_R, n, h))
                .count();
            println!(
                "ADR-0028 power: H={h} holdout trades={n} -> {passing}/1000 = {:.3}",
                f64::from(u32::try_from(passing).expect("at most 1000 seeds")) / 1000.0
            );
            measured.push((h, n, passing));
        }
    }
    for (h, n, passing) in measured {
        let pinned = PINNED_POWER
            .iter()
            .find(|(ph, pn, _)| *ph == h && *pn == n)
            .map(|(_, _, p)| *p)
            .expect("every measured cell is pinned");
        assert_eq!(
            passing, pinned,
            "H={h} trades={n}: the frozen power count (rule or generator change?)"
        );
    }
}

/// The exact power counts measured on 2026-10-08 (this item) — `(H, trades,
/// passes of 1000)`, the numbers the ADR's table carries.
const PINNED_POWER: [(u8, usize, usize); 10] = [
    (12, 100, 312),
    (12, 150, 455),
    (12, 160, 486),
    (12, 200, 628),
    (12, 250, 734),
    (6, 100, 395),
    (6, 150, 557),
    (6, 160, 592),
    (6, 200, 716),
    (6, 250, 817),
];

// ---------------------------------------------------------------------------
// The real-path harness null (C2): zero-cost random entries on real candles
// ---------------------------------------------------------------------------

/// The engine's own ATR(14) over every candle, `None` while warming.
fn atr_series(candles: &[Candle], period: u32) -> Vec<Option<Decimal>> {
    let mut atr = Atr::new(period).expect("period >= 1");
    candles.iter().map(|c| atr.next(c)).collect()
}

/// One seed's zero-cost random-entry walk-forward under wf-v2, over the real
/// M15 candles and the search span's six folds (ADR-0028's harness-null
/// conventions: entries drawn by the seeded PRNG at `1/96` per bar, long at
/// the next bar's open, stop `1.0·ATR(14)` / target `1.5·ATR(14)`, the stop
/// taken first on a same-bar both-touch, the span end closing at the bar's
/// close, zero fees, slippage and funding).
fn null_seed_passes(
    candles: &[Candle],
    atr: &[Option<Decimal>],
    folds: &[CandleWindow],
    seed: u64,
) -> bool {
    const ONE_R: Decimal = Decimal::ONE;
    let mut rng = SplitMix64::new(seed);
    let mut per_fold: Vec<Vec<Decimal>> = vec![Vec::new(); folds.len()];
    let mut i = 0_usize;
    while i + 1 < candles.len() {
        let draw = rng.next_f64();
        let in_span = candles[i].open_time >= SEARCH_FROM_MS && candles[i].open_time < SEARCH_TO_MS;
        let atr_value = atr[i].filter(|a| *a > Decimal::ZERO);
        if in_span
            && draw < 1.0 / 96.0
            && let Some(atr_value) = atr_value
        {
            let entry_index = i + 1;
            let entry = candles[entry_index].open;
            let stop = entry - atr_value;
            let target = entry + atr_value * Decimal::new(15, 1);
            let mut exit_index = entry_index;
            let mut realized = None;
            while exit_index < candles.len() {
                let bar = &candles[exit_index];
                if bar.low <= stop {
                    realized = Some(-ONE_R);
                    break;
                }
                if bar.high >= target {
                    realized = Some(Decimal::new(15, 1));
                    break;
                }
                if bar.open_time >= SEARCH_TO_MS {
                    realized = Some((bar.close - entry) / atr_value);
                    break;
                }
                exit_index += 1;
            }
            let realized = realized.unwrap_or_else(|| {
                let last = candles.last().expect("a non-empty series");
                (last.close - entry) / atr_value
            });
            if let Some(slot) = folds.iter().position(|w| {
                candles[entry_index].open_time >= w.from_ms
                    && candles[entry_index].open_time < w.to_ms
            }) {
                per_fold[slot].push(realized);
            }
            i = exit_index;
            continue;
        }
        i += 1;
    }
    let fold_verdicts: Vec<FoldVerdict> = per_fold
        .iter()
        .map(|rs| VerdictRule::WfV2.assess_fold(rs))
        .collect();
    let pooled: Vec<Decimal> = per_fold.iter().flatten().copied().collect();
    VerdictRule::WfV2.assess_run(&fold_verdicts, &pooled).pass
}

/// ADR-0028 table: **α, real-path harness null (C2)** — the wf-v2 pass rate of
/// zero-cost seeded random-entry strategies on the real M15 search span of
/// BTCUSDT and ETHUSDT, seeds 1..=200 per pair. Needs `PULSE_WF_V2_DATA_DIR`
/// (the data directory whose `candles/` tree holds the snapshots); skips with a
/// message when unset.
#[test]
#[ignore = "measurement harness — ADR-0028: alpha, real-path harness null (needs PULSE_WF_V2_DATA_DIR)"]
fn real_path_null_alpha() {
    let Some(dir) = std::env::var_os("PULSE_WF_V2_DATA_DIR") else {
        eprintln!(
            "SKIP real_path_null_alpha: PULSE_WF_V2_DATA_DIR is unset — no market data is \
             committed; point it at a data directory (e.g. the round-1 scratch store's \
             pulsetrader data dir)"
        );
        return;
    };
    let store = CandleStore::with_base_dir(PathBuf::from(dir));
    let span = CandleWindow::new(SEARCH_FROM_MS, SEARCH_TO_MS).expect("an ordered span");
    let folds = fold_windows(&span, CALIBRATION_K);
    for pair_name in ["BTCUSDT", "ETHUSDT"] {
        let pair = Pair::new(pair_name);
        let head = store
            .read_head(&pair, Timeframe::M15)
            .expect("read_head")
            .expect("the store has an M15 HEAD");
        let candles = store
            .read_snapshot(&pair, Timeframe::M15, &head)
            .expect("read_snapshot")
            .candles;
        let atr = atr_series(&candles, 14);
        let passing = (1..=200_u64)
            .filter(|s| null_seed_passes(&candles, &atr, &folds, *s))
            .count();
        println!(
            "ADR-0028 alpha, real-path harness null {pair_name}: {passing}/200 zero-edge seeds \
             pass over {pair_name} M15 [{SEARCH_FROM_MS}, {SEARCH_TO_MS})"
        );
    }
}

// ---------------------------------------------------------------------------
// The runtime row: one wf-v2 walk-forward over the full search span
// ---------------------------------------------------------------------------

/// ADR-0028 row: the **runtime** of one wf-v2 walk-forward over BTCUSDT M15 +
/// H4 on the full search span (the SPINE's "worth re-checking" item), with
/// `from` defaulted to the first fully-warm bar. Needs `PULSE_WF_V2_DATA_DIR`;
/// skips with a message when unset.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "measurement harness — ADR-0028: runtime row (needs PULSE_WF_V2_DATA_DIR)"]
async fn runtime_of_one_wf_v2_walk_forward() {
    let Some(dir) = std::env::var_os("PULSE_WF_V2_DATA_DIR") else {
        eprintln!(
            "SKIP runtime_of_one_wf_v2_walk_forward: PULSE_WF_V2_DATA_DIR is unset — no market \
             data is committed"
        );
        return;
    };
    let tmp = TempDir::new().expect("a temp dir");
    let (_path, db) = migrated_db(&tmp).await;
    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("wf-v2 runtime", None, &[])
        .await
        .expect("create strategy");
    let dsl_json =
        std::fs::read_to_string(manifest("tests/fixtures/strategies/rsi-oversold-long.json"))
            .expect("the oracle fixture reads");
    let dsl = Migrator::v1().load(&dsl_json).expect("dsl loads").dsl;
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: serde_json::to_string(&dsl).expect("dsl serializes"),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version")
        .id;

    let store = CandleStore::with_base_dir(PathBuf::from(dir));
    let pair = Pair::new("BTCUSDT");
    let m15 = store
        .read_head(&pair, Timeframe::M15)
        .expect("read_head")
        .expect("the store has an M15 HEAD");
    let h4 = store
        .read_head(&pair, Timeframe::H4)
        .expect("read_head")
        .expect("the store has an H4 HEAD");

    let started = std::time::Instant::now();
    let outcome = run_walk_forward(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &pulse::WalkForwardRequest {
            version_id: version,
            pair,
            primary_timeframe: Timeframe::M15,
            htf_timeframe: Some(Timeframe::H4),
            config: BacktestConfig::default(),
            snapshots: Some(SnapshotPins {
                primary: m15,
                htf: Some(h4),
                d1: None,
            }),
            // `from` defaults to the first fully-warm bar (2021-01-01 precedes
            // it, and an explicit earlier bound refuses `FromBeforeWarm`).
            from_ms: None,
            to_ms: Some(SEARCH_TO_MS),
            k: Some(i32::from(CALIBRATION_K)),
            rule: Some(VerdictRule::WfV2),
        },
    )
    .await
    .expect("the wf-v2 walk-forward completes over the real search span");
    let elapsed = started.elapsed();

    assert_eq!(outcome.run.rule, VerdictRule::WfV2);
    println!(
        "ADR-0028 runtime: one wf-v2 walk-forward BTCUSDT M15+H4 [first warm, 2025-07-01), \
         k={} took {elapsed:?}; span [{}, {}), {} out-of-sample trades, folds holding {}/{}",
        outcome.run.scheme.k(),
        outcome.run.span.from_ms,
        outcome.run.span.to_ms,
        outcome.run.verdict.pooled.n,
        outcome.run.verdict.folds_holding,
        outcome.run.verdict.folds_required,
    );
    assert!(
        outcome.run.verdict.pooled.n > 0,
        "the run must actually trade to be a runtime measurement"
    );
}
