//! r5.s4.w1 (PDR-0035, d102) — the streak and drawdown card.
//!
//! The hand-checked cases, the PDR-0006 anchor with its frozen goldens, the
//! observed-vs-stored proof, the determinism proof, and the read contract over
//! the spawned server.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use pulse::{
    BacktestResult, BacktestRunId, BacktestRunRepository, ClosedTrade, EngineFingerprint,
    EquityCurve, RegimeBreakdown, SkippedEntryCounts, SqliteBacktestRunRepo, StreakCard,
    StreakCardDto, StreakCardRequest, SummaryStats, Trade, VersionId, WalkForwardRunRepository,
    streak_card, streak_card_core,
};
use rust_decimal::Decimal;
use support::mcp::{seeded_inputs, seeded_trade_at_r, seeded_walk_forward_draft};
use support::server::{ServerOptions, TestServer, seed_version, spawn_server};

/// One closed trade as the card consumes it: `(realized_r, realized_pnl)`.
fn trade(realized_r: i64, realized_pnl: i64) -> ClosedTrade {
    ClosedTrade {
        realized_r: Decimal::new(realized_r, 0),
        realized_pnl: Decimal::new(realized_pnl, 0),
    }
}

/// `n` losing trades: `−1R`, `−100` USDT each.
fn losing(n: usize) -> Vec<ClosedTrade> {
    vec![trade(-1, -100); n]
}

/// `n` winning trades: `+2R`, `+200` USDT each.
fn winning(n: usize) -> Vec<ClosedTrade> {
    vec![trade(2, 200); n]
}

/// The PDR-0006 anchor: 100 trades in the cycle `[W, W, L, L, L] × 20` — 40
/// winners at `(+2R, +200)` against 60 losers at `(−1R, −100)`, the 40% win
/// rate PDR-0006 names. The cycle keeps the observed row hand-checkable: the
/// deepest dip is the three losers — 3 trades, 3R, 300 USDT, 3.00%.
fn anchor() -> Vec<ClosedTrade> {
    let mut trades = Vec::with_capacity(100);
    for _ in 0..20 {
        trades.extend([
            trade(2, 200),
            trade(2, 200),
            trade(-1, -100),
            trade(-1, -100),
            trade(-1, -100),
        ]);
    }
    trades
}

/// Print probe for the anchor's frozen goldens: run
/// `cargo test --test streak_card -- --ignored print_anchor_golden --nocapture`
/// and freeze the printed figures in `the_pdr_0006_anchor_freezes_its_goldens`.
#[test]
#[ignore = "print probe for the frozen goldens"]
fn print_anchor_golden() {
    let StreakCard::Ready {
        streak,
        drawdown_r,
        drawdown_usdt,
        drawdown_pct,
        ..
    } = streak_card(&anchor())
    else {
        panic!("the anchor is 100 closed trades");
    };
    println!(
        "anchor golden: streak p50={} p95={} observed={}",
        streak.p50, streak.p95, streak.observed
    );
    println!(
        "anchor golden: drawdown_r p50={} p95={} observed={}",
        drawdown_r.p50, drawdown_r.p95, drawdown_r.observed
    );
    println!(
        "anchor golden: drawdown_usdt p50={} p95={} observed={}",
        drawdown_usdt.p50, drawdown_usdt.p95, drawdown_usdt.observed
    );
    println!(
        "anchor golden: drawdown_pct p50={} p95={} observed={}",
        drawdown_pct.p50, drawdown_pct.p95, drawdown_pct.observed
    );
}

/// The ready card, or a panic naming what came back instead.
fn ready(trades: &[ClosedTrade]) -> StreakCard {
    let card = streak_card(trades);
    assert!(
        matches!(card, StreakCard::Ready { .. }),
        "expected a ready card, got {card:?}"
    );
    card
}

