//! The external-agent submit use case (r2.s1.w3).
//!
//! One typed request in — a target (parent version OR new strategy name), the
//! DSL document, the hypothesis, and the agent name the MCP layer resolved from
//! the live identity — and one persisted outcome out: the new `strategy_version`
//! attributed [`CreatedBy::ExternalAgent`] plus its `agent_submission` row.
//!
//! **Why a use case and not a port call.** The submit pipeline has an opinionated
//! failure order — `Hypothesis` → `AgentName` → target → load → validate →
//! compile → write — and the `dsl` stage collects EVERY validation error rather
//! than stopping at the first. Two different surfaces (today only `pulse mcp`;
//! a desktop rail could arrive later) get the same order because it lives here
//! once, generic over [`StrategyRepository`].
//!
//! The ring's boundary rules apply unchanged: no infrastructure type appears
//! here — the repository is the w1 port and the write path is its
//! `create_agent_version` / `create_agent_strategy_version`, so a failed
//! validation leaves the database untouched.

use crate::domain::strategy::{
    AgentName, AgentSubmission, CreatedBy, Hypothesis, NewAgentSubmission, NewVersion, StrategyId,
    StrategyVersion, VersionId,
};
use crate::domain::{DataError, Migrator, StrategyRepository, ValidationErrors, compile, validate};
use thiserror::Error;

/// Where the submitted version attaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubmitTarget {
    /// A child of an existing version (the normal discovery-loop step).
    Parent(VersionId),
    /// The first version of a NEW strategy — root submits must not collide with
    /// an existing name (the write transaction's in-lock re-check is the
    /// authority; `strategy.name` carries no schema-level UNIQUE).
    Root {
        /// The exact strategy name to create.
        strategy_name: String,
    },
}

/// The submit request the MCP tool builds from its arguments plus the resolved
/// identity name. `dsl` stays a `Value`: the tool schema advertises `type:
/// object`, but serialization to the stored `dsl_json` document is the use
/// case's step, not the caller's.
#[derive(Debug, Clone)]
pub struct SubmitRequest {
    /// Parent version or new-strategy name — exactly one.
    pub target: SubmitTarget,
    /// The DSL document as a JSON object.
    pub dsl: serde_json::Value,
    /// The agent's stated hypothesis (1–2000 chars, `Hypothesis::parse` rules).
    pub hypothesis: String,
    /// The resolved identity name (validated by `AgentName::parse` here —
    /// re-validated so this use case stays sound even if a future caller
    /// bypasses the identity layer).
    pub agent_name: String,
}

/// What a successful submit persisted.
#[derive(Debug, Clone)]
pub struct SubmitOutcome {
    /// The new immutable version row (`created_by: ExternalAgent`).
    pub version: StrategyVersion,
    /// The `agent_submission` row written beside it.
    pub submission: AgentSubmission,
    /// The owning strategy — the parent's strategy for `Parent`, the newly
    /// created row for `Root`.
    pub strategy_id: StrategyId,
}

/// A submit failure, staged so the wire shape can attach the right field path.
#[derive(Debug, Error)]
pub enum SubmitError {
    /// A field-pathed rejection of a scalar argument or the target. `path` is
    /// one of `hypothesis`, `agent_name`, `parent_version_id`, `strategy_name`.
    #[error("{path}: {message}")]
    Field {
        /// The argument the error attaches to.
        path: String,
        /// The human/agent-correctable description.
        message: String,
    },
    /// The DSL document would not load (bad JSON shape, missing/bad/future
    /// `schema_version`, no migration path, migration failure, final
    /// deserialize). `path` is always `"dsl"`.
    #[error("{path}: {message}")]
    Load {
        /// Always `"dsl"`.
        path: &'static str,
        /// The `LoadError` display.
        message: String,
    },
    /// The document loaded but failed semantic validation — EVERY
    /// [`FieldError`](crate::domain::dsl::FieldError), not just the first.
    #[error("dsl: {0}")]
    Validation(ValidationErrors),
    /// The validated document would not compile. `path` is always `"dsl"`.
    #[error("{path}: {message}")]
    Compile {
        /// Always `"dsl"`.
        path: &'static str,
        /// The `CompileError` display.
        message: String,
    },
    /// A repository read or write failed.
    #[error("{0}")]
    Data(DataError),
}

