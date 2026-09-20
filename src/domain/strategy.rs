//! Strategy-tree value types — the persisted entities of the strategy slice
//! (VS-1.1.4, FR-4 / FR-11).
//!
//! Pure, zero-I/O value types that mirror the `strategy` / `strategy_version`
//! tables one-to-one. This module declares the *shapes* and a *pure comparator*;
//! it performs **no** persistence, **no** id/hash/timestamp minting, and pulls
//! in **none** of `sqlx`/`serde_json`/the id-generation crate. The adapter
//! (1.03) fills the caller-supplied strings:
//!
//! - [`StrategyId`] / [`VersionId`] are `#[serde(transparent)]` `String`
//!   newtypes the adapter populates with UUID-hyphenated values (1.02 is
//!   deliberately free of the id-generation crate — ids are opaque strings
//!   here).
//! - `created_at` is a [`DateTime<Utc>`] the adapter supplies; the domain only
//!   declares the field.
//! - `version_hash` is an opaque hex `String` the adapter computes (ADR-0009
//!   length-prefixed SHA-256 in 1.03); **no hashing happens in the domain**.
//!
//! **Immutability of a [`StrategyVersion`] (FR-4)** is structural in the API: the
//! [`StrategyRepository`](crate::domain::port::StrategyRepository) port exposes
//! only create + read for versions (no `update_version`/`delete_version`); the DB
//! `BEFORE UPDATE`/`BEFORE DELETE` triggers (1.01) are the second guard.
//!
//! **`#[serde(deny_unknown_fields)]` (#17 money-safety):** [`Strategy`],
//! [`StrategyVersion`], and [`NewVersion`] deserialize stored DB content, so an
//! extra/unknown key surfaces an **error** rather than being silently dropped.
//! The transparent id newtypes do not carry it (it does not apply to a
//! transparent string).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::dsl::{SchemaVersion, StrategyDsl};

/// Identifier of a [`Strategy`] — a `#[serde(transparent)]` `String` newtype.
///
/// Holds a UUID-hyphenated value the **adapter** (1.03) generates; 1.02 mints no
/// ids of its own and treats the id as an opaque string. Serializes as a bare
/// JSON string (matching the `TEXT` primary-key column), not a `{ "0": … }`
/// object. `Hash`/`Ord` let an id key a map or sort a tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StrategyId(String);

impl StrategyId {
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

/// Identifier of a [`StrategyVersion`] — a `#[serde(transparent)]` `String`
/// newtype.
///
/// Same discipline as [`StrategyId`]: a UUID-hyphenated value minted by the
/// adapter, serialized as a bare JSON string matching the `TEXT` column.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VersionId(String);

impl VersionId {
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

/// The provenance of a [`StrategyVersion`] — who/what authored it.
///
/// Serializes to exactly `"human"`, `"composer_llm"`, `"coach_llm"`,
/// `"auto_optimizer"`, `"migration"`, `"external_agent"` — these strings ARE the
/// `strategy_version.created_by` column text (pinned by test, not prose).
/// `Copy` is safe (a fieldless enum). `Migration` covers a version minted by a
/// future DSL migration. `ExternalAgent` (r2.s1.w1) covers a version submitted
/// through `pulse mcp` by an external coding agent — its audit trail is an
/// [`AgentSubmission`] row, not a coaching session or an `LlmCall`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum CreatedBy {
    /// Authored directly by the human user.
    Human,
    /// Composed by the strategy-composer LLM (FR-3 NL → DSL).
    ComposerLlm,
    /// Suggested by the coach LLM's one-tweak-per-loop (FR-7).
    CoachLlm,
    /// Produced by the auto-optimizer (v4+ backlog).
    AutoOptimizer,
    /// Minted by a forward DSL migration (FR-4 read-path).
    Migration,
    /// Submitted by an external coding agent over `pulse mcp` (r2.s1).
    ExternalAgent,
}

/// The mutable strategy meta record — one-to-one with the `strategy` table
/// (FR-11 browse/clone/tag/pin/archive/compare).
///
/// `tags` ← the JSON-array `TEXT` column, `archived` ← the `INTEGER` bool,
/// `created_at` ← the RFC3339 `TEXT` column. `pinned_version_id` is `Option`
/// because a pin is set only **after** a version exists (the nullable,
/// self-referential column to `strategy_version`); `owner` is `Option` (a
/// nullable column). `#[serde(deny_unknown_fields)]`: a stored row with an extra
/// key is an error, not a silent drop (#17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Strategy {
    /// Primary key (adapter-generated UUID string).
    pub id: StrategyId,
    /// Human-readable strategy name.
    pub name: String,
    /// Free-form tags (the JSON-array `TEXT` column) — FR-11 tag/browse.
    pub tags: Vec<String>,
    /// Optional owner label (nullable column).
    pub owner: Option<String>,
    /// The pinned "canonical" version, if any (FR-11 pin; nullable,
    /// self-referential).
    pub pinned_version_id: Option<VersionId>,
    /// Whether the strategy is archived (FR-11 archive; the `INTEGER` bool).
    pub archived: bool,
    /// Creation timestamp (adapter-supplied; the RFC3339 `TEXT` column).
    pub created_at: DateTime<Utc>,
}

