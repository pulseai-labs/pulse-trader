//! LLM value types + the pure cost model (VS-1.3.1 work-1.01, FR-23 / FR-24,
//! README C2 / C3 / C5).
//!
//! `PulseTrader`-OWNED shapes that MIRROR `PulseHive`'s LLM types but are
//! re-declared here so `PulseHive`'s evolving 2.x API cannot ripple into the
//! domain (ADR-0012 insulation). The domain stays zero-I/O and free of any
//! `PulseHive` dependency — the only place `PulseHive` types appear is the 1.03
//! adapter (the anti-corruption layer). Zero external deps beyond
//! `serde`/`serde_json`/`rust_decimal`/`thiserror`.
//!
//! - [`Message`] / [`ToolCall`] / [`TokenUsage`] / [`LlmResponse`] /
//!   [`LlmConfig`] / [`LlmBackend`] / [`ReasoningEffort`] — the
//!   request/response/config value types (README C2).
//! - [`LlmError`] — the dedicated, `String`-payload, serde-serializable port
//!   error (README C3), mirroring the dedicated
//!   [`ExchangeError`](crate::domain::exchange::ExchangeError) precedent (audit
//!   C5) rather than folding into [`DataError`](crate::domain::error::DataError).
//! - [`ModelPrice`] / [`PriceTable`] — the pure cost model (README C5): a
//!   data-first price table plus a pure `Decimal` [`cost`](PriceTable::cost)
//!   compute. The actual GLM price VALUES and currency are config/DATA (moat)
//!   loaded by 1.04 via [`from_config`](PriceTable::from_config); this file ships
//!   ONLY the type, the pure fn, and the loader seam, with NO hardcoded price
//!   literals (decision 4). Cost is native-currency, not USD (audit ch3).

use std::collections::HashMap;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single chat message, internally tagged by `role` (the OpenAI/GLM wire shape
/// `PulseHive` speaks).
///
/// `#[serde(tag = "role", rename_all = "snake_case")]` yields the `"system"`,
/// `"user"`, `"assistant"` role tags and — via the explicit rename — `"tool"` for
/// a tool result. The `assistant` variant carries optional `tool_calls`
/// (forward-compat: the v1 port takes no tools IN, but a response MAY carry them
/// OUT); a `tool` message closes the loop with the `tool_call_id` it answers.
/// `PartialEq` but not `Eq` — [`ToolCall::arguments`] is a `serde_json::Value`
/// (floats are not `Eq`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// A system / developer instruction.
    System {
        /// The instruction text.
        content: String,
    },
    /// A user turn.
    User {
        /// The user's text.
        content: String,
    },
    /// An assistant turn — `content` is `None` on a tool-calls-only turn.
    Assistant {
        /// The assistant's text, if any.
        content: Option<String>,
        /// Any tool calls the assistant requested (empty in the v1 no-tools flow).
        #[serde(default)]
        tool_calls: Vec<ToolCall>,
    },
    /// A tool result closing a prior tool call (serialized with `role = "tool"`).
    #[serde(rename = "tool")]
    ToolResult {
        /// The id of the [`ToolCall`] this result answers.
        tool_call_id: String,
        /// The tool's output text.
        content: String,
    },
}

impl Message {
    /// A `system` message.
    #[must_use]
    pub fn system(content: impl Into<String>) -> Self {
        Self::System {
            content: content.into(),
        }
    }

    /// A `user` message.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self::User {
            content: content.into(),
        }
    }

    /// An `assistant` text message carrying no tool calls (the v1 flow).
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::Assistant {
            content: Some(content.into()),
            tool_calls: Vec::new(),
        }
    }

    /// A `tool` result message answering the tool call `tool_call_id`.
    #[must_use]
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self::ToolResult {
            tool_call_id: tool_call_id.into(),
            content: content.into(),
        }
    }
}

/// A tool/function call the assistant requested.
///
/// Carried on [`LlmResponse`] + [`Message::Assistant`] for forward-compat
/// (VS-1.3.2 wires composer tools + validates `arguments`); the v1 port takes no
/// tools IN.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// The provider-assigned call id (echoed on the answering
    /// [`Message::ToolResult`]).
    pub id: String,
    /// The tool/function name.
    pub name: String,
    /// The call arguments as opaque JSON (the tool layer validates them in 1.3.2).
    pub arguments: serde_json::Value,
}

