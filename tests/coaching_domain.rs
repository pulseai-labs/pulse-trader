//! AC-1 — the coaching session domain: never silence, and a disposition state
//! machine that refuses illegal transitions (r1.s2.w2, ADR-0021).
//!
//! The capability sentence this binary defends: a coach turn yields **exactly one
//! validated mutation with a stated hypothesis, or one typed failure — never
//! silence**. Here that is a type-level property rather than a convention:
//! [`SessionOutcome`] is an enum, so a session carrying both a proposal and a
//! failure, or neither, is not representable.
//!
//! What it asserts:
//!   1. A session's outcome is exactly one of proposal / typed failure.
//!   2. Every `CoachFailure` variant constructs and `Display`s with its context —
//!      each one has to read back as a recorded failure reason (`w3` persists it).
//!   3. `InapplicableMutation` carries a w1 `MutationError` **losslessly** — the
//!      error asserted here comes out of a real `apply()` call, not a hand-built
//!      copy.
//!   4. The disposition state machine rejects illegal transitions.
//!   5. `Proposal` serde round-trips with its typed `Mutation` (`w2` stores it
//!      typed; `r1.s4`'s modify path reads it back and edits it).
//!   6. A hypothesis cannot be empty or whitespace-only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::BacktestRunId;
use pulse::{
    CoachContext, CoachFailure, CoachingError, CoachingSession, CoachingSessionId, Comparator,
    Condition, Direction, Disposition, DispositionKind, ExitRule, Hypothesis, IndicatorSpec,
    LlmCallId, MfeMaeAggregates, Mutation, MutationError, ParamValue, Proposal, RegimeBreakdown,
    RiskParams, SchemaVersion, Series, SessionOutcome, SkippedEntryCounts, StrategyDsl,
    SummaryStats, SweepableValue, ValueSource, VersionId, apply,
};
use rust_decimal::Decimal;

/// The canonical RSI-oversold fixture (the DSL suite's shared strategy).
fn rsi_oversold_strategy() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "RSI Oversold".to_owned(),
        direction: Direction::Long,
        entry: Condition::Compare {
            lhs: ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Rsi {
                    period: SweepableValue::Fixed(14),
                },
            },
            op: Comparator::Lt,
            rhs: ValueSource::Constant {
                value: Decimal::new(30, 0),
            },
        },
        filters: vec![],
        exits: vec![
            ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
        },
    }
}

fn set_period(path: &str, value: u32) -> Mutation {
    Mutation::SetParam {
        path: path.to_owned(),
        new_value: ParamValue::Period { value },
    }
}

fn hypothesis(text: &str) -> Hypothesis {
    Hypothesis::new(text).expect("a non-empty hypothesis")
}

fn sample_proposal() -> Proposal {
    Proposal {
        mutation: set_period("entry.lhs.indicator.rsi.period", 21),
        hypothesis: hypothesis("a slower RSI should cut the whipsaw entries the run shows"),
        disposition: Disposition::Proposed,
        // r1.s4.w4: a fresh proposal has no accept attempt behind it.
        accept_failure: None,
    }
}

fn session_with(outcome: SessionOutcome, llm_call_id: Option<LlmCallId>) -> CoachingSession {
    CoachingSession {
        id: CoachingSessionId::new("sess-1"),
        backtest_run_id: BacktestRunId::new("run-1"),
        strategy_version_id: VersionId::new("ver-1"),
        created_at: "2026-08-29T00:00:00.000Z".to_owned(),
        llm_call_id,
        outcome,
    }
}

/// A REAL `MutationError` — produced by driving `apply()` into a failure rather
/// than hand-building the value the assertion then compares against.
fn a_real_mutation_error() -> MutationError {
    apply(
        &rsi_oversold_strategy(),
        &set_period("entry.lhs.indicator.rsi.period", 0),
    )
    .expect_err("an RSI period of 0 fails validation")
}

