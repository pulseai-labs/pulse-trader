//! OpenAI-compatible transport adapter (VS-1.3.1 work-1.03 → VS-1.3.2 work-2.01,
//! FR-23 / FR-3, README C2/C8) — the anti-corruption layer between
//! `PulseTrader`'s OWNED [`LlmProvider`] port and the `PulseHive`
//! OpenAI-compatible transport (ADR-0012 thin transport).
//!
//! [`OpenAiCompatProvider`] (generalized from VS-1.3.1's `GlmProvider`, now
//! pointed at Ollama Cloud) wraps an `OpenAICompatibleProvider` and translates
//! EVERY `PulseHive` LLM type to/from the PulseTrader-owned domain types, so
//! `PulseHive`'s evolving 2.x API cannot ripple inward. **This is the ONLY module
//! in the crate that imports the `PulseHive` SDK crate (AC-9).**
//!
//! Thin transport ONLY: no `HiveMind`/agent/lens substrate, no streaming (the
//! cost-logged path in the decorator needs `usage`, which only the non-streaming
//! `chat()` carries), no key env-read (the key is a ctor arg the composition root
//! supplies — `llm-check` reads the Keychain via
//! [`glm_api_key`](crate::adapters::secrets::glm_api_key); `compose` resolves it
//! through
//! [`resolve_llm_api_key`](crate::adapters::secrets::resolve_llm_api_key), the
//! r1.s1.w2 precedence chain — environment, `$PULSE_CONFIG_DIR/.env`, the
//! working/manifest `.env`, then the application data directory, each file
//! permission-validated fail-closed), and no redaction / cost / persistence (that
//! is the redacting-logging decorator).
//!
//! **Tool-calling (VS-1.3.2 work-2.01, FR-3).** `chat` now forwards a borrowed
//! `&[ToolDefinition]` slice — each `PulseTrader` [`ToolDefinition`] is translated to
//! the `PulseHive` type **field-by-field** (the anti-corruption per-field pattern,
//! NOT a serde round-trip), so the composer (2.04) can advertise its builder tools
//! and receive `tool_calls` back. An empty slice reproduces the no-tools behavior.

use std::time::Duration;

use pulsehive::error::PulseHiveError;
use pulsehive::llm::{
    LlmConfig as HiveLlmConfig, LlmProvider as HiveLlmProvider, LlmResponse as HiveLlmResponse,
    Message as HiveMessage, TokenUsage as HiveTokenUsage, ToolCall as HiveToolCall,
    ToolDefinition as HiveToolDefinition,
};
use pulsehive::pulsehive_openai::{OpenAICompatibleProvider, OpenAIConfig};

use crate::domain::{
    LlmConfig, LlmError, LlmProvider, LlmResponse, Message, TokenUsage, ToolCall, ToolDefinition,
};

/// The DEFAULT Ollama Cloud OpenAI-compatible base URL (provider pivot 2026-07-10 —
/// spike-verified `/v1` tool-calling). The `const` fallback when the config
/// `[llm].base_url` is absent; [`OpenAiCompatProvider::with_base_url`] accepts a
/// config override (slice-close FIX A).
///
/// `PulseHive`'s `chat_completions_url()` trims a trailing `/` and appends
/// `/chat/completions`, yielding `https://ollama.com/v1/chat/completions`.
const OLLAMA_BASE_URL: &str = "https://ollama.com/v1";

/// The default Ollama Cloud model id — the `OpenAIConfig` fallback carried by the
/// provider. The per-request model actually flows from [`LlmConfig::model`] (the
/// composition root resolves it: config `[llm].model` → const), so this is only the
/// transport-level default.
///
/// `glm-5.3-flash` since ADR-0023 (2026-08-29); the id is bare, no `:cloud` tag.
/// See ADR-0023 for why bare, and for the composer tool-loop run that qualified
/// the model. Kept in agreement with `config/prices.toml` by `agent::config`'s
/// identity test (#126).
pub(crate) const OLLAMA_MODEL_ID: &str = "glm-5.3-flash";

/// The DEFAULT request timeout (audit ch4 — a stalled provider must not hang a
/// future coach loop forever; an unset/infinite timeout is a v1 reliability gap).
///
/// The composer, `llm-check` and every plain single-attempt caller send it. The
/// coach passes its own, longer one explicitly
/// ([`single_attempt_with_timeout`](OpenAiCompatProvider::single_attempt_with_timeout)).
const OLLAMA_TIMEOUT: Duration = Duration::from_secs(60);

