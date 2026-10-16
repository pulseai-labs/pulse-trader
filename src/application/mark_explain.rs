//! The `explain_marks` use case (r5.s3.w2, PDR-0037 Q5, ADR-0031, SPINE G3/G4) —
//! which of a candidate rule's conditions held at each example mark's
//! **evaluation bar**, with the operand values behind each answer, plus the 17
//! candle-pattern values at the mark's own candle.
//!
//! # The one truth (G3)
//!
//! There is no second evaluator here. The use case builds the **same
//! [`EngineSession`]** a backtest builds, steps it with the backtest's own
//! [`step_series`] loop, and reads each conjunct through that session's
//! [`EvalProbe`] — the entry gate's own warm clause and the predicate's own
//! `eval`. A condition reported `held` at a mark's evaluation bar is therefore
//! exactly what the backtest's entry gate sees at that bar for that rule, and on
//! a backtest's own entry bars every condition reads `held`.
//!
//! # The fence (G4)
//!
//! Every loaded series is cut at the open freeze's holdout start **before the
//! first `step`**, so no engine is ever stepped onto a candle at or after it. A
//! mark whose evaluation bar opens at or after the start is **hidden and
//! counted**, never explained. A `4h` or `1d` candle reaches an evaluation only
//! through [`align`]'s closed-bar pairing, so it is seen only once it has closed.
//!
//! # The evaluation bar (Q5)
//!
//! A `15m` mark is evaluated at **its own bar**; a `4h` or `1d` mark at the
//! **first `15m` bar opening at that candle's close** — the earliest bar the
//! backtest could have entered on it.
//!
//! **One boundary consequence, deliberate.** A `4h` mark whose candle closes
//! EXACTLY at the holdout start is a legal mark (`guard_candle` refuses only a
//! close landing *after* the start) and `list_marks` lists it — but its
//! evaluation bar opens exactly at the start, where the 15m series has been cut,
//! so there is no bar to evaluate and this use case hides it. G4 forces that: the
//! bar the mark would be explained at is a holdout bar.
//!
//! # Two stepping passes, one per pair
//!
//! A pair's work is **bounded memory, not a per-bar table**: one pass answers the
//! marks' evaluation bars (each mark names a bar, and the pass reads the probe
//! only at those bars), and a second reads the marks' own candles for the 17
//! patterns. Both passes step the same way, so explain pairs HTF/D1 bars exactly
//! as a run does, and neither holds a per-bar record of the whole series.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::adapters::backtest::{
    BacktestConfig, EngineSession, EvalProbe, SessionTimeframes, indicator_values_at, step_series,
};
use crate::application::mcp_read::CANDLE_PATTERNS;
use crate::domain::strategy::VersionId;
use crate::domain::{
    CandleSeries, CandleSeriesRepository, CompiledCondition, CompiledStrategy, CompiledValue,
    Condition, DataError, Direction, ExampleMark, ExampleMarkRepository, ExchangeAdapter,
    HoldoutFreeze, IndicatorSpec, MarkError, MarkFilter, MarkSide, Migrator, Pair, Series,
    StrategyDsl, StrategyRepository, SymbolFilters, Timeframe, ValueSource, check_unknown_fields,
    compile, validate,
};

/// How many conjuncts one explanation carries, at most — a bound on the
/// response size, not a rule of the DSL.
const MAX_CONJUNCTS: usize = 64;

/// The rule to explain against: an inline DSL document (validated and never
/// stored) or the id of a stored version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleRequest {
    /// An inline DSL document, the caller's own JSON.
    Inline(Box<serde_json::Value>),
    /// A stored version's id.
    Version(VersionId),
}

/// Which marks to explain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkSelection {
    /// These mark ids, in this order.
    Ids(Vec<String>),
    /// Every stored mark carrying this setup label, optionally narrowed to one
    /// pair — gathered through `list_marks`' own read, so a withdrawn mark is
    /// absent and a freeze's covered marks are hidden and counted.
    Setup {
        /// The setup label, exactly as stored.
        setup: String,
        /// Narrow to this pair, when the caller named one.
        pair: Option<Pair>,
    },
}

/// One `explain_marks` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainRequest {
    /// Which marks to explain.
    pub marks: MarkSelection,
    /// The rule to explain them against.
    pub rule: RuleRequest,
}

/// One operand of a conjunct, at the evaluation bar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperandReport {
    /// The operand's label in the house DSL vocabulary (`rsi:14`,
    /// `h4:ema(200)`, `cdl:engulfing`, `lag(close, 5)`).
    pub label: String,
    /// The value at the bar, an exact decimal string; `null` where the engine
    /// holds no value (warmup, or a level/pattern with no level at this bar).
    pub value: Option<String>,
    /// The prior bar's value — present only on a cross comparison's operands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<Option<String>>,
}

/// What one conjunct reports: the condition, its state, and the operand values
/// behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConjunctReport {
    /// The conjunct's index in the flattened `entry ∧ filters` order.
    pub index: usize,
    /// Which half of the effective entry it came from: `entry` or `filter`.
    pub source: &'static str,
    /// The condition as DSL JSON — exactly what the caller wrote.
    pub condition: serde_json::Value,
    /// `held`, `not_held`, or `not_warm` (the entry gate is cold at this bar).
    pub state: &'static str,
    /// The operand values behind the answer. Empty while `not_warm`, because a
    /// partial history would report numbers the gate never saw.
    pub operands: Vec<OperandReport>,
}