#[test]
fn forty_all_losing_trades_read_a_full_house() {
    let card = ready(&losing(40));
    let StreakCard::Ready {
        closed_trades,
        horizon_trades,
        paths,
        seed,
        streak,
        drawdown_r,
        drawdown_usdt,
        drawdown_pct,
    } = card
    else {
        unreachable!("`ready` asserted the variant")
    };
    assert_eq!(closed_trades, 40);
    assert_eq!(horizon_trades, pulse::HORIZON_TRADES);
    assert_eq!(paths, pulse::PATHS);
    assert_eq!(seed, pulse::STREAK_CARD_SEED);
    // Every drawn path is 100 losses, so every percentile is the horizon.
    assert_eq!((streak.p50, streak.p95), (100, 100));
    assert_eq!(streak.observed, 40, "the run's own 40 losses");
    assert_eq!(
        (drawdown_r.p50, drawdown_r.p95),
        (Decimal::from(100), Decimal::from(100))
    );
    assert_eq!(drawdown_r.observed, Decimal::from(40));
    assert_eq!(
        (drawdown_usdt.p50, drawdown_usdt.p95),
        (Decimal::from(10_000), Decimal::from(10_000))
    );
    assert_eq!(drawdown_usdt.observed, Decimal::from(4_000));
    assert_eq!(
        (drawdown_pct.p50, drawdown_pct.p95),
        (Decimal::new(10_000, 2), Decimal::new(10_000, 2))
    );
    assert_eq!(drawdown_pct.observed, Decimal::new(4_000, 2));
}

#[test]
fn the_pdr_0006_anchor_freezes_its_goldens() {
    let StreakCard::Ready {
        streak,
        drawdown_r,
        drawdown_usdt,
        drawdown_pct,
        ..
    } = streak_card(&anchor())
    else {
        panic!("the anchor is 100 closed trades");
    };
    // PDR-0006's own reading of the anchor: a 40% win rate over 100 trades
    // shows a typical longest losing streak near 7.
    assert!(
        (6..=8).contains(&streak.p50),
        "the typical longest losing streak sits near 7, got {}",
        streak.p50
    );
    // The exact figures the pinned seed gives — printed by `print_anchor_golden`.
    assert_eq!((streak.p50, streak.p95), (7, 12));
    assert_eq!(streak.observed, 3, "the cycle's deepest losing run");
    assert_eq!(
        (drawdown_r.p50, drawdown_r.p95),
        (Decimal::from(10), Decimal::from(19))
    );
    assert_eq!(drawdown_r.observed, Decimal::from(3));
    assert_eq!(
        (drawdown_usdt.p50, drawdown_usdt.p95),
        (Decimal::from(1_000), Decimal::from(1_900))
    );
    assert_eq!(drawdown_usdt.observed, Decimal::from(300));
    assert_eq!(
        (drawdown_pct.p50, drawdown_pct.p95, drawdown_pct.observed),
        (
            Decimal::new(1_000, 2),
            Decimal::new(1_900, 2),
            Decimal::new(300, 2)
        )
    );
}

#[test]
fn forty_all_winning_trades_read_zero_everywhere() {
    let card = ready(&vec![trade(2, 200); 40]);
    let StreakCard::Ready {
        streak,
        drawdown_r,
        drawdown_usdt,
        drawdown_pct,
        ..
    } = card
    else {
        unreachable!("`ready` asserted the variant")
    };
    assert_eq!((streak.p50, streak.p95, streak.observed), (0, 0, 0));
    assert_eq!(
        (
            drawdown_r.p50,
            drawdown_r.p95,
            drawdown_r.observed,
            drawdown_usdt.observed,
        ),
        (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO)
    );
    assert_eq!(
        (drawdown_pct.p50, drawdown_pct.p95, drawdown_pct.observed),
        (Decimal::new(0, 2), Decimal::new(0, 2), Decimal::new(0, 2))
    );
}

#[test]
fn twenty_nine_closed_trades_is_not_enough() {
    assert_eq!(
        streak_card(&losing(29)),
        StreakCard::NotEnoughTrades { closed_trades: 29 }
    );
}

#[test]
fn thirty_closed_trades_is_enough() {
    assert!(matches!(
        streak_card(&winning(30)),
        StreakCard::Ready { .. }
    ));
}

#[test]
fn a_breakeven_trade_resets_the_losing_streak() {
    // 20 losers, one breakeven `realized_pnl == 0`, 20 losers: the longest run
    // is 20, not 40 — the same rule `SummaryStats::max_loss_streak` applies.
    let mut trades = losing(20);
    trades.push(trade(0, 0));
    trades.extend(losing(20));
    let card = ready(&trades);
    let StreakCard::Ready { streak, .. } = card else {
        unreachable!("`ready` asserted the variant")
    };
    assert_eq!(streak.observed, 20);
    // The breakeven carries 0R and 0 USDT, so the measures read the 40 losing
    // trades' own decline below the 0 peak — one continuous drawdown, 40 deep.
    let StreakCard::Ready {
        drawdown_r,
        drawdown_usdt,
        drawdown_pct,
        ..
    } = ready(&trades)
    else {
        unreachable!("`ready` asserted the variant")
    };
    assert_eq!(drawdown_r.observed, Decimal::from(40));
    assert_eq!(drawdown_usdt.observed, Decimal::from(4_000));
    assert_eq!(drawdown_pct.observed, Decimal::new(4_000, 2));
}