// ---------------------------------------------------------------------------
// 1. Never silence: exactly one outcome
// ---------------------------------------------------------------------------

#[test]
fn a_session_carries_a_proposal_or_a_failure() {
    let proposed = session_with(
        SessionOutcome::Proposed {
            proposal: sample_proposal(),
        },
        Some(LlmCallId::new("call-1")),
    );
    match &proposed.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(proposal.disposition, Disposition::Proposed);
            assert!(!proposal.hypothesis.as_str().is_empty());
        }
        SessionOutcome::Failed { failure } => panic!("expected a proposal, got {failure:?}"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }

    // A pre-call failure records no LlmCall row (audit C3): the session row IS the
    // audit trail, and this refusal happened BEFORE any call, so there is no ledger
    // row to correlate. (The converse does not hold — a timeout or transport fault
    // can be an attempt with no row.)
    let failed = session_with(
        SessionOutcome::Failed {
            failure: CoachFailure::ContextOverflow {
                detail: "the version's DSL is 42kB; the window is 32kB".to_owned(),
            },
        },
        None,
    );
    assert!(
        matches!(failed.outcome, SessionOutcome::Failed { .. }),
        "a failed turn is still a recorded session"
    );
    assert!(
        failed.llm_call_id.is_none(),
        "a pre-call failure has no LlmCall row"
    );
}

// ---------------------------------------------------------------------------
// 2. Every failure variant constructs and reads back as a reason
// ---------------------------------------------------------------------------

/// One instance of every [`CoachFailure`] variant, paired with the context its
/// `Display` must carry.
///
/// The `match` below is EXHAUSTIVE and has no `_` arm on purpose: an eighth
/// failure kind stops this file compiling until someone writes down what that
/// kind reads back as. A `vec![...]` alone cannot do that — it would just quietly
/// cover six of seven, which is how `TransportFailure` shipped untested here (and
/// nine of ten, once r1.s4.w4 widened the taxonomy again).
fn every_failure_case() -> Vec<(CoachFailure, &'static str)> {
    let cases = vec![
        (CoachFailure::ZeroCalls, "no"),
        (
            CoachFailure::SeveralCalls {
                count: 3,
                propose_mutation_count: 2,
            },
            "3",
        ),
        (
            CoachFailure::MalformedArguments {
                detail: "`path` was not a string".to_owned(),
            },
            "`path` was not a string",
        ),
        (
            CoachFailure::InapplicableMutation {
                mutation: set_period("entry.lhs.indicator.rsi.period", 0),
                error: a_real_mutation_error(),
            },
            "entry.lhs.indicator.rsi.period",
        ),
        (
            CoachFailure::ProviderTimeout { elapsed_ms: 30_000 },
            "30000",
        ),
        (
            CoachFailure::ContextOverflow {
                detail: "42kB of DSL against a 32kB window".to_owned(),
            },
            "42kB of DSL",
        ),
        (
            CoachFailure::TransportFailure {
                detail: "HTTP 503 from upstream".to_owned(),
            },
            "503",
        ),
        // r1.s4.w4 — the three `0008` adds. Each needle is the CONTEXT the
        // recorded reason has to carry: the advice a trader would otherwise lose,
        // the input that went missing, and what is known about the abandoned claim.
        (
            // r1.s4.w1 (#131): the payload became `intent` + `evidence` when the
            // `record_inapplicable` tool that produces it landed. The needle stays
            // the ADVICE ITSELF — the thing a trader would otherwise lose.
            CoachFailure::InapplicableAdvice {
                intent: "add an ADX filter above 25".to_owned(),
                evidence: "most losses are in the ranging regime".to_owned(),
            },
            "add an ADX filter above 25",
        ),
        (
            CoachFailure::MissingBacktestInputs {
                detail: "primary snapshot `v-primary` is not in the store".to_owned(),
            },
            "v-primary",
        ),
        (
            CoachFailure::Interrupted {
                detail: "claimed at 2026-08-29T00:00:00Z by a process that did not finish"
                    .to_owned(),
            },
            "2026-08-29T00:00:00Z",
        ),
    ];

    // The completeness gate: every variant must appear at least once, and the
    // exhaustive match is what makes "every variant" a compile-time list.
    let mut seen: Vec<&'static str> = cases.iter().map(|(f, _)| variant_tag(f)).collect();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(
        seen.len(),
        10,
        "every CoachFailure variant must be exercised, got {seen:?}"
    );
    cases
}