/// The **immutable** strategy version record — one-to-one with the
/// `strategy_version` table (FR-4).
///
/// Three load-bearing fields, each a settled upstream decision:
/// - `dsl` is the **migrated current** [`StrategyDsl`] (reconstructed on read by
///   routing `dsl_original` through `Migrator::load` — 1.03's job); 1.02 just
///   declares it as the existing type.
/// - `dsl_original` is the **VERBATIM** pre-migration source bytes as received
///   (a `String`, **never** a re-serialization) — mirrors `Loaded::dsl_original`
///   and is FR-4's immutable-shape guarantee.
/// - `version_hash` is an opaque content-hash hex `String` the **adapter**
///   computes; no hashing happens in the domain.
///
/// `#[serde(deny_unknown_fields)]`: an extra stored key is an error (#17).
/// Immutability is structural — the port has no update/delete for versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategyVersion {
    /// Primary key (adapter-generated UUID string).
    pub id: VersionId,
    /// The owning [`Strategy`].
    pub strategy_id: StrategyId,
    /// The parent version this was cloned from, if any (FR-11 clone; the
    /// self-referential version tree).
    pub parent_version_id: Option<VersionId>,
    /// The DSL document's schema version (a `"MAJOR.MINOR.PATCH"` string).
    pub dsl_schema_version: SchemaVersion,
    /// The migrated current typed document (reconstructed on read by 1.03).
    pub dsl: StrategyDsl,
    /// The **verbatim** pre-migration source bytes (NOT a re-serialization) —
    /// FR-4's immutability guarantee.
    pub dsl_original: String,
    /// The opaque content-hash hex string (adapter-computed; ADR-0009).
    pub version_hash: String,
    /// Who/what authored this version.
    pub created_by: CreatedBy,
    /// The LLM-call ids that produced this version (the JSON-array `TEXT`
    /// column; no `LLMCall` table this slice).
    pub creating_llm_call_ids: Vec<String>,
    /// Creation timestamp (adapter-supplied; the RFC3339 `TEXT` column).
    pub created_at: DateTime<Utc>,
}