// ---------------------------------------------------------------------------
// The read contract (PDR-0035 Q4): the version's latest FULL-SPAN backtest
// run, named, over the spawned server.
// ---------------------------------------------------------------------------

/// The 40-trade mixed run the read-contract tests store: the anchor's own
/// cycle (`[W, W, L, L, L] × 8`), so the stored run and the card describe the
/// same population.
fn mixed_rs() -> Vec<i64> {
    let mut rs = Vec::with_capacity(40);
    for _ in 0..8 {
        rs.extend([2, 2, -1, -1, -1]);
    }
    rs
}

/// Save one full-span run of `rs.len()` closed trades against `version` on the
/// server's own pool, through the same repository the read path uses.
async fn seed_full_span_run(server: &TestServer, version: &VersionId, rs: &[i64]) -> String {
    let trades: Vec<Trade> = rs
        .iter()
        .enumerate()
        .map(|(i, r)| seeded_trade_at_r(i64::try_from(i).unwrap_or(0), Decimal::new(*r, 0)))
        .collect();
    let starting_equity = Decimal::new(10_000, 0);
    let fees_total = trades
        .iter()
        .fold(Decimal::ZERO, |acc, t| acc + t.fees_total);
    let net_pnl = trades
        .iter()
        .fold(Decimal::ZERO, |acc, t| acc + t.realized_pnl);
    let equity_curve = EquityCurve::from_trades(0, starting_equity, &trades);
    let summary =
        SummaryStats::from_trades(&trades, net_pnl, fees_total, Decimal::ZERO, &equity_curve);
    let result = BacktestResult {
        trades,
        net_pnl,
        fees_total,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        summary,
        regime_breakdown: RegimeBreakdown::new(),
        skipped_entries: SkippedEntryCounts::new(),
        open_position: None,
        engine_fingerprint: EngineFingerprint::current(),
        equity_curve,
    };
    let runs = SqliteBacktestRunRepo::new(server.db.pool().clone());
    runs.save_run(
        version,
        &seeded_inputs(),
        &result,
        &result.summary,
        starting_equity,
    )
    .await
    .expect("the seeded full-span run persists")
    .as_str()
    .to_owned()
}

