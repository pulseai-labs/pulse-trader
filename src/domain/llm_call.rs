//! The `LlmCall` append-only ledger entity + its `LlmCallId` newtype (VS-1.3.1
//! work-1.01, FR-24, README C4).
//!
//! Pure, zero-I/O value types. [`LlmCallId`] is a `#[serde(transparent)]`
//! `String` newtype (matches [`StrategyId`](crate::domain::strategy::StrategyId))
//! so VS-1.3.2 can populate `StrategyVersion.creating_llm_call_ids: Vec<String>`
//! — **this slice adds NO FK / no attribution wiring** (`creating_llm_call_ids`
//! stays `vec![]`). The record stores the VERBATIM prompt **after redaction**
//! (NFR-6, applied by the 1.04 decorator) + the completion + tokens + the
//! `Decimal` cost in its native `cost_currency` (audit ch3).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use super::llm::{LlmBackend, LlmError, LlmResponse, Message};
use super::secret::CredentialSource;
use super::strategy::CreatedBy;

/// Identifier of an [`LlmCall`] — a `#[serde(transparent)]` `String` newtype.
///
/// Same discipline as [`StrategyId`](crate::domain::strategy::StrategyId): an
/// opaque adapter-minted string, serialized as a bare JSON string (matching the
/// `TEXT` primary-key column), so it drops straight into
/// `StrategyVersion.creating_llm_call_ids` (VS-1.3.2). `Hash`/`Ord` let an id key
/// a map or sort a ledger.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LlmCallId(String);

impl LlmCallId {
    /// Wrap a raw (adapter-generated) id string.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying id string (for SQL binding / map keys).
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The append-only LLM-call ledger record (FR-24, README C4).
///
/// One row per provider round-trip. `prompt_messages` + `completion` are stored
/// **verbatim after redaction** (NFR-6 — the 1.04 decorator scrubs the persisted
/// copy, never the sent bytes); `cost` is a `Decimal` in `cost_currency` (the
/// price table's native billing currency — GLM/Zhipu bills RMB/CNY; no silent FX,
/// audit ch3). `created_at` is injected via the `Clock` (RFC3339 on persist);
/// `created_by` reuses the existing [`CreatedBy`] provenance enum.
///
/// `#[serde(deny_unknown_fields)]` (#17 money-safety): a stored row with an extra
/// key is an error, not a silent drop. `PartialEq` but not `Eq` (it carries
/// [`Message`]s, which are not `Eq`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmCall {
    /// Primary key (adapter-generated).
    pub id: LlmCallId,
    /// Which backend served the call (stored as its serde tag, e.g. `"ollama"`).
    pub backend: LlmBackend,
    /// The model id, e.g. `"glm-5.2"`.
    pub model: String,
    /// The VERBATIM prompt AFTER redaction (NFR-6) — what we persist.
    pub prompt_messages: Vec<Message>,
    /// The response text AFTER redaction, if any.
    pub completion: Option<String>,
    /// Prompt (input) tokens billed.
    pub input_tokens: u32,
    /// Completion (output) tokens billed.
    pub output_tokens: u32,
    /// The computed cost (1.04 fills it from usage × the price table), a `Decimal`
    /// (NFR-2), in the currency named by `cost_currency`.
    pub cost: Decimal,
    /// The native billing currency of `cost` (e.g. `"CNY"`) — see README C5.
    pub cost_currency: String,
    /// Creation timestamp (adapter-supplied via the injected `Clock`).
    pub created_at: DateTime<Utc>,
    /// Who/what triggered the call (`Human` | `ComposerLlm` | `CoachLlm` | …).
    pub created_by: CreatedBy,
    /// Which credential source supplied the API key for this call — the r1.s1.w2
    /// audit-trail control, so a call's provenance is reconstructible from the
    /// ledger alone.
    ///
    /// A LABEL (`env` / `config-dir` / `cwd-dotenv` / `app-data-dir` / `keychain`),
    /// never the key
    /// or any fragment of it. `None` means the provenance was not recorded: either a
    /// row written before migration `0007`, or a caller that supplied none.
    /// `#[serde(default)]` so a pre-`0007` serialized row still deserializes instead
    /// of failing on the absent field — the same backward-compatibility reasoning
    /// that keeps `LLM_CALL_SCHEMA_VERSION` at 1.
    #[serde(default)]
    pub key_source: Option<CredentialSource>,
    /// Which version of the agent prompt produced this call — the content hash of
    /// the **resolved** prompt, whichever of the compiled-in default or the
    /// `$PULSE_PROMPT_DIR` overlay actually won (r1.s2 audit C2). An overlay edit
    /// therefore changes the recorded version with no release step, which is what
    /// makes the prompt overlay auditable rather than invisible.
    ///
    /// `None` means no prompt version was recorded: a row written before migration
    /// `0005`, or a caller that records none — composer rows stay `None`, and
    /// **computing the hash is `r1.s2.w3`'s**; this item carries the field and the
    /// column. `#[serde(default)]` for the same backward-compatibility reason
    /// `key_source` has it, and the same reason `LLM_CALL_SCHEMA_VERSION` stays 1.
    #[serde(default)]
    pub prompt_version: Option<String>,
}

// ---------------------------------------------------------------------------
// The attributed call (r1.s4.w1)
// ---------------------------------------------------------------------------

