//! The certification-record read (r4.s1.w5, spec A4/C5) — the **app's** view.
//!
//! `certify_version` is an `agent`-scope tool and its answer carries no holdout
//! number (grill Q5). The record itself carries all of them, and this is the
//! surface that shows it: one `app`-scope read of a version's certification
//! records, in full, rendered in the existing version detail view as one row
//! per record.
//!
//! **Additive by design (ADR-0020).** The read is a NEW plain route, a NEW bus
//! command and a NEW client method; not one existing Tauri signature changes.
//!
//! Timestamps cross as RFC 3339 millisecond text (a holdout's end is a candle's
//! `close_time`, `…:59.999` — seconds precision would truncate a real value),
//! decimals as exact strings (NFR-2), and the holdout's `z`/bound as the two
//! genuinely `f64` values the C1 test computes.

use serde::{Deserialize, Serialize};

use crate::application::walk_forward_read::rfc3339_ms;
use crate::domain::CertificationRepository;
use crate::domain::certification::{CertificationInputs, CertificationRecord};
use crate::domain::strategy::VersionId;

use super::commands::DesktopState;
use super::error::BusError;

/// What `certification_records` is asked for: one strategy version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CertificationRecordsRequest {
    /// The version whose certification records to read.
    pub version_id: String,
}

/// One side's recorded `(timeframe, data_version)` selections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CertificationSelectionDto {
    /// The candle interval, e.g. `M15`.
    pub timeframe: String,
    /// The exact immutable snapshot identity (`ADR-0009`'s content hash).
    pub data_version: String,
}

/// One certification record in full — everything the immutable row carries,
/// the holdout's numbers included (the `agent` surface sees none of them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CertificationRecordDto {
    /// The record's id.
    pub id: String,
    /// The certified version.
    pub version_id: String,
    /// The freeze the call ran under.
    pub freeze_id: String,
    /// The hypothesis's position under that freeze (`1..=H`).
    pub hypothesis_index: u32,
    /// The search rule's persisted name.
    pub rule: String,
    /// The pair both halves evaluated.
    pub pair: String,
    /// The persisted search-span walk-forward run.
    pub search_walk_forward_run_id: String,
    /// Whether the search-span `wf-v2` verdict passed.
    pub search_pass: bool,
    /// The holdout window's inclusive start, RFC 3339 ms.
    pub holdout_start: String,
    /// The holdout window's exclusive end, RFC 3339 ms (C5).
    pub holdout_end: String,
    /// The holdout's trade count (C5).
    pub holdout_n: u32,
    /// The holdout's mean expectancy in R, exact decimal string.
    pub holdout_mean_r: String,
    /// The C1 test's quantile at this freeze's H.
    pub holdout_z: f64,
    /// The one-sided lower bound on the holdout expectancy.
    pub holdout_lower_bound: f64,
    /// Whether the C1 holdout test passed.
    pub holdout_passes: bool,
    /// `search_pass AND holdout_passes`.
    pub certified: bool,
    /// The search span's data versions, per timeframe.
    pub search_data_versions: Vec<CertificationSelectionDto>,
    /// The holdout's data versions, per timeframe.
    pub holdout_data_versions: Vec<CertificationSelectionDto>,
    /// The search run's engine fingerprint.
    pub engine_fingerprint: String,
    /// When the record was written, RFC 3339 ms.
    pub created_at: String,
    /// The calling client's label.
    pub called_by: String,
}

/// What `certification_records` answers: one version's records, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CertificationRecordsDto {
    /// The version the records belong to.
    pub version_id: String,
    /// The records, newest (highest `hypothesis_index`) first.
    pub records: Vec<CertificationRecordDto>,
}

/// One side's selections, in primary → htf → d1 order.
fn selections(inputs: &CertificationInputs) -> Vec<CertificationSelectionDto> {
    let mut out = Vec::with_capacity(3);
    let mut push = |selection: &crate::domain::SnapshotSelection| {
        out.push(CertificationSelectionDto {
            timeframe: selection.timeframe.binance_interval().to_owned(),
            data_version: selection.data_version.as_str().to_owned(),
        });
    };
    push(&inputs.primary);
    if let Some(htf) = &inputs.htf {
        push(htf);
    }
    if let Some(d1) = &inputs.d1 {
        push(d1);
    }
    out
}

/// One record → its wire shape.
fn record_dto(record: &CertificationRecord) -> CertificationRecordDto {
    CertificationRecordDto {
        id: record.id.clone(),
        version_id: record.version_id.as_str().to_owned(),
        freeze_id: record.freeze_id.clone(),
        hypothesis_index: record.hypothesis_index,
        rule: record.rule.clone(),
        pair: record.pair.as_str().to_owned(),
        search_walk_forward_run_id: record.search_walk_forward_run_id.as_str().to_owned(),
        search_pass: record.search_pass,
        holdout_start: rfc3339_ms(record.holdout_start_ms),
        holdout_end: rfc3339_ms(record.holdout_end_ms),
        holdout_n: u32::try_from(record.holdout_n).unwrap_or(u32::MAX),
        holdout_mean_r: record.holdout_mean_r.to_string(),
        holdout_z: record.holdout_z,
        holdout_lower_bound: record.holdout_lower_bound,
        holdout_passes: record.holdout_passes,
        certified: record.certified,
        search_data_versions: selections(&record.search_inputs),
        holdout_data_versions: selections(&record.holdout_inputs),
        engine_fingerprint: record.engine_fingerprint.clone(),
        created_at: record.created_at.clone(),
        called_by: record.called_by.clone(),
    }
}

/// `certification_records`' transport-free core: one version id in, its
/// certification records out, newest first. An empty list is a real answer —
/// a version that has never been certified has no records, which is not an
/// error and must not read as one.
///
/// # Errors
///
/// Returns a [`BusError`] when the store read fails.
pub async fn certification_records_core(
    state: &DesktopState,
    request: CertificationRecordsRequest,
) -> Result<CertificationRecordsDto, BusError> {
    let records = state
        .certification_repo()
        .list_for_version(&VersionId::new(&request.version_id))
        .await?;
    Ok(CertificationRecordsDto {
        version_id: request.version_id,
        records: records.iter().map(record_dto).collect(),
    })
}