/// Max retry attempts for transient (429 / 5xx) errors (audit ch4).
///
/// The COMPOSER and `llm-check` posture. Neither records one exchange per attempt,
/// so absorbing a transient fault there costs nothing an auditor would want back.
const OLLAMA_MAX_RETRIES: u32 = 2;

/// The COACH posture: one turn is one attempt (PR #128, finding H1).
///
/// `run_turn` records exactly one exchange and names exactly one ledger row, and it
/// has no retries and no nudges by design (grill L3) — an adapter quietly making
/// three upstream attempts behind that one record contradicts the rule a layer
/// below it, and bills for the difference. A transient 5xx therefore becomes a
/// recorded `TransportFailure` on the first failure rather than an absorbed one:
/// fewer silent recoveries, more honest rows (operator-approved, 2026-08-30).
const COACH_MAX_RETRIES: u32 = 0;

/// The one place a provider config is built, so everything a posture does NOT
/// choose — the model id, the config shape — stays single-sourced.
///
/// The two things a posture DOES choose are its parameters: how long one request
/// may take, and how many attempts it may make. `timeout` is a [`Duration`] rather
/// than a bare `u64` so it cannot be transposed with `max_retries` — two adjacent
/// positional integers type-check in either order, and the resulting provider
/// (2 seconds, 60 retries) would look right until a live call (review R9).
fn provider_config(
    api_key: impl Into<String>,
    base_url: impl Into<String>,
    timeout: Duration,
    max_retries: u32,
) -> OpenAIConfig {
    OpenAIConfig::new(api_key, OLLAMA_MODEL_ID)
        .with_base_url(base_url)
        .with_timeout(timeout.as_secs())
        .with_max_retries(max_retries)
}

/// The OpenAI-compatible transport adapter — implements `PulseTrader`'s
/// [`LlmProvider`] port over the `PulseHive` OpenAI-compatible transport (README
/// C2/C8), pointed at Ollama Cloud.
///
/// Holds a pre-built provider pinned to the Ollama Cloud config; its
/// [`chat`](OpenAiCompatProvider::chat) translates domain types (messages, tools,
/// config, response) across the seam and never touches the network in tests
/// (translation is exercised by pure unit tests; the live round-trip is a
/// manual/demo concern — MASTER-SPEC §9.4).
pub struct OpenAiCompatProvider {
    inner: OpenAICompatibleProvider,
    /// The retry posture this provider was built with, kept ONLY under `cfg(test)`.
    ///
    /// It is evidence, not runtime state: `PulseHive`'s provider does not surface
    /// its config, so a composition root's choice would otherwise be unassertable
    /// without a live request. Carrying it in production builds would be a field
    /// nothing reads, so it is not carried there (operator ruling, 2026-08-30).
    #[cfg(test)]
    max_retries: u32,
    /// The request timeout this provider was built with — same reasoning, same
    /// `cfg(test)` posture (#164).
    #[cfg(test)]
    timeout_secs: u64,
}