/// The create-version request the adapter (1.03) consumes (FR-11 clone = parent
/// set).
///
/// Carries the **raw `dsl_json` string** (NOT a typed [`StrategyDsl`]): the
/// adapter routes it through `Migrator::v1().load(&dsl_json)` to obtain
/// `Loaded { dsl, dsl_original, .. }`, persists `dsl_original` verbatim +
/// `serde_json::to_string(&dsl)`, and derives the id/`version_hash`/`created_at`.
/// A `clone` is just a `NewVersion` with `parent_version_id = Some(parent)`.
/// `#[serde(deny_unknown_fields)]`: the CLI/agent deserializes this (#17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewVersion {
    /// The owning strategy this version belongs to.
    pub strategy_id: StrategyId,
    /// The parent version, if this is a clone (FR-11).
    pub parent_version_id: Option<VersionId>,
    /// The raw DSL JSON, routed through the `Migrator` by the adapter (NEVER
    /// raw-deserialized into [`StrategyDsl`]).
    pub dsl_json: String,
    /// Who/what authored this version.
    pub created_by: CreatedBy,
    /// The LLM-call ids that produced this version.
    pub creating_llm_call_ids: Vec<String>,
}

/// Identifier of an [`AgentSubmission`] — a `#[serde(transparent)]` `String`
/// newtype.
///
/// Same discipline as [`VersionId`]: a UUID-hyphenated value minted by the
/// adapter, serialized as a bare JSON string matching the `TEXT` column.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentSubmissionId(String);

impl AgentSubmissionId {
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

/// A field-pathed rejection of [`AgentName::parse`] (r2.s1.w1).
///
/// Carries `path` (`"agent_name"`) and `message` — the same shape `w3` surfaces
/// as a [`FieldError`](crate::domain::dsl::FieldError), so the MCP layer can
/// relay it without re-validating. No `code` field: the typed classification is
/// `w3`'s to supply at that boundary.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{path}: {message}")]
pub struct AgentNameError {
    /// The field path the error attaches to.
    pub path: String,
    /// Human/agent-correctable description.
    pub message: String,
}

/// A field-pathed rejection of [`Hypothesis::parse`] (r2.s1.w1).
///
/// Same contract as [`AgentNameError`], for the `"hypothesis"` field.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{path}: {message}")]
pub struct HypothesisError {
    /// The field path the error attaches to.
    pub path: String,
    /// Human/agent-correctable description.
    pub message: String,
}

/// The submitting agent's name — validated: 1–64 chars, `^[A-Za-z0-9._-]+$`,
/// stored **lowercased** (r2.s1.w1).
///
/// Constructible only through [`AgentName::parse`], and
/// `#[serde(try_from)]` so the invariant survives the read path too — the
/// `agent_submission.agent_name` column's `CHECK` mirrors the bounds, and a
/// hand-edited row cannot smuggle an invalid name past the constructor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentName(String);

impl AgentName {
    /// Parse and normalize an agent name (lowercased).
    ///
    /// # Errors
    ///
    /// Returns [`AgentNameError`] when the name is not 1–64 characters or
    /// contains characters outside `A–Z a–z 0–9 . _ -`.
    pub fn parse(raw: &str) -> Result<Self, AgentNameError> {
        let name = raw.to_lowercase();
        let len = name.chars().count();
        if !(1..=64).contains(&len) {
            return Err(AgentNameError {
                path: "agent_name".to_owned(),
                message: format!("agent_name must be 1-64 characters, got {len}"),
            });
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err(AgentNameError {
                path: "agent_name".to_owned(),
                message: "agent_name may contain only ASCII letters, digits, '.', '_' and '-'"
                    .to_owned(),
            });
        }
        Ok(Self(name))
    }

    /// Borrow the normalized name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AgentName {
    type Error = AgentNameError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<AgentName> for String {
    fn from(name: AgentName) -> Self {
        name.0
    }
}

/// An external agent's stated hypothesis for a strategy version — validated:
/// trimmed, 1–2000 chars, no control characters but `'\n'` (r2.s1.w1).
///
/// This is the **strategy-level** hypothesis an `agent_submission` carries —
/// deliberately a separate type from
/// [`coaching::Hypothesis`](crate::domain::coaching::Hypothesis) (the coach's
/// proposal text): different bounds, a different owner, and a different audit
/// table, and neither may silently stand in for the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Hypothesis(String);

impl Hypothesis {
    /// Parse a hypothesis, trimming surrounding whitespace.
    ///
    /// # Errors
    ///
    /// Returns [`HypothesisError`] when the text is not 1–2000 characters after
    /// trimming, or contains a control character other than `'\n'`.
    pub fn parse(raw: &str) -> Result<Self, HypothesisError> {
        let text = raw.trim();
        let len = text.chars().count();
        if !(1..=2000).contains(&len) {
            return Err(HypothesisError {
                path: "hypothesis".to_owned(),
                message: format!("hypothesis must be 1-2000 characters after trimming, got {len}"),
            });
        }
        if text.chars().any(|c| c.is_control() && c != '\n') {
            return Err(HypothesisError {
                path: "hypothesis".to_owned(),
                message: "hypothesis may not contain control characters other than '\\n'"
                    .to_owned(),
            });
        }
        Ok(Self(text.to_owned()))
    }