/// One candle-pattern value at the mark's own candle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PatternReport {
    /// The pattern's read-path token, `cdl:<snake_case>` — the SAME vocabulary
    /// `export_indicators` carries, so the two agree by value (the spec's
    /// "equal to `export_indicators` `cdl:` values").
    pub name: String,
    /// TA-Lib's integer as a string (`100` / `-100` / `0` / `80` / `-80`), or
    /// `null` before the pattern's look-back.
    pub value: Option<String>,
}

/// One mark's wire shape — the same `snake_case` JSON `mark_json` emits on the
/// MCP surface (`mark_id`, `bar_time`, `data_version`,
/// `writer.{kind,name,self_reported}`). `ExampleMark` is a domain value with no
/// serde derive, so the projection is explicit and lives here.
///
/// The rest of this response keeps the spec's field NAMES in that same casing —
/// `evaluation_bar`, `side_matches`, `all_held`, `marks_explained`,
/// `held_by_every_mark`, `hidden_by_holdout` — so the one MCP surface has one
/// casing (the top's ruling on this item; the reference response matched is
/// `list_marks`/`write_mark`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarkJson {
    /// The row id.
    pub mark_id: String,
    /// The pair.
    pub pair: String,
    /// The `Binance` interval text.
    pub timeframe: &'static str,
    /// The marked candle's open time, RFC 3339 UTC ms — `mark_json`'s own
    /// spelling, so the mark here is byte-identical to the one `list_marks`
    /// returns (the spec's "the mark, as `list_marks` returns it").
    pub bar_time: String,
    /// `long` or `short`.
    pub side: &'static str,
    /// The setup label.
    pub setup: String,
    /// The optional note.
    pub note: Option<String>,
    /// The writer: kind, stored name, and whether the name is self-reported.
    pub writer: MarkWriterJson,
    /// The snapshot the mark was written against.
    pub data_version: String,
    /// When the mark landed, RFC 3339 UTC ms (the `mark_json` spelling).
    pub created_at: String,
}

/// An epoch-ms instant as RFC 3339 UTC MILLISECONDS — the exact spelling
/// `src/mcp/tools.rs`'s `window_ms` uses, so an explain's mark object equals the
/// one `list_marks` returns field for field.
fn rfc3339_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(
        || format!("{ms} ms"),
        |dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
}

/// A mark's writer, as `mark_json` emits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarkWriterJson {
    /// `trader` for the app, `agent` for an MCP client.
    pub kind: &'static str,
    /// The writer's name as stored.
    pub name: String,
    /// Whether the name is the writer's own claim about itself (true for an agent).
    pub self_reported: bool,
}

impl MarkJson {
    /// The wire shape of one stored mark.
    #[must_use]
    pub fn of(mark: &ExampleMark) -> Self {
        Self {
            mark_id: mark.id.clone(),
            pair: mark.pair.to_string(),
            timeframe: mark.timeframe.binance_interval(),
            bar_time: rfc3339_ms(mark.bar_open_ms),
            side: mark.side.as_str(),
            setup: mark.setup.as_str().to_owned(),
            note: mark.note.as_ref().map(|note| note.as_str().to_owned()),
            writer: MarkWriterJson {
                kind: mark.writer.kind.as_str(),
                name: mark.writer.name.clone(),
                self_reported: mark.writer.kind.is_self_reported(),
            },
            data_version: mark.data_version.clone(),
            created_at: rfc3339_ms(mark.created_at_ms),
        }
    }
}

/// One mark, explained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarkExplanation {
    /// The mark, as `list_marks` returns it.
    pub mark: MarkJson,
    /// The evaluation bar's open time, RFC 3339 UTC ms — the `window_ms`
    /// spelling w1's `mark_json` uses. The mark's own bar for a `15m` mark, else
    /// the first `15m` bar opening at its candle's close.
    pub evaluation_bar: String,
    /// Whether the rule's direction allows this mark's side.
    pub side_matches: bool,
    /// Every conjunct of `entry ∧ filters`, in declared order.
    pub conjuncts: Vec<ConjunctReport>,
    /// Every conjunct held AND the side matching — the mark's own verdict.
    pub all_held: bool,
    /// The 17 candle-pattern values at the mark's own candle and timeframe.
    pub patterns: Vec<PatternReport>,
}

/// A mark the read could not explain, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarkRefusal {
    /// The mark's id.
    pub mark_id: String,
    /// `no_evaluation_bar` when the `15m` series holds no bar at the mark's
    /// evaluation instant; `no_mark_candle` when the mark's own recorded
    /// snapshot no longer holds its candle.
    pub reason: &'static str,
}

/// What the fence hid from this read (Q4, PDR-0011).
///
/// Not `Copy`: `holdout_start` crosses as an RFC 3339 string, so this value is
/// built once and moved into the report, exactly as `MarkExplanation` is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExplainHoldout {
    /// The open freeze's holdout start, RFC 3339 UTC ms — the same rendering
    /// `list_marks` gives its own `hidden_by_holdout` block.
    pub holdout_start: String,
    /// How many marks this read hid because their evaluation bar reaches it.
    pub count: u32,
}