impl OpenAiCompatProvider {
    /// Build an `OpenAiCompatProvider` from an API key, pinned to the DEFAULT
    /// [`OLLAMA_BASE_URL`].
    ///
    /// The key is a **ctor argument** (never env-read here) — the composition root
    /// supplies it (`llm-check` from the macOS Keychain via
    /// [`glm_api_key`](crate::adapters::secrets::glm_api_key); `compose` from
    /// [`resolve_llm_api_key`](crate::adapters::secrets::resolve_llm_api_key),
    /// which hands back an opaque `ApiKey` the composition root unwraps exactly
    /// once). Use [`with_base_url`](Self::with_base_url) to override the
    /// endpoint from config (slice-close FIX A). The timeout / retry posture is
    /// pinned to the Ollama Cloud config (README C2/C8, audit ch4).
    #[must_use]
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, OLLAMA_BASE_URL)
    }

    /// Build an `OpenAiCompatProvider` from an API key + an explicit OpenAI-compatible
    /// `base_url` (the config `[llm].base_url` override — slice-close FIX A). [`new`]
    /// delegates here with the [`OLLAMA_BASE_URL`] default.
    ///
    /// The key provenance is identical to [`new`](Self::new). The model / timeout /
    /// retry posture is unchanged; only the endpoint is caller-chosen.
    #[must_use]
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self::from_config(provider_config(
            api_key,
            base_url,
            OLLAMA_TIMEOUT,
            OLLAMA_MAX_RETRIES,
        ))
    }

    /// Build a provider that makes exactly ONE upstream attempt (PR #128, finding
    /// H1), pinned to the default [`OLLAMA_BASE_URL`].
    ///
    /// Identical to [`new`](Self::new) but for the retry count — including the
    /// [`OLLAMA_TIMEOUT`] request timeout, which a single-attempt caller inherits
    /// like any other. Use it wherever a caller records one exchange per turn and
    /// must not bill for attempts that exchange does not mention; use
    /// [`single_attempt_with_timeout`](Self::single_attempt_with_timeout) when that
    /// caller also needs longer than the default to answer.
    ///
    /// This is a WIRING obligation, not a property of the type: a caller who builds
    /// with [`new`](Self::new) and hands the result to a `Coach` still retries.
    #[must_use]
    pub fn single_attempt(api_key: impl Into<String>) -> Self {
        Self::single_attempt_with_base_url(api_key, OLLAMA_BASE_URL)
    }

    /// [`single_attempt`](Self::single_attempt) with an explicit OpenAI-compatible
    /// `base_url` (the config `[llm].base_url` override). Same default timeout.
    #[must_use]
    pub fn single_attempt_with_base_url(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        Self::from_config(provider_config(
            api_key,
            base_url,
            OLLAMA_TIMEOUT,
            COACH_MAX_RETRIES,
        ))
    }

    /// [`single_attempt`](Self::single_attempt) with a caller-chosen request
    /// `timeout` and an optional `base_url` (`None` = the default
    /// [`OLLAMA_BASE_URL`]) — the COACH's path
    /// (`adapters::llm::coach_transport::coach_provider`).
    ///
    /// The timeout is a per-caller decision and stays one: a coach turn must be
    /// allowed to reason for far longer than a composer step, and shipping that
    /// allowance to every single-attempt caller would make one surface's need the
    /// whole adapter's default (review R2).
    #[must_use]
    pub fn single_attempt_with_timeout(
        api_key: impl Into<String>,
        base_url: Option<&str>,
        timeout: Duration,
    ) -> Self {
        Self::from_config(provider_config(
            api_key,
            base_url.unwrap_or(OLLAMA_BASE_URL),
            timeout,
            COACH_MAX_RETRIES,
        ))
    }

    /// The retry posture this provider was built with — test-build evidence, so a
    /// composition root's choice is assertable without a live request.
    #[cfg(test)]
    pub(crate) const fn max_retries(&self) -> u32 {
        self.max_retries
    }

    /// The request timeout this provider was built with — test-build evidence, so
    /// a composition root's choice is assertable without a live request (#164).
    #[cfg(test)]
    pub(crate) const fn timeout_secs(&self) -> u64 {
        self.timeout_secs
    }

    fn from_config(config: OpenAIConfig) -> Self {
        #[cfg(test)]
        let max_retries = config.max_retries;
        #[cfg(test)]
        let timeout_secs = config.timeout_secs;
        Self {
            inner: OpenAICompatibleProvider::new(config),
            #[cfg(test)]
            max_retries,
            #[cfg(test)]
            timeout_secs,
        }
    }
}

impl LlmProvider for OpenAiCompatProvider {
    async fn chat(
        &self,
        messages: Vec<Message>,
        tools: &[ToolDefinition],
        config: &LlmConfig,
    ) -> Result<LlmResponse, LlmError> {
        let hive_messages: Vec<HiveMessage> = messages.into_iter().map(to_hive_message).collect();
        // Translate the advertised tool defs field-by-field (anti-corruption per-field
        // pattern, NOT a serde round-trip). An empty slice crosses as an empty Vec —
        // the no-tools flow reproduces VS-1.3.1 behavior exactly.
        let hive_tools: Vec<HiveToolDefinition> = tools.iter().map(to_hive_tool_def).collect();
        let hive_config = to_hive_config(config);
        // Non-streaming ONLY (v1): only `chat()` carries `usage`. `PulseHive`'s
        // LLM/transport error maps to `LlmError::Provider`.
        let response = self
            .inner
            .chat(hive_messages, hive_tools, &hive_config)
            .await
            .map_err(map_hive_error)?;
        Ok(from_hive_response(response))
    }
}

