//! AC-3 — `SqliteCoachingRepo`: every coach turn survives a round trip, typed
//! (r1.s2.w2, ADR-0021 / audit C3).
//!
//! The session row is the audit trail, so "it persisted" is not enough: what comes
//! back has to be the same turn. This binary drives the real adapter against a real
//! migrated database and asserts:
//!
//!   1. a proposal session round-trips with its **typed `Mutation`** intact — not a
//!      string, not a lossy projection (`r1.s4`'s modify path edits it);
//!   2. every `CoachFailure` variant round-trips, including
//!      `InapplicableMutation`, which carries a w1 `MutationError` verbatim;
//!   3. `llm_call_id` persists NULL and non-NULL, and NULL means "no ledger row was
//!      correlated to this turn" (audit C3) — not that no attempt was made;
//!   4. `record_disposition` writes exactly one target — `Rejected` — and REFUSES
//!      the accepted disposition outright (r1.s4.w2, `#149`): accepted lineage has
//!      one writer, `CoachAcceptanceRepository::commit_acceptance`, which mints the
//!      child and its run inside the transaction that settles the proposal, so the
//!      accepted cases here drive THAT and this operation owes a refusal;
//!   5. at most one proposal per session, end to end — the accept idempotency key.
//!
//! Offline (`SQLX_OFFLINE=true` + the committed `.sqlx/` + the in-process
//! `MIGRATOR`), `TempDir`-isolated, and deterministic through a `FakeClock`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use chrono::{DateTime, SecondsFormat};
use pulse::{
    AcceptFailureStage, BacktestRunId, CoachAcceptFailure, CoachAcceptanceRepository, CoachFailure,
    CoachRequestFingerprint, CoachSessionClaim, CoachSessionClaimResult, CoachingRepository,
    CoachingSession, CoachingSessionId, Comparator, Condition, Db, Direction, Disposition,
    ExitRule, FakeClock, Hypothesis, InMemoryCoachAcceptanceRepo, IndicatorSpec,
    InitialCoachOutcome, LlmCallId, MIGRATOR, MemoryCoachTurn, Mutation, MutationError, ParamValue,
    PreparedBacktest, PreparedCoachAcceptance, Proposal, RiskParams, SchemaVersion, SeqIdSource,
    Series, SessionOutcome, SqliteCoachAcceptanceRepo, SqliteCoachingRepo, StrategyDsl, StrategyId,
    SweepableValue, ValueSource, VersionId, WalkForwardRunId, apply,
};
use rust_decimal::Decimal;
use sqlx::SqlitePool;
use support::mcp::seeded_walk_forward_draft;
use tempfile::TempDir;

const NOW_MS: i64 = 1_756_425_600_000; // 2026-08-29T00:00:00Z