/// A tool/function schema advertised to the model on a [`chat`](crate::domain::LlmProvider::chat)
/// request (VS-1.3.2 work-2.01, FR-23 / FR-3, README C1).
///
/// `PulseTrader`-OWNED and re-declared here (it mirrors `PulseHive`'s identical
/// tool-schema shape but is NOT the same type — ADR-0012 insulation, so
/// `PulseHive`'s evolving 2.x API cannot ripple into the domain). The 2.01
/// OpenAI-compat adapter translates this to the `PulseHive` type field-by-field
/// (the anti-corruption per-field pattern, not a serde round-trip).
/// `parameters` is an opaque JSON-Schema `serde_json::Value` — the composer (2.02+)
/// owns the actual schemas; this slice ships only the transport type. `PartialEq`
/// but not `Eq` (`parameters` is a `serde_json::Value`, whose floats are not `Eq`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// The tool/function name the model calls (echoed back on a [`ToolCall`]).
    pub name: String,
    /// A description the model uses to decide when to invoke this tool.
    pub description: String,
    /// The tool's parameters as an opaque JSON Schema (the tool layer owns it).
    pub parameters: serde_json::Value,
}

/// Token accounting for one completion — the cost-model input (README C5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// Prompt (input) tokens billed.
    pub input_tokens: u32,
    /// Completion (output) tokens billed.
    pub output_tokens: u32,
}

/// One non-streaming chat completion (README C2).
///
/// `content` is `None` on a tool-calls-only turn; `usage` feeds the cost model
/// (README C5). `PartialEq` but not `Eq` (it carries [`ToolCall`]s).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmResponse {
    /// The assistant's text, if any.
    pub content: Option<String>,
    /// Any tool calls (empty in the v1 no-tools flow; carried for forward-compat).
    pub tool_calls: Vec<ToolCall>,
    /// Token accounting for this completion.
    pub usage: TokenUsage,
}

/// One chat request's knobs (README C2).
///
/// `temperature` is `f32` (grill Q3b): a wire-level sampling knob that NEVER feeds
/// `result_content_hash` / money-math / any determinism oracle (LLM calls are
/// fixture-doubled in tests, MASTER-SPEC §9.4), so the `Decimal`-not-float rule
/// (NFR-2) does not apply and `LlmConfig` sits outside the `determinism_guard.rs`
/// scan targets. `PartialEq` but not `Eq` (`f32`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Which backend to dispatch to (the FR-23 "config flag").
    pub backend: LlmBackend,
    /// The model id, e.g. `"glm-5.1"`.
    pub model: String,
    /// Sampling temperature (wire-level `f32`; see the type note).
    pub temperature: f32,
    /// The response token cap.
    pub max_tokens: u32,
    /// The reasoning budget to ask a reasoning model for, or `None` to send nothing
    /// and leave it to the model (#164).
    ///
    /// Absent from the serialized form when `None`, so a config serialized before
    /// the field existed loads unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
}

/// The typed backend selector (README C2, FR-23 "config flag").
///
/// v1 ships a single backend; v2 adds DeepSeek/Gemini/`ClaudeCode`/Codex (roadmap
/// Sprint 2.3) as new arms + adapters — no domain refactor. A typed enum (NOT a
/// `provider: String`) so a typo cannot select a nonexistent backend at runtime.
/// Serializes as its `snake_case` tag, e.g. `"ollama"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmBackend {
    /// Ollama Cloud (`gpt-oss:120b`), reached via `PulseHive`'s OpenAI-compatible
    /// transport in the 2.01 adapter (the VS-1.3.2 provider pivot from GLM).
    Ollama,
}

/// How much a reasoning model may think before it answers — the OpenAI-standard
/// `reasoning_effort` request field (#164).
///
/// Owned here so the SDK's type never reaches a call site (ADR-0012 insulation); the
/// adapter maps it field by field. Serializes as its `snake_case` tag, e.g. `"low"`,
/// the same discipline as [`LlmBackend`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// The least reasoning the model will do.
    Minimal,
    /// A low reasoning budget.
    Low,
    /// A balanced reasoning budget.
    Medium,
    /// The highest reasoning budget.
    High,
}

/// The dedicated `LlmProvider` port error (README C3).
///
/// Deliberately NOT folded into [`DataError`](crate::domain::error::DataError) —
/// mirrors the dedicated
/// [`ExchangeError`](crate::domain::exchange::ExchangeError) precedent (audit C5).
/// `String`-payload so the domain never learns `PulseHive`'s error type (the 1.03
/// adapter maps into these variants); `#[non_exhaustive]` so later slices extend
/// additively; serde-serializable so it can cross the Tauri boundary later.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LlmError {
    /// An upstream / transport failure (the 1.03 adapter maps `PulseHive`'s LLM
    /// error here).
    #[error("llm provider error: {0}")]
    Provider(String),
    /// Missing / invalid configuration (e.g. an absent keychain secret bubbling
    /// up, or an unknown model in the [`PriceTable`]).
    #[error("llm config error: {0}")]
    Config(String),
    /// A fault raised on the call path by THIS process rather than by the provider
    /// — the ledger write failing, a clock out of range.
    ///
    /// Separated from [`LlmError::Provider`] because a caller that RECORDS a
    /// provider fault as a domain outcome must not record a local one the same way
    /// (r1.s2 PR #128, finding 5): the coach's
    /// [`TransportFailure`](crate::domain::CoachFailure::TransportFailure) audit
    /// row says "the coach's provider call failed", and writing that for our own
    /// failed ledger insert puts a false reason in the one record the operator has
    /// to trust.
    #[error("llm local error: {0}")]
    Local(String),
}

