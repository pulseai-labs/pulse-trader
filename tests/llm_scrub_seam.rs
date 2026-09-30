//! r3.s1.w5 — the LLM credential path is safe (demo ledger line d46).
//!
//! Five properties, all driven from outside the crate:
//!
//! (i)  A provider error body echoing a configured credential is scrubbed
//!      BEFORE it is bounded, in every persisted `llm_call.completion` — at
//!      several offsets across the 4096-byte cut — and the pre-change order
//!      (bound, then redact) demonstrably leaks a fragment of the same body.
//! (ii) The same echoed-credential error reaching a coach turn is recorded in
//!      the failure detail with no credential fragment.
//! (iii)A credential that cannot become an `Authorization` header (empty,
//!      space, newline, control character) is refused before dispatch as
//!      `LlmError::Config`: no ledger row, zero provider requests, and an
//!      error message that never carries the value.
//! (iv) The `--dsl` backtest route runs the shared request-shape guard before
//!      any snapshot I/O, and the versioned route refuses through the same
//!      function.
//! (v)  The structural guard: no second truncation of provider detail may
//!      exist outside `src/domain/redaction.rs`, and the scan demonstrably
//!      fails a planted violation.
//!
//! The credential here is deliberately a dashed all-alpha phrase rather than
//! an `sk-`-shaped token: the demo in (i) must show the pre-change ORDER
//! leaking — a fragment the redactor cannot recognize — which needs a
//! credential whose cut fragment defeats both the exact-match replacement and
//! the structural api-key-shape heuristic. An opaque-shaped credential would
//! let the structural rule mask the ordering defect under test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod coach_support;

use std::future::Future;
use std::io::Write as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pulse::{
    BacktestConfig, BacktestRequest, BacktestRunRepository, BinanceAdapter, CandleStore,
    CoachFailure, CoachWiring, CreatedBy, Db, FakeClock, LlmBackend, LlmCallCapture, LlmConfig,
    LlmError, LlmProvider, LlmResponse, MIGRATOR, Message, NewVersion, OpenAiCompatProvider, Pair,
    RedactingLoggingProvider, Redactor, SessionOutcome, SqliteBacktestRunRepo,
    SqliteCoachTurnSource, SqliteCoachingRepo, SqliteLlmCallRepo, SqliteStrategyRepo,
    StrategyRepository, Timeframe, ToolDefinition, run_coach_with, run_version_backtest,
};
use rust_decimal::Decimal;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// The credential, the cut, and the assembled detail
// ---------------------------------------------------------------------------

/// A configured credential that defeats the structural redactor in pieces: no
/// `sk-`-style prefix and no mixed letter+digit segment, so only the exact
/// whole-string replacement can remove it — the control bounding destroys.
const CREDENTIAL: &str = "the-quick-brown-fox-leaps-over-the-lazy-transport-canary-key";

/// The byte bound the spec pins (r3.s1.w5 step 1) and the marker a cut ends in.
const CUT: usize = 4096;
const MARKER: &str = "[truncated]";

/// Whether `haystack` carries any window of at least 8 bytes of `needle`. The
/// AC's fragment rule: neither the credential nor any substring of it of
/// length >= 8. `CREDENTIAL` is ASCII, so byte windows are character windows.
fn carries_fragment(haystack: &str, needle: &str) -> bool {
    if needle.len() < 8 {
        return haystack.contains(needle);
    }
    (0..=needle.len() - 8).any(|start| haystack.contains(&needle[start..start + 8]))
}