    /// Borrow the hypothesis text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Hypothesis {
    type Error = HypothesisError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Hypothesis> for String {
    fn from(hypothesis: Hypothesis) -> Self {
        hypothesis.0
    }
}

/// The immutable audit row an `external_agent` version carries (r2.s1.w1) —
/// one-to-one with the `agent_submission` table.
///
/// Where a coach-authored version's audit trail is its coaching session,
/// proposal and `LlmCall`, an external agent's cost is **external to the app**:
/// no `LlmCall` row exists, and this normalized row is the only durable link
/// between the version and the agent + hypothesis that produced it. Immutability
/// is structural — the port has no update/delete for submissions, and the
/// `0009` triggers are the second guard.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSubmission {
    /// Primary key (adapter-generated UUID string).
    pub id: AgentSubmissionId,
    /// The version this submission produced (unique — one submission per
    /// version).
    pub version_id: VersionId,
    /// The submitting agent's normalized name.
    pub agent_name: AgentName,
    /// The agent's stated hypothesis for this version.
    pub hypothesis: Hypothesis,
    /// Creation timestamp (adapter-supplied; the RFC3339 `TEXT` column).
    pub created_at: DateTime<Utc>,
}

/// The submission payload [`StrategyRepository::create_agent_version`]
/// consumes alongside the [`NewVersion`] request (r2.s1.w1).
///
/// Both fields are validated newtypes, so an invalid submission is
/// unconstructible by the time it reaches the port.
/// `#[serde(deny_unknown_fields)]`: the MCP layer deserializes this (#17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAgentSubmission {
    /// The submitting agent's name.
    pub agent_name: AgentName,
    /// The agent's stated hypothesis.
    pub hypothesis: Hypothesis,
}

/// The result of [`diff_versions`] — a field-level "what changed" report (FR-11
/// compare).
///
/// A flat, serde-serializable struct of booleans so a UI/LLM can render the
/// diff. `created_at`/`strategy_id` are intentionally NOT diffed (timestamp +
/// owning-strategy are provenance, not content). `version_hash_changed` is the
/// cheap byte-identity proxy; `dsl_changed`/`dsl_original_changed` are the
/// human-readable detail.
// `struct_excessive_bools` (pedantic) is intentionally allowed: the spec mandates
// this flat, serde-serializable boolean report verbatim so a UI/LLM can render
// "what changed" per field — bundling the flags into a bitflag/enum would defeat
// that named-field rendering contract (FR-11 compare).
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionDiff {
    /// `a.id == b.id` (the two args are the identical version).
    pub same_version: bool,
    /// The `dsl_schema_version` fields differ.
    pub dsl_schema_version_changed: bool,
    /// The migrated current typed documents differ (`a.dsl != b.dsl`).
    pub dsl_changed: bool,
    /// The verbatim source bytes differ.
    pub dsl_original_changed: bool,
    /// The content-identity hash differs.
    pub version_hash_changed: bool,
    /// The provenance (`created_by`) differs.
    pub created_by_changed: bool,
    /// The parent version differs (`a.parent_version_id != b.parent_version_id`).
    pub parent_changed: bool,
}

/// A pure, zero-I/O field-level compare of two already-fetched versions (FR-11
/// "compare").
///
/// Compare is **not** a port method — it operates on values the caller fetched
/// via `get_version`. It only reads the two structs' fields and compares with
/// `==`/`!=` ([`StrategyDsl`] / [`SchemaVersion`] both derive `PartialEq`). No
/// I/O, no allocation beyond the returned struct.
#[must_use]
pub fn diff_versions(a: &StrategyVersion, b: &StrategyVersion) -> VersionDiff {
    VersionDiff {
        same_version: a.id == b.id,
        dsl_schema_version_changed: a.dsl_schema_version != b.dsl_schema_version,
        dsl_changed: a.dsl != b.dsl,
        dsl_original_changed: a.dsl_original != b.dsl_original,
        version_hash_changed: a.version_hash != b.version_hash,
        created_by_changed: a.created_by != b.created_by,
        parent_changed: a.parent_version_id != b.parent_version_id,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        CreatedBy, NewVersion, Strategy, StrategyId, StrategyVersion, VersionId, diff_versions,
    };
    use crate::domain::dsl::{
        Comparator, Condition, Direction, ExitRule, IndicatorSpec, RiskParams, SchemaVersion,
        Series, StrategyDsl, SweepableValue, ValueSource,
    };
    use chrono::{TimeZone, Utc};
    use rust_decimal::Decimal;

    /// The canonical `1.0.0` RSI-oversold strategy (mirrors 2.02's shape) —
    /// the typed DSL embedded inside a test `StrategyVersion`.
    fn canonical_dsl() -> StrategyDsl {
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
            exits: vec![ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
            }],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        }
    }

    /// A fully-populated `StrategyVersion` for serde + diff tests. `dsl_original`
    /// is a stand-in verbatim string (the adapter fills the real bytes).
    fn sample_version() -> StrategyVersion {
        StrategyVersion {
            id: VersionId::new("ver-1"),
            strategy_id: StrategyId::new("strat-1"),
            parent_version_id: None,
            dsl_schema_version: SchemaVersion::CURRENT,
            dsl: canonical_dsl(),
            dsl_original: r#"{"schema_version":"1.0.0","name":"RSI Oversold"}"#.to_owned(),
            version_hash: "deadbeef".to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        }
    }

    /// AC-14: each `CreatedBy` variant serializes to its EXACT `snake_case`
    /// string (these strings are the `created_by` column text) + round-trips equal.
    #[test]
    fn created_by_serializes_snake_case() {
        let cases = [
            (CreatedBy::Human, "\"human\""),
            (CreatedBy::ComposerLlm, "\"composer_llm\""),
            (CreatedBy::CoachLlm, "\"coach_llm\""),
            (CreatedBy::AutoOptimizer, "\"auto_optimizer\""),
            (CreatedBy::Migration, "\"migration\""),
            (CreatedBy::ExternalAgent, "\"external_agent\""),
        ];
        for (variant, expected_json) in cases {
            let json = serde_json::to_string(&variant).expect("serialize CreatedBy");
            assert_eq!(json, expected_json, "{variant:?} must be {expected_json}");
            let back: CreatedBy = serde_json::from_str(&json).expect("deserialize CreatedBy");
            assert_eq!(back, variant, "{variant:?} must round-trip equal");
        }
    }

    /// AC-15 (FR-4): a `StrategyVersion` JSON with an EXTRA key fails to
    /// deserialize (`deny_unknown_fields` active, #17); the same JSON WITHOUT
    /// the extra key deserializes `Ok`. The valid JSON is serialized from a
    /// hand-built value so the shape is guaranteed current.
    #[test]
    fn strategy_version_rejects_unknown_field() {
        let valid = serde_json::to_string(&sample_version()).expect("serialize StrategyVersion");

        // Companion: the clean shape deserializes Ok.
        let ok: Result<StrategyVersion, _> = serde_json::from_str(&valid);
        assert!(ok.is_ok(), "valid StrategyVersion JSON must deserialize Ok");

        // Inject an unknown key just inside the opening brace.
        let tampered = valid.replacen('{', r#"{"sneaky":1,"#, 1);
        let err: Result<StrategyVersion, _> = serde_json::from_str(&tampered);
        assert!(
            err.is_err(),
            "an unknown field must be rejected (deny_unknown_fields, #17)"
        );
    }

    /// AC-16 (FR-11): the pure comparator's field-level behavior. Identical
    /// version → `same_version` true + every `*_changed` false; a pair differing
    /// only in `dsl_original` (and thus `version_hash`) flips exactly those two.
    #[test]
    fn diff_versions_flags_changed_fields() {
        let v = sample_version();

        // (a) identical version compared to itself.
        let same = diff_versions(&v, &v);
        assert!(same.same_version, "a.id == b.id");
        assert!(!same.dsl_schema_version_changed);
        assert!(!same.dsl_changed);
        assert!(!same.dsl_original_changed);
        assert!(!same.version_hash_changed);
        assert!(!same.created_by_changed);
        assert!(!same.parent_changed);

        // (b) a sibling differing ONLY in dsl_original + version_hash.
        let mut other = sample_version();
        other.id = VersionId::new("ver-2");
        other.dsl_original = r#"{"schema_version":"1.0.0","name":"Changed"}"#.to_owned();
        other.version_hash = "cafef00d".to_owned();

        let diff = diff_versions(&v, &other);
        assert!(!diff.same_version, "different ids");
        assert!(diff.dsl_original_changed, "verbatim source differs");
        assert!(diff.version_hash_changed, "content identity differs");
        // The migrated typed dsl is unchanged here.
        assert!(!diff.dsl_changed);
        assert!(!diff.dsl_schema_version_changed);
        assert!(!diff.created_by_changed);
        assert!(!diff.parent_changed);
    }

    /// The `Strategy` meta record + `NewVersion` request round-trip through
    /// serde value-equal (exercises the FR-11 mutable-meta shape + the
    /// create-request shape).
    #[test]
    fn strategy_and_new_version_round_trip() {
        let strat = Strategy {
            id: StrategyId::new("strat-1"),
            name: "Demo".to_owned(),
            tags: vec!["scalp".to_owned(), "btc".to_owned()],
            owner: Some("alice".to_owned()),
            pinned_version_id: Some(VersionId::new("ver-1")),
            archived: false,
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        };
        let json = serde_json::to_string(&strat).expect("serialize Strategy");
        let back: Strategy = serde_json::from_str(&json).expect("deserialize Strategy");
        assert_eq!(back, strat);

        let req = NewVersion {
            strategy_id: StrategyId::new("strat-1"),
            parent_version_id: Some(VersionId::new("ver-1")),
            dsl_json: r#"{"schema_version":"1.0.0"}"#.to_owned(),
            created_by: CreatedBy::ComposerLlm,
            creating_llm_call_ids: vec!["call-1".to_owned()],
        };
        let json = serde_json::to_string(&req).expect("serialize NewVersion");
        let back: NewVersion = serde_json::from_str(&json).expect("deserialize NewVersion");
        assert_eq!(back, req);
    }

    /// The id newtypes (de)serialize as bare transparent strings (matching the
    /// `TEXT` columns), and `new`/`as_str` round-trip.
    #[test]
    fn id_newtypes_are_transparent_strings() {
        let sid = StrategyId::new("abc-123");
        assert_eq!(sid.as_str(), "abc-123");
        let json = serde_json::to_string(&sid).expect("serialize StrategyId");
        assert_eq!(json, "\"abc-123\"", "transparent: a bare JSON string");
        let back: StrategyId = serde_json::from_str(&json).expect("deserialize StrategyId");
        assert_eq!(back, sid);

        let vid = VersionId::new("def-456");
        assert_eq!(vid.as_str(), "def-456");
        let json = serde_json::to_string(&vid).expect("serialize VersionId");
        assert_eq!(json, "\"def-456\"");
    }
}