/// The whole `explain_marks` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExplainReport {
    /// One entry per explained mark, in the selection's order.
    pub marks: Vec<MarkExplanation>,
    /// Marks the read could not explain, with a typed reason each.
    pub refusals: Vec<MarkRefusal>,
    /// How many marks were explained.
    pub marks_explained: usize,
    /// The conjunct indexes held at EVERY explained mark — the conditions a rule
    /// every example satisfies would carry. Empty when nothing was explained.
    pub held_by_every_mark: Vec<usize>,
    /// The fence's count, or `null` while no freeze is open.
    pub hidden_by_holdout: Option<ExplainHoldout>,
}

/// The use case's typed refusal.
#[derive(Debug, thiserror::Error)]
pub enum ExplainError {
    /// The request's shape is wrong (an empty selection, an unknown mark, too
    /// many conjuncts).
    #[error("{path}: {message}")]
    Field {
        /// The request member the refusal attaches to.
        path: &'static str,
        /// The correctable description.
        message: String,
    },
    /// The rule would not load, migrate, validate, check for unknown fields, or
    /// compile. The path is the document's own — `dsl` when the failure is the
    /// document's, or the validator's field path.
    #[error("{path}: {message}")]
    Dsl {
        /// The document's path within the DSL, or `dsl`.
        path: String,
        /// The validator's or compiler's own message.
        message: String,
    },
    /// The named version does not exist.
    #[error("no strategy version with id {} exists", .0.as_str())]
    VersionNotFound(VersionId),
    /// The rule asks for something the store or the engine cannot supply: a
    /// series the rule reads with no HEAD snapshot, or a strategy the engine
    /// refuses to construct.
    #[error("{0}")]
    Rule(String),
    /// The mark use case's own refusal.
    #[error(transparent)]
    Mark(#[from] MarkError),
    /// A candle-series read failed.
    #[error(transparent)]
    Data(#[from] DataError),
}

/// Explain every selected mark against one rule.
///
/// The freeze is passed in — the edge reads it inside the call it serves,
/// exactly as the mark use cases do — so the application ring stays generic over
/// its ports and no second reader of the freeze table exists.
///
/// # Errors
///
/// [`ExplainError::Field`] for a malformed request, [`ExplainError::Dsl`] for a
/// rule that will not validate or compile, [`ExplainError::VersionNotFound`] for
/// an unknown version id, [`ExplainError::Rule`] for a rule the engine or the
/// store cannot serve, and [`ExplainError::Data`]/[`ExplainError::Mark`] for
/// store failures.
pub async fn explain_marks<R, C, S, X>(
    marks: &R,
    candles: &C,
    strategies: &S,
    exchange: &X,
    freeze: Option<HoldoutFreeze>,
    request: ExplainRequest,
) -> Result<ExplainReport, ExplainError>
where
    R: ExampleMarkRepository,
    C: CandleSeriesRepository,
    S: StrategyRepository,
    X: ExchangeAdapter,
{
    // 1. The rule: an inline document validated exactly as
    //    `submit_strategy_version` validates it, or a stored version by id.
    //    Nothing is written on either path.
    let (dsl, direction) = resolve_rule(strategies, &request.rule).await?;
    let validated = validate(&dsl).map_err(|errors| ExplainError::Dsl {
        path: "dsl".to_owned(),
        message: validation_text(&errors),
    })?;
    let compiled = compile(&validated).map_err(|error| ExplainError::Dsl {
        path: "dsl".to_owned(),
        message: error.to_string(),
    })?;

    // 2. The conjuncts, flattened on both sides in the same order.
    let conjuncts = flatten(&dsl, &compiled)?;

    // 3. The marks, gathered through the port (withdrawn marks absent, a
    //    freeze's covered marks hidden and counted).
    let (selected, hidden_from_list) = gather(marks, &request.marks, freeze).await?;

    // 4. Group by pair — one pair, one pass.
    let mut by_pair: BTreeMap<Pair, Vec<ExampleMark>> = BTreeMap::new();
    let mut refusals: Vec<MarkRefusal> = Vec::new();
    // Memo of each mark's evaluation bar, so the fence, the pass and the answer
    // agree on one number.
    let mut evaluation: BTreeMap<String, i64> = BTreeMap::new();
    let mut hidden_fence: u32 = 0;

    for mark in selected {
        let Some(eval_ms) = evaluation_bar_ms(&mark, candles)? else {
            refusals.push(MarkRefusal {
                mark_id: mark.id.clone(),
                reason: "no_evaluation_bar",
            });
            continue;
        };
        // The fence, on the EVALUATION bar: a bar opening at or after the holdout
        // start is a bar this read may not evaluate at all (G4).
        if let Some(freeze) = freeze
            && eval_ms >= freeze.holdout_start_ms
        {
            hidden_fence = hidden_fence.saturating_add(1);
            continue;
        }
        evaluation.insert(mark.id.clone(), eval_ms);
        by_pair.entry(mark.pair.clone()).or_default().push(mark);
    }

    // 5. One pass per pair.
    let mut explained: Vec<MarkExplanation> = Vec::new();
    for (pair, pair_marks) in by_pair {
        let pass = PairPass::build(
            &pair,
            &compiled,
            candles,
            exchange,
            freeze,
            &conjuncts,
            &pair_marks,
            &evaluation,
        )?;
        for mark in &pair_marks {
            let Some(eval_ms) = evaluation.get(&mark.id).copied() else {
                continue;
            };
            match pass.explain(mark, eval_ms, &conjuncts, direction) {
                Ok(explanation) => explained.push(explanation),
                Err(reason) => refusals.push(MarkRefusal {
                    mark_id: mark.id.clone(),
                    reason,
                }),
            }
        }
    }

    // The selection's own order is the ids' order, or the list's candle order;
    // re-sorting by the original selection keeps one stable answer.
    let order = |id: &str| selected_order(&request.marks, id);
    explained.sort_by_key(|mark| order(&mark.mark.mark_id));

    let held_by_every_mark = held_by_every_mark(&explained);
    let hidden = freeze.map(|freeze| ExplainHoldout {
        holdout_start: rfc3339_ms(freeze.holdout_start_ms),
        count: hidden_fence.saturating_add(hidden_from_list),
    });

    Ok(ExplainReport {
        marks_explained: explained.len(),
        marks: explained,
        refusals,
        held_by_every_mark,
        hidden_by_holdout: hidden,
    })
}

/// Where a mark sits in the request's own selection order. An id selection uses
/// the ids' order; a setup selection keeps the list read's candle order, so the
/// index is the mark's position there (large for a mark the read did not list,
/// which cannot happen for an explained mark).
fn selected_order(selection: &MarkSelection, id: &str) -> usize {
    match selection {
        MarkSelection::Ids(ids) => ids.iter().position(|want| want == id).unwrap_or(usize::MAX),
        MarkSelection::Setup { .. } => usize::MAX,
    }
}

/// Resolve the request's rule to a DSL document and its direction.
async fn resolve_rule<S>(
    strategies: &S,
    rule: &RuleRequest,
) -> Result<(StrategyDsl, Direction), ExplainError>
where
    S: StrategyRepository,
{
    match rule {
        RuleRequest::Inline(raw) => {
            let text = serde_json::to_string(raw.as_ref()).map_err(|error| ExplainError::Dsl {
                path: "dsl".to_owned(),
                message: error.to_string(),
            })?;
            let loaded = Migrator::v1()
                .load(&text)
                .map_err(|error| ExplainError::Dsl {
                    path: "dsl".to_owned(),
                    message: error.to_string(),
                })?;
            // The write path's strictness (r3.s2): a raw key the parsed document
            // cannot express is the caller's own typo, never a silent no-op. The
            // walk compares the MIGRATED raw value (ADR-0024's amendment).
            let migrated = Migrator::v1()
                .migrated_value(raw.as_ref().clone())
                .map_err(|error| ExplainError::Dsl {
                    path: "dsl".to_owned(),
                    message: error.to_string(),
                })?;
            check_unknown_fields(&migrated, &loaded.dsl).map_err(|errors| ExplainError::Dsl {
                path: "dsl".to_owned(),
                message: validation_text(&errors),
            })?;
            let direction = loaded.dsl.direction;
            Ok((loaded.dsl, direction))
        }
        RuleRequest::Version(version_id) => {
            let stored = strategies
                .get_version(version_id)
                .await?
                .ok_or_else(|| ExplainError::VersionNotFound(version_id.clone()))?;
            Ok((stored.dsl.clone(), stored.dsl.direction))
        }
    }
}

/// One conjunct, paired across the DSL and the compiled tree.
struct Conjunct {
    /// `entry` or `filter`.
    source: &'static str,
    /// The condition as DSL JSON.
    dsl: serde_json::Value,
    /// The compiled condition the entry gate evaluates.
    compiled: CompiledCondition,
    /// The operand leaves to report: their label, their compiled value, and
    /// whether the prior bar's value is reported too (the two crosses).
    operands: Vec<(String, CompiledValue, bool)>,
}

/// Flatten `entry ∧ filters` with top-level `And` nodes expanded — on the DSL
/// side and the compiled side alike — and pair them by position.
///
/// The two walks agree because `compile` folds the conjunction in exactly this
/// order (`compile.rs`: `And([compile(entry), compile(f0), …])`, or the bare
/// compiled entry when there are no filters) and maps `And` shape-preservingly.
fn flatten(dsl: &StrategyDsl, compiled: &CompiledStrategy) -> Result<Vec<Conjunct>, ExplainError> {
    let mut dsl_parts: Vec<(&'static str, &Condition)> = Vec::new();
    push_dsl(&dsl.entry, "entry", &mut dsl_parts);
    for filter in &dsl.filters {
        push_dsl(filter, "filter", &mut dsl_parts);
    }
    let mut compiled_parts: Vec<&CompiledCondition> = Vec::new();
    push_compiled(compiled.entry(), &mut compiled_parts);

    if dsl_parts.len() != compiled_parts.len() {
        // Unreachable while `compile`'s fold rule holds. A divergence would
        // mispair a label with a value, which is worse than a refusal.
        return Err(ExplainError::Rule(format!(
            "the rule's {} DSL conjuncts do not pair with its {} compiled ones",
            dsl_parts.len(),
            compiled_parts.len()
        )));
    }
    if dsl_parts.len() > MAX_CONJUNCTS {
        return Err(ExplainError::Field {
            path: "dsl",
            message: format!(
                "a rule may carry at most {MAX_CONJUNCTS} conjuncts, got {}",
                dsl_parts.len()
            ),
        });
    }

    let mut out = Vec::with_capacity(dsl_parts.len());
    for ((source, condition), compiled_condition) in dsl_parts.into_iter().zip(compiled_parts) {
        out.push(Conjunct {
            source,
            dsl: serde_json::to_value(condition).unwrap_or(serde_json::Value::Null),
            operands: operand_pairs(condition, compiled_condition),
            compiled: compiled_condition.clone(),
        });
    }
    Ok(out)
}

/// Expand the DSL side: a top-level `And` contributes its children, each
/// keeping the enclosing `source`.
fn push_dsl<'a>(
    condition: &'a Condition,
    source: &'static str,
    out: &mut Vec<(&'static str, &'a Condition)>,
) {
    match condition {
        Condition::And { conditions } => {
            for child in conditions {
                push_dsl(child, source, out);
            }
        }
        other => out.push((source, other)),
    }
}

/// The compiled side's mirror of [`push_dsl`].
fn push_compiled<'a>(condition: &'a CompiledCondition, out: &mut Vec<&'a CompiledCondition>) {
    match condition {
        CompiledCondition::And(children) => {
            for child in children {
                push_compiled(child, out);
            }
        }
        other => out.push(other),
    }
}

/// The validator's every field error as text — the SAME shape
/// `submit_strategy_version` reports to an agent, so one rule refused by one
/// validator reads the same on both surfaces.
fn validation_text(errors: &crate::domain::ValidationErrors) -> String {
    errors
        .errors()
        .iter()
        .map(|error| format!("{}: {} ({:?})", error.path, error.message, error.code))
        .collect::<Vec<_>>()
        .join("; ")
}

/// An operand leaf's label: the house expression vocabulary for
/// `Price`/`Const`/`Arith`, and the read path's own tokens for an indicator
/// (`rsi:14`, `h4:ema(200)`, `cdl:engulfing`) — never `render::value`'s prose,
/// which spells a candle pattern `dark cloud cover` rather than `cdl:dark_cloud_cover`.
fn operand_label(value: &ValueSource) -> String {
    use crate::application::mcp_read::indicator_label;
    fn series_prefix(series: Series) -> &'static str {
        match series {
            Series::Primary => "",
            Series::Htf => "h4:",
            Series::D1 => "d1:",
        }
    }
    match value {
        ValueSource::Constant { value } => value.normalize().to_string(),
        ValueSource::Price { series, field } => format!(
            "{}{}",
            series_prefix(*series),
            match field {
                crate::domain::PriceField::Open => "open",
                crate::domain::PriceField::High => "high",
                crate::domain::PriceField::Low => "low",
                crate::domain::PriceField::Close => "close",
                crate::domain::PriceField::Volume => "volume",
            }
        ),
        ValueSource::Indicator { series, spec } => {
            format!("{}{}", series_prefix(*series), indicator_label(spec))
        }
        ValueSource::Arith { op, lhs, rhs } => format!(
            "({} {} {})",
            operand_label(lhs),
            match op {
                crate::domain::ArithOp::Add => "+",
                crate::domain::ArithOp::Sub => "-",
                crate::domain::ArithOp::Mul => "*",
                crate::domain::ArithOp::Div => "/",
            },
            operand_label(rhs)
        ),
        ValueSource::Lag { value, bars } => format!("lag({}, {bars})", operand_label(value)),
    }
}

