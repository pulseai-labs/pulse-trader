//! The example-trade mark use case (r5.s3.w1, PDR-0037, ADR-0031) — the ONE code
//! that writes `example_mark`, for every writer (SPINE G1).
//!
//! Three calls, one door:
//!
//! - [`write_mark`] — parse and validate, **then the holdout fence**, then the
//!   candle lookup, then the insert. That order is the whole of G2: a time at or
//!   after the holdout start is refused for the holdout reason even where no
//!   candle exists, because the fence answers before anything is loaded, and a
//!   refused write writes nothing at all.
//! - [`list_marks`] — the marks a reader may still act on: not withdrawn, and not
//!   covered by a freeze that opened after they were written. What the fence
//!   hides is counted, never silently dropped (Q4, PDR-0011).
//! - [`withdraw_mark`] — the one-way door, with Q3's authority rule: a trader may
//!   withdraw any mark, an agent only the mark it wrote. The same fence applies,
//!   but **after** the authority checks and not before (#440, PDR-0039).
//!
//! **The fence is passed in, and the edge reads it.** Every caller reads the open
//! freeze once, inside the call it serves, and hands the guard
//! `Option<HoldoutFreeze>` — the shape `guard_backtest_window` and
//! `chart_data_core` already use, so the application ring stays generic over its
//! ports and no second reader of the freeze table exists. A read failure is the
//! edge's error and the write writes nothing; an edge never passes a hard-coded
//! `None`, which would be a fence switched off rather than a fence passed.
//!
//! **What this module deliberately does not do.** It holds no SQL, names no
//! adapter type, and re-implements no part of the fence: `guard_candle` is the
//! ONE single-candle decision, and there is no second spelling of it here or in
//! the repository.

use crate::application::holdout::guard_candle;
use crate::domain::{
    CandleSeriesRepository, ExampleMark, ExampleMarkDraft, ExampleMarkRepository, HoldoutFreeze,
    MarkError, MarkFilter, MarkNote, MarkSetup, MarkSide, MarkWriter, MarkWriterKind, Pair,
    Timeframe,
};

/// A mark as a writer asks for it — the raw request, before any field is
/// validated.
///
/// Every member is a `String` (or an already-typed instant) on purpose: parsing
/// and validation are this use case's FIRST step, so a caller cannot reach the
/// store with a side, a timeframe or a label nobody checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkRequest {
    /// The pair the marked candle belongs to.
    pub pair: String,
    /// The marked candle's timeframe: `15m`, `4h` or `1d`.
    pub timeframe: String,
    /// The marked candle's open time, epoch ms.
    pub bar_open_ms: i64,
    /// `long` or `short`.
    pub side: String,
    /// The setup label.
    pub setup: String,
    /// The optional note.
    pub note: Option<String>,
}

/// What the fence hid from one read, and where it starts (Q4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkHoldout {
    /// The open freeze's holdout start, epoch ms.
    pub holdout_start_ms: i64,
    /// How many marks this read hid because their candle reaches it.
    pub count: u32,
}

/// One `list_marks` answer: the marks a reader may act on, and what the fence hid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkList {
    /// The marks that survived the filter and the fence, in candle order.
    pub marks: Vec<ExampleMark>,
    /// The fence's count, or `None` while no freeze is open.
    pub hidden_by_holdout: Option<MarkHoldout>,
}

/// Write one example-trade mark.
///
/// Steps, in this order and no other: parse every field; apply the holdout fence;
/// load the HEAD snapshot and require the candle; insert. Nothing before the
/// insert touches the database, and nothing at all is written when any step
/// refuses.
///
/// # Errors
///
/// [`MarkError::Field`] for an unreadable or out-of-range member;
/// [`MarkError::Holdout`] when the candle reaches an open freeze's holdout start;
/// [`MarkError::CandleNotFound`] when the HEAD snapshot holds no candle opening
/// at exactly `bar_open_ms`; [`MarkError::Data`] on a store failure.
pub async fn write_mark<R, C>(
    marks: &R,
    candles: &C,
    freeze: Option<HoldoutFreeze>,
    writer: MarkWriter,
    request: MarkRequest,
) -> Result<ExampleMark, MarkError>
where
    R: ExampleMarkRepository,
    C: CandleSeriesRepository,
{
    // 1. Parse and validate EVERY field before anything else happens. The order
    //    inside this step is the wire's own order, so the first thing a caller
    //    reads about is the first thing it sent wrong.
    let pair = Pair::parse(request.pair).map_err(|e| MarkError::field("pair", e.to_string()))?;
    let timeframe = parse_timeframe(&request.timeframe)?;
    let side = MarkSide::parse(&request.side)?;
    let setup = MarkSetup::parse(&request.setup)?;
    let note = match request.note.as_deref() {
        Some(note) => MarkNote::parse(note)?,
        None => None,
    };
    let writer = writer.validate()?;

    // 2. The fence, BEFORE any candle is loaded (G2). A candle reaches the
    //    holdout when its close lands after the holdout start; the close is the
    //    open plus the timeframe's own duration, so this answers without I/O.
    guard_candle(freeze, &pair, timeframe, request.bar_open_ms)?;

    // 3. The candle must exist in the HEAD snapshot (Q2). `load_head` is the
    //    caller's snapshot — never a pinned one: a mark is written against what
    //    the store serves now, and the row records which one that was.
    //    A pair and timeframe with NO `HEAD` at all is the same answer as a
    //    snapshot without that bar: there is no candle here to mark.
    let stored = candles
        .load_head(&pair, timeframe)?
        .ok_or_else(|| MarkError::CandleNotFound {
            pair: pair.clone(),
            timeframe,
            bar_open_ms: request.bar_open_ms,
        })?;
    //    `any` is `find` here: a series carries one candle per open time
    //    (`CandleSeries::validate` refuses a duplicate), so "a candle opens at
    //    exactly this time" is both questions at once.
    if !stored
        .series
        .candles
        .iter()
        .any(|candle| candle.open_time == request.bar_open_ms)
    {
        return Err(MarkError::CandleNotFound {
            pair,
            timeframe,
            bar_open_ms: request.bar_open_ms,
        });
    }

    // 4. One insert, through the one door. The row records WHICH snapshot held
    //    the candle, so a mark says what data it was marked against.
    let data_version = stored.series.version.as_str().to_owned();
    let mark = marks
        .insert(&ExampleMarkDraft {
            pair,
            timeframe,
            bar_open_ms: request.bar_open_ms,
            side,
            setup,
            note,
            writer,
            data_version,
        })
        .await?;
    Ok(mark)
}

