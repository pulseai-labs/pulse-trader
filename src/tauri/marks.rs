//! The example-trade mark contract (r5.s3.w1, PDR-0037, ADR-0031) — the app's
//! view of the marks a trader or an agent pointed at a candle: write one, list
//! them, withdraw one.
//!
//! **One use case, three transports.** Every core here is a thin projection over
//! `application::example_marks`, which is the ONE code that writes
//! `example_mark` (SPINE G1); the app route, its bus command and the `pulse mcp`
//! tools all reach the same call, so a refusal is the same refusal everywhere.
//!
//! **The writer is never a body member.** A trader mark is named by the app
//! token's LABEL — read off the authenticated request by the route, never sent by
//! the caller — and an agent mark by the identity the MCP session resolved. The
//! DTO says which, and marks an agent's name as self-reported (PDR-0015).
//!
//! **The open freeze is read at the edge, in the same call.** Each core reads it
//! once, immediately before the use case, and passes it in; the fence is then the
//! use case's first check, before any candle is loaded (G2). No caller passes a
//! hard-coded `None`.
//!
//! **Wire shape.** camelCase; times cross as millisecond integer strings, counts
//! as `u32` (specta refuses `i64`/`u64`). The refusal codes the routes answer with
//! are the use case's own ([`crate::domain::MarkError::code`]): `holdout`,
//! `candle_not_found`, `not_found`, `already_withdrawn`, `not_writer`, or
//! `validation` with the field named in the message.
//!
//! **Additive by design (ADR-0020).** Three new plain routes, three new bus
//! commands and three new client methods; not one existing Tauri signature changes.

use serde::{Deserialize, Serialize};

use crate::application::example_marks::{MarkRequest, list_marks, write_mark};
use crate::domain::{
    DataError, ExampleMark, HoldoutFreeze, MarkError, MarkFilter, MarkWriter, MarkWriterKind, Pair,
    Timeframe,
};
use crate::tauri::backtest::ms;

use super::commands::DesktopState;

/// The open freeze, read ONCE per call at the delivery edge and handed straight
/// to the use case, whose first check it becomes (G2).
///
/// A read failure is an ERROR, never a silent `None`: a freeze table that cannot
/// be read must not quietly switch the fence off. Every mark core calls this — no
/// edge passes a hard-coded `None`.
async fn edge_holdout(state: &DesktopState) -> Result<Option<HoldoutFreeze>, MarkError> {
    state.open_holdout().await.map_err(|error| {
        MarkError::Data(DataError::Db(format!(
            "the open holdout freeze could not be read: {error}"
        )))
    })
}

/// A mark's writer, as the frontend shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct MarkWriterDto {
    /// `trader` (the app, named by its token's label) or `agent` (an MCP client).
    pub kind: String,
    /// The writer's name as stored: an app token's LABEL, or the agent's own name.
    pub name: String,
    /// Whether the name is the writer's own claim about itself (PDR-0015) — true
    /// for an agent, false for the token-labelled trader.
    pub self_reported: bool,
}

/// One example-trade mark, as the app reads it.
///
/// Times cross as millisecond integer strings. There is deliberately no stop, no
/// target and no "what happened next": a mark is a reference trade, never an order
/// (PDR-0037 Q1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct MarkDto {
    /// The mark's id.
    pub mark_id: String,
    /// The pair the marked candle belongs to.
    pub pair: String,
    /// The `Binance` interval text (`15m` / `4h` / `1d`).
    pub timeframe: String,
    /// The marked candle's open time, epoch ms as an exact integer string.
    pub bar_time: String,
    /// `long` or `short`.
    pub side: String,
    /// The setup label the writer gave it.
    pub setup: String,
    /// The writer's note, or `null`.
    pub note: Option<String>,
    /// Who wrote it.
    pub writer: MarkWriterDto,
    /// The HEAD snapshot the candle was found in when the mark was written.
    pub data_version: String,
    /// When the mark landed, epoch ms as an exact integer string.
    pub created_at: String,
}

/// What an open freeze hid from one list read (Q4, PDR-0007).
///
/// The hidden marks are still stored — hiding is a read-side act — so the count is
/// the honest report of what the reader is not being shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct MarkHoldoutDto {
    /// The open freeze's holdout start, epoch ms as an exact integer string.
    pub holdout_start_ms: String,
    /// How many marks this read hid because their candle reaches it.
    pub count: u32,
}

/// The list's answer: the marks a reader may act on, and what the fence hid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct MarkListDto {
    /// The marks that stand, in candle order.
    pub marks: Vec<MarkDto>,
    /// The fence's count, or `null` while no freeze is open.
    pub hidden_by_holdout: Option<MarkHoldoutDto>,
}

/// What the app asks `POST /api/v1/marks` to write. The writer is NOT here: it
/// comes from the authenticated token (ADR-0026).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WriteMarkRequest {
    /// The pair the marked candle belongs to (e.g. `BTCUSDT`).
    pub pair: String,
    /// The `Binance` interval text (`15m` / `4h` / `1d`).
    pub timeframe: String,
    /// The marked candle's open time, epoch ms as an exact integer string. It
    /// must be a candle the HEAD snapshot actually holds.
    pub bar_time: String,
    /// `long` or `short`.
    pub side: String,
    /// The setup label (1–64 characters).
    pub setup: String,
    /// The optional note (at most 280 characters).
    #[serde(default)]
    pub note: Option<String>,
}

