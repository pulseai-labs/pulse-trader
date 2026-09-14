//! The redacting + cost-logging decorator (VS-1.3.1 work-1.04, FR-24 / NFR-6,
//! README C7).
//!
//! [`RedactingLoggingProvider`] wraps ANY inner [`LlmProvider`] (1.05 wraps
//! 1.03's `GlmProvider`) and turns a bare provider call into the audited,
//! cost-logged, leak-at-rest-safe ledger write FR-24 requires — WITHOUT changing
//! what the model receives. On each non-streaming `chat()` it:
//!
//! 1. calls the inner provider with the **real, un-redacted** messages — grill
//!    OQ-A: redaction guards the STORED copy, never the sent bytes (API keys ride
//!    the `Authorization` header inside the transport, and the coach legitimately
//!    needs real numeric context);
//! 2. computes the `Decimal` cost from the response `usage` times the
//!    [`PriceTable`] (README C5), in the table's native billing currency,
//!    fail-closed on an unknown model;
//! 3. persists an [`LlmCall`] whose prompt + completion have been passed through
//!    the [`Redactor`] (a COPY — the inner call already happened on the real
//!    bytes), timestamped from the injected [`Clock`]; and
//! 4. returns the inner [`LlmResponse`] to the caller, unchanged.
//!
//! A call that ERRORED after reaching the provider is still a billed round-trip
//! and still gets its row (PR #169, R1): the scrubbed error detail rides
//! `completion` (kind, provider body and all) and the usage is what the error
//! exposes — zero, since a `PulseHive` transport error carries no token counts.
//! A failure that happens BEFORE the call leaves this process produces no row —
//! the pre-flight price check, the ledger insert itself, and any inner
//! `Config`/`Local` fault, which by the port's own taxonomy is THIS process
//! faulting rather than the provider answering (PR #169, round 2: the Tauri
//! compose path's cancellation guard refuses as `Local` without calling its
//! inner provider, and a zero-token row for that refusal would be a phantom
//! provider round-trip in the accounting ledger).
//!
//! The [`Redactor`] is deliberately scoped (audit ch1): it strips (a)
//! API-key-shaped tokens and (b) caller-declared tagged secret VALUES, and does
//! NOT free-text-regex numbers/balances (which share no lexical shape — a
//! "strip any number" rule would nuke the coach's context, worse than nothing).
//! Its secret ruleset is DATA loaded via [`Redactor::from_config`] (decision 4),
//! with a minimal safe [`Default`] for tests. Generic over `P`/`R`/`C`, never
//! `dyn` (the established port-composition discipline); tested against fakes,
//! fully offline (MASTER-SPEC section 9.4).

use chrono::DateTime;
use uuid::Uuid;

use crate::domain::strategy::CreatedBy;
// r1.s4.w2 (#150): the PURE `Redactor` now lives in the domain ring
// (`crate::domain::redaction`). This decorator still owns the provider concerns —
// the inner call, the price table, the ledger write — and still redacts BEFORE
// persisting (ADR-0016); only the text kernel moved.
use crate::domain::{
    Clock, CredentialSource, LlmCall, LlmCallId, LlmCallRepository, LlmConfig, LlmError,
    LlmProvider, LlmResponse, Message, PriceTable, Redactor, TokenUsage, ToolDefinition,
};

/// The redacting + cost-logging [`LlmProvider`] decorator (README C7).
///
/// Generic over the inner provider `P`, the ledger repo `R`, and the [`Clock`]
/// `C` (never `dyn`), so 1.05 wraps the concrete `GlmProvider` at zero cost. It
/// IS an [`LlmProvider`], so it substitutes transparently for the raw provider.
///
/// No `#[derive(Debug)]`: `C: Clock` carries no `Debug` bound (mirrors
/// `SqliteLlmCallRepo`).
pub struct RedactingLoggingProvider<P, R, C> {
    inner: P,
    repo: R,
    clock: C,
    redactor: Redactor,
    prices: PriceTable,
    created_by: CreatedBy,
    key_source: Option<CredentialSource>,
    prompt_version: Option<String>,
}