/// The operand leaves to report for one conjunct.
///
/// A `Compare` reports its two sides' current values; a cross reports both sides
/// on the current AND the prior bar; a `Rising`/`Falling` reports the value and
/// its lagged twin (it compiles to exactly `Compare{value, Gt|Lt,
/// lag(value, bars)}`, `compile.rs`), labelled with the existing `lag(...)`
/// spelling. A nested `Or`/`Not` reports state only — the spec's rule.
fn operand_pairs(
    dsl: &Condition,
    compiled: &CompiledCondition,
) -> Vec<(String, CompiledValue, bool)> {
    match (dsl, compiled) {
        (
            Condition::Compare { lhs, rhs, .. },
            CompiledCondition::Compare {
                lhs: cl, rhs: cr, ..
            },
        ) => vec![
            (operand_label(lhs), cl.clone(), false),
            (operand_label(rhs), cr.clone(), false),
        ],
        (
            Condition::CrossesAbove { lhs, rhs } | Condition::CrossesBelow { lhs, rhs },
            CompiledCondition::CrossesAbove {
                lhs: cl, rhs: cr, ..
            }
            | CompiledCondition::CrossesBelow {
                lhs: cl, rhs: cr, ..
            },
        ) => vec![
            (operand_label(lhs), cl.clone(), true),
            (operand_label(rhs), cr.clone(), true),
        ],
        (
            Condition::Rising { value, bars } | Condition::Falling { value, bars },
            CompiledCondition::Compare {
                lhs: cl, rhs: cr, ..
            },
        ) => vec![
            (operand_label(value), cl.clone(), false),
            (
                operand_label(&ValueSource::Lag {
                    value: value.clone(),
                    bars: *bars,
                }),
                cr.clone(),
                false,
            ),
        ],
        _ => Vec::new(),
    }
}