/// The detail a transport error carries across the seam, assembled in the
/// shape the adapter builds — SDK `Display` head, then the verbatim provider
/// body — with `CREDENTIAL` placed at an exact offset of the string the
/// DECORATOR sees (`llm provider error: {payload}`, a 20-byte head). The body
/// is padded well past the cut, so every offset here is a cut detail.
fn assembled_error(key_display_offset: usize) -> (String, LlmError) {
    const DISPLAY_HEAD: usize = "llm provider error: ".len();
    assert!(
        key_display_offset >= DISPLAY_HEAD,
        "the key cannot sit inside the Display head"
    );
    let key_payload_offset = key_display_offset - DISPLAY_HEAD;
    let mut payload = String::new();
    payload.push_str("server error after 1 attempt(s): upstream refused (HTTP 503)");
    payload.push_str(" | body: {\"error\":{\"message\":\"");
    payload.push_str(&"x".repeat(key_payload_offset.saturating_sub(payload.len())));
    payload.push_str(CREDENTIAL);
    payload.push_str("\"}}");
    while payload.len() < CUT + 512 {
        payload.push('x');
    }
    (payload.clone(), LlmError::Provider(payload))
}

/// Both orderings over the same assembled detail — the RED demonstration that
/// the order is the defect. `bound_then_redact` reconstructs the pre-change
/// adapter (bound the payload at 4096, redact later at the consumer);
/// `redact_then_bound` reconstructs the seam's contract. The reconstruction
/// stays local to this file on purpose: the real seam's own order is pinned by
/// the domain unit tests, and this demo must survive a rename of either.
fn bound_then_redact(redactor: &Redactor, payload: &str) -> String {
    let cut = CUT - MARKER.len();
    let mut bounded = payload[..cut].to_owned();
    bounded.push_str(MARKER);
    redactor.redact(&bounded)
}

fn redact_then_bound(redactor: &Redactor, payload: &str) -> String {
    let scrubbed = redactor.redact(payload);
    if scrubbed.len() <= CUT {
        return scrubbed;
    }
    let cut = CUT - MARKER.len();
    let mut out = scrubbed[..cut].to_owned();
    out.push_str(MARKER);
    out
}

fn chat_config() -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: "glm-5.3-flash".to_owned(),
        temperature: 0.0,
        max_tokens: 2_048,
        reasoning_effort: None,
    }
}

/// A provider double that fails every call with a fixed transport error — the
/// stand-in for a scripted provider whose error body echoes the request.
struct FailingProvider {
    error: LlmError,
}

impl LlmProvider for FailingProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
        std::future::ready(Err(self.error.clone()))
    }
}

// ---------------------------------------------------------------------------
// (i) the straddle, persisted
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_echoed_credential_is_scrubbed_before_the_persisted_bound() {
    // fully before the cut, crossing it, and starting exactly at the cut —
    // offsets measured on the string the decorator redacts (Display of the
    // error, 20-byte head over the payload the old adapter cut at 4096).
    for key_display_offset in [1_000usize, 4_060, CUT] {
        let (_payload, error) = assembled_error(key_display_offset);
        let tmp = tempfile::tempdir().expect("db tempdir");
        let db = Db::with_path(&tmp.path().join("pulse.db"))
            .await
            .expect("open db");
        MIGRATOR.run(db.pool()).await.expect("run migrations");

        let clock = FakeClock::at(1_700_000_000_000);
        let provider = RedactingLoggingProvider::new(
            FailingProvider { error },
            SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            clock,
            Redactor::from_config(vec![CREDENTIAL.to_owned()]),
            coach_support::test_prices(),
        );
        let no_tools: &[ToolDefinition] = &[];
        let result = provider
            .chat(
                vec![Message::user("run".to_owned())],
                no_tools,
                &chat_config(),
            )
            .await;
        assert!(
            matches!(&result, Err(LlmError::Provider(_))),
            "offset {key_display_offset}: a transport fault stays a transport fault: {result:?}"
        );

        let completion: Option<String> =
            sqlx::query_scalar("SELECT completion FROM llm_call WHERE completion IS NOT NULL")
                .fetch_one(db.pool())
                .await
                .unwrap_or_else(|e| {
                    panic!("offset {key_display_offset}: the errored call booked its detail: {e}")
                });
        let completion = completion.unwrap_or_else(|| {
            panic!("offset {key_display_offset}: the errored call booked its detail")
        });

        assert!(
            !carries_fragment(&completion, CREDENTIAL),
            "offset {key_display_offset}: a credential fragment survived in the persisted completion: {completion}"
        );
        assert!(
            completion.len() <= CUT,
            "offset {key_display_offset}: the stored detail exceeds the bound: {} bytes",
            completion.len()
        );
        assert!(
            completion.ends_with(MARKER),
            "offset {key_display_offset}: a cut detail ends in the marker: …{}",
            &completion[completion.len() - 48..]
        );
        assert!(
            completion.contains("llm provider error"),
            "offset {key_display_offset}: the error kind survives the scrub: {completion}"
        );
    }
}

