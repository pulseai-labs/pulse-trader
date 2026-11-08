//! The Library's power-review read (r5.s5.w3, PDR-0040, ADR-0032) — the **app's** view.
//!
//! Before a certify attempt spends one of an open freeze's hypotheses, the operator sees what C1
//! (ADR-0028) is worth on a candidate: the holdout trade **count** it would test and the chance C1 would
//! pass a true `+0.25R` edge at that count, against the bar fixed at plan time. `pulse certify power`
//! (r5.s5.w1) writes one immutable `power_review` row per read; this is the surface that shows them.
//!
//! **Count only (SPINE G1).** The DTO crosses the count, the pass chance, the bar and the decision — and
//! nothing about how the holdout's trades went: no `z`, no window, no pinned inputs, no σ, no assumed edge,
//! no fingerprint, no holdout verdict number. The record's field list is the contract, and the suite asserts
//! the wire key set mechanically. The search span's wf-v2 verdict crosses as well (r6.s5.w2, ADR-0032's
//! addendum): it is search-span data, read from the review's stored search walk-forward.
//!
//! **App scope only (SPINE G6).** The route is mounted through `mount_post_json`, so an `agent` token is a
//! 403 `scope_refused` with an audit row. There is no power read over MCP, and this module names none.
//!
//! **Additive by design (ADR-0020).** One new plain route, one new bus command, one new client method; not
//! one existing Tauri signature changes.

use serde::{Deserialize, Serialize};

use crate::application::power_review::search_span_verdict;
use crate::domain::PowerReviewRepository;
use crate::domain::power_review::{PowerReviewRecord, SearchSpanVerdict};
use crate::domain::strategy::VersionId;

use super::commands::DesktopState;
use super::error::{BusError, BusErrorCode};

/// What `power_reviews` is asked for: one strategy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct PowerReviewsRequest {
    /// The version whose power reviews to read.
    pub version_id: String,
}

/// The search span's wf-v2 verdict as the Library shows it: the rule, the fold count, the folds holding of the
/// folds required, and the pooled trade count, mean R, lower bound and pass. Search-span data only (G1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SearchSpanVerdictDto {
    /// The verdict rule's name (`"wf-v2"`).
    pub rule: String,
    /// The fold count.
    pub k: u32,
    /// How many folds held.
    pub folds_holding: u32,
    /// How many folds the rule requires to hold.
    pub folds_required: u32,
    /// The pooled out-of-sample trade count.
    pub pooled_n: u32,
    /// The pooled mean R, as a decimal string.
    pub pooled_mean_r: String,
    /// The pooled lower bound, in R.
    pub pooled_lower_bound: f64,
    /// Whether the search span's verdict passed.
    pub pass: bool,
}

impl From<&SearchSpanVerdict> for SearchSpanVerdictDto {
    fn from(verdict: &SearchSpanVerdict) -> Self {
        Self {
            rule: verdict.rule.clone(),
            k: u32::from(verdict.k),
            folds_holding: u32::from(verdict.folds_holding),
            folds_required: u32::from(verdict.folds_required),
            pooled_n: u32::try_from(verdict.pooled_n).unwrap_or(u32::MAX),
            pooled_mean_r: verdict.pooled_mean_r.clone(),
            pooled_lower_bound: verdict.pooled_lower_bound,
            pass: verdict.pass,
        }
    }
}