/// Every mark the request selects, plus the count the LIST path hid.
async fn gather<R>(
    marks: &R,
    selection: &MarkSelection,
    freeze: Option<HoldoutFreeze>,
) -> Result<(Vec<ExampleMark>, u32), ExplainError>
where
    R: ExampleMarkRepository,
{
    match selection {
        MarkSelection::Ids(ids) => {
            if ids.is_empty() {
                return Err(ExplainError::Field {
                    path: "markIds",
                    message: "markIds must name at least one mark".to_owned(),
                });
            }
            let listed =
                crate::application::example_marks::list_marks(marks, freeze, MarkFilter::default())
                    .await?;
            let hidden = listed.hidden_by_holdout.map_or(0, |hidden| hidden.count);
            // Resolved through the ONE list read, so a withdrawn mark is absent
            // by construction (Q3) and a mark a freeze covers is hidden, never
            // explained.
            let found: Vec<ExampleMark> = ids
                .iter()
                .filter_map(|id| listed.marks.iter().find(|mark| &mark.id == id).cloned())
                .collect();
            if found.is_empty() {
                return Err(ExplainError::Mark(MarkError::NotFound {
                    mark_id: ids.first().cloned().unwrap_or_default(),
                }));
            }
            Ok((found, hidden))
        }
        MarkSelection::Setup { setup, pair } => {
            let listed = crate::application::example_marks::list_marks(
                marks,
                freeze,
                MarkFilter {
                    pair: pair.clone(),
                    timeframe: None,
                    setup: Some(setup.clone()),
                },
            )
            .await?;
            let hidden = listed.hidden_by_holdout.map_or(0, |hidden| hidden.count);
            Ok((listed.marks, hidden))
        }
    }
}