/// List the marks a reader may still act on, and say what the fence hid.
///
/// The store hands back only what is not withdrawn; the fence then removes every
/// mark whose candle reaches an open freeze's holdout start and counts them. The
/// rows stay stored — hiding is a read-side act (Q4).
///
/// # Errors
///
/// [`MarkError::Data`] on a store failure.
pub async fn list_marks<R>(
    marks: &R,
    freeze: Option<HoldoutFreeze>,
    filter: MarkFilter,
) -> Result<MarkList, MarkError>
where
    R: ExampleMarkRepository,
{
    let found = marks.list(&filter).await?;
    let Some(freeze) = freeze else {
        return Ok(MarkList {
            marks: found,
            hidden_by_holdout: None,
        });
    };

    let mut visible = Vec::with_capacity(found.len());
    let mut hidden: u32 = 0;
    for mark in found {
        if guard_candle(Some(freeze), &mark.pair, mark.timeframe, mark.bar_open_ms).is_ok() {
            visible.push(mark);
        } else {
            hidden = hidden.saturating_add(1);
        }
    }
    Ok(MarkList {
        marks: visible,
        hidden_by_holdout: Some(MarkHoldout {
            holdout_start_ms: freeze.holdout_start_ms,
            count: hidden,
        }),
    })
}

/// Withdraw one mark, once.
///
/// A trader may withdraw any mark; an agent only the mark it wrote, and only
/// while that mark still stands. The row itself is never touched — the withdraw
/// appends to `example_mark_withdrawal` — so the list stops showing the mark
/// while the audit keeps it (Q3).
///
/// **The fence runs here too, and it runs LAST of the checks (#440, PDR-0039).**
/// The order is `NotFound` → `AlreadyWithdrawn` → `NotWriter` → the holdout
/// guard → the withdraw, and the guard's place is deliberate: a mark a freeze
/// covers is a mark `list_marks` hides and counts and `explain_marks` refuses,
/// so withdrawing one is refused with the SAME `holdout` refusal a write gets:
/// a refused withdraw withdraws nothing and returns no DTO field of the mark.
/// (The refusal's own text names the mark's pair and the holdout start, as a
/// refused write's does; it carries nothing else about the mark.) Putting the
/// guard after `NotWriter` is what keeps the refusal from
/// saying anything about another writer's mark: an agent that reaches for a mark
/// it did not write learns only that the mark is not its own, never where that
/// mark sits against the holdout. A trader, who may withdraw any mark, gets the
/// holdout refusal.
///
/// # Errors
///
/// [`MarkError::NotFound`] for an id that names nothing;
/// [`MarkError::AlreadyWithdrawn`] for a second withdraw; [`MarkError::NotWriter`]
/// when an agent reaches for a mark that is not its own; [`MarkError::Holdout`]
/// when the mark's candle reaches an open freeze's holdout start;
/// [`MarkError::Field`] for an unusable actor; [`MarkError::Data`] on a store
/// failure.
pub async fn withdraw_mark<R>(
    marks: &R,
    freeze: Option<HoldoutFreeze>,
    actor: MarkWriter,
    mark_id: &str,
) -> Result<ExampleMark, MarkError>
where
    R: ExampleMarkRepository,
{
    let actor = actor.validate()?;
    let stored = marks
        .get(mark_id)
        .await?
        .ok_or_else(|| MarkError::NotFound {
            mark_id: mark_id.to_owned(),
        })?;
    if stored.is_withdrawn() {
        return Err(MarkError::AlreadyWithdrawn {
            mark_id: mark_id.to_owned(),
        });
    }
    if actor.kind == MarkWriterKind::Agent
        && (stored.mark.writer.kind != MarkWriterKind::Agent
            || stored.mark.writer.name != actor.name)
    {
        return Err(MarkError::NotWriter {
            actor: actor.name,
            mark_id: mark_id.to_owned(),
            writer_kind: stored.mark.writer.kind.as_str(),
            writer_name: stored.mark.writer.name.clone(),
        });
    }

    // The fence, on the SAME single-candle decision a write uses (#440): a mark
    // whose candle reaches the open holdout start is refused, and nothing is
    // withdrawn. The mark is already in hand, so this answers without I/O.
    guard_candle(
        freeze,
        &stored.mark.pair,
        stored.mark.timeframe,
        stored.mark.bar_open_ms,
    )?;

    marks.withdraw(mark_id, &actor).await?;
    Ok(stored.mark)
}

/// The `Binance` interval text back to its [`Timeframe`] (the inverse of
/// `Timeframe::binance_interval`), as a `timeframe` field refusal.
fn parse_timeframe(raw: &str) -> Result<Timeframe, MarkError> {
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