/// The per-model price (README C5), expressed per 1,000,000 tokens in the owning
/// [`PriceTable`]'s currency.
///
/// Pure DATA — the actual GLM values load from config (1.04), never a hardcoded
/// public-Rust literal (decision 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    /// Price per 1M input (prompt) tokens, in the table's currency.
    pub input_per_mtok: Decimal,
    /// Price per 1M output (completion) tokens, in the table's currency.
    pub output_per_mtok: Decimal,
}

/// The pure cost model (README C5): a currency-tagged map of model id →
/// [`ModelPrice`] plus a pure `Decimal` [`cost`](Self::cost) compute.
///
/// **Data-first, native-currency (audit ch3).** The price VALUES and `currency`
/// are DATA (moat) loaded from a private config file by 1.04 via
/// [`from_config`](Self::from_config); this file ships ONLY the type, the pure
/// fn, and the loader seam, with NO GLM price literals. GLM (Zhipu) bills in
/// RMB/CNY, so a table's currency is e.g. `"CNY"`, and the resulting
/// [`LlmCall`](crate::domain::llm_call::LlmCall) stores the native figure in that
/// currency (no silent FX baked in).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceTable {
    currency: String,
    models: HashMap<String, ModelPrice>,
}

impl PriceTable {
    /// Build a price table from loaded config DATA (the README C5 seam).
    ///
    /// 1.04 supplies `currency` + the `model id → ModelPrice` map after
    /// deserializing the private config file; this crate ships only the seam (no
    /// price VALUES live here).
    #[must_use]
    pub fn from_config(currency: impl Into<String>, models: HashMap<String, ModelPrice>) -> Self {
        Self {
            currency: currency.into(),
            models,
        }
    }

    /// The table's native billing currency (e.g. `"CNY"`), stored verbatim on the
    /// resulting [`LlmCall`](crate::domain::llm_call::LlmCall)'s `cost_currency`.
    #[must_use]
    pub fn currency(&self) -> &str {
        &self.currency
    }

    /// Compute the pure `Decimal` cost of `usage` for `model`, in
    /// [`currency`](Self::currency).
    ///
    /// `cost = input_tokens/1e6 · input_per_mtok + output_tokens/1e6 ·
    /// output_per_mtok`, all in `Decimal` (NFR-2 — no float ever touches a billed
    /// figure).
    ///
    /// # Errors
    ///
    /// Returns [`LlmError::Config`] when `model` has no entry in the table.
    pub fn cost(&self, model: &str, usage: &TokenUsage) -> Result<Decimal, LlmError> {
        let price = self
            .models
            .get(model)
            .ok_or_else(|| LlmError::Config(format!("no price for model {model}")))?;
        let per_mtok = Decimal::from(1_000_000_u32);
        let input = Decimal::from(usage.input_tokens) * price.input_per_mtok / per_mtok;
        let output = Decimal::from(usage.output_tokens) * price.output_per_mtok / per_mtok;
        Ok(input + output)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        LlmBackend, LlmConfig, LlmError, Message, ModelPrice, PriceTable, ReasoningEffort,
        TokenUsage,
    };
    use rust_decimal::Decimal;
    use std::collections::HashMap;

    #[test]
    fn message_role_tags_serialize_snake_case_with_tool_rename() {
        // System/User/Assistant → snake_case role; ToolResult → "tool".
        let cases = vec![
            (Message::system("s"), "\"role\":\"system\""),
            (Message::user("u"), "\"role\":\"user\""),
            (Message::assistant("a"), "\"role\":\"assistant\""),
            (Message::tool_result("call-1", "42"), "\"role\":\"tool\""),
        ];
        for (msg, needle) in cases {
            let json = serde_json::to_string(&msg).expect("serialize Message");
            assert!(json.contains(needle), "json {json} lacks {needle}");
            let back: Message = serde_json::from_str(&json).expect("deserialize Message");
            assert_eq!(msg, back);
        }
    }

    #[test]
    fn assistant_ctor_has_some_content_and_no_tool_calls() {
        match Message::assistant("hi") {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("hi"));
                assert!(tool_calls.is_empty());
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }

