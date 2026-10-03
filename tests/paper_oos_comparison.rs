//! AC-4 — the OOS comparison (r3.s4.w4, spec §3; A8/A12/E3): the pure
//! `comparison` over a replayed log, plus the w3-era replay arm.
//!
//! - `Pending` until `min_trades` (exactly 19, then 20);
//! - `Within`, `Below` and `Above` against a certifying run with known fold
//!   `mean_r` values;
//! - a fixture-certified session ⇒ `NotApplicable` with the A12 text;
//! - an override session ⇒ `NotApplicable`;
//! - two epochs ⇒ `engine_builds = 2`;
//! - a foreign certifying fingerprint ⇒ `certification_stale = true` while the
//!   comparison still renders;
//! - w3-era fills without `realized_r` replay, and are not counted toward `n`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use pulse::{
    BacktestRunId, CandleWindow, ComparisonVerdict, EngineFingerprint, FoldVerdict, Graduation,
    NonEmptyLabel, NonEmptyReason, OosComparison, PaperEvent, PaperSession, PaperSessionId,
    PaperSessionState, PaperSide, RunVerdict, Timeframe, VersionId, WalkForwardFold,
    WalkForwardRun, WalkForwardRunId, comparison,
};
use rust_decimal::Decimal;

const CURRENT: &str = "a57cfadd63f4177d2a6b1a63fd7c915732661bb81d6a4ca4c18c692c56c104fa";
const FOREIGN: &str = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

/// The A12 text, verbatim.
const FIXTURE_TEXT: &str = "OOS comparison: n/a, certified on fixture data";

fn session(graduation: Graduation, fixture: bool) -> PaperSession {
    PaperSession {
        id: PaperSessionId::new("session-1".to_owned()),
        seq: 1,
        strategy_version_id: VersionId::new("version-1".to_owned()),
        created_at: "2025-02-01T00:00:00.000Z".to_owned(),
        pair: pulse::Pair::new("BTCUSDT"),
        primary_timeframe: Timeframe::M15,
        htf_timeframe: None,
        uses_d1: false,
        starting_equity: Decimal::from(10_000),
        taker_fee_bps: Decimal::from(4),
        slippage_bps: Decimal::from(1),
        engine_fingerprint: EngineFingerprint::from_stored(CURRENT.to_owned()),
        graduation,
        fixture,
        min_trades: 20,
        promoted_by: NonEmptyLabel::try_new("operator-token").unwrap(),
    }
}

fn certified_session(fixture: bool) -> PaperSession {
    session(
        Graduation::Certified {
            walk_forward_run_id: WalkForwardRunId::new("run-1"),
            data_versions: Vec::new(),
        },
        fixture,
    )
}

fn override_session() -> PaperSession {
    session(
        Graduation::Override {
            reason: NonEmptyReason::try_new("promoted early").unwrap(),
            at: "2025-02-01T00:00:00.000Z".to_owned(),
        },
        false,
    )
}

/// A certifying run whose folds' `mean_r` are exactly `means`, under
/// `fingerprint`.
fn certifying_run(means: &[Decimal], fingerprint: &str) -> WalkForwardRun {
    let folds: Vec<WalkForwardFold> = means
        .iter()
        .enumerate()
        .map(|(index, mean)| WalkForwardFold {
            index: u8::try_from(index).unwrap(),
            window: CandleWindow::new(1_735_702_200_000, 1_738_000_000_000).unwrap(),
            backtest_run_id: BacktestRunId::new(format!("fold-run-{index}")),
            verdict: FoldVerdict {
                n: 20,
                mean_r: *mean,
                lower_bound: 0.1,
                holds: true,
            },
        })
        .collect();
    let pooled = FoldVerdict {
        n: 40,
        mean_r: means.iter().copied().sum::<Decimal>() / Decimal::from(means.len().max(1)),
        lower_bound: 0.1,
        holds: true,
    };
    WalkForwardRun {
        id: WalkForwardRunId::new("run-1"),
        strategy_version_id: VersionId::new("version-1"),
        created_at: "2025-01-01T00:00:00.000Z".to_owned(),
        scheme: pulse::FoldScheme::rolling_oos(2).unwrap(),
        rule: pulse::VerdictRule::WfV1,
        span: CandleWindow::new(1_735_702_200_000, 1_738_000_000_000).unwrap(),
        from_defaulted: false,
        engine_fingerprint: fingerprint.to_owned(),
        verdict: RunVerdict {
            folds_holding: u8::try_from(means.len()).unwrap_or(u8::MAX),
            folds_required: 1,
            pooled,
            pass: true,
        },
        folds,
    }
}