/// The evaluation bar's open time for one mark (Q5): its own bar for a `15m`
/// mark, else the first `15m` bar opening at the mark candle's close. `None`
/// when the `15m` HEAD holds no such bar — the `no_evaluation_bar` answer.
///
/// The holdout start is NOT consulted here: a bar at or after it is present in
/// the raw snapshot, so the mapping still resolves and the caller applies the
/// fence to the answer.
fn evaluation_bar_ms<C>(mark: &ExampleMark, candles: &C) -> Result<Option<i64>, ExplainError>
where
    C: CandleSeriesRepository,
{
    let wanted = match mark.timeframe {
        Timeframe::M15 => mark.bar_open_ms,
        // The close is the open plus the timeframe's nominal duration — the same
        // arithmetic `guard_candle` uses.
        Timeframe::H4 | Timeframe::D1 => mark.bar_open_ms + mark.timeframe.duration_ms(),
    };
    let Some(stored) = candles.load_head(&mark.pair, Timeframe::M15)? else {
        return Ok(None);
    };
    Ok(stored
        .series
        .candles
        .iter()
        .find(|candle| candle.open_time == wanted)
        .map(|candle| candle.open_time))
}

/// The conjunct indexes held at every explained mark.
fn held_by_every_mark(explained: &[MarkExplanation]) -> Vec<usize> {
    let Some(first) = explained.first() else {
        return Vec::new();
    };
    first
        .conjuncts
        .iter()
        .filter(|conjunct| {
            conjunct.state == "held"
                && explained.iter().all(|mark| {
                    mark.conjuncts
                        .get(conjunct.index)
                        .is_some_and(|other| other.state == "held")
                })
        })
        .map(|conjunct| conjunct.index)
        .collect()
}

/// One pair's whole pass: the cut series, the backtest's own session, and the
/// per-mark answers read out of ONE stepping run.
struct PairPass {
    /// Per mark id: the evaluation bar's state/operand reading.
    bars: BTreeMap<String, BarReading>,
    /// Per mark id: its own candle's 17 pattern values.
    patterns: BTreeMap<String, Vec<PatternReport>>,
}

/// What one evaluation bar's probe answered.
struct BarReading {
    /// One state per conjunct, flattened in the request's order. `not_warm` for
    /// every conjunct is the gate's cold answer, so the warm clause needs no
    /// separate field: the states carry it.
    states: Vec<&'static str>,
    /// One operand list per conjunct, flattened in the request's order.
    operands: Vec<Vec<OperandReport>>,
}