/// One power review as the Library shows it: the count, the pass chance, the bar, the decision and the search
/// span's wf-v2 verdict — no holdout outcome (G1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct PowerReviewDto {
    /// The record's id.
    pub id: String,
    /// The version the review is about.
    pub version_id: String,
    /// The freeze the review ran under.
    pub freeze_id: String,
    /// The pair the count was taken on.
    pub pair: String,
    /// The freeze's hypothesis budget `H` (which fixes `z`).
    pub h: u32,
    /// The holdout's trade count — the one thing this read reports about the holdout.
    pub holdout_n: u32,
    /// The chance C1 would pass a true `+0.25R` edge at this count.
    pub pass_chance: f64,
    /// The bar the chance was judged against, as stored (`"0.50"`).
    pub bar: String,
    /// The trade-count floor the decision applied, as stored.
    pub min_holdout_trades: u32,
    /// Whether a certify attempt is allowed.
    pub allowed: bool,
    /// The first failing reason's stored spelling, or `None` when allowed.
    pub reason: Option<String>,
    /// The wf-v2 verdict of the search span the review ran — search-span data, never the holdout's.
    pub search: SearchSpanVerdictDto,
    /// When the review was written, RFC 3339 ms.
    pub created_at: String,
    /// The calling client's label.
    pub called_by: String,
}

/// What `power_reviews` answers: the latest review per pair under the open freeze, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct PowerReviewsDto {
    /// The version the reviews belong to.
    pub version_id: String,
    /// The freeze currently open, or `None` when none is.
    pub open_freeze_id: Option<String>,
    /// One row per pair, newest first; empty when the open freeze has no review for this version.
    pub reviews: Vec<PowerReviewDto>,
}

/// One record → its wire shape. **Only the decision's own fields cross** (G1): the record's `z`, window,
/// inputs, σ, assumed edge and fingerprint stay on the stored row. The search span's wf-v2 verdict crosses as
/// well, because it is search-span data (ADR-0032).
fn review_dto(record: &PowerReviewRecord, search: &SearchSpanVerdict) -> PowerReviewDto {
    PowerReviewDto {
        id: record.id.clone(),
        version_id: record.version_id.as_str().to_owned(),
        freeze_id: record.freeze_id.clone(),
        pair: record.pair.as_str().to_owned(),
        h: u32::from(record.h),
        holdout_n: u32::try_from(record.holdout_n).unwrap_or(u32::MAX),
        pass_chance: record.pass_chance,
        bar: record.bar.clone(),
        min_holdout_trades: u32::try_from(record.min_holdout_trades).unwrap_or(u32::MAX),
        allowed: record.allowed,
        reason: record.reason.clone(),
        search: SearchSpanVerdictDto::from(search),
        created_at: record.created_at.clone(),
        called_by: record.called_by.clone(),
    }
}

/// `power_reviews`' transport-free core: one version id in, the latest review per pair under the open
/// freeze out. With no open freeze the answer is an empty list and nothing else is read — a closed freeze's
/// rows are not this read's to show. A version with no review answers an empty list, which is a real
/// answer and must not read as an error.
///
/// # Errors
///
/// Returns a [`BusError`] when the freeze table or the review store cannot be read.
pub async fn power_reviews_core(
    state: &DesktopState,
    request: PowerReviewsRequest,
) -> Result<PowerReviewsDto, BusError> {
    let open = state
        .freeze_repo()
        .open_freeze()
        .await
        .map_err(BusError::from)?;
    let Some(freeze) = open else {
        return Ok(PowerReviewsDto {
            version_id: request.version_id,
            open_freeze_id: None,
            reviews: Vec::new(),
        });
    };

    let records = state
        .power_review_repo()
        .list_for_version(&VersionId::new(&request.version_id))
        .await?;

    // The list is newest first — insertion order (`rowid DESC`), never `created_at`, which can tie at one
    // millisecond or step backwards — so the first record seen for a pair is that pair's latest under this
    // freeze. A `HashSet` of pairs keeps the filter one pass.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let runs = state.backtest_run_repo();
    let mut reviews = Vec::new();
    for record in records
        .iter()
        .filter(|record| record.freeze_id == freeze.id)
        .filter(|record| seen.insert(record.pair.as_str()))
    {
        let search = search_span_verdict(&runs, record)
            .await
            .map_err(|e| BusError::new(BusErrorCode::Data, e.to_string()))?;
        reviews.push(review_dto(record, &search));
    }

    Ok(PowerReviewsDto {
        version_id: request.version_id,
        open_freeze_id: Some(freeze.id),
        reviews,
    })
}