/// The RFC3339-millis rendering of [`NOW_MS`] — what the adapter's injected clock
/// writes, so a sample built with it round-trips EQUAL.
fn now_rfc3339() -> String {
    DateTime::from_timestamp_millis(NOW_MS)
        .expect("clock in range")
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A `(repo, pool, tempdir)` triple over a fresh migrated tempfile DB with the FK
/// parents seeded and a deterministic clock.
async fn repo() -> (SqliteCoachingRepo<FakeClock>, SqlitePool, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let pool = db.pool().clone();
    seed_parents(&pool).await;
    (
        SqliteCoachingRepo::with_deps(pool.clone(), FakeClock::at(NOW_MS)),
        pool,
        tmp,
    )
}

/// The FK parents a coaching session needs: a strategy, two versions (parent +
/// a child for an accept), two runs, and an `llm_call`.
async fn seed_parents(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO strategy (id, name, tags, archived, created_at) \
         VALUES ('strat-1', 'RSI Oversold', '[]', 0, '2026-08-29T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed strategy");

    // A SECOND strategy, so a cross-strategy child is expressible.
    sqlx::query(
        "INSERT INTO strategy (id, name, tags, archived, created_at) \
         VALUES ('strat-2', 'Another Strategy', '[]', 0, '2026-08-29T00:00:00.000Z')",
    )
    .execute(pool)
    .await
    .expect("seed second strategy");

    // The version TREE the accept path has to reason about (PR #128, finding G2).
    // `ver-1` is what the seeded runs — and therefore the sessions — were produced
    // against; `ver-2` and `ver-3` are its real children, the shape an accept mints.
    // The rest are the wrong shapes, each expressible only because the schema's FK
    // says "some version", not "this one's child".
    for (id, strategy, parent, hash, by) in [
        ("ver-1", "strat-1", None, "hash-1", "human"),
        ("ver-2", "strat-1", Some("ver-1"), "hash-2", "coach_llm"),
        ("ver-3", "strat-1", Some("ver-1"), "hash-3", "coach_llm"),
        ("ver-root", "strat-1", None, "hash-root", "human"),
        (
            "ver-sibling",
            "strat-1",
            Some("ver-root"),
            "hash-sib",
            "coach_llm",
        ),
        (
            "ver-foreign",
            "strat-2",
            Some("ver-1"),
            "hash-foreign",
            "coach_llm",
        ),
    ] {
        sqlx::query(
            "INSERT INTO strategy_version \
             (id, strategy_id, parent_version_id, dsl_schema_version, dsl, dsl_original, \
              version_hash, created_by, creating_llm_call_ids, created_at) \
             VALUES (?1, ?2, ?3, '1.0.0', '{}', '{}', ?4, ?5, '[]', \
                     '2026-08-29T00:00:00.000Z')",
        )
        .bind(id)
        .bind(strategy)
        .bind(parent)
        .bind(hash)
        .bind(by)
        .execute(pool)
        .await
        .expect("seed strategy_version");
    }

    // r1.s4.w4: ONE RUN PER VERSION, not two runs on `ver-1`. `0008` requires an
    // accepted proposal to name the re-backtest OF its child, so every version an
    // accept could name — the two real children and the three wrong-shaped ones —
    // needs a run of its own. Without them the run-ownership trigger would refuse
    // the wrong-lineage tests first and they would stop testing lineage at all.
    for (run, version) in [
        ("run-1", "ver-1"),
        ("run-2", "ver-1"),
        ("run-child-2", "ver-2"),
        ("run-child-3", "ver-3"),
        ("run-root", "ver-root"),
        ("run-sibling", "ver-sibling"),
        ("run-foreign", "ver-foreign"),
    ] {
        // r1.s3.w2: `0006`'s BEFORE INSERT completeness trigger requires every fresh
        // row to name its input provenance. These are FK parents for coaching rows,
        // so the tuple is a plain complete one.
        sqlx::query(
            "INSERT INTO backtest_run \
             (id, strategy_version_id, schema_version, created_at, engine_fingerprint, \
              engine_target, result_content_hash, starting_equity, net_pnl, fees_total, \
              funding_total, slippage_total, pair, primary_timeframe, primary_data_version, \
              taker_fee_bps, slippage_bps, funding_config) \
             VALUES (?1, ?2, '1', '2026-08-29T00:00:00.000Z', 'fp-1', 'test-target', \
                     'rch-1', '10000', '0', '0', '0', '0', \
                     'BTCUSDT', '15m', 'v-primary', '4', '1', 'snapshot_rates')",
        )
        .bind(run)
        .bind(version)
        .execute(pool)
        .await
        .expect("seed backtest_run");
    }

    sqlx::query(
        "INSERT INTO llm_call \
         (id, backend, model, prompt_messages, completion, input_tokens, output_tokens, cost, \
          cost_currency, created_at, created_by, schema_version) \
         VALUES ('call-1', 'ollama', 'glm-5.3-flash', '[]', NULL, 1, 1, '0', 'CNY', \
                 '2026-08-29T00:00:00.000Z', 'coach_llm', 1)",
    )
    .execute(pool)
    .await
    .expect("seed llm_call");
}

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

/// A REAL `MutationError`, driven out of `apply()` rather than hand-built.
fn a_real_mutation_error() -> MutationError {
    apply(
        &rsi_oversold_strategy(),
        &set_period("entry.lhs.indicator.rsi.period", 0),
    )
    .expect_err("an RSI period of 0 fails validation")
}

fn session(id: &str, run: &str, outcome: SessionOutcome, call: Option<&str>) -> CoachingSession {
    CoachingSession {
        id: CoachingSessionId::new(id),
        backtest_run_id: BacktestRunId::new(run),
        strategy_version_id: VersionId::new("ver-1"),
        created_at: now_rfc3339(),
        llm_call_id: call.map(LlmCallId::new),
        outcome,
    }
}

fn a_proposal() -> Proposal {
    Proposal {
        mutation: set_period("entry.lhs.indicator.rsi.period", 21),
        hypothesis: Hypothesis::new("a slower RSI should cut the whipsaw entries")
            .expect("non-empty"),
        disposition: Disposition::Proposed,
        // r1.s4.w4: nothing has tried to accept this yet.
        accept_failure: None,
    }
}

/// One instance of EVERY [`CoachFailure`] variant.
///
/// The trailing match is exhaustive with no `_` arm, so an eighth failure kind
/// stops this file compiling until it is added to the list above it. A bare
/// `vec![...]` cannot hold that line — it silently covered six of seven until
/// `TransportFailure` was caught in review.
fn every_failure_variant() -> Vec<CoachFailure> {
    let all = vec![
        CoachFailure::ZeroCalls,
        CoachFailure::SeveralCalls {
            count: 3,
            propose_mutation_count: 2,
        },
        CoachFailure::MalformedArguments {
            detail: "`path` was not a string".to_owned(),
        },
        CoachFailure::InapplicableMutation {
            mutation: set_period("entry.lhs.indicator.rsi.period", 0),
            error: a_real_mutation_error(),
        },
        CoachFailure::ProviderTimeout { elapsed_ms: 30_000 },
        CoachFailure::ContextOverflow {
            detail: "42kB of DSL against a 32kB window".to_owned(),
        },
        CoachFailure::TransportFailure {
            detail: "HTTP 503 from upstream".to_owned(),
        },
        // r1.s4.w4 — the three `0008` adds.
        // r1.s4.w1 (#131): two fields now — what the coach wanted to change, and
        // which observed numbers motivated it. A serde-JSON payload column, so this
        // is a payload change and not a schema one.
        CoachFailure::InapplicableAdvice {
            intent: "add an ADX filter above 25".to_owned(),
            evidence: "most losing trades opened in the ranging regime".to_owned(),
        },
        CoachFailure::MissingBacktestInputs {
            detail: "the parent run's primary snapshot `v-primary` is not in the store".to_owned(),
        },
        CoachFailure::Interrupted {
            detail: "claimed at 2026-08-29T00:00:00Z by a process that did not finish".to_owned(),
        },
    ];

    let mut tags: Vec<&'static str> = all
        .iter()
        .map(|f| match f {
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
        })
        .collect();
    tags.sort_unstable();
    tags.dedup();
    assert_eq!(tags.len(), all.len(), "one instance per variant: {tags:?}");
    all
}

// ---------------------------------------------------------------------------
// 1. A proposal session round-trips, typed
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_proposal_session_round_trips_with_its_typed_mutation() {
    let (repo, _pool, _tmp) = repo().await;
    let saved = session(
        "sess-1",
        "run-1",
        SessionOutcome::Proposed {
            proposal: a_proposal(),
        },
        Some("call-1"),
    );

    let id = repo.save_session(&saved).await.expect("save_session");
    assert_eq!(id, CoachingSessionId::new("sess-1"));

    let got = repo
        .get_session(&id)
        .await
        .expect("get_session")
        .expect("row present");
    assert_eq!(got, saved, "the turn round-trips verbatim");

    // The typed Mutation specifically — r1.s4's modify path edits it in place.
    match &got.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(
                proposal.mutation,
                set_period("entry.lhs.indicator.rsi.period", 21),
                "the typed Mutation survives storage"
            );
            assert_eq!(
                proposal.hypothesis.as_str(),
                "a slower RSI should cut the whipsaw entries"
            );
            assert_eq!(proposal.disposition, Disposition::Proposed);
        }
        SessionOutcome::Failed { failure } => panic!("expected a proposal, got {failure:?}"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }

    // An absent id is Ok(None), not an error.
    assert!(
        repo.get_session(&CoachingSessionId::new("nope"))
            .await
            .expect("get_session absent")
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// 2. Every failure variant round-trips
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_failure_variant_round_trips() {
    let (repo, _pool, _tmp) = repo().await;

    let failures = every_failure_variant();
    assert_eq!(failures.len(), 10, "every failure kind must round-trip");

    for (i, failure) in failures.into_iter().enumerate() {
        let id = format!("sess-fail-{i}");
        // Which failures leave a ledger row, by EXHAUSTIVE match (no `_` arm): a
        // pre-call refusal never reached the provider, and a transport fault reached
        // it but produced no usable exchange for this process to price — so neither
        // correlates a ledger row and both record `llm_call_id` NULL (audit C3). The
        // transport case is an ATTEMPT that may still have cost something upstream;
        // NULL records what was correlated here, not what was spent there. An eighth
        // variant has to answer this question before this file compiles.
        let call = match &failure {
            // r1.s4.w4: `MissingBacktestInputs` is decided while BUILDING the
            // context, before the provider is reached, so it correlates nothing —
            // the `ContextOverflow` case exactly. `Interrupted` is written by a
            // process that did not make the call and therefore cannot name one;
            // that NULL is the strongest kind, since nothing here even attempted.
            CoachFailure::ContextOverflow { .. }
            | CoachFailure::TransportFailure { .. }
            | CoachFailure::MissingBacktestInputs { .. }
            | CoachFailure::Interrupted { .. } => None,
            // `InapplicableAdvice` is a turn that got a usable RESPONSE — the coach
            // answered, it just answered with structure this release cannot apply —
            // so it names its ledger row like every other answered turn.
            CoachFailure::ZeroCalls
            | CoachFailure::SeveralCalls { .. }
            | CoachFailure::MalformedArguments { .. }
            | CoachFailure::InapplicableMutation { .. }
            | CoachFailure::InapplicableAdvice { .. }
            | CoachFailure::ProviderTimeout { .. } => Some("call-1"),
        };
        let saved = session(
            &id,
            "run-1",
            SessionOutcome::Failed {
                failure: failure.clone(),
            },
            call,
        );

        repo.save_session(&saved).await.expect("save a failed turn");
        let got = repo
            .get_session(&CoachingSessionId::new(&id))
            .await
            .expect("get_session")
            .expect("row present");

        assert_eq!(got, saved, "{failure:?} must round-trip verbatim");
        match got.outcome {
            SessionOutcome::Failed { failure: back } => assert_eq!(back, failure),
            SessionOutcome::Proposed { .. } => panic!("a failed turn must read back failed"),
            SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_inapplicable_mutation_keeps_its_typed_error_through_the_database() {
    let (repo, _pool, _tmp) = repo().await;
    let error = a_real_mutation_error();
    let saved = session(
        "sess-1",
        "run-1",
        SessionOutcome::Failed {
            failure: CoachFailure::InapplicableMutation {
                mutation: set_period("entry.lhs.indicator.rsi.period", 0),
                error: error.clone(),
            },
        },
        Some("call-1"),
    );

    repo.save_session(&saved).await.expect("save");
    let got = repo
        .get_session(&CoachingSessionId::new("sess-1"))
        .await
        .expect("get")
        .expect("present");

    match got.outcome {
        SessionOutcome::Failed {
            failure: CoachFailure::InapplicableMutation { error: back, .. },
        } => {
            assert_eq!(
                back, error,
                "the w1 MutationError survives the database verbatim"
            );
            assert!(matches!(back, MutationError::ValidationFailed { .. }));
        }
        other => panic!("expected an InapplicableMutation failure, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// 3. llm_call_id — NULL means "no ledger row was correlated to this turn" (audit
//    C3); it does NOT mean no attempt was made
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_call_id_persists_null_and_non_null() {
    let (repo, pool, _tmp) = repo().await;

    repo.save_session(&session(
        "sess-called",
        "run-1",
        SessionOutcome::Proposed {
            proposal: a_proposal(),
        },
        Some("call-1"),
    ))
    .await
    .expect("save a turn that reached the provider");

    repo.save_session(&session(
        "sess-precall",
        "run-1",
        SessionOutcome::Failed {
            failure: CoachFailure::ContextOverflow {
                detail: "the DSL does not fit".to_owned(),
            },
        },
        None,
    ))
    .await
    .expect("save a pre-call failure");

    let called = repo
        .get_session(&CoachingSessionId::new("sess-called"))
        .await
        .expect("get")
        .expect("present");
    assert_eq!(called.llm_call_id, Some(LlmCallId::new("call-1")));

    let precall = repo
        .get_session(&CoachingSessionId::new("sess-precall"))
        .await
        .expect("get")
        .expect("present");
    assert!(
        precall.llm_call_id.is_none(),
        "a pre-call failure records no ledger row"
    );

    // And the column really is NULL, not an empty string.
    let raw: Option<String> =
        sqlx::query_scalar("SELECT llm_call_id FROM coaching_sessions WHERE id = 'sess-precall'")
            .fetch_one(&pool)
            .await
            .expect("read llm_call_id");
    assert!(raw.is_none(), "the stored column is SQL NULL, got {raw:?}");
}

// ---------------------------------------------------------------------------
// 4. Disposition — `Rejected` is the only target this operation writes
//
//    r1.s4.w2 (`pulseai-labs/pulse-trader#149`): the accepted arm is RETIRED.
//    `CoachAcceptanceRepository::commit_acceptance` is the ONE writer of accepted
//    lineage — it mints the child version and its run inside the transaction that
//    settles the proposal — so the cases below that used to drive `Accepted` through
//    `record_disposition` now drive it through the acceptance adapter, and what
//    `record_disposition` owes is a refusal.
// ---------------------------------------------------------------------------

/// The SQLite acceptance adapter over the test pool, deterministic in both the ids
/// it mints and the timestamp it stamps.
fn accepts(pool: &SqlitePool) -> SqliteCoachAcceptanceRepo<FakeClock, SeqIdSource> {
    SqliteCoachAcceptanceRepo::with_deps(
        pool.clone(),
        FakeClock::at(NOW_MS),
        SeqIdSource::with_prefix("minted"),
    )
}

/// A prepared acceptance for `session`, with an empty trade log — these cases are
/// about the LINKS an accept writes, not about the run's contents.
fn prepared_for(session: &str) -> PreparedCoachAcceptance {
    PreparedCoachAcceptance {
        session_id: CoachingSessionId::new(session),
        ..prepared_acceptance()
    }
}

/// A prepared acceptance built from a mutation that is NOT the one on record —
/// what an accept holds after another process modified the proposal underneath it.
fn prepared_from_stale_mutation(session: &str) -> PreparedCoachAcceptance {
    PreparedCoachAcceptance {
        session_id: CoachingSessionId::new(session),
        expected_mutation: set_period("entry.lhs.indicator.rsi.period", 99),
        ..prepared_acceptance()
    }
}

/// A modify that lands while an accept is being computed does not let that accept
/// commit a child built from the mutation it replaced.
///
/// Applying, loading snapshots and re-running all happen outside the final
/// transaction — they must, because none of them may hold a write lock across CPU
/// work — so a second process can record a modify in that window. The proposal is
/// still OPEN afterwards, so "is it open" cannot catch this; only comparing the
/// mutation can. Without the check the child commits, every constraint passes, and
/// the row claims a child its stored mutation never produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accept_whose_proposal_changed_underneath_it_is_refused() {
    let (repo, pool, _tmp) = repo().await;
    let id = a_proposed_session(&repo, "sess-1").await;

    // The proposal on record is the fixture's; this accept was computed from
    // another mutation, exactly as it would be after a concurrent modify.
    let err = accepts(&pool)
        .commit_acceptance(prepared_from_stale_mutation("sess-1"))
        .await
        .expect_err("an accept built from a replaced mutation is refused");
    assert!(
        err.to_string().contains("changed while this accept"),
        "the refusal says the proposal moved, not merely that something failed: {err}"
    );

    // Nothing was written: no child, and the proposal is still actionable.
    let got = repo.get_session(&id).await.expect("get").expect("present");
    match &got.outcome {
        SessionOutcome::Proposed { proposal } => assert_eq!(
            proposal.disposition,
            Disposition::Proposed,
            "the refused accept left the proposal open"
        ),
        other => panic!("expected an open proposal, got {other:?}"),
    }
}

/// Point `ver-1`'s `latest_walk_forward_run_id` at a real run row — the cell
/// the accept's second optimistic lock re-reads inside its transaction.
async fn certify_ver_1(pool: &SqlitePool) {
    sqlx::query(
        "INSERT INTO walk_forward_run \
         (id, seq, strategy_version_id, created_at, scheme, rule, k, \
          span_from_ms, span_to_ms, from_defaulted, engine_fingerprint, \
          folds_holding, folds_required, pooled_n, pooled_mean_r, \
          pooled_lower_bound, pass) \
         VALUES ('wf-1', (SELECT COALESCE(MAX(seq), 0) + 1 FROM walk_forward_run), \
                 'ver-1', '2026-08-29T01:00:00.000Z', 'rolling-oos/v1', 'wf-v1', \
                 6, 0, 1_000, 1, 'fp-1', 4, 4, 80, '0.5', 0.2, 1)",
    )
    .execute(pool)
    .await
    .expect("seed the certifying walk-forward run");
    sqlx::query(
        "UPDATE strategy_version SET latest_walk_forward_run_id = 'wf-1' WHERE id = 'ver-1'",
    )
    .execute(pool)
    .await
    .expect("advance the parent's certification pointer");
}

/// A walk-forward landing between the gate's read and the commit moves the
/// parent's `latest_walk_forward_run_id` — and the accept must refuse, exactly
/// like a proposal whose mutation changed underneath it: the certification
/// draft was computed against the run the pointer USED to name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accept_whose_parent_certification_pointer_moved_is_refused() {
    let (repo, pool, _tmp) = repo().await;
    let id = a_proposed_session(&repo, "sess-1").await;
    certify_ver_1(&pool).await;

    // `prepared_for` carries `None` — the pointer as it stood when the accept
    // was computed, before `certify_ver_1` moved it.
    let err = accepts(&pool)
        .commit_acceptance(prepared_for("sess-1"))
        .await
        .expect_err("an accept computed against a stale certification pointer is refused");
    assert!(
        err.to_string().contains("certification pointer moved"),
        "the refusal names the moved pointer, not merely that something failed: {err}"
    );

    // Nothing was written: no child, and the proposal is still actionable.
    let got = repo.get_session(&id).await.expect("get").expect("present");
    match &got.outcome {
        SessionOutcome::Proposed { proposal } => assert_eq!(
            proposal.disposition,
            Disposition::Proposed,
            "the refused accept left the proposal open"
        ),
        other => panic!("expected an open proposal, got {other:?}"),
    }

    // The same accept with the pointer it SHOULD have read commits — the guard
    // is a compare, not a blanket refusal.
    let mut matching = prepared_for("sess-1");
    matching.expected_certification_pointer = Some(WalkForwardRunId::new("wf-1"));
    accepts(&pool)
        .commit_acceptance(matching)
        .await
        .expect("a pointer that still matches the parent's commits");
}

/// An accept whose proposal was modified AND accepted by someone else is refused —
/// it does not get the other accept's ids back as an idempotent replay.
///
/// The replay branch exists so a client that lost the response can retry: the
/// session id is the accept idempotency key, so the same accept must return the
/// same two ids. It is not a licence to hand ANY caller the accepted ids. Process A
/// prepares an accept for M1, process B modifies to M2 and accepts first, and A's
/// commit lands on a proposal that is accepted and carries M2 — returning B's child
/// would tell A's trader the mutation they reviewed was accepted, when the stored
/// child came from a different one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accept_replay_is_refused_when_the_accepted_mutation_is_not_the_one_prepared() {
    let (repo, pool, _tmp) = repo().await;
    let id = a_proposed_session(&repo, "sess-1").await;

    // B's accept lands first, against the proposal as it stands.
    let landed = accepts(&pool)
        .commit_acceptance(prepared_for("sess-1"))
        .await
        .expect("B's accept commits");

    // A arrives with a child built from a mutation the proposal no longer carries.
    let err = accepts(&pool)
        .commit_acceptance(prepared_from_stale_mutation("sess-1"))
        .await
        .expect_err("A's accept is refused, not replayed");
    assert!(
        err.to_string().contains("changed while this accept"),
        "the refusal says the proposal moved: {err}"
    );
    assert!(
        !err.to_string().contains(landed.child_version_id.as_str()),
        "and it does not hand back the other accept's child: {err}"
    );

    // B's accept is untouched — the refusal settled nothing and minted nothing.
    let got = repo.get_session(&id).await.expect("get").expect("present");
    match &got.outcome {
        SessionOutcome::Proposed { proposal } => assert_eq!(
            proposal.disposition,
            Disposition::Accepted {
                child_version_id: landed.child_version_id.clone(),
                accepted_run_id: landed.accepted_run_id.clone(),
            },
            "the accepted row still names B's child and run"
        ),
        other => panic!("expected an accepted proposal, got {other:?}"),
    }
}

/// The in-memory adapter refuses the same replay, in the same order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_memory_adapter_refuses_a_replay_of_a_different_accepted_mutation() {
    let repo = in_memory_repo();

    let landed = repo
        .commit_acceptance(prepared_for("sess-1"))
        .await
        .expect("B's accept commits");

    let err = repo
        .commit_acceptance(prepared_from_stale_mutation("sess-1"))
        .await
        .expect_err("A's accept is refused, not replayed");
    assert!(
        err.to_string().contains("changed while this accept"),
        "the refusal says the proposal moved: {err}"
    );

    let children = repo.accepted_children().expect("read the children");
    assert_eq!(
        children.len(),
        1,
        "the refused replay minted nothing further"
    );
    assert_eq!(children[0].child_version_id, landed.child_version_id);
}

/// The in-memory adapter refuses the same race the SQLite one refuses.
///
/// A test adapter that admits an interleaving the real one rejects certifies the
/// wrong thing, so this is the same case against the other implementation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_memory_adapter_refuses_an_accept_whose_proposal_changed() {
    let repo = in_memory_repo();

    let err = repo
        .commit_acceptance(prepared_from_stale_mutation("sess-1"))
        .await
        .expect_err("an accept built from a replaced mutation is refused");
    assert!(
        err.to_string().contains("changed while this accept"),
        "the refusal says the proposal moved: {err}"
    );
    assert!(
        repo.accepted_children()
            .expect("read the children")
            .is_empty(),
        "the refused accept minted no child"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejection_names_no_child_and_a_failed_turn_has_no_proposal() {
    let (repo, pool, _tmp) = repo().await;
    let id = CoachingSessionId::new("sess-1");
    repo.save_session(&session(
        "sess-1",
        "run-1",
        SessionOutcome::Proposed {
            proposal: a_proposal(),
        },
        Some("call-1"),
    ))
    .await
    .expect("save");

    // A non-accepting disposition leaves BOTH links NULL.
    repo.record_disposition(&id, &Disposition::Rejected)
        .await
        .expect("record rejected");
    let (child, run): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT child_version_id, accepted_run_id FROM coaching_proposals WHERE session_id = ?1",
    )
    .bind("sess-1")
    .fetch_one(&pool)
    .await
    .expect("read the links");
    assert!(
        child.is_none() && run.is_none(),
        "only an accepted proposal names a child and a run, got {child:?} / {run:?}"
    );
    let got = repo.get_session(&id).await.expect("get").expect("present");
    match &got.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(proposal.disposition, Disposition::Rejected);
        }
        SessionOutcome::Failed { .. } => panic!("still a proposal session"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }

    // Recording a disposition on a FAILED session has no proposal to move.
    repo.save_session(&session(
        "sess-failed",
        "run-1",
        SessionOutcome::Failed {
            failure: CoachFailure::ZeroCalls,
        },
        Some("call-1"),
    ))
    .await
    .expect("save a failed turn");
    assert!(
        repo.record_disposition(
            &CoachingSessionId::new("sess-failed"),
            &Disposition::Rejected
        )
        .await
        .is_err(),
        "a failed turn has no proposal to disposition"
    );
}

/// The accepted state is reached through `commit_acceptance`, and reads back through
/// the ORDINARY session read carrying both links.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_accept_reads_back_with_both_links_through_the_session() {
    let (repo, pool, _tmp) = repo().await;
    let id = a_proposed_session(&repo, "sess-1").await;

    let outcome = accepts(&pool)
        .commit_acceptance(prepared_for("sess-1"))
        .await
        .expect("commit the acceptance");

    let got = repo.get_session(&id).await.expect("get").expect("present");
    match &got.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(
                proposal.disposition,
                Disposition::Accepted {
                    child_version_id: outcome.child_version_id.clone(),
                    accepted_run_id: outcome.accepted_run_id.clone(),
                },
                "the accepted disposition reads back with BOTH links"
            );
        }
        SessionOutcome::Failed { .. } => panic!("still a proposal session"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }

    // The child really is a child of the COACHED version, derived from the session
    // row rather than supplied — which is why no caller can name a foreign one.
    let parent: Option<String> =
        sqlx::query_scalar("SELECT parent_version_id FROM strategy_version WHERE id = ?1")
            .bind(outcome.child_version_id.as_str())
            .fetch_one(&pool)
            .await
            .expect("read the child");
    assert_eq!(parent.as_deref(), Some("ver-1"));
}

/// Seed one `proposed` session and return its id.
async fn a_proposed_session(repo: &SqliteCoachingRepo<FakeClock>, id: &str) -> CoachingSessionId {
    repo.save_session(&session(
        id,
        "run-1",
        SessionOutcome::Proposed {
            proposal: a_proposal(),
        },
        Some("call-1"),
    ))
    .await
    .expect("save");
    CoachingSessionId::new(id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_proposal_cannot_be_re_dispositioned() {
    let (repo, pool, _tmp) = repo().await;
    let id = a_proposed_session(&repo, "sess-1").await;
    let outcome = accepts(&pool)
        .commit_acceptance(prepared_for("sess-1"))
        .await
        .expect("accept an open proposal");

    // `Accepted` is terminal (`Proposal::transition`). Neither a rejection nor a
    // second accept may move it.
    assert!(
        repo.record_disposition(&id, &Disposition::Rejected)
            .await
            .is_err(),
        "an accepted proposal must not be re-recorded as rejected"
    );
    let replay = accepts(&pool)
        .commit_acceptance(prepared_for("sess-1"))
        .await
        .expect("replaying the accept is idempotent, not a second child");
    assert_eq!(replay, outcome, "the SAME child and run come back");

    // The row is untouched by the refused write, and still names the first child.
    let child: Option<String> =
        sqlx::query_scalar("SELECT child_version_id FROM coaching_proposals WHERE session_id = ?1")
            .bind("sess-1")
            .fetch_one(&pool)
            .await
            .expect("read child_version_id");
    assert_eq!(
        child.as_deref(),
        Some(outcome.child_version_id.as_str()),
        "the refused write changed nothing"
    );
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM strategy_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(versions, 7, "the six seeded versions plus ONE minted child");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaying_the_same_accept_is_a_no_op() {
    let (repo, pool, _tmp) = repo().await;
    let id = a_proposed_session(&repo, "sess-1").await;
    let accepts = accepts(&pool);

    let first = accepts
        .commit_acceptance(prepared_for("sess-1"))
        .await
        .expect("the accept");
    // The session id IS the accept idempotency key: a retry of the SAME accept has
    // to succeed, or a client that lost the response can never safely retry.
    let second = accepts
        .commit_acceptance(prepared_for("sess-1"))
        .await
        .expect("replaying the identical accept is idempotent");
    assert_eq!(first, second);

    let got = repo.get_session(&id).await.expect("get").expect("present");
    match &got.outcome {
        SessionOutcome::Proposed { proposal } => assert_eq!(
            proposal.disposition,
            Disposition::Accepted {
                child_version_id: first.child_version_id.clone(),
                accepted_run_id: first.accepted_run_id.clone(),
            }
        ),
        SessionOutcome::Failed { .. } => panic!("still a proposal session"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }
}

/// `record_disposition` is not a writer of accepted lineage AT ALL (#149).
///
/// This used to be three tests, one per false-provenance shape a caller could name:
/// a root version, a version parented elsewhere, and another strategy's version.
/// Each was individually legal as a row and jointly a false lineage claim, and each
/// was caught by an in-transaction provenance proof. `commit_acceptance` removes the
/// class rather than checking it — it DERIVES the parent and the strategy from the
/// claimed session row, so `PreparedCoachAcceptance` has nowhere to put a foreign
/// child — and this operation now refuses the accepted disposition outright. So the
/// three shapes collapse into one statement: whatever ids you name, including the
/// legitimate ones, the answer is no.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_disposition_refuses_every_accepted_disposition() {
    let (repo, pool, _tmp) = repo().await;
    let id = a_proposed_session(&repo, "sess-1").await;

    for (label, child, run) in [
        ("a legitimately shaped child", "ver-2", "run-child-2"),
        ("a root version with no parent", "ver-root", "run-root"),
        ("a version parented elsewhere", "ver-sibling", "run-sibling"),
        ("another strategy's version", "ver-foreign", "run-foreign"),
    ] {
        let outcome = repo
            .record_disposition(
                &id,
                &Disposition::Accepted {
                    child_version_id: VersionId::new(child),
                    accepted_run_id: BacktestRunId::new(run),
                },
            )
            .await;
        let err = outcome.expect_err(&format!("`{label}` must be refused"));
        assert!(
            err.to_string().contains("commit_acceptance"),
            "the refusal names the one writer that may do this: {err}"
        );
        assert_untouched(&pool, "sess-1").await;
    }
}

/// The proposal row is still open and unsettled after a refused accept.
async fn assert_untouched(pool: &SqlitePool, session_id: &str) {
    let (disposition, child): (String, Option<String>) = sqlx::query_as(
        "SELECT disposition, child_version_id FROM coaching_proposals WHERE session_id = ?1",
    )
    .bind(session_id)
    .fetch_one(pool)
    .await
    .expect("read the proposal row");

    assert_eq!(disposition, "proposed", "a refused accept settles nothing");
    assert!(
        child.is_none(),
        "a refused accept names no child, got {child:?}"
    );
}

/// Two clients replaying the SAME accept must both succeed (PR #128, finding H2).
///
/// The accept is idempotent by session id, so a client that lost the response
/// retries — and two retries can land together. `commit_acceptance` takes the write
/// lock with its FIRST statement rather than upgrading a read snapshot: in WAL, an
/// upgrade whose snapshot another commit has moved past fails immediately with
/// `SQLITE_BUSY_SNAPSHOT`, which `busy_timeout` does not cover.
///
/// Rounds rather than a single pair, because the interleaving is the scheduler's to
/// choose: each round is an independent session, and every one of them must settle
/// on exactly one child.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_identical_accepts_both_succeed() {
    let (repo, pool, _tmp) = repo().await;
    let accepts = accepts(&pool);

    for round in 0..20 {
        let session_id = format!("sess-race-{round}");
        a_proposed_session(&repo, &session_id).await;

        let (first, second) = tokio::join!(
            accepts.commit_acceptance(prepared_for(&session_id)),
            accepts.commit_acceptance(prepared_for(&session_id)),
        );

        let first = first
            .unwrap_or_else(|e| panic!("round {round}: the first accept must succeed, got {e}"));
        let second = second
            .unwrap_or_else(|e| panic!("round {round}: the second accept must succeed, got {e}"));
        assert_eq!(
            first, second,
            "round {round}: both accepts must settle on the ONE child"
        );

        let child: Option<String> = sqlx::query_scalar(
            "SELECT child_version_id FROM coaching_proposals WHERE session_id = ?1",
        )
        .bind(&session_id)
        .fetch_one(&pool)
        .await
        .expect("read child_version_id");
        assert_eq!(
            child.as_deref(),
            Some(first.child_version_id.as_str()),
            "round {round}: the stored link is the one both accepts returned"
        );
    }
}

/// A stored `created_at` that is not RFC3339 is a corrupt row, and reading it back
/// as if it were a timestamp is how a corrupt row becomes a wrong answer to an audit
/// question (PR #128, finding H3). `SqliteLlmCallRepo` already fails closed on this;
/// the coaching read did not. The stored text is returned UNCHANGED on success — the
/// validation is a check, not a normalisation, because rewriting an audit value on
/// read is its own kind of lie.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_created_at_is_refused_on_read() {
    let (repo, pool, _tmp) = repo().await;
    insert_raw_failed_session(&pool, "sess-bad", "yesterday").await;

    let outcome = repo.get_session(&CoachingSessionId::new("sess-bad")).await;

    let err = outcome.expect_err("a malformed created_at must fail the read closed");
    let rendered = err.to_string();
    assert!(
        rendered.contains("created_at") && rendered.contains("yesterday"),
        "the error must name the column and the value it refused: {rendered}"
    );

    // A well-formed row beside it still reads, and its timestamp comes back byte for
    // byte as stored.
    insert_raw_failed_session(&pool, "sess-good", "2026-08-29T00:00:00.000Z").await;
    let good = repo
        .get_session(&CoachingSessionId::new("sess-good"))
        .await
        .expect("a well-formed row still reads")
        .expect("present");
    assert_eq!(good.created_at, "2026-08-29T00:00:00.000Z");
}

/// The list path must fail closed on the same row: it reads ids and then routes each
/// through `get_session`, which is the one place the decoding lives — a list that
/// decoded rows a second way would be a second truth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_row_fails_the_list_too() {
    let (repo, pool, _tmp) = repo().await;
    insert_raw_failed_session(&pool, "sess-bad", "2026-08-29").await;

    let outcome = repo
        .list_sessions_for_run(&BacktestRunId::new("run-1"))
        .await;

    assert!(
        outcome.is_err(),
        "one corrupt row must fail the list, not be skipped out of it"
    );
}

/// Write a `failed` coaching session straight to SQL, `created_at` and all — the
/// only way to produce a row the adapter itself would never write.
async fn insert_raw_failed_session(pool: &SqlitePool, id: &str, created_at: &str) {
    let detail = serde_json::to_string(&CoachFailure::ContextOverflow {
        detail: "a pathological document".to_owned(),
    })
    .expect("serialize the failure");

    sqlx::query(
        "INSERT INTO coaching_sessions \
         (id, backtest_run_id, strategy_version_id, created_at, llm_call_id, outcome, \
          failure_kind, failure_detail, schema_version) \
         VALUES (?1, 'run-1', 'ver-1', ?2, NULL, 'failed', 'context_overflow', ?3, 1)",
    )
    .bind(id)
    .bind(created_at)
    .bind(detail)
    .execute(pool)
    .await
    .expect("seed the raw session row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modified_and_proposed_are_refused_as_targets() {
    let (repo, _pool, _tmp) = repo().await;
    let id = a_proposed_session(&repo, "sess-1").await;

    // A modify is an EDIT: it replaces the proposal's stored mutation. Recording
    // the state alone would leave a row that says "edited" while carrying the
    // un-edited mutation, so this operation refuses it rather than half-writing it.
    assert!(
        repo.record_disposition(&id, &Disposition::Modified)
            .await
            .is_err(),
        "a `modified` disposition must be written with the edited mutation, not alone"
    );
    // Nothing returns to the initial state.
    assert!(
        repo.record_disposition(&id, &Disposition::Proposed)
            .await
            .is_err(),
        "nothing returns to `proposed`"
    );

    let got = repo.get_session(&id).await.expect("get").expect("present");
    match &got.outcome {
        SessionOutcome::Proposed { proposal } => {
            assert_eq!(
                proposal.disposition,
                Disposition::Proposed,
                "the refused writes left the proposal open"
            );
        }
        SessionOutcome::Failed { .. } => panic!("still a proposal session"),
        SessionOutcome::Pending => panic!("expected a settled turn, got an open claim"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_kind_that_disagrees_with_its_detail_is_refused_on_read() {
    let (repo, pool, _tmp) = repo().await;
    repo.save_session(&session(
        "sess-1",
        "run-1",
        SessionOutcome::Failed {
            failure: CoachFailure::ZeroCalls,
        },
        Some("call-1"),
    ))
    .await
    .expect("save a failed turn");

    // `failure_kind` is the QUERYABLE column — an audit scan for every
    // `provider_timeout` reads it, not the JSON. A row whose tag disagrees with its
    // detail would answer that scan wrongly, which is worse than answering with an
    // error.
    sqlx::query(
        "UPDATE coaching_sessions SET failure_kind = 'provider_timeout' WHERE id = 'sess-1'",
    )
    .execute(&pool)
    .await
    .expect("desync the kind from the detail");

    let err = repo
        .get_session(&CoachingSessionId::new("sess-1"))
        .await
        .expect_err("a disagreeing failure_kind must fail closed");
    let message = err.to_string();
    assert!(
        message.contains("provider_timeout") && message.contains("zero_calls"),
        "the error must name both sides of the disagreement: {message}"
    );
}

// ---------------------------------------------------------------------------
// 5. One proposal per session, end to end
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_session_carries_at_most_one_proposal() {
    let (repo, pool, _tmp) = repo().await;
    let first = session(
        "sess-1",
        "run-1",
        SessionOutcome::Proposed {
            proposal: a_proposal(),
        },
        Some("call-1"),
    );
    repo.save_session(&first).await.expect("the first save");

    // Re-saving the same session id is the retry the accept idempotency key exists
    // to make safe: it must be refused, not silently doubled.
    let again = repo.save_session(&first).await;
    assert!(
        again.is_err(),
        "a second proposal for the same session must be refused"
    );

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM coaching_proposals WHERE session_id = 'sess-1'")
            .fetch_one(&pool)
            .await
            .expect("count proposals");
    assert_eq!(count, 1, "exactly one proposal row survives the retry");
}

// ---------------------------------------------------------------------------
// 6. Listing a run's turns
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_sessions_for_run_returns_that_runs_turns() {
    let (repo, _pool, _tmp) = repo().await;

    repo.save_session(&session(
        "sess-1",
        "run-1",
        SessionOutcome::Proposed {
            proposal: a_proposal(),
        },
        Some("call-1"),
    ))
    .await
    .expect("save");
    repo.save_session(&session(
        "sess-2",
        "run-1",
        SessionOutcome::Failed {
            failure: CoachFailure::ZeroCalls,
        },
        Some("call-1"),
    ))
    .await
    .expect("save");
    repo.save_session(&session(
        "sess-3",
        "run-2",
        SessionOutcome::Failed {
            failure: CoachFailure::ProviderTimeout { elapsed_ms: 1_000 },
        },
        Some("call-1"),
    ))
    .await
    .expect("save");

    let run_1 = repo
        .list_sessions_for_run(&BacktestRunId::new("run-1"))
        .await
        .expect("list run-1");
    let ids: Vec<&str> = run_1.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["sess-1", "sess-2"],
        "both of run-1's turns, and only those"
    );

    // Both outcome kinds come back — a failed turn is a turn.
    assert!(matches!(run_1[0].outcome, SessionOutcome::Proposed { .. }));
    assert!(matches!(run_1[1].outcome, SessionOutcome::Failed { .. }));

    let none = repo
        .list_sessions_for_run(&BacktestRunId::new("run-absent"))
        .await
        .expect("list an unknown run");
    assert!(none.is_empty(), "an unknown run has no turns");
}

// ---------------------------------------------------------------------------
// 6. The in-memory `CoachAcceptanceRepository` — a test adapter that MINTS the
//    same way (r1.s4.w4)
// ---------------------------------------------------------------------------

/// An in-memory acceptance repo with one proposed turn registered.
fn in_memory_repo() -> InMemoryCoachAcceptanceRepo<FakeClock, SeqIdSource> {
    let repo =
        InMemoryCoachAcceptanceRepo::new(FakeClock::at(NOW_MS), SeqIdSource::with_prefix("minted"));
    repo.register_turn(MemoryCoachTurn {
        session_id: CoachingSessionId::new("sess-1"),
        strategy_id: StrategyId::new("strat-1"),
        parent_version_id: VersionId::new("ver-1"),
        llm_call_id: Some(LlmCallId::new("call-1")),
        parent_certification: None,
        outcome: SessionOutcome::Proposed {
            proposal: a_proposal(),
        },
    })
    .expect("register the turn");
    repo
}

/// A prepared acceptance with an EMPTY trade log. The in-memory adapter stores the
/// prepared result rather than mapping it to columns, so the run's contents are not
/// what this pair of tests is about — the identity and the provenance are.
fn prepared_acceptance() -> PreparedCoachAcceptance {
    let trades = Vec::new();
    let starting_equity = Decimal::new(10_000, 0);
    let equity_curve = pulse::EquityCurve::from_trades(0, starting_equity, &trades);
    let summary = pulse::SummaryStats::from_trades(
        &trades,
        Decimal::ZERO,
        Decimal::ZERO,
        Decimal::ZERO,
        &equity_curve,
    );
    PreparedCoachAcceptance {
        session_id: CoachingSessionId::new("sess-1"),
        // The accept's optimistic lock: the fixture proposal's own mutation, so the
        // guard passes for every case that is not testing the guard itself.
        expected_mutation: a_proposal().mutation,
        // The second optimistic lock: the fixture versions carry no certification
        // pointer, so `None` is what the accept read — the cases that test the
        // guard itself move the pointer explicitly.
        expected_certification_pointer: None,
        child_dsl: rsi_oversold_strategy(),
        prepared_run: PreparedBacktest {
            inputs: pulse::BacktestInputs {
                pair: pulse::Pair::new("BTCUSDT"),
                primary: pulse::SnapshotSelection {
                    timeframe: pulse::Timeframe::M15,
                    data_version: pulse::DataVersion::new("v-primary"),
                },
                htf: None,
                taker_fee_bps: Decimal::new(4, 0),
                slippage_bps: Decimal::new(1, 0),
                funding: pulse::FundingConfig::SnapshotRates,
                window: None,
                lead_in_from_ms: None,
            },
            result: pulse::BacktestResult {
                trades,
                net_pnl: Decimal::ZERO,
                fees_total: Decimal::ZERO,
                funding_total: Decimal::ZERO,
                slippage_total: Decimal::ZERO,
                regime_breakdown: pulse::RegimeBreakdown::default(),
                skipped_entries: pulse::SkippedEntryCounts::default(),
                open_position: None,
                engine_fingerprint: pulse::EngineFingerprint::current(),
                summary: summary.clone(),
                equity_curve,
            },
            summary,
            starting_equity,
        },
        walk_forward: None,
    }
}

/// The in-memory adapter MINTS the child and run the same way the SQLite one does —
/// from the injected `IdSource`, child first — and DERIVES provenance from the
/// registered turn. A test double that took provenance from the caller would
/// certify exactly the mismatch `PreparedCoachAcceptance`'s missing identity fields
/// exist to make impossible.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_memory_adapter_mints_and_derives_the_same_way() {
    let repo = in_memory_repo();

    let outcome = repo
        .commit_acceptance(prepared_acceptance())
        .await
        .expect("commit");
    assert_eq!(
        outcome.child_version_id,
        VersionId::new("minted-0"),
        "the child id comes from the injected source, first"
    );
    assert_eq!(
        outcome.accepted_run_id,
        BacktestRunId::new("minted-1"),
        "then the run id — the SQLite adapter's order"
    );

    let children = repo.accepted_children().expect("children");
    assert_eq!(children.len(), 1, "one accept, one child");
    let child = &children[0];
    assert_eq!(child.strategy_id, StrategyId::new("strat-1"));
    assert_eq!(child.parent_version_id, VersionId::new("ver-1"));
    assert_eq!(child.created_by, pulse::CreatedBy::CoachLlm);
    assert_eq!(child.creating_llm_call_ids, vec!["call-1".to_owned()]);
    assert_eq!(child.created_at, now_rfc3339(), "from the injected clock");

    // The proposal is settled with BOTH links, and the replay is a no-op.
    let proposal = repo
        .proposal(&CoachingSessionId::new("sess-1"))
        .expect("read")
        .expect("present");
    assert_eq!(
        proposal.disposition,
        Disposition::Accepted {
            child_version_id: outcome.child_version_id.clone(),
            accepted_run_id: outcome.accepted_run_id.clone(),
        }
    );
    let replay = repo
        .commit_acceptance(prepared_acceptance())
        .await
        .expect("replaying an accept is idempotent");
    assert_eq!(replay, outcome, "the same pair comes back");
    assert_eq!(
        repo.accepted_children().expect("children").len(),
        1,
        "and nothing new was minted"
    );
}

/// The in-memory adapter carries the same second optimistic lock the SQLite one
/// does: a parent certification pointer that moved between the gate's read and
/// the commit is refused, not committed against.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_memory_adapter_refuses_a_moved_certification_pointer() {
    let repo = in_memory_repo();

    // A memory-side walk-forward save landing between the accept's computation
    // and its commit — the parent's pointer is no longer the `None` the gate read.
    repo.record_certification(&VersionId::new("ver-1"), WalkForwardRunId::new("wf-later"))
        .expect("move the parent's pointer");

    let err = repo
        .commit_acceptance(prepared_acceptance())
        .await
        .expect_err("a stale expected certification pointer is refused");
    assert!(
        err.to_string().contains("certification pointer moved"),
        "the refusal names the moved pointer: {err}"
    );
    assert!(
        repo.accepted_children().expect("children").is_empty(),
        "nothing was minted"
    );

    // The pointer the accept SHOULD have read commits — the guard compares, it
    // does not blanket-refuse.
    let mut matching = prepared_acceptance();
    matching.expected_certification_pointer = Some(WalkForwardRunId::new("wf-later"));
    repo.commit_acceptance(matching)
        .await
        .expect("a pointer that still matches commits");
}

/// The in-memory replay answers the child's CURRENT certification pointer, the
/// way the SQLite replay does
/// (`coach_walk_forward_gate::a_replay_reports_the_childs_current_pointer`): the
/// accept writes the child's initial pointer in the same act as its row, and a
/// later memory-side walk-forward save — the `record_certification` seam — is
/// what a replay reports afterwards. Reading the accept-time field instead would
/// certify stale replay behavior in every test built on this adapter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_memory_replay_reports_the_childs_current_pointer() {
    let repo = in_memory_repo();

    // A gate-passing draft, so the accept mints the child's certification id.
    let mut certified = prepared_acceptance();
    certified.walk_forward = Some(seeded_walk_forward_draft(true));
    let committed = repo
        .commit_acceptance(certified)
        .await
        .expect("the commit lands");
    let original = committed
        .walk_forward_run_id
        .clone()
        .expect("the accept certifies the child");

    // The first replay answers the accept's own certifying run.
    let replay = repo
        .commit_acceptance(prepared_acceptance())
        .await
        .expect("replaying an accept is idempotent");
    assert_eq!(
        replay
            .walk_forward_run_id
            .as_ref()
            .map(WalkForwardRunId::as_str),
        Some(original.as_str()),
        "the child's pointer is the run the accept committed"
    );

    // A LATER walk-forward ON THE CHILD — the memory-side save moves the child's
    // pointer — and the replay must answer with the new run, not the original.
    repo.record_certification(
        &committed.child_version_id,
        WalkForwardRunId::new("wf-later"),
    )
    .expect("move the child's pointer");
    let replay = repo
        .commit_acceptance(prepared_acceptance())
        .await
        .expect("the second replay resolves");
    assert_eq!(
        replay
            .walk_forward_run_id
            .as_ref()
            .map(WalkForwardRunId::as_str),
        Some("wf-later"),
        "the replay names the child's CURRENT pointer, not the accept's run"
    );
}

/// The in-memory adapter refuses what the real one refuses: a failed accept on a
/// settled proposal, and an accept on a turn that produced none.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_in_memory_adapter_refuses_what_the_sqlite_one_refuses() {
    let repo = in_memory_repo();
    let id = CoachingSessionId::new("sess-1");
    let failure = CoachAcceptFailure {
        stage: AcceptFailureStage::Compile,
        message: "the child DSL does not compile".to_owned(),
        subject: None,
    };

    // On an open proposal it lands, and it is the LATEST outcome.
    let proposal = repo
        .record_accept_failure(&id, failure.clone())
        .await
        .expect("record on an open proposal");
    assert_eq!(proposal.accept_failure.as_ref(), Some(&failure));
    assert_eq!(
        proposal.disposition,
        Disposition::Proposed,
        "a failed accept settles nothing"
    );

    // A successful accept clears it, in the same act.
    repo.commit_acceptance(prepared_acceptance())
        .await
        .expect("commit");
    let settled = repo.proposal(&id).expect("read").expect("present");
    assert!(
        settled.accept_failure.is_none(),
        "a successful accept clears the stale failure"
    );

    // And now the proposal is terminal.
    assert!(
        repo.record_accept_failure(&id, failure.clone())
            .await
            .is_err(),
        "an accepted proposal is not an attempt that can still fail"
    );

    // A turn that produced no proposal has nothing to accept.
    let failed_turn =
        InMemoryCoachAcceptanceRepo::new(FakeClock::at(NOW_MS), SeqIdSource::with_prefix("minted"));
    failed_turn
        .register_turn(MemoryCoachTurn {
            session_id: CoachingSessionId::new("sess-1"),
            strategy_id: StrategyId::new("strat-1"),
            parent_version_id: VersionId::new("ver-1"),
            llm_call_id: None,
            parent_certification: None,
            outcome: SessionOutcome::Failed {
                failure: CoachFailure::ZeroCalls,
            },
        })
        .expect("register");
    assert!(
        failed_turn
            .commit_acceptance(prepared_acceptance())
            .await
            .is_err(),
        "a failed turn has no proposal to accept"
    );
    assert!(
        failed_turn
            .record_accept_failure(&id, failure)
            .await
            .is_err(),
        "and no accept failure to record against it"
    );
}

// ---------------------------------------------------------------------------
// r2.s1.w1 AC-4 / #158: one pending claim per run.
//
// `0009`'s `coaching_sessions_one_pending_per_run` partial unique index makes
// the run the claim's second arbiter: two LIVE claims can never coexist on one
// run, in any process. `claim_session` maps that violation to
// `CoachSessionClaimResult::ExistingPending` carrying the WINNING session's row
// — the same answer the process-local registry gives — and a claim that has
// settled frees its run for the next turn.
// ---------------------------------------------------------------------------

fn a_claim(session: &str, run: &str, fp: &str) -> CoachSessionClaim {
    CoachSessionClaim {
        session_id: CoachingSessionId::new(session),
        backtest_run_id: BacktestRunId::new(run),
        strategy_version_id: VersionId::new("ver-1"),
        request_fingerprint: CoachRequestFingerprint::new(fp).expect("a non-empty digest"),
        created_at: now_rfc3339(),
    }
}

/// A second pending claim on the same run does not land: the index refuses the
/// insert and the adapter answers with the session that already holds the run —
/// never an error, never the claimant's own (nonexistent) row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_pending_claim_on_the_same_run_returns_the_firsts_row() {
    let (repo, pool, _tmp) = repo().await;
    repo.claim_session(a_claim("sess-1", "run-1", "fp-a"))
        .await
        .expect("the first claim owns the run");

    match repo
        .claim_session(a_claim("sess-2", "run-1", "fp-b"))
        .await
        .expect("the run-level conflict is a typed answer, not an error")
    {
        CoachSessionClaimResult::ExistingPending(session) => {
            assert_eq!(
                session.id,
                CoachingSessionId::new("sess-1"),
                "ExistingPending carries the row that already holds the run"
            );
            assert_eq!(session.backtest_run_id, BacktestRunId::new("run-1"));
            assert!(matches!(session.outcome, SessionOutcome::Pending));
        }
        other => panic!("expected ExistingPending with the first claim's row, got {other:?}"),
    }

    // The refused claim left no trace: one session row, and it is the winner's.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM coaching_sessions")
        .fetch_one(&pool)
        .await
        .expect("count sessions");
    assert_eq!(rows, 1, "the second claim was refused, not written");
    let outcome: String =
        sqlx::query_scalar("SELECT outcome FROM coaching_sessions WHERE id = 'sess-1'")
            .fetch_one(&pool)
            .await
            .expect("the winner's row");
    assert_eq!(outcome, "pending");
}