impl PairPass {
    #[allow(clippy::too_many_arguments)]
    fn build<C, X>(
        pair: &Pair,
        compiled: &CompiledStrategy,
        candles: &C,
        exchange: &X,
        freeze: Option<HoldoutFreeze>,
        conjuncts: &[Conjunct],
        marks: &[ExampleMark],
        evaluation: &BTreeMap<String, i64>,
    ) -> Result<Self, ExplainError>
    where
        C: CandleSeriesRepository,
        X: ExchangeAdapter,
    {
        // Each series is cut at the holdout start BEFORE any engine steps (G4).
        let (primary, htf, d1) = load_series(pair, compiled, candles, freeze)?;
        let filters: SymbolFilters = exchange
            .symbol_filters(pair)
            .map_err(|error| ExplainError::Rule(error.to_string()))?;
        let timeframes = SessionTimeframes {
            primary: Timeframe::M15,
            htf: htf.as_ref().map(|series| series.timeframe),
            d1: d1.as_ref().map(|series| series.timeframe),
        };

        // ---- PASS 1: the entry-gate truth at each mark's evaluation bar ----
        let wanted: Vec<(String, i64)> = marks
            .iter()
            .filter_map(|mark| evaluation.get(&mark.id).map(|ms| (mark.id.clone(), *ms)))
            .collect();
        let specs: Vec<(String, CompiledValue, bool)> = conjuncts
            .iter()
            .flat_map(|conjunct| conjunct.operands.iter().cloned())
            .collect();
        let mut bars: BTreeMap<String, BarReading> = BTreeMap::new();
        build_and_step(
            compiled,
            pair,
            timeframes,
            &filters,
            &primary,
            htf.as_ref(),
            d1.as_ref(),
            |open_ms, probe| {
                let Some((mark_id, _)) = wanted.iter().find(|(_, eval_ms)| *eval_ms == open_ms)
                else {
                    return;
                };
                bars.insert(
                    mark_id.clone(),
                    read_bar(probe, conjuncts, &wanted, &specs, open_ms),
                );
            },
        )?;

        // ---- PASS 2: the 17 pattern values at each mark's own candle ----
        let pattern_specs: Vec<IndicatorSpec> = CANDLE_PATTERNS
            .iter()
            .map(|(pattern, _)| IndicatorSpec::CandlePattern { pattern: *pattern })
            .collect();
        // The marks' own candles, grouped by the timeframe they were marked on.
        let mut by_timeframe: BTreeMap<Timeframe, Vec<(String, i64)>> = BTreeMap::new();
        for mark in marks {
            by_timeframe
                .entry(mark.timeframe)
                .or_default()
                .push((mark.id.clone(), mark.bar_open_ms));
        }
        let mut patterns: BTreeMap<String, Vec<PatternReport>> = BTreeMap::new();
        for (timeframe, wanted_bars) in by_timeframe {
            // The mark's own snapshot, cut at the holdout start, and the 17
            // values read by the ADAPTER's own helper — so this ring names no
            // adapter type (ADR-0015's one-import rule).
            let series = load_mark_snapshot(candles, pair, timeframe, freeze)?;
            let wanted_opens: Vec<i64> = wanted_bars.iter().map(|(_, open_ms)| *open_ms).collect();
            let values = indicator_values_at(&pattern_specs, &series, &wanted_opens)
                .map_err(ExplainError::Rule)?;
            let by_open: BTreeMap<i64, Vec<PatternReport>> = values
                .into_iter()
                .map(|(open_ms, values)| {
                    let reports = values
                        .into_iter()
                        .zip(CANDLE_PATTERNS.iter())
                        .map(|(value, (_, snake))| PatternReport {
                            name: format!("cdl:{snake}"),
                            value: value.map(|value| value.normalize().to_string()),
                        })
                        .collect();
                    (open_ms, reports)
                })
                .collect();
            for (mark_id, open_ms) in &wanted_bars {
                if let Some(reports) = by_open.get(open_ms) {
                    patterns.insert(mark_id.clone(), reports.clone());
                }
            }
        }

        Ok(Self { bars, patterns })
    }

    /// Read one mark out of the pass.
    fn explain(
        &self,
        mark: &ExampleMark,
        evaluation_bar_ms: i64,
        conjuncts: &[Conjunct],
        direction: Direction,
    ) -> Result<MarkExplanation, &'static str> {
        let Some(reading) = self.bars.get(&mark.id) else {
            return Err("no_evaluation_bar");
        };
        let patterns = self
            .patterns
            .get(&mark.id)
            .cloned()
            .ok_or("no_mark_candle")?;

        let reports = conjuncts
            .iter()
            .enumerate()
            .map(|(index, conjunct)| ConjunctReport {
                index,
                source: conjunct.source,
                condition: conjunct.dsl.clone(),
                state: reading.states.get(index).copied().unwrap_or("not_warm"),
                operands: reading.operands.get(index).cloned().unwrap_or_default(),
            })
            .collect::<Vec<_>>();

        let side_matches = matches!(
            (direction, mark.side),
            (Direction::Long, MarkSide::Long) | (Direction::Short, MarkSide::Short)
        );
        let all_held = side_matches && reports.iter().all(|report| report.state == "held");
        Ok(MarkExplanation {
            mark: MarkJson::of(mark),
            evaluation_bar: rfc3339_ms(evaluation_bar_ms),
            side_matches,
            conjuncts: reports,
            all_held,
            patterns,
        })
    }
}

/// Read one bar's conjunct states and operand values out of the probe.
///
/// Nothing is recorded but the marks' own bars, so the pass's memory is O(marks)
/// and not O(bars).
fn read_bar(
    probe: &EvalProbe<'_>,
    conjuncts: &[Conjunct],
    _wanted: &[(String, i64)],
    specs: &[(String, CompiledValue, bool)],
    _open_ms: i64,
) -> BarReading {
    let warm = probe.is_warm();
    let states: Vec<&'static str> = if warm {
        conjuncts
            .iter()
            .map(|conjunct| {
                if probe.eval(&conjunct.compiled) {
                    "held"
                } else {
                    "not_held"
                }
            })
            .collect()
    } else {
        conjuncts.iter().map(|_| "not_warm").collect()
    };
    // The flat operand list, split back per conjunct in the same order it was
    // pushed. Values are read only when the gate is warm: before warm-up a
    // partial history would report numbers the gate never saw.
    let mut flat: Vec<OperandReport> = Vec::new();
    if warm {
        for (label, value, previous) in specs {
            flat.push(OperandReport {
                label: label.clone(),
                value: probe
                    .operand(value, false)
                    .map(|d| d.normalize().to_string()),
                previous: previous.then(|| {
                    probe
                        .operand(value, true)
                        .map(|d| d.normalize().to_string())
                }),
            });
        }
    }
    let mut cursor = 0_usize;
    let operands = conjuncts
        .iter()
        .map(|conjunct| {
            let count = conjunct.operands.len();
            let slice = if warm {
                flat.get(cursor..cursor + count)
                    .map(<[OperandReport]>::to_vec)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            cursor += count;
            slice
        })
        .collect();
    BarReading { states, operands }
}