/// The variant tag, by exhaustive match — no `_` arm, so an added variant is a
/// compile error here and in [`every_failure_case`]'s count.
fn variant_tag(failure: &CoachFailure) -> &'static str {
    match failure {
        CoachFailure::ZeroCalls => "zero_calls",
        CoachFailure::SeveralCalls { .. } => "several_calls",
        CoachFailure::MalformedArguments { .. } => "malformed_arguments",
        CoachFailure::InapplicableMutation { .. } => "inapplicable_mutation",
        CoachFailure::ProviderTimeout { .. } => "provider_timeout",
        CoachFailure::ContextOverflow { .. } => "context_overflow",
        CoachFailure::TransportFailure { .. } => "transport_failure",
        CoachFailure::InapplicableAdvice { .. } => "inapplicable_advice",
        CoachFailure::MissingBacktestInputs { .. } => "missing_backtest_inputs",
        CoachFailure::Interrupted { .. } => "interrupted",
    }
}

#[test]
fn every_failure_variant_displays_with_its_context() {
    for (failure, needle) in every_failure_case() {
        let rendered = failure.to_string();
        assert!(
            rendered.contains(needle),
            "a recorded failure reason must carry its context: {rendered:?} lacks {needle:?}"
        );
        assert!(
            !rendered.trim().is_empty(),
            "no failure may read back as silence"
        );
    }
}

#[test]
fn several_calls_does_not_claim_a_foreign_call_was_a_proposal() {
    let two_proposals = CoachFailure::SeveralCalls {
        count: 2,
        propose_mutation_count: 2,
    };
    let one_foreign = CoachFailure::SeveralCalls {
        count: 2,
        propose_mutation_count: 1,
    };

    assert!(
        one_foreign
            .to_string()
            .contains("1 of them propose_mutation"),
        "a turn that made one propose_mutation call plus one foreign call must not \
         read back as two proposals: {one_foreign}"
    );
    assert_ne!(
        two_proposals.to_string(),
        one_foreign.to_string(),
        "two proposals and one-plus-a-foreign-tool are different mistakes and must \
         read back differently"
    );
}

#[test]
fn inapplicable_mutation_carries_the_mutation_error_losslessly() {
    let error = a_real_mutation_error();
    let failure = CoachFailure::InapplicableMutation {
        mutation: set_period("entry.lhs.indicator.rsi.period", 0),
        error: error.clone(),
    };

    match &failure {
        CoachFailure::InapplicableMutation { error: carried, .. } => {
            assert_eq!(
                carried, &error,
                "the w1 MutationError must be carried verbatim, not flattened to a string"
            );
            // And it is the typed variant, still matchable downstream.
            assert!(
                matches!(carried, MutationError::ValidationFailed { .. }),
                "the typed variant survives: {carried:?}"
            );
        }
        other => panic!("expected InapplicableMutation, got {other:?}"),
    }

    // Lossless across serde too — `w2` persists the failure and reads it back.
    let json = serde_json::to_string(&failure).expect("serialize the failure");
    let back: CoachFailure = serde_json::from_str(&json).expect("deserialize the failure");
    assert_eq!(back, failure, "a failure round-trips losslessly");
}

// ---------------------------------------------------------------------------
// 3. The disposition state machine
// ---------------------------------------------------------------------------