/// The order is the defect: over the same straddling body, the pre-change
/// order (bound, then redact) leaks a fragment the redactor cannot recognize,
/// and the seam's order (redact, then bound) does not.
#[test]
fn the_old_bound_then_redact_order_leaks_the_straddled_credential() {
    let (payload, _error) = assembled_error(4_060);
    assert!(
        payload.len() > CUT,
        "the demo body must straddle: {} bytes",
        payload.len()
    );
    let redactor = Redactor::from_config(vec![CREDENTIAL.to_owned()]);

    let old_row = bound_then_redact(&redactor, &payload);
    assert!(
        carries_fragment(&old_row, CREDENTIAL),
        "the pre-change order must demonstrably leak a fragment of the same body; \
         if this ever fails, the leak demo is no longer honest and this test \
         must be re-earned with a different offset, not weakened"
    );

    let new_row = redact_then_bound(&redactor, &payload);
    assert!(
        !carries_fragment(&new_row, CREDENTIAL),
        "the seam's order must leave no fragment: {new_row}"
    );
    assert!(new_row.len() <= CUT, "the seam's order still bounds");
}

// ---------------------------------------------------------------------------
// (ii) the coach failure record
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_coach_failure_record_scrubs_the_echoed_credential() {
    let (_tmp, db, run_id) = seeded_coach_world().await;
    let (payload, error) = assembled_error(4_060);
    assert!(carries_fragment(&payload, CREDENTIAL), "the body straddles");

    let clock = FakeClock::at(1_700_000_000_000);
    // One capture buffer, both ends — the correctly wired pairing.
    let shared: LlmCallCapture = coach_support::capture();
    let wiring = CoachWiring {
        provider: FailingProvider { error },
        llm_repo: coach_support::CapturingLlmRepo::new(
            SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            Arc::clone(&shared),
        ),
        redactor: Redactor::from_config(vec![CREDENTIAL.to_owned()]),
        prices: coach_support::test_prices(),
        clock,
        key_source: None,
        config: coach_support::config(),
        prompt_dir: None,
        turn_timeout: None,
        max_dsl_bytes: None,
        captured: Arc::clone(&shared),
        session_id: None,
        registry: None,
    };
    let source = SqliteCoachTurnSource::new(db.pool().clone());
    let coaching_repo = SqliteCoachingRepo::with_deps(db.pool().clone(), clock);
    let outcome = run_coach_with(wiring, &source, &coaching_repo, &run_id)
        .await
        .expect("a deviant turn is still a completed turn");

    let failure = match &outcome.session.outcome {
        SessionOutcome::Failed { failure } => failure,
        other => panic!("expected a recorded transport failure, got {other:?}"),
    };
    let CoachFailure::TransportFailure { detail: recorded } = failure else {
        panic!("expected TransportFailure, got {failure:?}");
    };
    assert!(
        !carries_fragment(recorded, CREDENTIAL),
        "the coach failure record carries a credential fragment: {recorded}"
    );
    assert!(
        recorded.contains("503"),
        "the provider's own error text is preserved around the scrub: {recorded}"
    );
    assert!(
        recorded.len() <= CUT,
        "the recorded failure detail is bounded: {} bytes",
        recorded.len()
    );

    // The billed row the same turn wrote is scrubbed too.
    let llm_call_id = outcome
        .session
        .llm_call_id
        .clone()
        .expect("a billed transport fault names its ledger row");
    let completion: Option<String> =
        sqlx::query_scalar("SELECT completion FROM llm_call WHERE id = ?1")
            .bind(llm_call_id.as_str())
            .fetch_one(db.pool())
            .await
            .expect("read the billed row");
    assert!(
        !carries_fragment(&completion.unwrap_or_default(), CREDENTIAL),
        "the billed row carries a credential fragment"
    );
}