/// Build a session and step the whole cut series once, calling `on_bar` at each
/// stepped bar. The closure receives the read-only probe, so no caller can step.
#[allow(clippy::too_many_arguments)]
fn build_and_step(
    compiled: &CompiledStrategy,
    pair: &Pair,
    timeframes: SessionTimeframes,
    filters: &SymbolFilters,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    d1: Option<&CandleSeries>,
    mut on_bar: impl FnMut(i64, &EvalProbe<'_>),
) -> Result<(), ExplainError> {
    let mut session = EngineSession::new(
        compiled,
        pair,
        timeframes,
        BacktestConfig::default(),
        filters.clone(),
        // Unwindowed: the run shape whose gate truth G3 names. Every bar counts,
        // exactly as an unwindowed backtest's does.
        None,
    )
    .map_err(|error| ExplainError::Rule(error.to_string()))?;
    step_series(&mut session, primary, htf, d1, |open_ms, probe| {
        on_bar(open_ms, probe);
    })
    .map_err(|error| ExplainError::Rule(error.to_string()))
}

/// Load a pair's three series, each cut at the holdout start before any step.
fn load_series<C>(
    pair: &Pair,
    compiled: &CompiledStrategy,
    candles: &C,
    freeze: Option<HoldoutFreeze>,
) -> Result<(CandleSeries, Option<CandleSeries>, Option<CandleSeries>), ExplainError>
where
    C: CandleSeriesRepository,
{
    let primary = required_series(candles, pair, Timeframe::M15, freeze, "15m")?;
    // A rule that reads a higher series needs it to exist: the backtest refuses
    // (`HtfRequired`/`D1Required`) rather than silently warming on nothing, and a
    // silent never-warm would report `not_warm` for every conjunct — a claimed
    // answer with no truth behind it.
    let htf = if compiled.needs_htf() {
        Some(required_series(candles, pair, Timeframe::H4, freeze, "4h")?)
    } else {
        optional_series(candles, pair, Timeframe::H4, freeze)?
    };
    let d1 = if compiled.needs_d1() {
        Some(required_series(candles, pair, Timeframe::D1, freeze, "1d")?)
    } else {
        optional_series(candles, pair, Timeframe::D1, freeze)?
    };
    Ok((primary, htf, d1))
}

/// One (pair, timeframe) HEAD, cut at the holdout start.
fn optional_series<C>(
    candles: &C,
    pair: &Pair,
    timeframe: Timeframe,
    freeze: Option<HoldoutFreeze>,
) -> Result<Option<CandleSeries>, ExplainError>
where
    C: CandleSeriesRepository,
{
    let Some(stored) = candles.load_head(pair, timeframe)? else {
        return Ok(None);
    };
    let mut series = stored.series;
    cut(&mut series, freeze);
    Ok(Some(series))
}

/// One (pair, timeframe) HEAD that the rule requires.
fn required_series<C>(
    candles: &C,
    pair: &Pair,
    timeframe: Timeframe,
    freeze: Option<HoldoutFreeze>,
    label: &str,
) -> Result<CandleSeries, ExplainError>
where
    C: CandleSeriesRepository,
{
    optional_series(candles, pair, timeframe, freeze)?.ok_or_else(|| {
        ExplainError::Rule(format!(
            "the rule reads the {label} series, and no {pair} {label} snapshot exists to \
             explain it against"
        ))
    })
}

/// The mark's own recorded snapshot, cut at the holdout start.
///
/// A mark records `data_version` — the snapshot whose candle it named — so the
/// canonical read is that exact version. When the store no longer resolves that
/// tag (a pruned snapshot), the HEAD of the same `(pair, timeframe)` is the
/// honest second choice: it is the data the store serves now, and the mark's own
/// candle is still required to be in it (a missing candle is `no_mark_candle`).
fn load_mark_snapshot<C>(
    candles: &C,
    pair: &Pair,
    timeframe: Timeframe,
    freeze: Option<HoldoutFreeze>,
) -> Result<CandleSeries, ExplainError>
where
    C: CandleSeriesRepository,
{
    optional_series(candles, pair, timeframe, freeze)?
        .ok_or_else(|| ExplainError::Rule(format!("no {pair} {timeframe:?} snapshot exists")))
        .map(|mut series| {
            cut(&mut series, freeze);
            series
        })
}

/// Cut a series at the holdout start — the same half-open rule the MCP reads
/// use (`withhold_holdout_candles`), applied before any engine steps.
fn cut(series: &mut CandleSeries, freeze: Option<HoldoutFreeze>) {
    let Some(freeze) = freeze else { return };
    series
        .candles
        .retain(|candle| candle.open_time < freeze.holdout_start_ms);
}