impl<P, R, C> RedactingLoggingProvider<P, R, C> {
    /// Wrap `inner` with redaction + cost-logging into `repo`, timestamping each
    /// [`LlmCall`] from `clock`. `redactor` supplies the NFR-6 secret ruleset;
    /// `prices` the README-C5 cost table. `created_by` defaults to
    /// [`CreatedBy::Human`] this slice (the composer/coach supply it in 1.3.2+).
    #[must_use]
    pub fn new(inner: P, repo: R, clock: C, redactor: Redactor, prices: PriceTable) -> Self {
        Self {
            inner,
            repo,
            clock,
            redactor,
            prices,
            created_by: CreatedBy::Human,
            key_source: None,
            prompt_version: None,
        }
    }

    /// Override the provenance actor stamped on every persisted [`LlmCall`]
    /// (default [`CreatedBy::Human`], set by [`new`](Self::new)).
    ///
    /// An agent-driven composition root MUST call this: `llm_call` is
    /// UPDATE/DELETE-trigger-immutable, so a row written under the wrong actor
    /// can never be corrected in place. The composer wires
    /// [`CreatedBy::ComposerLlm`] so the ledger agrees with the
    /// `StrategyVersion.created_by` it provenance-links.
    #[must_use]
    pub fn with_created_by(mut self, created_by: CreatedBy) -> Self {
        self.created_by = created_by;
        self
    }

    /// Record WHICH credential source supplied the API key on every persisted
    /// [`LlmCall`] (r1.s1.w2 — the risk gate's audit-trail control).
    ///
    /// A LABEL, never the key: the decorator is handed
    /// [`ApiKey::source()`](crate::domain::ApiKey::source), a value type that cannot
    /// carry the credential, so this seam is incapable of leaking one.
    ///
    /// A builder rather than a `new` parameter so every existing call site keeps
    /// compiling and defaults to `None` (provenance not recorded) — the same shape
    /// as [`with_created_by`](Self::with_created_by). The live composition root
    /// (`src/cli/compose.rs`) sets it; a test double that does not care may omit it.
    #[must_use]
    pub fn with_key_source(mut self, key_source: Option<CredentialSource>) -> Self {
        self.key_source = key_source;
        self
    }

    /// Record WHICH version of the agent prompt drove every persisted
    /// [`LlmCall`] — the SHA-256 hex of the RESOLVED prompt file, whichever of the
    /// compiled-in default or the `$PULSE_PROMPT_DIR` overlay won (r1.s2 audit C2).
    ///
    /// A builder rather than a `new` parameter, mirroring
    /// [`with_key_source`](Self::with_key_source) exactly: every existing call site
    /// keeps compiling and keeps recording `None`, so **the composer path is
    /// unchanged**. It has to be stamped here rather than after the fact, because
    /// `llm_call` is UPDATE-trigger-immutable (migration `0004`) — a row written
    /// without its prompt version can never gain one.
    ///
    /// A hash, never prompt text: the value is incapable of carrying prompt or
    /// credential content.
    #[must_use]
    pub fn with_prompt_version(mut self, prompt_version: Option<String>) -> Self {
        self.prompt_version = prompt_version;
        self
    }
}