    #[test]
    fn token_usage_defaults_to_zero() {
        let usage = TokenUsage::default();
        assert_eq!(usage.input_tokens, 0);
        assert_eq!(usage.output_tokens, 0);
    }

    #[test]
    fn llm_config_roundtrips_and_carries_f32_temperature() {
        let cfg = LlmConfig {
            backend: LlmBackend::Ollama,
            model: "gpt-oss:120b".to_owned(),
            temperature: 0.5,
            max_tokens: 512,
            reasoning_effort: None,
        };
        let json = serde_json::to_string(&cfg).expect("serialize LlmConfig");
        assert!(
            json.contains("\"backend\":\"ollama\""),
            "backend tag: {json}"
        );
        let back: LlmConfig = serde_json::from_str(&json).expect("deserialize LlmConfig");
        assert_eq!(cfg, back);
    }

    /// An UNSET reasoning effort is not serialized, and a config serialized before
    /// the field existed still loads with it unset (#164): the field is additive.
    #[test]
    fn an_unset_reasoning_effort_is_omitted_and_an_old_config_still_loads() {
        let cfg = LlmConfig {
            backend: LlmBackend::Ollama,
            model: "glm-5.3-flash".to_owned(),
            temperature: 0.0,
            max_tokens: 16_384,
            reasoning_effort: None,
        };
        let json = serde_json::to_string(&cfg).expect("serialize LlmConfig");
        assert!(
            !json.contains("reasoning_effort"),
            "an unset effort puts nothing in the serialized config: {json}"
        );
        let pre_effort =
            r#"{"backend":"ollama","model":"glm-5.3-flash","temperature":0.0,"max_tokens":16384}"#;
        let loaded: LlmConfig =
            serde_json::from_str(pre_effort).expect("a pre-effort config still loads");
        assert_eq!(loaded, cfg);
    }

    /// The effort serializes as its `snake_case` tag, the same discipline as
    /// [`LlmBackend`], and round-trips on the config.
    #[test]
    fn reasoning_effort_serializes_as_its_snake_case_tag() {
        for (effort, tag) in [
            (ReasoningEffort::Minimal, "\"minimal\""),
            (ReasoningEffort::Low, "\"low\""),
            (ReasoningEffort::Medium, "\"medium\""),
            (ReasoningEffort::High, "\"high\""),
        ] {
            assert_eq!(
                serde_json::to_string(&effort).expect("serialize ReasoningEffort"),
                tag
            );
            let back: ReasoningEffort =
                serde_json::from_str(tag).expect("deserialize ReasoningEffort");
            assert_eq!(back, effort);
        }
        let cfg = LlmConfig {
            backend: LlmBackend::Ollama,
            model: "glm-5.3-flash".to_owned(),
            temperature: 0.0,
            max_tokens: 16_384,
            reasoning_effort: Some(ReasoningEffort::Low),
        };
        let json = serde_json::to_string(&cfg).expect("serialize LlmConfig");
        assert!(
            json.contains("\"reasoning_effort\":\"low\""),
            "effort tag: {json}"
        );
        let back: LlmConfig = serde_json::from_str(&json).expect("deserialize LlmConfig");
        assert_eq!(back, cfg);
    }

    #[test]
    fn llm_error_is_serde_roundtrippable_and_displays() {
        for err in [
            LlmError::Provider("boom".to_owned()),
            LlmError::Config("nope".to_owned()),
        ] {
            let json = serde_json::to_string(&err).expect("serialize LlmError");
            let back: LlmError = serde_json::from_str(&json).expect("deserialize LlmError");
            assert_eq!(err, back);
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn price_table_cost_is_pure_decimal_in_native_currency() {
        // 1000 in @ 1/Mtok + 2000 out @ 2/Mtok = 0.001 + 0.004 = 0.005 CNY.
        let mut models = HashMap::new();
        models.insert(
            "glm-5.1".to_owned(),
            ModelPrice {
                input_per_mtok: Decimal::new(1, 0),
                output_per_mtok: Decimal::new(2, 0),
            },
        );
        let table = PriceTable::from_config("CNY", models);
        assert_eq!(table.currency(), "CNY");
        let cost = table
            .cost(
                "glm-5.1",
                &TokenUsage {
                    input_tokens: 1000,
                    output_tokens: 2000,
                },
            )
            .expect("known model prices");
        assert_eq!(cost.normalize(), Decimal::new(5, 3).normalize());
    }

    #[test]
    fn price_table_cost_errors_on_unknown_model() {
        let table = PriceTable::default();
        let err = table
            .cost("nope", &TokenUsage::default())
            .expect_err("unknown model errors");
        assert!(matches!(err, LlmError::Config(_)));
    }
}