/// Translate a `PulseTrader` [`Message`] into the `PulseHive` wire message.
fn to_hive_message(message: Message) -> HiveMessage {
    match message {
        Message::System { content } => HiveMessage::System { content },
        Message::User { content } => HiveMessage::User { content },
        Message::Assistant {
            content,
            tool_calls,
        } => HiveMessage::Assistant {
            content,
            tool_calls: tool_calls.into_iter().map(to_hive_tool_call).collect(),
        },
        Message::ToolResult {
            tool_call_id,
            content,
        } => HiveMessage::ToolResult {
            tool_call_id,
            content,
        },
    }
}

/// Translate a `PulseTrader` [`ToolCall`] into the `PulseHive` tool call (same wire
/// shape: `id` / `name` / opaque JSON `arguments`).
fn to_hive_tool_call(tool_call: ToolCall) -> HiveToolCall {
    HiveToolCall {
        id: tool_call.id,
        name: tool_call.name,
        arguments: tool_call.arguments,
    }
}

/// Translate a `PulseTrader` [`ToolDefinition`] into the `PulseHive` tool schema
/// **field-by-field** (the anti-corruption per-field pattern — the two types share
/// a shape but are deliberately distinct so `PulseHive`'s API cannot ripple inward,
/// ADR-0012). Borrows the def and clones each field.
fn to_hive_tool_def(tool: &ToolDefinition) -> HiveToolDefinition {
    HiveToolDefinition {
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: tool.parameters.clone(),
    }
}

/// Translate a `PulseTrader` [`LlmConfig`] into the `PulseHive` request config.
///
/// `temperature` is `f32` on BOTH sides — no conversion. `provider` is a routing
/// label unused on a direct `OpenAICompatibleProvider` call (set to the backend tag
/// for legibility); `model` flows through (the composition root sets the demo model,
/// which `OpenAIConfig` also carries as the fallback).
fn to_hive_config(config: &LlmConfig) -> HiveLlmConfig {
    HiveLlmConfig {
        provider: "ollama".to_owned(),
        model: config.model.clone(),
        temperature: config.temperature,
        max_tokens: config.max_tokens,
    }
}

/// Translate a `PulseHive` [`LlmResponse`](HiveLlmResponse) back into the
/// PulseTrader-owned response.
fn from_hive_response(response: HiveLlmResponse) -> LlmResponse {
    LlmResponse {
        content: response.content,
        tool_calls: response
            .tool_calls
            .into_iter()
            .map(from_hive_tool_call)
            .collect(),
        usage: from_hive_usage(&response.usage),
    }
}

/// Translate a `PulseHive` tool call back into the PulseTrader-owned one.
fn from_hive_tool_call(tool_call: HiveToolCall) -> ToolCall {
    ToolCall {
        id: tool_call.id,
        name: tool_call.name,
        arguments: tool_call.arguments,
    }
}

/// Translate `PulseHive` token usage into the PulseTrader-owned usage (the
/// cost-model input consumed by the decorator).
fn from_hive_usage(usage: &HiveTokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
    }
}