/// One response, and the exact ledger row that call minted — learned together.
///
/// The pair is what makes a coach turn attributable: a session that names an
/// [`LlmCallId`] some OTHER call produced is individually valid and collectively
/// false, which is `#132`'s complaint. Returned by
/// [`AttributedCoachProvider`](crate::domain::port::AttributedCoachProvider).
pub(crate) struct AttributedCall {
    /// The model's answer.
    pub(crate) response: LlmResponse,
    /// The `LlmCall` row the decorator persisted for THIS call.
    pub(crate) llm_call_id: LlmCallId,
}

/// Why an attributed call produced no `(response, ledger row)` pair.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AttributedCallError {
    /// The call returned an error. `llm_call_id` is `Some` when the decorator still
    /// managed to write a row for the attempt — a transport fault can be billed.
    #[error("{error}")]
    Provider {
        /// What went wrong. [`LlmError::Provider`] and
        /// [`LlmError::MalformedToolCall`] are TRANSPORT faults (recorded
        /// coaching outcomes); `Config`/`Local` are this process faulting on the
        /// call path and are not coaching outcomes at all (PR #128, finding 5).
        error: LlmError,
        /// The ledger row the attempt minted, if any.
        llm_call_id: Option<LlmCallId>,
    },
    /// A usable response came back and NO ledger row appeared for it.
    ///
    /// `llm_call_id = NULL` on a post-call session says no row was correlated to a
    /// turn that was made and billed — a false record, so the turn refuses instead
    /// (PR #128, finding G1).
    #[error(
        "the coach turn reached the provider but captured no ledger row: the provider or the \
         capture handle is not the one the ledger decorator writes through"
    )]
    LedgerRowMissing,
    /// Several ledger rows appeared for one call. One turn is one call is one row;
    /// no choice among several is honest.
    #[error("the coach turn captured {seen} ledger rows; one turn is one call is one row")]
    LedgerRowsAmbiguous {
        /// How many ids appeared for the one call.
        seen: usize,
    },
    /// The capture buffer is SHORTER than it was when the call began.
    ///
    /// Only clearing or replacing the buffer mid-call does that, and it means the
    /// ids this call minted are gone. Reading the shrunk buffer as "no rows
    /// appeared" would put `llm_call_id = NULL` on a turn that reached the provider
    /// and was billed — the same false record [`LedgerRowMissing`] refuses, arrived
    /// at by losing the evidence instead of never having it.
    ///
    /// [`LedgerRowMissing`]: AttributedCallError::LedgerRowMissing
    #[error(
        "the capture buffer shrank during the call (was {start}, now {len}): the ids this turn \
         minted are gone, so no honest attribution is available"
    )]
    CaptureBufferShrank {
        /// Where the buffer stood when the call began.
        start: usize,
        /// Where it stands now.
        len: usize,
    },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{LlmCall, LlmCallId};
    use crate::domain::llm::{LlmBackend, Message};
    use crate::domain::secret::CredentialSource;
    use crate::domain::strategy::CreatedBy;
    use chrono::{TimeZone, Utc};
    use rust_decimal::Decimal;

    #[test]
    fn llm_call_id_is_transparent_string() {
        let id = LlmCallId::new("call-abc");
        assert_eq!(id.as_str(), "call-abc");
        let json = serde_json::to_string(&id).expect("serialize LlmCallId");
        // transparent: a bare JSON string, not a `{ "0": ... }` object.
        assert_eq!(json, "\"call-abc\"");
        let back: LlmCallId = serde_json::from_str(&json).expect("deserialize LlmCallId");
        assert_eq!(id, back);
    }

    #[test]
    fn llm_call_roundtrips_verbatim_and_stays_native_currency() {
        let call = LlmCall {
            id: LlmCallId::new("call-1"),
            backend: LlmBackend::Ollama,
            model: "glm-5.2".to_owned(),
            prompt_messages: vec![Message::system("be terse"), Message::user("hello")],
            completion: Some("hi".to_owned()),
            input_tokens: 12,
            output_tokens: 3,
            cost: Decimal::new(1234, 4), // 0.1234
            cost_currency: "CNY".to_owned(),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            created_by: CreatedBy::ComposerLlm,
            key_source: Some(CredentialSource::ConfigDir),
            prompt_version: None,
        };
        let json = serde_json::to_string(&call).expect("serialize LlmCall");
        let back: LlmCall = serde_json::from_str(&json).expect("deserialize LlmCall");
        assert_eq!(call, back);
        // native currency + verbatim prompt survived the round-trip.
        assert_eq!(back.cost_currency, "CNY");
        assert_eq!(back.prompt_messages.len(), 2);
        assert_eq!(back.backend, LlmBackend::Ollama);
    }

    #[test]
    fn llm_call_id_degrades_to_the_string_the_future_field_holds() {
        // C4: LlmCallId is a String newtype so VS-1.3.2 can push into
        // `StrategyVersion.creating_llm_call_ids: Vec<String>`. This slice wires no
        // FK — prove the id degrades cleanly to the String that field holds.
        let ids: Vec<String> = vec![LlmCallId::new("call-1").as_str().to_owned()];
        assert_eq!(ids, vec!["call-1".to_owned()]);
    }
}