#[test]
fn legal_dispositions_transition() {
    let proposal = sample_proposal();

    for next in [
        Disposition::Rejected,
        Disposition::Modified,
        Disposition::Accepted {
            child_version_id: VersionId::new("ver-2"),
            accepted_run_id: BacktestRunId::new("run-child-2"),
        },
    ] {
        let moved = proposal
            .transition(next.clone())
            .unwrap_or_else(|e| panic!("proposed -> {:?} must be legal: {e}", next.kind()));
        assert_eq!(moved.disposition, next);
        // Everything else is carried through unchanged.
        assert_eq!(moved.mutation, proposal.mutation);
        assert_eq!(moved.hypothesis, proposal.hypothesis);
    }

    // Modified is a working state, not a terminal one: r1.s4 edits, then accepts.
    let modified = proposal
        .transition(Disposition::Modified)
        .expect("proposed -> modified");
    let accepted = modified
        .transition(Disposition::Accepted {
            child_version_id: VersionId::new("ver-2"),
            accepted_run_id: BacktestRunId::new("run-child-2"),
        })
        .expect("modified -> accepted");
    assert_eq!(accepted.disposition.kind(), DispositionKind::Accepted);
}

#[test]
fn illegal_dispositions_are_rejected() {
    let rejected = sample_proposal()
        .transition(Disposition::Rejected)
        .expect("proposed -> rejected");

    // A terminal state accepts nothing further — the spec's own example.
    let err = rejected
        .transition(Disposition::Accepted {
            child_version_id: VersionId::new("ver-2"),
            accepted_run_id: BacktestRunId::new("run-child-2"),
        })
        .expect_err("rejected -> accepted must be refused");
    match err {
        CoachingError::IllegalTransition { from, to } => {
            assert_eq!(from, DispositionKind::Rejected);
            assert_eq!(to, DispositionKind::Accepted);
        }
        other @ (CoachingError::EmptyHypothesis | CoachingError::EmptyRequestFingerprint) => {
            panic!("expected IllegalTransition, got {other:?}")
        }
    }

    let accepted = sample_proposal()
        .transition(Disposition::Accepted {
            child_version_id: VersionId::new("ver-2"),
            accepted_run_id: BacktestRunId::new("run-child-2"),
        })
        .expect("proposed -> accepted");
    for next in [
        Disposition::Rejected,
        Disposition::Modified,
        Disposition::Accepted {
            child_version_id: VersionId::new("ver-3"),
            accepted_run_id: BacktestRunId::new("run-child-3"),
        },
    ] {
        assert!(
            accepted.transition(next.clone()).is_err(),
            "accepted is terminal; -> {:?} must be refused",
            next.kind()
        );
    }

    // And nothing may return to the initial state.
    for start in [
        sample_proposal(),
        sample_proposal()
            .transition(Disposition::Modified)
            .expect("proposed -> modified"),
    ] {
        assert!(
            start.transition(Disposition::Proposed).is_err(),
            "nothing may transition back to Proposed"
        );
    }
}

#[test]
fn child_version_id_exists_only_on_accepted() {
    // Structural, not a nullable field: only the Accepted variant has the payload,
    // so "a rejected proposal with a child version" is not representable.
    let accepted = Disposition::Accepted {
        child_version_id: VersionId::new("ver-2"),
        accepted_run_id: BacktestRunId::new("run-child-2"),
    };
    assert_eq!(
        accepted.child_version_id(),
        Some(&VersionId::new("ver-2")),
        "an accepted disposition names its child version"
    );
    for other in [
        Disposition::Proposed,
        Disposition::Rejected,
        Disposition::Modified,
    ] {
        assert!(
            other.child_version_id().is_none(),
            "{:?} carries no child version",
            other.kind()
        );
    }
}

// ---------------------------------------------------------------------------
// 4. serde
// ---------------------------------------------------------------------------