/// What the app asks `POST /api/v1/marks/list` for; every member narrows only
/// when it is given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ListMarksRequest {
    /// Only this pair.
    #[serde(default)]
    pub pair: Option<String>,
    /// Only this timeframe.
    #[serde(default)]
    pub timeframe: Option<String>,
    /// Only this setup label.
    #[serde(default)]
    pub setup: Option<String>,
}

/// What the app asks `POST /api/v1/marks/withdraw` to withdraw. The actor is NOT
/// here: it comes from the authenticated token (ADR-0026).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WithdrawMarkRequest {
    /// The mark to withdraw.
    pub mark_id: String,
}

/// One stored mark as the wire's DTO.
fn mark_dto(mark: &ExampleMark) -> MarkDto {
    MarkDto {
        mark_id: mark.id.clone(),
        pair: mark.pair.to_string(),
        timeframe: mark.timeframe.binance_interval().to_owned(),
        bar_time: ms(mark.bar_open_ms),
        side: mark.side.as_str().to_owned(),
        setup: mark.setup.as_str().to_owned(),
        note: mark.note.as_ref().map(|note| note.as_str().to_owned()),
        writer: MarkWriterDto {
            kind: mark.writer.kind.as_str().to_owned(),
            name: mark.writer.name.clone(),
            self_reported: mark.writer.kind.is_self_reported(),
        },
        data_version: mark.data_version.clone(),
        created_at: ms(mark.created_at_ms),
    }
}

/// The `Binance` interval text as a [`Timeframe`], for the FILTER's members — the
/// written mark's own timeframe is parsed by the use case, which is where the rule
/// lives.
fn filter_timeframe(raw: &str) -> Result<Timeframe, MarkError> {
    match raw {
        "15m" => Ok(Timeframe::M15),
        "4h" => Ok(Timeframe::H4),
        "1d" => Ok(Timeframe::D1),
        other => Err(MarkError::field(
            "timeframe",
            format!("timeframe must be `15m`, `4h` or `1d`, got `{other}`"),
        )),
    }
}

/// The wire's millisecond string as an `i64`.
fn parse_ms(raw: &str) -> Result<i64, MarkError> {
    raw.trim().parse::<i64>().map_err(|e| {
        MarkError::field(
            "barTime",
            format!("barTime must be an epoch-millisecond integer: {e}"),
        )
    })
}

/// `write_mark`'s transport-free core: read the open freeze, then write through
/// the ONE use case.
///
/// The `writer` is the delivery edge's own answer — the token's label on the app
/// route, the resolved identity on MCP — and never a request member.
///
/// # Errors
///
/// [`MarkError`], exactly as `application::example_marks::write_mark` returns it;
/// the route maps [`MarkError::code`] to the HTTP token.
pub async fn write_mark_core(
    state: &DesktopState,
    writer: MarkWriter,
    request: WriteMarkRequest,
) -> Result<MarkDto, MarkError> {
    let freeze = edge_holdout(state).await?;
    let marks = state.example_mark_repo();
    let candles = state.candles();
    let mark = write_mark(
        &marks,
        &candles,
        freeze,
        writer,
        MarkRequest {
            pair: request.pair,
            timeframe: request.timeframe,
            bar_open_ms: parse_ms(&request.bar_time)?,
            side: request.side,
            setup: request.setup,
            note: request.note,
        },
    )
    .await?;
    Ok(mark_dto(&mark))
}

/// `list_marks`'s transport-free core.
///
/// # Errors
///
/// [`MarkError::Field`] for an unreadable filter member; [`MarkError::Data`] on a
/// store failure.
pub async fn list_marks_core(
    state: &DesktopState,
    request: ListMarksRequest,
) -> Result<MarkListDto, MarkError> {
    let freeze = edge_holdout(state).await?;
    let pair = request
        .pair
        .map(|pair| Pair::parse(pair).map_err(|e| MarkError::field("pair", e.to_string())))
        .transpose()?;
    let timeframe = request
        .timeframe
        .map(|timeframe| filter_timeframe(&timeframe))
        .transpose()?;
    let marks = state.example_mark_repo();
    let outcome = list_marks(
        &marks,
        freeze,
        MarkFilter {
            pair,
            timeframe,
            setup: request.setup,
        },
    )
    .await?;
    Ok(MarkListDto {
        marks: outcome.marks.iter().map(mark_dto).collect(),
        hidden_by_holdout: outcome.hidden_by_holdout.map(|hidden| MarkHoldoutDto {
            holdout_start_ms: ms(hidden.holdout_start_ms),
            count: hidden.count,
        }),
    })
}

/// `withdraw_mark`'s transport-free core, with the actor the edge resolved.
///
/// # Errors
///
/// [`MarkError`], exactly as `application::example_marks::withdraw_mark` returns
/// it.
pub async fn withdraw_mark_core(
    state: &DesktopState,
    actor: MarkWriter,
    request: WithdrawMarkRequest,
) -> Result<MarkDto, MarkError> {
    let marks = state.example_mark_repo();
    let mark =
        crate::application::example_marks::withdraw_mark(&marks, actor, &request.mark_id).await?;
    Ok(mark_dto(&mark))
}

/// The writer an app token writes as: kind `trader`, name = the token's LABEL.
///
/// An empty label cannot name a mark, and the route answers it as the 500
/// `internal` the promote route already answers with — there is nothing the
/// caller could have sent differently.
#[must_use]
pub fn trader_writer(label: &str) -> MarkWriter {
    MarkWriter {
        kind: MarkWriterKind::Trader,
        name: label.to_owned(),
    }
}