impl<P, R, C> LlmProvider for RedactingLoggingProvider<P, R, C>
where
    P: LlmProvider + Send + Sync,
    R: LlmCallRepository + Send + Sync,
    C: Clock + Send + Sync,
{
    async fn chat(
        &self,
        messages: Vec<Message>,
        tools: &[ToolDefinition],
        config: &LlmConfig,
    ) -> Result<LlmResponse, LlmError> {
        // Preflight the price table BEFORE the billed inner call: an unknown-model
        // config error is knowable up front (zero usage → Ok(0) if priceable, else
        // Err::Config), so surface it here rather than let a real, BILLED provider
        // call succeed and then escape the FR-24 ledger when the post-call cost
        // lookup fails (close-review Codex C2).
        self.prices.cost(&config.model, &TokenUsage::default())?;

        // OQ-A: the inner provider gets the REAL, un-redacted prompt AND tools; we
        // keep a copy of the messages only to scrub the persisted record.
        let prompt_copy = messages.clone();
        let result = self.inner.chat(messages, tools, config).await;

        // A row is owed only for an error that represents an actual provider
        // attempt (PR #169, round 2): `Provider`/`MalformedToolCall` are the
        // transport faults — billed round-trips that errored upstream.
        // `Config`/`Local` are THIS process faulting before the call left it —
        // e.g. the Tauri compose path's `RefusingProvider` answering `chat()`
        // as `Local` WITHOUT calling its inner provider when the event sink
        // dies between turns — so writing a zero-token row for one would book a
        // phantom provider round-trip in the ledger. The refusal passes through
        // to the caller unchanged; only the row is skipped.
        if matches!(&result, Err(LlmError::Config(_) | LlmError::Local(_))) {
            return result;
        }

        // A call that reached the provider is a billed round-trip whether it
        // answered or errored (PR #169, R1): under PulseHive 3.0.0 a billed
        // HTTP 200 with a truncated tool call arrives as `Err`, and an early
        // return here left the spend with no ledger row. The error rides
        // `completion` — its kind and provider body included — scrubbed like
        // every other stored string; the SDK's error exposes no usage, so the
        // row records the tokens that are visible (zero), not invented ones.
        let (usage, completion) = match &result {
            Ok(response) => (
                response.usage,
                response.content.as_ref().map(|c| self.redactor.redact(c)),
            ),
            Err(error) => (
                TokenUsage::default(),
                Some(self.redactor.redact(&error.to_string())),
            ),
        };

        // Cost from usage times the price table — model already validated above.
        let cost = self.prices.cost(&config.model, &usage)?;
        let cost_currency = self.prices.currency().to_owned();

        // Redact the STORED copy (prompt + completion) — never the sent bytes.
        let prompt_messages = self.redactor.redact_messages(&prompt_copy);

        let now_ms = self.clock.now_ms();
        let created_at = DateTime::from_timestamp_millis(now_ms)
            .ok_or_else(|| LlmError::Local(format!("clock.now_ms() {now_ms} out of range")))?;

        let call = LlmCall {
            id: LlmCallId::new(Uuid::new_v4().to_string()),
            backend: config.backend,
            model: config.model.clone(),
            prompt_messages,
            completion,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cost,
            cost_currency,
            created_at,
            created_by: self.created_by,
            key_source: self.key_source,
            // r1.s2.w3: the resolved-prompt content hash the composition root
            // stamped, or `None` (the composer's rows, and any caller that records
            // none) — audit C2.
            prompt_version: self.prompt_version.clone(),
        };

        self.repo
            .save_call(&call)
            .await
            // LOCAL, not `Provider`: the provider answered (or faulted); it is our
            // ledger write that failed. A caller that records provider faults as
            // domain outcomes (the coach) must be able to tell the two apart
            // (PR #128, finding 5).
            .map_err(|e| LlmError::Local(format!("llm_call persist failed: {e}")))?;

        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{RedactingLoggingProvider, Redactor};
    use crate::adapters::clock::FakeClock;
    use crate::cli::compose::COMPOSE_CANCELLED;
    use crate::domain::redaction::REDACTED;
    use crate::domain::strategy::CreatedBy;
    use crate::domain::{
        DataError, LlmBackend, LlmCall, LlmCallId, LlmCallRepository, LlmConfig, LlmError,
        LlmProvider, LlmResponse, Message, ModelPrice, PriceTable, TokenUsage, ToolCall,
        ToolDefinition,
    };
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::future::Future;
    use std::sync::{Arc, Mutex};

    /// A canned inner provider that RECORDS the exact messages it received, so a
    /// test can assert the decorator forwarded the REAL, un-redacted prompt
    /// (OQ-A). Returns a fixed [`LlmResponse`] with a known [`TokenUsage`].
    struct FakeProvider {
        received: Arc<Mutex<Vec<Vec<Message>>>>,
        response: LlmResponse,
    }

    impl LlmProvider for FakeProvider {
        fn chat(
            &self,
            messages: Vec<Message>,
            _tools: &[ToolDefinition],
            _config: &LlmConfig,
        ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
            self.received.lock().expect("received lock").push(messages);
            std::future::ready(Ok(self.response.clone()))
        }
    }

    /// A provider whose `chat` returns a transport error — the shape a billed
    /// HTTP 200 takes under `PulseHive` 3.0.0 when the tool arguments cannot be
    /// read into a call. The call happened; no usable response came back.
    struct FailingProvider {
        received: Arc<Mutex<Vec<Vec<Message>>>>,
        error: LlmError,
    }

    impl LlmProvider for FailingProvider {
        fn chat(
            &self,
            messages: Vec<Message>,
            _tools: &[ToolDefinition],
            _config: &LlmConfig,
        ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
            self.received.lock().expect("received lock").push(messages);
            std::future::ready(Err(self.error.clone()))
        }
    }

    /// An in-memory ledger repo that CAPTURES every saved [`LlmCall`] verbatim, so
    /// a test can inspect the PERSISTED (redacted) copy. Mirrors 1.02's private
    /// `FakeLlmCallRepo` but exposes the captured rows.
    struct RecordingRepo {
        saved: Arc<Mutex<Vec<LlmCall>>>,
    }

    impl LlmCallRepository for RecordingRepo {
        fn save_call(&self, call: &LlmCall) -> impl Future<Output = Result<LlmCallId, DataError>> {
            self.saved.lock().expect("saved lock").push(call.clone());
            std::future::ready(Ok(call.id.clone()))
        }

        fn get_call(
            &self,
            id: &LlmCallId,
        ) -> impl Future<Output = Result<Option<LlmCall>, DataError>> {
            std::future::ready(Ok(self
                .saved
                .lock()
                .expect("saved lock")
                .iter()
                .find(|c| c.id == *id)
                .cloned()))
        }
    }

    fn config() -> LlmConfig {
        LlmConfig {
            backend: LlmBackend::Ollama,
            model: "gpt-oss:120b".to_owned(),
            temperature: 0.2,
            max_tokens: 256,
            reasoning_effort: None,
        }
    }

    /// A minimal test price table keyed on `gpt-oss:120b` (README C5), CNY-native.
    /// These are TEST values, not the production moat data (decision 4).
    fn prices() -> PriceTable {
        let mut models = HashMap::new();
        models.insert(
            "gpt-oss:120b".to_owned(),
            ModelPrice {
                input_per_mtok: Decimal::from(2),
                output_per_mtok: Decimal::from(8),
            },
        );
        PriceTable::from_config("CNY", models)
    }

    fn response(content: &str, input_tokens: u32, output_tokens: u32) -> LlmResponse {
        LlmResponse {
            content: Some(content.to_owned()),
            tool_calls: Vec::new(),
            usage: TokenUsage {
                input_tokens,
                output_tokens,
            },
        }
    }

    struct Driven {
        saved: Vec<LlmCall>,
        received: Vec<Vec<Message>>,
        returned: LlmResponse,
    }

    /// Wire the decorator over the fakes + a `FakeClock`, run one `chat`, and hand
    /// back what was persisted, what the inner provider received, and what the
    /// caller got.
    async fn drive(
        prompt: Vec<Message>,
        redactor: Redactor,
        canned: LlmResponse,
        now_ms: i64,
    ) -> Driven {
        let received = Arc::new(Mutex::new(Vec::new()));
        let saved = Arc::new(Mutex::new(Vec::new()));
        let provider = FakeProvider {
            received: Arc::clone(&received),
            response: canned,
        };
        let repo = RecordingRepo {
            saved: Arc::clone(&saved),
        };
        let decorator = RedactingLoggingProvider::new(
            provider,
            repo,
            FakeClock::at(now_ms),
            redactor,
            prices(),
        );
        let returned = decorator
            .chat(prompt, &[], &config())
            .await
            .expect("decorator chat succeeds");
        let saved = saved.lock().expect("saved lock").clone();
        let received = received.lock().expect("received lock").clone();
        Driven {
            saved,
            received,
            returned,
        }
    }

    fn user_text(message: &Message) -> &str {
        match message {
            Message::User { content } => content,
            other => panic!("expected a User message, got {other:?}"),
        }
    }

    const FAKE_KEY: &str = "sk-ABCD1234efGH5678ijKL9012mnOP3456";
    const TAGGED_SECRET: &str = "ACCT-9F3K-SECRET";

    #[tokio::test]
    async fn redacts_api_key_from_persisted_prompt() {
        let prompt = vec![
            Message::system("be terse"),
            Message::user(format!("use my key {FAKE_KEY} now")),
        ];
        let canned = response(&format!("stored {FAKE_KEY} ok"), 10, 4);
        let driven = drive(prompt, Redactor::default(), canned, 1_700_000_000_000).await;

        // (i) the PERSISTED prompt has the key replaced ...
        let call = &driven.saved[0];
        let stored = user_text(&call.prompt_messages[1]);
        assert!(
            !stored.contains(FAKE_KEY),
            "stored prompt still leaks the key: {stored}"
        );
        assert!(
            stored.contains(REDACTED),
            "stored prompt not redacted: {stored}"
        );
        // ... surrounding words preserved.
        assert!(stored.contains("use my key"));
        assert!(stored.contains("now"));
        // completion redacted too.
        let completion = call.completion.as_deref().expect("completion present");
        assert!(!completion.contains(FAKE_KEY));
        assert!(completion.contains(REDACTED));

        // (ii) the inner provider received the UN-redacted messages (OQ-A).
        let sent = user_text(&driven.received[0][1]);
        assert!(
            sent.contains(FAKE_KEY),
            "inner provider must receive the real key, got {sent}"
        );
        assert!(!sent.contains(REDACTED));
        // caller got the real (un-redacted) response back.
        assert_eq!(
            driven.returned.content.as_deref(),
            Some(format!("stored {FAKE_KEY} ok").as_str())
        );
    }

    #[tokio::test]
    async fn redacts_tagged_secret_field_but_preserves_numbers() {
        let body = format!("token {TAGGED_SECRET} balance 12345.67 at 3.5R over 1000 trades");
        let prompt = vec![Message::user(body.clone())];
        let redactor = Redactor::from_config(vec![TAGGED_SECRET.to_owned()]);
        let driven = drive(prompt, redactor, response("ok", 5, 1), 1_700_000_000_000).await;

        let stored = user_text(&driven.saved[0].prompt_messages[0]);
        // tagged secret stripped ...
        assert!(
            !stored.contains(TAGGED_SECRET),
            "tagged secret leaked: {stored}"
        );
        assert!(
            stored.contains(REDACTED),
            "tagged secret not redacted: {stored}"
        );
        // ... but plain numbers / balances / R-multiples are PRESERVED (we did NOT
        // build a strip-any-number redactor).
        assert!(
            stored.contains("12345.67"),
            "balance wrongly stripped: {stored}"
        );
        assert!(
            stored.contains("3.5"),
            "R-multiple wrongly stripped: {stored}"
        );
        assert!(
            stored.contains("1000"),
            "trade count wrongly stripped: {stored}"
        );

        // OQ-A: the inner provider received the UN-redacted prompt.
        let sent = user_text(&driven.received[0][0]);
        assert_eq!(sent, body);
        assert!(sent.contains(TAGGED_SECRET));
    }

    /// FIX C: the at-rest scrubber shares the compose-time prefix heuristic, so a
    /// `xox`-prefixed (Slack-style) token is redacted from the PERSISTED copy even
    /// with NO tagged secrets. Before the unification the at-rest test only matched
    /// `sk-`/≥32-char, so a `xoxb-…` token LEAKED at rest — this guards the
    /// "never weaker than compose-time" property.
    ///
    /// The fixture carries a REAL Slack token shape: PR #93 review tightened the
    /// heuristic so a bare `xox` prefix no longer matches on its own (`xoxo` in a
    /// trader's prose must survive), which a toy 18-char fixture would not exercise.
    #[tokio::test]
    async fn at_rest_scrubber_redacts_prefixed_secret_tokens() {
        // Assembled from fragments so the literal never appears whole in source:
        // GitHub push protection matches the real `xox<type>-…` pattern and would
        // (correctly) block a verbatim fixture.
        const SLACK_TOKEN: &str = concat!("xoxb", "-2401234567-1234567890123-AbCdEfGhIjKlMnOp");
        let prompt = vec![Message::user(format!("token {SLACK_TOKEN} here"))];
        // Redactor::default() has NO tagged secrets — only the structural heuristic
        // can catch this, so the assertion proves the heuristic (not a tagged value).
        let driven = drive(
            prompt,
            Redactor::default(),
            response("ok", 3, 1),
            1_700_000_000_000,
        )
        .await;

        let stored = user_text(&driven.saved[0].prompt_messages[0]);
        assert!(
            !stored.contains(SLACK_TOKEN),
            "at-rest scrubber leaked the prefixed token: {stored}"
        );
        assert!(
            stored.contains(REDACTED),
            "token not redacted at rest: {stored}"
        );
        // surrounding words survive.
        assert!(stored.contains("token") && stored.contains("here"));
    }

    #[tokio::test]
    async fn persists_llm_call_with_cost_and_tokens() {
        let prompt = vec![Message::user("size a long")];
        let now_ms = 1_700_000_123_000;
        let driven = drive(
            prompt,
            Redactor::default(),
            response("done", 1500, 500),
            now_ms,
        )
        .await;

        assert_eq!(driven.saved.len(), 1, "exactly one ledger row persisted");
        let call = &driven.saved[0];
        assert_eq!(call.backend, LlmBackend::Ollama);
        assert_eq!(call.model, "gpt-oss:120b");
        assert_eq!(call.input_tokens, 1500);
        assert_eq!(call.output_tokens, 500);
        // cost is the price-table figure, in native currency (no silent FX).
        let expected = prices()
            .cost(
                "gpt-oss:120b",
                &TokenUsage {
                    input_tokens: 1500,
                    output_tokens: 500,
                },
            )
            .expect("cost");
        assert_eq!(call.cost, expected);
        assert!(call.cost > Decimal::ZERO);
        assert_eq!(call.cost_currency, "CNY");
        // created_at came from the injected FakeClock (deterministic).
        assert_eq!(call.created_at.timestamp_millis(), now_ms);
        // this slice's default provenance.
        assert_eq!(call.created_by, CreatedBy::Human);
    }

    #[tokio::test]
    async fn cost_computed_from_usage_and_price_table() {
        let usage = TokenUsage {
            input_tokens: 1500,
            output_tokens: 500,
        };
        let driven = drive(
            vec![Message::user("hi")],
            Redactor::default(),
            LlmResponse {
                content: Some("ok".to_owned()),
                tool_calls: Vec::new(),
                usage,
            },
            1_700_000_000_000,
        )
        .await;

        // 1500/1e6 * 2  +  500/1e6 * 8  =  0.003 + 0.004  =  0.007 CNY.
        let call = &driven.saved[0];
        assert_eq!(
            call.cost,
            prices().cost("gpt-oss:120b", &usage).expect("cost")
        );
        assert_eq!(call.cost.normalize(), Decimal::new(7, 3).normalize());
        assert_eq!(call.cost_currency, "CNY");
    }

    #[tokio::test]
    async fn redacts_overlapping_tagged_secrets_longest_first() {
        // Two tagged secrets where the shorter is a substring of the longer. With
        // naive in-list-order replacement the short one scrubs part of the long one
        // and leaves its tail exposed; longest-first ordering must strip both fully
        // (close-review Codex C4).
        let short = "9F3K";
        let long = "ACCT-9F3K-TOKEN-XYZ";
        // Pass the SHORT one FIRST — `from_config` must reorder to longest-first.
        let redactor = Redactor::from_config(vec![short.to_owned(), long.to_owned()]);
        let prompt = vec![Message::user(format!("audit {long} please"))];
        let driven = drive(prompt, redactor, response("ok", 3, 1), 1_700_000_000_000).await;

        let stored = user_text(&driven.saved[0].prompt_messages[0]);
        assert!(stored.contains(REDACTED), "not redacted: {stored}");
        for leak in [long, short, "ACCT-", "TOKEN-XYZ"] {
            assert!(
                !stored.contains(leak),
                "persisted prompt still leaks `{leak}`: {stored}"
            );
        }
    }

    #[tokio::test]
    async fn unknown_model_errors_before_billing_and_ledger() {
        // A model absent from the price table must fail BEFORE the billed inner
        // call, so no provider request is spent and no ledger row is written — the
        // config error is knowable up front (close-review Codex C2).
        let received = Arc::new(Mutex::new(Vec::new()));
        let saved = Arc::new(Mutex::new(Vec::new()));
        let provider = FakeProvider {
            received: Arc::clone(&received),
            response: response("unreached", 1, 1),
        };
        let repo = RecordingRepo {
            saved: Arc::clone(&saved),
        };
        let decorator = RedactingLoggingProvider::new(
            provider,
            repo,
            FakeClock::at(1_700_000_000_000),
            Redactor::default(),
            prices(), // keyed on "gpt-oss:120b" only
        );
        let cfg = LlmConfig {
            backend: LlmBackend::Ollama,
            model: "unpriced-model".to_owned(),
            temperature: 0.2,
            max_tokens: 64,
            reasoning_effort: None,
        };

        let err = decorator
            .chat(vec![Message::user("hi")], &[], &cfg)
            .await
            .expect_err("an unpriced model must error");
        assert!(
            matches!(err, LlmError::Config(_)),
            "expected Config, got {err:?}"
        );
        assert!(
            received.lock().expect("received lock").is_empty(),
            "inner provider must NOT be billed for an unpriced model"
        );
        assert!(
            saved.lock().expect("saved lock").is_empty(),
            "no ledger row may be written when the model is unpriced"
        );
    }

    /// R1: a call that reached the provider and came back a transport error is
    /// still a billed round-trip — under `PulseHive` 3.0.0 a malformed tool call
    /// is `Err`, and an early `?` here left the spend with no ledger row.
    #[tokio::test]
    async fn a_billed_call_that_errors_still_reaches_the_ledger() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let saved = Arc::new(Mutex::new(Vec::new()));
        let provider = FailingProvider {
            received: Arc::clone(&received),
            error: LlmError::MalformedToolCall(format!(
                "malformed_tool_call after 1 attempt(s): the arguments are not a JSON object \
                 (HTTP 200) | body: {{\"api_key\": \"{FAKE_KEY}\""
            )),
        };
        let repo = RecordingRepo {
            saved: Arc::clone(&saved),
        };
        let decorator = RedactingLoggingProvider::new(
            provider,
            repo,
            FakeClock::at(1_700_000_000_000),
            Redactor::default(),
            prices(),
        );

        let err = decorator
            .chat(vec![Message::user("a prompt")], &[], &config())
            .await
            .expect_err("the inner error passes through to the caller");

        // The caller's error is unchanged — the row is a side effect, not a rewrite.
        assert!(matches!(err, LlmError::MalformedToolCall(_)));

        let saved = saved.lock().expect("saved lock").clone();
        assert_eq!(saved.len(), 1, "the billed call is still a ledger row");
        let call = &saved[0];
        // The SDK's transport error exposes no usage — the row records the
        // tokens that are visible (zero), never invented numbers.
        assert_eq!(call.input_tokens, 0);
        assert_eq!(call.output_tokens, 0);
        assert_eq!(call.cost, Decimal::ZERO);
        // The failure rides `completion`, scrubbed: the kind names the error
        // and the body's canary is gone.
        let completion = call.completion.as_deref().expect("the error is stored");
        assert!(
            completion.contains("malformed_tool_call"),
            "the error kind reaches the row: {completion}"
        );
        assert!(
            !completion.contains(FAKE_KEY),
            "the stored detail is still scrubbed: {completion}"
        );
        assert!(completion.contains(REDACTED), "not redacted: {completion}");
        assert!(
            !received.lock().expect("received lock").is_empty(),
            "the inner provider really was called"
        );
    }

    /// Round-2 sibling: a plain [`LlmError::Provider`] transport fault (timeout,
    /// connect, HTTP status) is the other billed-call shape — the row still lands.
    #[tokio::test]
    async fn a_transport_fault_still_reaches_the_ledger() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let saved = Arc::new(Mutex::new(Vec::new()));
        let provider = FailingProvider {
            received: Arc::clone(&received),
            error: LlmError::Provider(
                "request_failed after 3 attempt(s): connect timeout (HTTP 0)".to_owned(),
            ),
        };
        let repo = RecordingRepo {
            saved: Arc::clone(&saved),
        };
        let decorator = RedactingLoggingProvider::new(
            provider,
            repo,
            FakeClock::at(1_700_000_000_000),
            Redactor::default(),
            prices(),
        );

        let err = decorator
            .chat(vec![Message::user("a prompt")], &[], &config())
            .await
            .expect_err("the inner error passes through to the caller");
        assert!(matches!(err, LlmError::Provider(_)));

        let saved = saved.lock().expect("saved lock").clone();
        assert_eq!(saved.len(), 1, "the billed call is still a ledger row");
        let call = &saved[0];
        assert_eq!(call.input_tokens, 0);
        assert_eq!(call.output_tokens, 0);
        assert_eq!(call.cost, Decimal::ZERO);
        let completion = call.completion.as_deref().expect("the error is stored");
        assert!(
            completion.contains("request_failed"),
            "the error kind reaches the row: {completion}"
        );
    }

    /// PR #169, round 2: an error raised BEFORE the call left this process is
    /// not a provider attempt, and no ledger row may be written for it. The
    /// Tauri compose path's `RefusingProvider` answers `chat()` with
    /// [`LlmError::Local`] — WITHOUT calling its inner provider — when the event
    /// sink dies between turns; persisting a zero-token row for that refusal
    /// would book a phantom provider round-trip in the accounting ledger. The
    /// R1 row is owed to transport faults (`Provider`/`MalformedToolCall`)
    /// only — `Config`/`Local` are this process faulting, never an upstream
    /// answer.
    #[tokio::test]
    async fn a_refusal_before_the_provider_writes_no_row() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let saved = Arc::new(Mutex::new(Vec::new()));
        let provider = FailingProvider {
            received: Arc::clone(&received),
            // The exact refusal `RefusingProvider` emits (src/tauri/commands.rs).
            error: LlmError::Local(COMPOSE_CANCELLED.to_owned()),
        };
        let repo = RecordingRepo {
            saved: Arc::clone(&saved),
        };
        let decorator = RedactingLoggingProvider::new(
            provider,
            repo,
            FakeClock::at(1_700_000_000_000),
            Redactor::default(),
            prices(),
        );

        let err = decorator
            .chat(vec![Message::user("a prompt")], &[], &config())
            .await
            .expect_err("the refusal passes through to the caller unchanged");
        assert!(matches!(err, LlmError::Local(_)));

        assert!(
            !received.lock().expect("received lock").is_empty(),
            "the wrapper WAS invoked — the refusal is its answer"
        );
        assert!(
            saved.lock().expect("saved lock").is_empty(),
            "no ledger row may be written for a call that never reached the provider"
        );
    }

    /// An inner `Config` fault is the other never-dispatched shape: a provider
    /// that discovers a missing credential at call time faults in THIS process,
    /// and nothing was billed.
    #[tokio::test]
    async fn an_inner_config_fault_writes_no_row() {
        let saved = Arc::new(Mutex::new(Vec::new()));
        let provider = FailingProvider {
            received: Arc::new(Mutex::new(Vec::new())),
            error: LlmError::Config("keychain entry absent".to_owned()),
        };
        let repo = RecordingRepo {
            saved: Arc::clone(&saved),
        };
        let decorator = RedactingLoggingProvider::new(
            provider,
            repo,
            FakeClock::at(1_700_000_000_000),
            Redactor::default(),
            prices(),
        );

        let err = decorator
            .chat(vec![Message::user("a prompt")], &[], &config())
            .await
            .expect_err("the config fault passes through");
        assert!(matches!(err, LlmError::Config(_)));
        assert!(
            saved.lock().expect("saved lock").is_empty(),
            "a pre-dispatch config fault is not a billed round-trip"
        );
    }

    /// Extract the first tool call's `arguments` `Value` from an `Assistant` message.
    fn assistant_tool_call_args(message: &Message) -> &serde_json::Value {
        match message {
            Message::Assistant { tool_calls, .. } => &tool_calls[0].arguments,
            other => panic!("expected an Assistant message, got {other:?}"),
        }
    }

    /// AC-8 (#81 proof): the decorator scrubs `Assistant.tool_calls[i].arguments`
    /// in the PERSISTED copy. A secret nested inside a JSON string leaf of a tool
    /// call's arguments is redacted at rest (via the recursive `redact_value`),
    /// while the inner provider still received the arguments in the CLEAR (OQ-A).
    /// Non-secret structural params (numbers) survive verbatim.
    #[tokio::test]
    async fn redacts_tool_call_arguments() {
        // An Assistant turn in the PROMPT carrying a tool call whose arguments embed
        // BOTH a structural api-key-shaped token AND a caller-tagged secret, NESTED
        // inside a JSON object (proves the redactor recurses into string leaves).
        let arguments = serde_json::json!({
            "config": {
                "note": format!("use {FAKE_KEY} and {TAGGED_SECRET}"),
            },
            "size": 1000,
        });
        let prompt = vec![Message::Assistant {
            content: Some("calling a tool".to_owned()),
            tool_calls: vec![ToolCall {
                id: "call-1".to_owned(),
                name: "set_risk".to_owned(),
                arguments: arguments.clone(),
            }],
        }];
        let redactor = Redactor::from_config(vec![TAGGED_SECRET.to_owned()]);
        let driven = drive(prompt, redactor, response("ok", 5, 1), 1_700_000_000_000).await;

        // (i) the PERSISTED tool-call arguments have BOTH secrets scrubbed ...
        let stored_args = assistant_tool_call_args(&driven.saved[0].prompt_messages[0]);
        let stored_str = stored_args.to_string();
        assert!(
            !stored_str.contains(FAKE_KEY),
            "persisted args leak the api key: {stored_str}"
        );
        assert!(
            !stored_str.contains(TAGGED_SECRET),
            "persisted args leak the tagged secret: {stored_str}"
        );
        assert!(
            stored_str.contains(REDACTED),
            "persisted args not redacted: {stored_str}"
        );
        // ... while a non-secret numeric param survives (we did NOT strip numbers).
        assert_eq!(stored_args["size"], serde_json::json!(1000));

        // (ii) OQ-A: the inner provider received the UN-redacted arguments in the clear.
        let sent_args = assistant_tool_call_args(&driven.received[0][0]);
        assert_eq!(sent_args, &arguments);
    }
}