/// The index guards PENDING rows only: once the first claim settles, its run is
/// free and a second claim on it lands as `Claimed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settled_first_claim_admits_a_second_on_the_same_run() {
    let (repo, _pool, _tmp) = repo().await;
    repo.claim_session(a_claim("sess-1", "run-1", "fp-a"))
        .await
        .expect("claim");
    repo.finish_session(
        &CoachingSessionId::new("sess-1"),
        InitialCoachOutcome {
            llm_call_id: None,
            outcome: SessionOutcome::Failed {
                failure: CoachFailure::ZeroCalls,
            },
        },
    )
    .await
    .expect("the first claim settles");

    match repo
        .claim_session(a_claim("sess-2", "run-1", "fp-b"))
        .await
        .expect("a settled claim frees its run")
    {
        CoachSessionClaimResult::Claimed => {}
        other => panic!("expected Claimed after the first claim settled, got {other:?}"),
    }
}

/// The guard is per RUN, not global: pending claims on different runs coexist.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_claims_on_different_runs_do_not_conflict() {
    let (repo, _pool, _tmp) = repo().await;
    repo.claim_session(a_claim("sess-1", "run-1", "fp-a"))
        .await
        .expect("claim run-1");

    match repo
        .claim_session(a_claim("sess-2", "run-2", "fp-b"))
        .await
        .expect("a different run is a different lane")
    {
        CoachSessionClaimResult::Claimed => {}
        other => panic!("expected Claimed on a different run, got {other:?}"),
    }
}

/// A same-id reclaim still reaches the identity path even when the run holds a
/// DIFFERENT live claim: reusing `sess-1`'s id with a different fingerprint is
/// the same `Err` it always was — the run-level guard must not turn it into an
/// `ExistingPending` that laundered the collision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_id_reclaim_with_a_different_fingerprint_is_still_an_error() {
    let (repo, _pool, _tmp) = repo().await;
    repo.claim_session(a_claim("sess-1", "run-1", "fp-a"))
        .await
        .expect("claim");

    assert!(
        repo.claim_session(a_claim("sess-1", "run-1", "fp-b"))
            .await
            .is_err(),
        "the id is already held by a turn with a different fingerprint"
    );
}