/// The coach world: a migrated db, a persisted version, and one run row the
/// turn certifies against — the `coach_failures` seeded fixture, reduced.
async fn seeded_coach_world() -> (TempDir, Db, pulse::BacktestRunId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategy_repo
        .create_strategy("RSI Oversold", None, &[])
        .await
        .expect("create strategy");
    let version = strategy_repo
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: coach_support::canonical_dsl_json(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let clock = FakeClock::at(1_700_000_000_000);
    let run_repo = SqliteBacktestRunRepo::with_deps(db.pool().clone(), clock);
    let result = pulse::BacktestResult {
        trades: Vec::new(),
        net_pnl: Decimal::ZERO,
        fees_total: Decimal::ZERO,
        funding_total: Decimal::ZERO,
        slippage_total: Decimal::ZERO,
        regime_breakdown: pulse::RegimeBreakdown::default(),
        skipped_entries: pulse::SkippedEntryCounts::default(),
        open_position: None,
        engine_fingerprint: pulse::EngineFingerprint::current(),
        summary: pulse::SummaryStats::default(),
        equity_curve: pulse::EquityCurve::default(),
    };
    let run_id = run_repo
        .save_run(
            &version.id,
            &seed_inputs(),
            &result,
            &pulse::SummaryStats::default(),
            Decimal::new(10_000, 0),
        )
        .await
        .expect("save run");
    (tmp, db, run_id)
}

fn seed_inputs() -> pulse::BacktestInputs {
    pulse::BacktestInputs {
        pair: Pair::new("BTCUSDT"),
        primary: pulse::SnapshotSelection {
            timeframe: Timeframe::M15,
            data_version: pulse::DataVersion::new("v-primary"),
        },
        htf: None,
        d1: None,
        taker_fee_bps: Decimal::new(4, 0),
        slippage_bps: Decimal::new(1, 0),
        funding: pulse::FundingConfig::SnapshotRates,
        symbol_filters: None,
        window: None,
        lead_in_from_ms: None,
    }
}

// ---------------------------------------------------------------------------
// (iii) the credential preflight
// ---------------------------------------------------------------------------

/// A real TCP listener that counts every connection and answers each with a
/// canned HTTP 500 — the "scripted provider" whose request count the refusal
/// cases assert is ZERO, and whose answer the positive control asserts.
struct CountingProvider {
    _handle: std::thread::JoinHandle<()>,
    port: u16,
    requests: Arc<AtomicUsize>,
}

fn counting_provider() -> CountingProvider {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the scripted provider");
    let port = listener.local_addr().expect("bound address").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    let handle = std::thread::spawn(move || {
        // A bounded accept loop: enough for the positive control, then home.
        for _ in 0..8 {
            let Ok(mut stream) = listener.accept().map(|(stream, _)| stream) else {
                break;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let _ = stream.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\n\
                  Content-Type: application/json\r\n\
                  Content-Length: 2\r\n\
                  Connection: close\r\n\
                  \r\n\
                  {}",
            );
        }
    });
    CountingProvider {
        _handle: handle,
        port,
        requests,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_credential_that_cannot_become_a_header_is_refused_before_dispatch() {
    let cases: [(&str, String); 4] = [
        ("empty", String::new()),
        ("space", "spilled key".to_owned()),
        ("newline", "spilled\nkey".to_owned()),
        ("control", "spilled\u{0001}key".to_owned()),
    ];
    for (label, bad) in cases {
        let scripted = counting_provider();
        let tmp = tempfile::tempdir().expect("db tempdir");
        let db = Db::with_path(&tmp.path().join("pulse.db"))
            .await
            .expect("open db");
        MIGRATOR.run(db.pool()).await.expect("run migrations");
        let clock = FakeClock::at(1_700_000_000_000);
        let provider = RedactingLoggingProvider::new(
            OpenAiCompatProvider::single_attempt_with_base_url(
                bad.clone(),
                format!("http://127.0.0.1:{}", scripted.port),
            ),
            SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
            clock,
            Redactor::from_config(vec![CREDENTIAL.to_owned()]),
            coach_support::test_prices(),
        );
        let no_tools: &[ToolDefinition] = &[];
        let result = provider
            .chat(
                vec![Message::user("go".to_owned())],
                no_tools,
                &chat_config(),
            )
            .await;

        let Err(LlmError::Config(message)) = result else {
            panic!(
                "{label}: a credential that cannot become an Authorization header is refused \
                 as Config before dispatch, got {result:?}"
            );
        };
        assert!(
            message.contains("credential"),
            "{label}: the refusal names the credential: {message}"
        );
        if !bad.is_empty() {
            assert!(
                !message.contains(bad.as_str()),
                "{label}: the refusal echoed the value: {message}"
            );
            if bad.len() >= 8 {
                assert!(
                    !carries_fragment(&message, &bad),
                    "{label}: the refusal carries a fragment of the value: {message}"
                );
            }
        }
        assert_eq!(
            scripted.requests.load(Ordering::SeqCst),
            0,
            "{label}: the scripted provider saw a request"
        );
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM llm_call")
            .fetch_one(db.pool())
            .await
            .expect("count rows");
        assert_eq!(rows, 0, "{label}: no ledger row for a refused credential");
    }
}

/// The counter is not vacuous, and the preflight does not over-refuse: a
/// dispatchable credential reaches the wire (one request) and the failure is
/// the scripted provider's answer — a transport fault, never `Config`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dispatchable_credential_still_reaches_the_provider() {
    let scripted = counting_provider();
    let tmp = tempfile::tempdir().expect("db tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let clock = FakeClock::at(1_700_000_000_000);
    let provider = RedactingLoggingProvider::new(
        OpenAiCompatProvider::single_attempt_with_base_url(
            "visible-ascii-credential-0123456789",
            format!("http://127.0.0.1:{}", scripted.port),
        ),
        SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
        clock,
        Redactor::from_config(vec![CREDENTIAL.to_owned()]),
        coach_support::test_prices(),
    );
    let no_tools: &[ToolDefinition] = &[];
    let result = provider
        .chat(
            vec![Message::user("go".to_owned())],
            no_tools,
            &chat_config(),
        )
        .await;
    assert!(
        matches!(
            &result,
            Err(LlmError::Provider(_) | LlmError::MalformedToolCall(_))
        ),
        "the scripted 500 arrives as a transport fault, not a config refusal: {result:?}"
    );
    assert_eq!(
        scripted.requests.load(Ordering::SeqCst),
        1,
        "the dispatchable credential produced exactly one request"
    );
}

// ---------------------------------------------------------------------------
// (iv) one pre-dispatch guard for every backtest entry point
// ---------------------------------------------------------------------------

/// An HTF-using strategy (entry `htf.ema(20) < close`) as DSL JSON — derived
/// from the typed value, never hand-written (serde tag shapes drift).
fn htf_dsl_json() -> String {
    let dsl = pulse::StrategyDsl {
        schema_version: pulse::SchemaVersion::CURRENT,
        name: "HTF EMA20 Long (scrub seam)".to_owned(),
        direction: pulse::Direction::Long,
        entry: pulse::Condition::Compare {
            lhs: pulse::ValueSource::Indicator {
                series: pulse::Series::Htf,
                spec: pulse::IndicatorSpec::Ema {
                    period: pulse::SweepableValue::Fixed(20),
                },
            },
            op: pulse::Comparator::Lt,
            rhs: pulse::ValueSource::Price {
                series: pulse::Series::Primary,
                field: pulse::PriceField::Close,
            },
        },
        filters: vec![],
        exits: vec![
            pulse::ExitRule::StopLoss {
                distance_pct: pulse::SweepableValue::Fixed(Decimal::new(5, 2)),
            },
            pulse::ExitRule::TakeProfit {
                target_r: pulse::SweepableValue::Fixed(Decimal::new(2, 0)),
            },
        ],
        risk: pulse::RiskParams {
            risk_per_trade_pct: pulse::SweepableValue::Fixed(Decimal::new(1, 2)),
            max_leverage: pulse::SweepableValue::Fixed(Decimal::new(3, 0)),
        },
    };
    serde_json::to_string(&dsl).expect("dsl serializes")
}

#[test]
fn the_dsl_route_refuses_an_invalid_htf_request_before_any_snapshot_io() {
    let dsl_dir = tempfile::tempdir().expect("dsl tempdir");
    let dsl_path = dsl_dir.path().join("htf.json");
    std::fs::write(&dsl_path, htf_dsl_json()).expect("write the HTF DSL");
    // An EMPTY store: if the route reached snapshot I/O, the error would be a
    // missing-snapshot error — which is exactly what this test forbids.
    let empty_store = tempfile::tempdir().expect("empty store tempdir");

    // (a) an HTF-using run with no --htf at all.
    let refused = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            dsl_path.to_str().expect("dsl path is utf8"),
            "--pair",
            "BTCUSDT",
            "--tf",
            "M15",
            "--store",
            empty_store.path().to_str().expect("store path is utf8"),
        ])
        .output()
        .expect("run pulse backtest");
    assert!(
        !refused.status.success(),
        "an HTF-using run without --htf is refused; stdout:\n{}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("was not supplied"),
        "the refusal is the request-shape error: {stderr}"
    );
    assert!(
        !stderr.contains("no HEAD snapshot"),
        "the refusal must precede any snapshot I/O: {stderr}"
    );

    // (b) --tf H4 --htf M15: an HTF that is not strictly higher.
    let refused = Command::new(env!("CARGO_BIN_EXE_pulse"))
        .args([
            "backtest",
            "--dsl",
            dsl_path.to_str().expect("dsl path is utf8"),
            "--pair",
            "BTCUSDT",
            "--tf",
            "H4",
            "--htf",
            "M15",
            "--store",
            empty_store.path().to_str().expect("store path is utf8"),
        ])
        .output()
        .expect("run pulse backtest");
    assert!(
        !refused.status.success(),
        "an HTF below the primary is refused; stdout:\n{}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("strictly higher timeframe"),
        "the refusal is the not-higher shape error: {stderr}"
    );
    assert!(
        !stderr.contains("no HEAD snapshot"),
        "the refusal must precede any snapshot I/O: {stderr}"
    );
}

/// The versioned route refuses through the same function: an HTF-needing
/// version on an EMPTY store comes back as the shape error, never a
/// missing-snapshot error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_versioned_route_refuses_through_the_same_guard() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("pulse.db");
    let db = Db::with_path(&db_path).await.expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");

    let strategies = SqliteStrategyRepo::new(db.pool().clone());
    let strategy = strategies
        .create_strategy("HTF EMA20", None, &[])
        .await
        .expect("create strategy");
    let version = strategies
        .create_version(NewVersion {
            strategy_id: strategy.id,
            parent_version_id: None,
            dsl_json: htf_dsl_json(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    // An EMPTY candle store — the guard must refuse long before I/O.
    let store = CandleStore::with_base_dir(tmp.path().join("candles"));
    let runs = SqliteBacktestRunRepo::new(db.pool().clone());

    let error = run_version_backtest(
        &strategies,
        &store,
        &BinanceAdapter::new(),
        &runs,
        &BacktestRequest {
            version_id: version.id,
            pair: Pair::new("BTCUSDT"),
            primary_timeframe: Timeframe::M15,
            htf_timeframe: None,
            config: BacktestConfig::default(),
            snapshots: None,
            window: None,
        },
    )
    .await
    .expect_err("an HTF-needing request without an HTF is refused");
    let message = error.to_string();
    assert!(
        message.contains("was not supplied"),
        "the versioned route gives the request-shape refusal: {message}"
    );
    assert!(
        !message.contains("no HEAD snapshot"),
        "the refusal must precede any snapshot I/O: {message}"
    );
}

// ---------------------------------------------------------------------------
// (v) the structural guard
// ---------------------------------------------------------------------------

/// Every truncation of provider-derived detail lives in exactly one file: the
/// seam. `bound_transport_detail` may not reappear anywhere, and the
/// truncation marker may not be applied outside `domain/redaction.rs`.
fn truncation_violations(rel: &Path, code: &str) -> Vec<String> {
    let rel = rel.to_string_lossy().replace('\\', "/");
    let mut violations = Vec::new();
    if code.contains("bound_transport_detail") {
        violations.push(format!(
            "{rel}: `bound_transport_detail` reappeared — the adapter must not bound provider detail"
        ));
    }
    if rel != "domain/redaction.rs" && code.contains("DETAIL_TRUNCATED") {
        violations.push(format!(
            "{rel}: applies the truncation marker outside the seam (domain/redaction.rs)"
        ));
    }
    violations
}

fn rust_sources(src_root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, String)>) {
        for entry in
            std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let code = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                out.push((
                    path.strip_prefix(root).expect("under src").to_path_buf(),
                    code,
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(src_root, src_root, &mut out);
    out
}

#[test]
fn no_second_truncation_of_provider_detail_exists_outside_the_seam() {
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let corpus = rust_sources(&src_root);
    assert!(
        corpus.len() > 50,
        "the scan is reading the real src tree, not an empty directory: {} files",
        corpus.len()
    );
    let mut violations = Vec::new();
    for (rel, code) in &corpus {
        violations.extend(truncation_violations(rel, code));
    }
    assert!(
        violations.is_empty(),
        "provider-detail truncation must live only in domain/redaction.rs:\n{}",
        violations.join("\n")
    );
    // Positive control: the seam this guard protects is present and is the one
    // home of the marker.
    let seam = &corpus
        .iter()
        .find(|(rel, _)| rel == Path::new("domain/redaction.rs"))
        .expect("domain/redaction.rs is in the corpus")
        .1;
    assert!(
        seam.contains("pub fn scrub_then_bound"),
        "the seam the guard protects is present"
    );
    assert!(
        seam.contains("TRANSPORT_DETAIL_MAX_BYTES") && seam.contains("DETAIL_TRUNCATED"),
        "the one bound and its marker live beside the seam"
    );
}

/// The guard demonstrably fails a source carrying a second truncation of
/// provider detail outside `domain/redaction.rs` — the AC's demonstration.
#[test]
fn the_guard_fails_a_planted_second_truncation() {
    let planted = "fn bound_transport_detail(detail: String) -> String { detail }";
    let violations = truncation_violations(Path::new("adapters/llm/other.rs"), planted);
    assert!(
        !violations.is_empty(),
        "a planted second truncation outside the seam must be flagged"
    );
    let planted_marker = "const MARKER: &str = DETAIL_TRUNCATED;";
    let violations = truncation_violations(Path::new("application/coach.rs"), planted_marker);
    assert!(
        violations
            .iter()
            .any(|v| v.contains("application/coach.rs")),
        "a planted marker use outside the seam must be flagged: {violations:?}"
    );
}