/// The card the core answers for `version`.
async fn card_for(state: &pulse::DesktopState, version: &VersionId) -> StreakCardDto {
    streak_card_core(
        state,
        StreakCardRequest {
            version_id: version.as_str().to_owned(),
        },
    )
    .await
    .expect("the card reads")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_observed_row_equals_the_runs_stored_stats() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let run_id = seed_full_span_run(&server, &version, &mixed_rs()).await;

    let run = SqliteBacktestRunRepo::new(server.db.pool().clone())
        .get_run(&BacktestRunId::new(&run_id))
        .await
        .expect("the run reads back")
        .expect("the run exists");
    let dto = card_for(&state, &version).await;
    let StreakCardDto::Ready {
        run_id: named,
        streak,
        drawdown_usdt,
        ..
    } = dto
    else {
        panic!("the run has 40 closed trades: {dto:?}")
    };
    assert_eq!(named, run_id, "the card names the run it read");
    assert_eq!(
        streak.observed,
        u32::try_from(run.summary.max_loss_streak).unwrap_or(u32::MAX),
        "the card's observed streak is the run's stored max_loss_streak"
    );
    assert_eq!(
        drawdown_usdt
            .observed
            .parse::<Decimal>()
            .expect("an exact decimal string"),
        run.summary.max_drawdown,
        "the card's observed USDT drawdown is the run's stored max_drawdown"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_walk_forward_fold_still_reads_the_full_span_run() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let run_id = seed_full_span_run(&server, &version, &mixed_rs()).await;
    // A walk-forward lands AFTER the full-span run, so its fold rows are the
    // newer ones: the read must still name the full-span run (PDR-0035 Q4).
    SqliteBacktestRunRepo::new(server.db.pool().clone())
        .save_walk_forward_run(&version, &seeded_walk_forward_draft(true))
        .await
        .expect("the walk-forward rows persist");

    let dto = card_for(&state, &version).await;
    let StreakCardDto::Ready { run_id: named, .. } = dto else {
        panic!("the full-span run is on the version: {dto:?}")
    };
    assert_eq!(named, run_id, "a fold is never the card's run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_version_without_a_run_answers_no_run() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    assert_eq!(card_for(&state, &version).await, StreakCardDto::NoRun);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_version_id_is_a_not_found_refusal() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let err = streak_card_core(
        &state,
        StreakCardRequest {
            version_id: "ver-names-nothing".to_owned(),
        },
    )
    .await
    .expect_err("an unknown version is refused");
    let body = serde_json::to_value(&err).expect("the error serializes");
    assert_eq!(body["code"], "not_found");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_route_is_app_scope_and_carries_the_tagged_dto() {
    let server = spawn_server(ServerOptions::default()).await;
    let state = server.desktop().await;
    let version = seed_version(&state).await;
    let run_id = seed_full_span_run(&server, &version, &mixed_rs()).await;

    let client = reqwest::Client::new();
    let card = client
        .post(format!("{}/api/v1/streak-card", server.base))
        .bearer_auth(&server.app_token)
        .json(&serde_json::json!({ "versionId": version.as_str() }))
        .send()
        .await
        .expect("POST the app route");
    assert_eq!(card.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = card.json().await.expect("the route's JSON");
    assert_eq!(body["state"], "ready");
    assert_eq!(body["runId"], run_id);
    assert_eq!(body["closedTrades"], 40);
    assert_eq!(body["horizonTrades"], 100);
    assert_eq!(body["paths"], 10_000);
    assert!(body["seed"].is_string(), "the seed crosses as text: {body}");
    assert!(
        body["streak"]["p50"].is_u64(),
        "a count crosses as a number: {body}"
    );
    assert!(
        body["drawdownR"]["p95"].is_string(),
        "a decimal crosses as an exact string: {body}"
    );
    assert!(body["drawdownPct"]["observed"].is_string());

    let unknown = client
        .post(format!("{}/api/v1/streak-card", server.base))
        .bearer_auth(&server.app_token)
        .json(&serde_json::json!({ "versionId": "ver-names-nothing" }))
        .send()
        .await
        .expect("POST an unknown version");
    assert_eq!(
        unknown.status(),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown version id is a 422 through the existing error mapping"
    );
    let body: serde_json::Value = unknown.json().await.expect("the refusal's JSON");
    assert_eq!(body["code"], "not_found");

    let refused = client
        .post(format!("{}/api/v1/streak-card", server.base))
        .bearer_auth(&server.agent_token)
        .json(&serde_json::json!({ "versionId": version.as_str() }))
        .send()
        .await
        .expect("POST the app route with an agent token");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::FORBIDDEN,
        "an agent token cannot read the card"
    );
    let body: serde_json::Value = refused.json().await.expect("the refusal's JSON");
    assert_eq!(body["code"], "scope_refused");
}

// ---------------------------------------------------------------------------
// Determinism (SPINE risk gate): the same trades give the same card, repeated
// and in parallel.
// ---------------------------------------------------------------------------

#[test]
fn the_card_is_identical_across_repeated_and_parallel_runs() {
    let trades = anchor();
    let baseline = streak_card(&trades);
    for round in 0..20 {
        assert_eq!(
            streak_card(&trades),
            baseline,
            "sequential rerun {round} differs"
        );
    }
    let parallel: Vec<StreakCard> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..20)
            .map(|_| scope.spawn(|| streak_card(&trades)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a parallel card"))
            .collect()
    });
    for (round, card) in parallel.iter().enumerate() {
        assert_eq!(card, &baseline, "parallel card {round} differs");
    }
}

/// The timing probe for one card: [`HORIZON_TRADES`] × [`PATHS`] draws with
/// replacement over 179 trades — the d101 version's own run length (the SPINE's
/// "worth re-checking": one card stays well under a second). Run
/// `cargo test --test streak_card -- --ignored timing_probe_card_over_179_trades --nocapture`.
#[test]
#[ignore = "timing probe: a measurement, not a gate"]
fn timing_probe_card_over_179_trades() {
    let trades: Vec<ClosedTrade> = (0..179)
        .map(|i| {
            if i % 5 < 2 {
                trade(2, 200)
            } else {
                trade(-1, -100)
            }
        })
        .collect();
    let started = std::time::Instant::now();
    let card = streak_card(&trades);
    let elapsed = started.elapsed();
    assert!(
        matches!(card, StreakCard::Ready { .. }),
        "179 closed trades is a ready card"
    );
    println!(
        "timing probe: one card over {} trades = {} draws in {elapsed:?}",
        trades.len(),
        pulse::HORIZON_TRADES * pulse::PATHS
    );
}