/// Map a [`PulseHiveError`] into the `PulseTrader` port error.
///
/// The thin transport only ever yields `PulseHiveError::Llm` (every error path in
/// the OpenAI-compatible provider's `chat` uses it); it maps to
/// [`LlmError::Provider`], preserving the message verbatim. Any other variant (not
/// reachable on this path) also maps to `Provider` defensively, so the mapping is
/// total and the domain never learns `PulseHive`'s error type.
fn map_hive_error(error: PulseHiveError) -> LlmError {
    match error {
        PulseHiveError::Llm(message) => LlmError::Provider(message),
        other => LlmError::Provider(other.to_string()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        COACH_MAX_RETRIES, Duration, OLLAMA_BASE_URL, OLLAMA_MAX_RETRIES, OLLAMA_MODEL_ID,
        OLLAMA_TIMEOUT, OpenAiCompatProvider, from_hive_response, map_hive_error, provider_config,
        to_hive_config, to_hive_message, to_hive_tool_def,
    };
    use crate::domain::{LlmBackend, LlmConfig, LlmError, Message, ToolCall, ToolDefinition};
    use pulsehive::error::PulseHiveError;
    use pulsehive::llm::{
        LlmResponse as HiveLlmResponse, Message as HiveMessage, TokenUsage as HiveTokenUsage,
        ToolCall as HiveToolCall,
    };

    fn sample_config() -> LlmConfig {
        LlmConfig {
            backend: LlmBackend::Ollama,
            model: "gpt-oss:120b".to_owned(),
            temperature: 0.3,
            max_tokens: 256,
        }
    }

    #[test]
    fn provider_constructs_with_pinned_ollama_config() {
        // Smoke test: the adapter builds against the pinned Ollama Cloud config with
        // NO network. The consts are the provider-pivot endpoint + default model id
        // (README C2/C8); `glm-5.3-flash` is the default model (ADR-0023).
        let _provider = OpenAiCompatProvider::new("test-key");
        // FIX A: the config `[llm].base_url` override ctor also builds (NO network).
        let _override = OpenAiCompatProvider::with_base_url("test-key", "https://example.test/v1");
        assert_eq!(OLLAMA_BASE_URL, "https://ollama.com/v1");
        assert_eq!(OLLAMA_MODEL_ID, "glm-5.3-flash");
    }

    #[test]
    fn to_hive_message_preserves_every_variant() {
        match to_hive_message(Message::system("sys")) {
            HiveMessage::System { content } => assert_eq!(content, "sys"),
            other => panic!("expected System, got {other:?}"),
        }
        match to_hive_message(Message::user("hi")) {
            HiveMessage::User { content } => assert_eq!(content, "hi"),
            other => panic!("expected User, got {other:?}"),
        }
        match to_hive_message(Message::assistant("ok")) {
            HiveMessage::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("ok"));
                assert!(tool_calls.is_empty());
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
        match to_hive_message(Message::tool_result("call-1", "42")) {
            HiveMessage::ToolResult {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "call-1");
                assert_eq!(content, "42");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    #[test]
    fn to_hive_message_translates_assistant_tool_calls() {
        let message = Message::Assistant {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_owned(),
                name: "search".to_owned(),
                arguments: serde_json::json!({"q": "btc"}),
            }],
        };
        match to_hive_message(message) {
            HiveMessage::Assistant {
                content,
                tool_calls,
            } => {
                assert!(content.is_none());
                assert_eq!(tool_calls.len(), 1);
                assert_eq!(tool_calls[0].id, "call-1");
                assert_eq!(tool_calls[0].name, "search");
                assert_eq!(tool_calls[0].arguments["q"], "btc");
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn to_hive_tool_def_translates_field_by_field() {
        // The anti-corruption per-field translation: name/description/parameters cross
        // the seam verbatim into the (distinct) PulseHive type (README C2, ADR-0012).
        let tool = ToolDefinition {
            name: "set_entry".to_owned(),
            description: "Set the entry condition".to_owned(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "indicator": { "type": "string" } }
            }),
        };
        let hive = to_hive_tool_def(&tool);
        assert_eq!(hive.name, "set_entry");
        assert_eq!(hive.description, "Set the entry condition");
        assert_eq!(hive.parameters["type"], "object");
        assert_eq!(hive.parameters["properties"]["indicator"]["type"], "string");
    }

    #[test]
    fn to_hive_config_maps_fields_without_temperature_conversion() {
        let hive = to_hive_config(&sample_config());
        assert_eq!(hive.model, "gpt-oss:120b");
        assert!((hive.temperature - 0.3).abs() < f32::EPSILON);
        assert_eq!(hive.max_tokens, 256);
    }

    #[test]
    fn from_hive_response_maps_content_usage_and_tool_calls() {
        let hive = HiveLlmResponse {
            content: Some("pong".to_owned()),
            tool_calls: vec![HiveToolCall {
                id: "c1".to_owned(),
                name: "noop".to_owned(),
                arguments: serde_json::json!({}),
            }],
            usage: HiveTokenUsage {
                input_tokens: 11,
                output_tokens: 4,
            },
        };
        let response = from_hive_response(hive);
        assert_eq!(response.content.as_deref(), Some("pong"));
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].id, "c1");
        assert_eq!(response.tool_calls[0].name, "noop");
        assert_eq!(response.usage.input_tokens, 11);
        assert_eq!(response.usage.output_tokens, 4);
    }

    #[test]
    fn map_hive_error_llm_becomes_provider_verbatim() {
        let err = map_hive_error(PulseHiveError::Llm("upstream 500".to_owned()));
        assert!(
            matches!(&err, LlmError::Provider(message) if message == "upstream 500"),
            "expected Provider(\"upstream 500\"), got {err:?}"
        );
    }

    /// One coach turn is one upstream attempt (PR #128, finding H1).
    ///
    /// The adapter retried transient 429/5xx twice for every surface, so a coach
    /// turn could be three attempts behind one recorded exchange and one ledger row
    /// — the turn's own "no retries and no nudges" rule, contradicted a layer below
    /// it. The composer and `llm-check` keep the retrying posture on purpose:
    /// neither records one exchange per attempt, so absorbing a transient fault
    /// there costs no honesty.
    #[test]
    fn the_coach_path_pins_a_single_attempt_and_the_other_surfaces_still_retry() {
        assert_eq!(
            OpenAiCompatProvider::new("k").max_retries(),
            OLLAMA_MAX_RETRIES,
            "the composer / llm-check default keeps its retries"
        );
        assert_eq!(
            OpenAiCompatProvider::with_base_url("k", "https://example.test/v1").max_retries(),
            OLLAMA_MAX_RETRIES,
            "a base-url override does not change the retry posture"
        );
        assert_eq!(
            OpenAiCompatProvider::single_attempt("k").max_retries(),
            0,
            "the coach path attempts once"
        );
        assert_eq!(
            OpenAiCompatProvider::single_attempt_with_base_url("k", "https://example.test/v1")
                .max_retries(),
            0,
            "and still once behind a base-url override"
        );
    }

    /// Retries are the ONLY thing the two DEFAULT postures disagree about — the
    /// shared builder is what keeps a future edit from quietly widening that gap.
    #[test]
    fn the_two_default_postures_differ_in_retries_and_nothing_else() {
        let retrying = provider_config(
            "k",
            "https://example.test/v1",
            OLLAMA_TIMEOUT,
            OLLAMA_MAX_RETRIES,
        );
        let single = provider_config(
            "k",
            "https://example.test/v1",
            OLLAMA_TIMEOUT,
            COACH_MAX_RETRIES,
        );

        assert_eq!(retrying.model, single.model);
        assert_eq!(retrying.base_url, single.base_url);
        assert_eq!(retrying.timeout_secs, single.timeout_secs);
        assert_eq!(retrying.max_retries, 2);
        assert_eq!(single.max_retries, 0);
    }

    /// A LONGER request timeout is a per-caller choice, not something a
    /// single-attempt caller inherits (review R2).
    ///
    /// The coach needs one — a turn at its 16 384-token cap generates for well over
    /// a minute — but `single_attempt` is the general "one exchange, one row"
    /// posture, and handing every such caller the coach's patience would make one
    /// surface's need the adapter's default. The coach's own number and the
    /// guard it sits under live in `adapters::llm::coach_transport`.
    #[test]
    fn only_the_explicit_timeout_ctor_departs_from_the_default_wait() {
        for provider in [
            OpenAiCompatProvider::new("k"),
            OpenAiCompatProvider::with_base_url("k", "https://example.test/v1"),
            OpenAiCompatProvider::single_attempt("k"),
            OpenAiCompatProvider::single_attempt_with_base_url("k", "https://example.test/v1"),
        ] {
            assert_eq!(
                provider.timeout_secs(),
                OLLAMA_TIMEOUT.as_secs(),
                "every default-posture ctor keeps the 60s request timeout"
            );
        }
        let explicit =
            OpenAiCompatProvider::single_attempt_with_timeout("k", None, Duration::from_secs(100));
        assert_eq!(explicit.timeout_secs(), 100);
        assert_eq!(explicit.max_retries(), 0, "and it still attempts once");
        assert_eq!(
            OpenAiCompatProvider::single_attempt_with_timeout(
                "k",
                Some("https://example.test/v1"),
                Duration::from_secs(100)
            )
            .timeout_secs(),
            100,
            "a base-url override does not restore the default wait"
        );
    }
}