/// A log with one consumed bar and `rs.len()` closed trades, each exit fill
/// carrying its realized R.
fn log_with_rs(rs: &[Decimal]) -> Vec<PaperEvent> {
    let mut log = vec![PaperEvent::BarProcessed {
        seq: 1,
        at: "2025-02-01T00:00:00.000Z".to_owned(),
        bars: vec![pulse::BarRef {
            timeframe: Timeframe::M15,
            open_time: 1_000,
        }],
    }];
    let mut seq = 2_i64;
    for r in rs {
        log.push(PaperEvent::Fill {
            seq,
            at: "2025-02-01T00:15:00.000Z".to_owned(),
            side: PaperSide::Long,
            qty: Decimal::ONE,
            price: Decimal::from(60_000),
            exit_reason: None,
            realized_r: None,
        });
        seq += 1;
        log.push(PaperEvent::Fill {
            seq,
            at: "2025-02-01T00:30:00.000Z".to_owned(),
            side: PaperSide::Long,
            qty: Decimal::ONE,
            price: Decimal::from(60_100),
            exit_reason: Some(pulse::ExitReason::TakeProfit),
            realized_r: Some(*r),
        });
        seq += 1;
    }
    log
}

fn state_of(session: &PaperSession, log: &[PaperEvent]) -> PaperSessionState {
    PaperSessionState::replay(session, log).expect("the scripted log replays")
}

/// `Pending` until `min_trades`: exactly 19 counted trades is still pending,
/// 20 is not.
#[test]
fn pending_until_the_minimum_then_a_verdict() {
    let session = certified_session(false);
    let run = certifying_run(&[Decimal::new(5, 1), Decimal::new(15, 1)], CURRENT);

    let nineteen: Vec<Decimal> = std::iter::repeat_n(Decimal::ONE, 19).collect();
    let state = state_of(&session, &log_with_rs(&nineteen));
    let rendered = comparison(&session, &state, Some(&run), &EngineFingerprint::current());
    assert_eq!(
        rendered.verdict,
        ComparisonVerdict::Pending { n: 19, of: 20 }
    );

    let twenty: Vec<Decimal> = std::iter::repeat_n(Decimal::ONE, 20).collect();
    let state = state_of(&session, &log_with_rs(&twenty));
    let rendered = comparison(&session, &state, Some(&run), &EngineFingerprint::current());
    match rendered.verdict {
        ComparisonVerdict::Within {
            live_mean_r,
            n,
            fold_min,
            fold_max,
        } => {
            assert_eq!(live_mean_r, Decimal::ONE);
            assert_eq!(n, 20);
            assert_eq!(fold_min, Decimal::new(5, 1));
            assert_eq!(fold_max, Decimal::new(15, 1));
        }
        other => panic!("expected Within, got {other:?}"),
    }
}

/// The three verdicts against a known fold range.
#[test]
fn within_below_and_above_compare_the_live_mean_to_the_fold_range() {
    let session = certified_session(false);
    let run = certifying_run(&[Decimal::new(5, 1), Decimal::new(15, 1)], CURRENT);
    for (r, expected_kind) in [
        (Decimal::new(25, 2), "below"), // 0.25 < 0.5
        (Decimal::ONE, "within"),
        (Decimal::new(175, 2), "above"), // 1.75 > 1.5
    ] {
        let rs: Vec<Decimal> = std::iter::repeat_n(r, 20).collect();
        let state = state_of(&session, &log_with_rs(&rs));
        let rendered = comparison(&session, &state, Some(&run), &EngineFingerprint::current());
        let (kind, live) = match rendered.verdict {
            ComparisonVerdict::Below { live_mean_r, .. } => ("below", live_mean_r),
            ComparisonVerdict::Within { live_mean_r, .. } => ("within", live_mean_r),
            ComparisonVerdict::Above { live_mean_r, .. } => ("above", live_mean_r),
            other => panic!("expected a verdict, got {other:?}"),
        };
        assert_eq!(kind, expected_kind, "mean {r}");
        assert_eq!(live, r);
    }
}