#[test]
fn a_proposal_round_trips_with_its_typed_mutation() {
    let proposal = sample_proposal();

    let json = serde_json::to_string(&proposal).expect("serialize the proposal");
    let back: Proposal = serde_json::from_str(&json).expect("deserialize the proposal");

    assert_eq!(back, proposal, "a proposal round-trips value-equal");
    assert_eq!(
        back.mutation,
        set_period("entry.lhs.indicator.rsi.period", 21),
        "the typed Mutation survives the round-trip intact"
    );
}

#[test]
fn an_accepted_disposition_round_trips_with_its_child_version() {
    let accepted = sample_proposal()
        .transition(Disposition::Accepted {
            child_version_id: VersionId::new("ver-2"),
            accepted_run_id: BacktestRunId::new("run-child-2"),
        })
        .expect("proposed -> accepted");

    let json = serde_json::to_string(&accepted).expect("serialize");
    let back: Proposal = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, accepted);
}

// ---------------------------------------------------------------------------
// 5. A hypothesis is never empty
// ---------------------------------------------------------------------------

#[test]
fn a_hypothesis_must_not_be_empty() {
    for blank in ["", "   ", "\t", "\n  \n"] {
        assert!(
            matches!(Hypothesis::new(blank), Err(CoachingError::EmptyHypothesis)),
            "{blank:?} must be refused as a hypothesis"
        );
    }

    let ok = Hypothesis::new("  a slower RSI cuts whipsaw entries  ").expect("non-empty");
    assert_eq!(
        ok.as_str(),
        "a slower RSI cuts whipsaw entries",
        "surrounding whitespace is trimmed"
    );
}

#[test]
fn a_deserialized_hypothesis_cannot_be_empty() {
    // The invariant has to survive the READ path too: a row edited by hand, or a
    // proposal reloaded from a database written by an older binary, must not be
    // able to smuggle an empty hypothesis past the constructor.
    assert!(
        serde_json::from_str::<Hypothesis>("\"\"").is_err(),
        "an empty stored hypothesis must not deserialize"
    );
    assert!(
        serde_json::from_str::<Hypothesis>("\"   \"").is_err(),
        "a whitespace-only stored hypothesis must not deserialize"
    );
    let ok: Hypothesis =
        serde_json::from_str("\"a slower RSI cuts whipsaw entries\"").expect("a real hypothesis");
    assert_eq!(ok.as_str(), "a slower RSI cuts whipsaw entries");
}

// ---------------------------------------------------------------------------
// 7. What the MFE/MAE numbers actually mean (PR #128, finding G3)
// ---------------------------------------------------------------------------

/// The excursion aggregates are POTENTIAL bounds, not a path any trade walked, and
/// the rendered context has to say so. The engine folds every bar the position was
/// open into the running MFE/MAE, the exit bar included and in full — so a trade
/// that exits at a bar's open still carries that whole bar's range, movement after
/// the close included (`engine.rs`, and `mfe_r >= realized_r >= mae_r` is explicitly
/// NOT guaranteed there). A model reading "MFE / MAE (R multiples, aggregated)"
/// alone will read them as reachable, which is the coaching mistake this label
/// prevents. Known behaviour, tracked as #55 and NOT closed here.
#[test]
fn the_rendered_context_labels_mfe_mae_as_full_bar_potential() {
    let context = CoachContext {
        summary: SummaryStats::default(),
        regime_breakdown: RegimeBreakdown::default(),
        mfe_mae: MfeMaeAggregates::from_trades(&[]),
        skipped_entries: SkippedEntryCounts::default(),
        engine_fingerprint: "fp-test".to_owned(),
        dsl: rsi_oversold_strategy(),
    };

    let rendered = context.render().to_lowercase();

    for needle in [
        "full-bar potential",
        "entry-through-exit bar ranges",
        "the entire exit bar is folded in even when the trade exits at its open",
        "not an experienced path",
    ] {
        assert!(
            rendered.contains(needle),
            "the rendered context must carry {needle:?}, or the numbers read as reachable: {rendered}"
        );
    }
}
