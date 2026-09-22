//! AC-2 (r2.s3.w3): the `wf-v1` verdict and `rolling-oos/v1` fold math, proven
//! pure — no database, no fixture, no engine. These are the pinned surfaces:
//! `fold_windows` over every legal K, `FoldVerdict`/`RunVerdict` on synthetic R
//! vectors, and the constants `1.645` / `20` / `⌈2K/3⌉` / the persisted names.
//! A change to any of them is a new rule name, not an edit — this file is what
//! makes that true.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{
    CandleWindow, FoldScheme, FoldVerdict, K_MAX, K_MIN, N_MIN, RunVerdict, VerdictRule,
    WalkForwardError, Z, fold_windows, folds_required,
};
use rust_decimal::Decimal;

fn r(v: i64) -> Decimal {
    Decimal::new(v, 0)
}

fn holding_fold(n: usize) -> FoldVerdict {
    FoldVerdict::from_rs(&vec![r(1); n])
}

// ---------------------------------------------------------------------------
// fold_windows — every legal K: contiguity, exact union, remainder in the last
// ---------------------------------------------------------------------------

#[test]
fn fold_windows_cover_the_span_for_every_legal_k() {
    // A span with a non-zero remainder for every K in 2..=12 (1_000_003 is
    // divisible by none of them).
    let span = CandleWindow {
        from_ms: 0,
        to_ms: 1_000_003,
    };
    for k in K_MIN..=K_MAX {
        let windows = fold_windows(&span, k);
        assert_eq!(windows.len(), usize::from(k), "K={k}: fold count");

        let step = (span.to_ms - span.from_ms) / i64::from(k);
        for (i, w) in windows.iter().enumerate() {
            assert!(w.from_ms < w.to_ms, "K={k} fold {i}: non-empty window");
            if i + 1 < windows.len() {
                // Equal length for every fold but the last …
                assert_eq!(
                    w.to_ms - w.from_ms,
                    step,
                    "K={k} fold {i}: equal-length window"
                );
                // … and contiguous with the next fold.
                assert_eq!(
                    w.to_ms,
                    windows[i + 1].from_ms,
                    "K={k} fold {i}: contiguous"
                );
            }
        }
        // The last fold absorbs the integer-division remainder …
        let last = windows.last().unwrap();
        assert_eq!(
            last.to_ms - last.from_ms,
            step + (span.to_ms - span.from_ms) % i64::from(k),
            "K={k}: remainder placement"
        );
        // … and the union is exactly [from, to).
        assert_eq!(windows.first().unwrap().from_ms, span.from_ms, "K={k}");
        assert_eq!(last.to_ms, span.to_ms, "K={k}");
    }
}

#[test]
fn fold_windows_are_exactly_equal_when_the_span_divides() {
    let span = CandleWindow {
        from_ms: 1_000,
        to_ms: 1_000 + 6 * 10_000,
    };
    let windows = fold_windows(&span, 6);
    for w in &windows {
        assert_eq!(w.to_ms - w.from_ms, 10_000);
    }
}

// ---------------------------------------------------------------------------
// FoldScheme — the K bounds and the pinned persisted names
// ---------------------------------------------------------------------------

#[test]
fn rolling_oos_refuses_k_outside_two_to_twelve() {
    // The wire value arrives as i64 — negatives and values past u8 refuse as
    // themselves, not as a decode failure (F4).
    for k in [0_i64, 1, 13, i64::from(u8::MAX), 256, -1] {
        assert_eq!(
            FoldScheme::rolling_oos(k),
            Err(WalkForwardError::KOutOfRange { k, min: 2, max: 12 }),
            "K={k}"
        );
    }
    for k in K_MIN..=K_MAX {
        assert_eq!(
            FoldScheme::rolling_oos(i64::from(k)).unwrap().k(),
            k,
            "K={k}"
        );
    }
}

#[test]
fn persisted_names_and_constants_are_pinned() {
    assert_eq!(FoldScheme::rolling_oos(6).unwrap().name(), "rolling-oos/v1");
    assert_eq!(VerdictRule::WfV1.name(), "wf-v1");
    assert_eq!(Z.to_bits(), 1.645f64.to_bits());
    assert_eq!(N_MIN, 20);
    assert_eq!(K_MIN, 2);
    assert_eq!(K_MAX, 12);
    assert_eq!(pulse::K_DEFAULT, 6);
    // The serialized forms carry the same names — the persisted `scheme`/`rule`
    // TEXT columns and any serde projection agree.
    assert_eq!(
        serde_json::to_value(FoldScheme::rolling_oos(6).unwrap()).unwrap(),
        serde_json::json!({"name": "rolling-oos/v1", "k": 6})
    );
    assert_eq!(
        serde_json::to_value(VerdictRule::WfV1).unwrap(),
        serde_json::json!("wf-v1")
    );
    assert_eq!(
        serde_json::from_value::<FoldScheme>(serde_json::json!({
            "name": "rolling-oos/v1",
            "k": 6
        }))
        .unwrap(),
        FoldScheme::rolling_oos(6).unwrap()
    );
    assert_eq!(
        serde_json::from_value::<VerdictRule>(serde_json::json!("wf-v1")).unwrap(),
        VerdictRule::WfV1
    );
}

// ---------------------------------------------------------------------------
// folds_required — ⌈2K/3⌉ over the legal range
// ---------------------------------------------------------------------------

#[test]
fn folds_required_is_ceil_two_thirds_of_k() {
    let expected = [
        (2, 2),
        (3, 2),
        (4, 3),
        (5, 4),
        (6, 4),
        (7, 5),
        (8, 6),
        (9, 6),
        (10, 7),
        (11, 8),
        (12, 8),
    ];
    for (k, required) in expected {
        assert_eq!(folds_required(k), required, "K={k}");
    }
}