/// A fixture-certified session shows the A12 text; an override shows the
/// shadow-only text; both are `NotApplicable`.
#[test]
fn fixture_and_override_sessions_are_not_applicable() {
    let run = certifying_run(&[Decimal::new(5, 1), Decimal::new(15, 1)], CURRENT);
    let rs: Vec<Decimal> = std::iter::repeat_n(Decimal::ONE, 20).collect();

    let fixture = certified_session(true);
    let state = state_of(&fixture, &log_with_rs(&rs));
    let rendered = comparison(&fixture, &state, Some(&run), &EngineFingerprint::current());
    assert_eq!(
        rendered.verdict,
        ComparisonVerdict::NotApplicable {
            reason: FIXTURE_TEXT.to_owned()
        },
        "the fixture text is verbatim (A12)"
    );

    let overridden = override_session();
    let state = state_of(&overridden, &log_with_rs(&rs));
    let rendered = comparison(&overridden, &state, None, &EngineFingerprint::current());
    assert!(
        matches!(rendered.verdict, ComparisonVerdict::NotApplicable { .. }),
        "an override is not OOS-comparable: {rendered:?}"
    );
    assert_eq!(rendered.engine_builds, 1);
    assert!(!rendered.certification_stale);
}

/// Two epochs report `engine_builds = 2`.
#[test]
fn an_engine_upgrade_opens_a_second_epoch() {
    let session = certified_session(false);
    let run = certifying_run(&[Decimal::new(5, 1), Decimal::new(15, 1)], CURRENT);
    let mut log = log_with_rs(&[Decimal::ONE]);
    log.push(PaperEvent::EngineUpgraded {
        seq: 4,
        at: "2025-02-01T01:00:00.000Z".to_owned(),
        old: EngineFingerprint::from_stored(CURRENT.to_owned()),
        new: EngineFingerprint::from_stored(FOREIGN.to_owned()),
    });
    let state = state_of(&session, &log);
    assert_eq!(state.epochs.len(), 2);
    let rendered = comparison(&session, &state, Some(&run), &EngineFingerprint::current());
    assert_eq!(rendered.engine_builds, 2);
}

/// A foreign certifying fingerprint is stale — and the comparison still
/// renders (E3: shown, never enforced).
#[test]
fn a_foreign_certification_is_stale_but_still_renders() {
    let session = certified_session(false);
    let run = certifying_run(&[Decimal::new(5, 1), Decimal::new(15, 1)], FOREIGN);
    let rs: Vec<Decimal> = std::iter::repeat_n(Decimal::ONE, 20).collect();
    let state = state_of(&session, &log_with_rs(&rs));
    let rendered: OosComparison =
        comparison(&session, &state, Some(&run), &EngineFingerprint::current());
    assert!(rendered.certification_stale, "{rendered:?}");
    match rendered.verdict {
        ComparisonVerdict::Within { n, .. } => assert_eq!(n, 20),
        other => panic!("the comparison still renders: {other:?}"),
    }
}

/// A w3-era exit fill (no `realized_r` in its payload) replays with `None` and
/// is not counted toward `n`.
#[test]
fn w3_era_fills_replay_without_r_and_are_not_counted() {
    let session = certified_session(false);
    let run = certifying_run(&[Decimal::new(5, 1), Decimal::new(15, 1)], CURRENT);

    // One legacy exit fill, decoded from a payload written before the field
    // existed (the `#[serde(default)]` arm).
    let legacy = PaperEvent::decode(
        "fill",
        r#"{"type":"fill","seq":2,"at":"2025-02-01T00:30:00.000Z","side":"long","qty":"1","price":"60100","exit_reason":"take_profit"}"#,
    )
    .expect("a w3-era payload decodes");
    let PaperEvent::Fill { realized_r, .. } = &legacy else {
        panic!("the payload is a fill");
    };
    assert_eq!(*realized_r, None, "the missing field defaults to None");

    // 19 modern fills + 1 legacy fill = 20 closed trades, 19 counted.
    let modern: Vec<Decimal> = std::iter::repeat_n(Decimal::ONE, 19).collect();
    let mut log = log_with_rs(&modern);
    let last_seq = log.last().expect("an exit fill").seq();
    log.push(legacy.with_seq(last_seq + 1));

    let state = state_of(&session, &log);
    assert_eq!(state.closed_trades.len(), 20, "both fills closed trades");
    assert_eq!(
        state
            .closed_trades
            .iter()
            .filter(|trade| trade.realized_r.is_none())
            .count(),
        1,
        "the legacy fill replays with no R"
    );
    let rendered = comparison(&session, &state, Some(&run), &EngineFingerprint::current());
    assert_eq!(
        rendered.verdict,
        ComparisonVerdict::Pending { n: 19, of: 20 },
        "the legacy fill is not counted toward n"
    );
}