/// Submit one agent-authored strategy version.
///
/// Stages run in the declared order; every failure before the write leaves the
/// database untouched. For `Parent` the write is a single
/// [`StrategyRepository::create_agent_version`] call. For `Root` it is a single
/// [`StrategyRepository::create_agent_strategy_version`] call — the name check,
/// the strategy row, its root version and the submission commit or roll back as
/// one transaction, so a taken name is refused under the write lock and a
/// failed write can never leave an orphan strategy behind.
///
/// # Errors
///
/// [`SubmitError::Field`] for scalar/target rejections, [`SubmitError::Load`]
/// for a document that will not load, [`SubmitError::Validation`] collecting
/// every field error, [`SubmitError::Compile`] for a compile failure, and
/// [`SubmitError::Data`] for repository failures.
pub async fn submit_agent_version<S>(
    strategies: &S,
    request: SubmitRequest,
) -> Result<SubmitOutcome, SubmitError>
where
    S: StrategyRepository,
{
    // 1–2. Scalar fields first, in the declared order. Both newtypes are
    // re-parsed here even though the MCP layer already vetted them: the use
    // case is the last soundness boundary before the write.
    let hypothesis = Hypothesis::parse(&request.hypothesis).map_err(|e| SubmitError::Field {
        path: e.path.clone(),
        message: e.message.clone(),
    })?;
    let agent_name = AgentName::parse(&request.agent_name).map_err(|e| SubmitError::Field {
        path: e.path.clone(),
        message: e.message.clone(),
    })?;
    // `strategy_name` is a scalar too: a blank name would land an unnamed
    // strategy in the Library (the document's own `name` is covered by
    // `validate` below — this is the row the Library lists).
    if let SubmitTarget::Root { strategy_name } = &request.target
        && strategy_name.trim().is_empty()
    {
        return Err(SubmitError::Field {
            path: "strategy_name".to_owned(),
            message: "strategy_name must not be blank".to_owned(),
        });
    }

    // 3. Target resolution.
    let (strategy_id, parent_version_id) = resolve_target(strategies, &request.target).await?;

    // 4–7. Serialize → load → validate → compile. The document as submitted IS
    // `dsl_json`: `to_string` of the request `Value`, never a re-parse of a
    // pre-serialized string, so what persisted is what the agent sent.
    let dsl_json = serde_json::to_string(&request.dsl).map_err(|e| SubmitError::Load {
        path: "dsl",
        message: e.to_string(),
    })?;
    let loaded = Migrator::v1()
        .load(&dsl_json)
        .map_err(|e| SubmitError::Load {
            path: "dsl",
            message: e.to_string(),
        })?;
    let validated = validate(&loaded.dsl).map_err(SubmitError::Validation)?;
    compile(&validated).map_err(|e| SubmitError::Compile {
        path: "dsl",
        message: e.to_string(),
    })?;

    // 8. Write. Each arm is ONE port call — `Parent` adds the version to an
    // existing strategy, `Root` creates strategy + version + submission
    // atomically (the name check is inside that transaction, not a pre-read).
    let submission = NewAgentSubmission {
        agent_name,
        hypothesis,
    };
    match request.target {
        SubmitTarget::Parent(_) => {
            let (version, submission) = strategies
                .create_agent_version(
                    NewVersion {
                        strategy_id: strategy_id.clone(),
                        parent_version_id,
                        dsl_json,
                        created_by: CreatedBy::ExternalAgent,
                        creating_llm_call_ids: vec![],
                    },
                    submission,
                )
                .await
                .map_err(SubmitError::Data)?;
            Ok(SubmitOutcome {
                version,
                submission,
                strategy_id,
            })
        }
        SubmitTarget::Root { strategy_name } => {
            let (strategy, version, submission) = strategies
                .create_agent_strategy_version(&strategy_name, dsl_json, submission)
                .await
                .map_err(|e| match e {
                    // The caller-correctable refusal keeps the wire's
                    // `strategy_name` field path; every other repository
                    // failure is storage.
                    DataError::StrategyNameTaken { name } => SubmitError::Field {
                        path: "strategy_name".to_owned(),
                        message: format!("a strategy named {name} already exists"),
                    },
                    other => SubmitError::Data(other),
                })?;
            Ok(SubmitOutcome {
                version,
                submission,
                strategy_id: strategy.id,
            })
        }
    }
}

/// Resolve the submit target to `(strategy_id, parent_version_id)`.
///
/// `Parent` requires the parent version to exist (its `strategy_id` is the
/// child's owner). `Root` resolves to an empty `StrategyId` placeholder with
/// no read at all: the name-uniqueness refusal is decided inside
/// [`StrategyRepository::create_agent_strategy_version`]'s transaction — a
/// `list_strategies` check here would be a check-then-act race (a concurrent
/// same-named create landing between the read and the write passes the check
/// and produces a duplicate).
async fn resolve_target<S>(
    strategies: &S,
    target: &SubmitTarget,
) -> Result<(StrategyId, Option<VersionId>), SubmitError>
where
    S: StrategyRepository,
{
    match target {
        SubmitTarget::Parent(parent_id) => {
            let parent = strategies
                .get_version(parent_id)
                .await
                .map_err(SubmitError::Data)?
                .ok_or_else(|| SubmitError::Field {
                    path: "parent_version_id".to_owned(),
                    message: format!("no strategy version with id {} exists", parent_id.as_str()),
                })?;
            Ok((parent.strategy_id.clone(), Some(parent.id.clone())))
        }
        SubmitTarget::Root { .. } => Ok((StrategyId::new(""), None)),
    }
}