// ---------------------------------------------------------------------------
// FoldVerdict — n<2 is defined, n<20 cannot hold, lb must be strictly positive
// ---------------------------------------------------------------------------

#[test]
fn fold_verdict_under_two_trades_is_defined_as_zero() {
    for rs in [&[][..], &[r(5)][..]] {
        let v = FoldVerdict::from_rs(rs);
        assert_eq!(v.n, rs.len());
        assert_eq!(v.mean_r, Decimal::ZERO);
        assert_eq!(v.lower_bound.to_bits(), 0.0f64.to_bits());
        assert!(!v.holds);
    }
}

#[test]
fn fold_verdict_nineteen_positive_trades_does_not_hold() {
    // n = 19 < N_MIN: a positive lower bound is not enough.
    let v = FoldVerdict::from_rs(&vec![r(1); 19]);
    assert_eq!(v.n, 19);
    assert_eq!(v.mean_r, r(1));
    assert!(v.lower_bound > 0.0);
    assert!(!v.holds);
}

#[test]
fn fold_verdict_twenty_trades_with_positive_lower_bound_holds() {
    let v = FoldVerdict::from_rs(&vec![r(1); 20]);
    assert_eq!(v.n, 20);
    assert_eq!(v.mean_r, r(1));
    assert_eq!(v.lower_bound.to_bits(), 1.0f64.to_bits()); // zero variance: lb = mean exactly
    assert!(v.holds);
}

#[test]
fn fold_verdict_lower_bound_of_exactly_zero_does_not_hold() {
    // 20 trades all at r = 0: mean 0, variance 0, lb = 0.0 exactly — and a
    // bound of exactly zero does not hold (the rule is strictly above zero).
    let v = FoldVerdict::from_rs(&vec![r(0); 20]);
    assert_eq!(v.n, 20);
    assert_eq!(v.mean_r, Decimal::ZERO);
    assert_eq!(v.lower_bound.to_bits(), 0.0f64.to_bits());
    assert!(!v.holds);
}

#[test]
fn fold_verdict_negative_lower_bound_does_not_hold() {
    // Mean slightly positive, variance large: the lower bound is under water.
    let mut rs = vec![r(3); 10];
    rs.extend(vec![r(-2); 10]);
    let v = FoldVerdict::from_rs(&rs);
    assert_eq!(v.n, 20);
    assert!(v.lower_bound < 0.0, "lb was {}", v.lower_bound);
    assert!(!v.holds);
}

#[test]
fn fold_verdict_math_matches_the_pinned_formula() {
    // Classic sample-variance vector: mean 5, sample variance 32/7.
    let rs: Vec<Decimal> = [2, 4, 4, 4, 5, 5, 7, 9].into_iter().map(r).collect();
    let v = FoldVerdict::from_rs(&rs);
    assert_eq!(v.n, 8);
    assert_eq!(v.mean_r, r(5));
    // lb = 5 − 1.645·sqrt((32/7)/8) = 5 − 1.645·sqrt(4/7).
    let expected = 5.0 - 1.645 * (4.0f64 / 7.0).sqrt();
    assert!(
        (v.lower_bound - expected).abs() < 1e-9,
        "lb {} vs expected {}",
        v.lower_bound,
        expected
    );
    assert!(!v.holds); // n < 20 regardless of the bound
}

#[test]
fn fold_verdict_is_deterministic_to_the_bit() {
    let mut rs = vec![r(3), r(-1), r(2), r(2), r(-1)];
    rs.extend(vec![r(1); 15]);
    let a = FoldVerdict::from_rs(&rs);
    for _ in 0..100 {
        assert_eq!(
            a.lower_bound.to_bits(),
            FoldVerdict::from_rs(&rs).lower_bound.to_bits()
        );
    }
}

// ---------------------------------------------------------------------------
// RunVerdict — ⌈2K/3⌉ folds AND a holding pooled bound
// ---------------------------------------------------------------------------

#[test]
fn run_verdict_requires_both_enough_holding_folds_and_a_holding_pool() {
    for (k, required) in [(2u8, 2u8), (5, 4), (6, 4), (12, 8)] {
        let folds: Vec<FoldVerdict> = (0..k).map(|_| holding_fold(20)).collect();
        let pooled_rs = vec![r(1); usize::from(k) * 20];

        let verdict = RunVerdict::assess(&folds, &pooled_rs);
        assert_eq!(verdict.folds_holding, k, "K={k}");
        assert_eq!(verdict.folds_required, required, "K={k}");
        assert!(verdict.pooled.holds, "K={k}");
        assert!(verdict.pass, "K={k}: all folds hold and the pool holds");

        // One short of the required holding count fails even with a holding pool.
        let mut short = folds.clone();
        short[0] = FoldVerdict::from_rs(&vec![r(0); 20]); // lb = 0 → not holding
        let verdict = RunVerdict::assess(&short, &pooled_rs);
        assert_eq!(verdict.folds_holding, k - 1, "K={k}");
        assert_eq!(
            verdict.pass,
            (k - 1) >= required,
            "K={k}: {} holding < {required} required",
            k - 1
        );

        // Enough holding folds but a non-holding pool still fails.
        let verdict = RunVerdict::assess(&folds, &vec![r(-1); 40]);
        assert_eq!(verdict.folds_holding, k, "K={k}");
        assert!(!verdict.pooled.holds, "K={k}");
        assert!(!verdict.pass, "K={k}: pooled bound under water");
    }
}
