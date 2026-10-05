//! The paper session's typed event log (r3.s4.w2, ADR-0027).
//!
//! A session is a `paper_session` row plus an append-only `paper_event` log;
//! the state is a replay of the log (`state.rs`, AC-3). This module is the
//! typed face of the log: one variant per `kind` vocabulary word the
//! `0018` CHECK admits, serialized as the row's JSON `payload` with an
//! internal `"type"` tag that must agree with the row's `kind` column on the
//! way out.
//!
//! What w2 defines vs what w3 fills: the variant set and every payload here
//! is w2's; `ShadowChecked`'s `result` stays an opaque JSON carrier until w3
//! types the shadow check itself. A `stop` carries the stopping actor — the
//! client token's label, or `stop_all` plus the token label that issued it
//! (audit #6) — and the domain type refuses an unlabeled stop, which the
//! table CHECK cannot express.
//!
//! Payloads are data, never behaviour: nothing here reads the store or the
//! clock, and no variant carries logic beyond construction validation.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::backtest::ExitReason;
use crate::domain::paper::session::{CertifiedDataVersion, NonEmptyLabel};
use crate::domain::{EngineFingerprint, Timeframe};

/// The side of a paper `order`/`fill`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaperSide {
    /// A buy.
    Long,
    /// A sell.
    Short,
}

/// Who stopped a session (audit #6): one client token's label, or a
/// `stop_all` sweep naming the token that issued it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopActor {
    /// A single session stopped by this client token.
    Token {
        /// The stopping client token's label (never empty — the newtype
        /// refuses empty/whitespace text at deserialization).
        label: NonEmptyLabel,
    },
    /// Every running session stopped by a `stop_all` sweep.
    StopAll {
        /// The client token label that issued the sweep.
        issuer: NonEmptyLabel,
    },
}

/// The typed event log. `seq` and `at` ride every variant because replay
/// consumes the log in `(seq, at)` order — `seq` per session is the row's
/// `MAX(seq)+1` mint, `at` the event's RFC3339 instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaperEvent {
    /// One bar was consumed; names the `paper_bar` rows it produced by
    /// `(timeframe, open_time)` — the per-timeframe bars of one consumed bar.
    BarProcessed {
        /// Per-session insertion sequence.
        seq: i64,
        /// The event's RFC3339 instant.
        at: String,
        /// The bar rows this consumption produced, one per timeframe.
        bars: Vec<BarRef>,
    },
    /// The strategy signalled an entry or exit order (w3 fills the live path).
    Order {
        /// Per-session insertion sequence.
        seq: i64,
        /// The event's RFC3339 instant.
        at: String,
        /// The side signalled.
        side: PaperSide,
        /// The quantity signalled.
        qty: Decimal,
    },
    /// An order filled (paper). A stop-loss is a `Fill` with
    /// `exit_reason: Some(ExitReason::StopLoss)`.
    Fill {
        /// Per-session insertion sequence.
        seq: i64,
        /// The event's RFC3339 instant.
        at: String,
        /// The side that filled.
        side: PaperSide,
        /// The quantity that filled.
        qty: Decimal,
        /// The fill price.
        price: Decimal,
        /// Why the position closed, when this fill closes one.
        exit_reason: Option<ExitReason>,
        /// The closed trade's realized R-multiple (r3.s4.w4, spec §2): filled
        /// from the engine `Trade` on an exit fill; `None` on an entry fill
        /// and on every w3-era payload (`#[serde(default)]`), which the OOS
        /// comparison then does not count.
        #[serde(default)]
        realized_r: Option<Decimal>,
        /// The engine's own fill instant (epoch ms): the `Trade`'s entry or
        /// exit fill time, or the open position's entry fill time. `at` stays
        /// the row's poll instant; replay reads fill times from this field
        /// when present. `None` on every event written before the field
        /// existed (`#[serde(default)]`), which replays with `at` as before.
        #[serde(default)]
        fill_time_ms: Option<i64>,
    },
    /// A funding payment accrued on the open position.
    Funding {
        /// Per-session insertion sequence.
        seq: i64,
        /// The event's RFC3339 instant.
        at: String,
        /// The funding rate applied.
        rate: Decimal,
        /// The payment the rate accrued, in USDT (what the state sums into
        /// `funding_total`).
        amount: Decimal,
    },
    /// The session was stopped. After this event the log is read-only
    /// (`0018`'s BEFORE INSERT trigger aborts any later insert).
    Stop {
        /// Per-session insertion sequence.
        seq: i64,
        /// The event's RFC3339 instant.
        at: String,
        /// Who stopped the session — never unlabeled (audit #6).
        actor: StopActor,
    },
    /// A data-layer event on the session's feeds (w3 fills the live path).
    DataEvent {
        /// Per-session insertion sequence.
        seq: i64,
        /// The event's RFC3339 instant.
        at: String,
        /// A short human-readable summary of what happened to the feed.
        summary: String,
    },
    /// The engine binary changed under a running session; opens a new epoch
    /// (E3). Replays refuse an `old` that is not the current epoch.
    EngineUpgraded {
        /// Per-session insertion sequence.
        seq: i64,
        /// The event's RFC3339 instant.
        at: String,
        /// The fingerprint the session ran under until now.
        old: EngineFingerprint,
        /// The fingerprint the session runs under from this event on.
        new: EngineFingerprint,
    },
    /// One shadow check over the materialised bars completed. w2 defines the
    /// carrier; w3 types `result` when the shadow itself lands.
    ShadowChecked {
        /// Per-session insertion sequence.
        seq: i64,
        /// The event's RFC3339 instant.
        at: String,
        /// The materialised data versions the check ran over.
        data_versions: Vec<CertifiedDataVersion>,
        /// How many recorded bars the check covered.
        bar_count: u64,
        /// The check's outcome — opaque JSON until w3 types it.
        result: serde_json::Value,
    },
}

/// One recorded bar's `(timeframe, open_time)` reference, as a
/// `bar_processed` payload names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BarRef {
    /// The bar's timeframe.
    pub timeframe: Timeframe,
    /// The bar's `open_time` (the `paper_bar` key within the session).
    pub open_time: i64,
}

/// The `kind` CHECK vocabulary word for an event.
impl PaperEvent {
    /// The row's `kind` column value — must equal the payload's internal tag.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::BarProcessed { .. } => "bar_processed",
            Self::Order { .. } => "order",
            Self::Fill { .. } => "fill",
            Self::Funding { .. } => "funding",
            Self::Stop { .. } => "stop",
            Self::DataEvent { .. } => "data_event",
            Self::EngineUpgraded { .. } => "engine_upgraded",
            Self::ShadowChecked { .. } => "shadow_checked",
        }
    }

    /// The event's per-session sequence.
    #[must_use]
    pub fn seq(&self) -> i64 {
        match self {
            Self::BarProcessed { seq, .. }
            | Self::Order { seq, .. }
            | Self::Fill { seq, .. }
            | Self::Funding { seq, .. }
            | Self::Stop { seq, .. }
            | Self::DataEvent { seq, .. }
            | Self::EngineUpgraded { seq, .. }
            | Self::ShadowChecked { seq, .. } => *seq,
        }
    }

    /// Re-key the event to the sequence the repository minted for it, so the
    /// payload's internal `seq` always equals the row's `seq` column.
    #[must_use]
    pub fn with_seq(self, seq: i64) -> Self {
        match self {
            Self::BarProcessed { at, bars, .. } => Self::BarProcessed { seq, at, bars },
            Self::Order { at, side, qty, .. } => Self::Order { seq, at, side, qty },
            Self::Fill {
                at,
                side,
                qty,
                price,
                exit_reason,
                realized_r,
                fill_time_ms,
                ..
            } => Self::Fill {
                seq,
                at,
                side,
                qty,
                price,
                exit_reason,
                realized_r,
                fill_time_ms,
            },
            Self::Funding {
                at, rate, amount, ..
            } => Self::Funding {
                seq,
                at,
                rate,
                amount,
            },
            Self::Stop { at, actor, .. } => Self::Stop { seq, at, actor },
            Self::DataEvent { at, summary, .. } => Self::DataEvent { seq, at, summary },
            Self::EngineUpgraded { at, old, new, .. } => Self::EngineUpgraded { seq, at, old, new },
            Self::ShadowChecked {
                at,
                data_versions,
                bar_count,
                result,
                ..
            } => Self::ShadowChecked {
                seq,
                at,
                data_versions,
                bar_count,
                result,
            },
        }
    }
}

/// Why a `(kind, payload)` pair failed to decode into a [`PaperEvent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaperEventDecodeError {
    /// The payload JSON does not parse.
    MalformedJson(String),
    /// The payload parses but its internal `"type"` tag disagrees with the
    /// row's `kind` column — a lying row is refused, never silently trusted.
    KindMismatch {
        /// The row's `kind` column.
        kind: String,
        /// The payload's internal tag.
        payload_kind: String,
    },
    /// The payload violates a domain type after parsing. Today every
    /// domain violation (an unlabeled `stop` actor — audit #6 — an empty
    /// label or reason) surfaces at SERDE time through
    /// [`Self::MalformedJson`], because the types refuse in their
    /// `Deserialize` impls; this variant is reserved for a post-parse
    /// domain check and `decode` does not construct it yet.
    InvalidPayload {
        /// The row's `kind` column.
        kind: String,
    },
}

impl PaperEvent {
    /// Decode a row's `(kind, payload)` into the typed event, refusing a
    /// payload whose internal tag disagrees with the `kind` column.
    ///
    /// # Errors
    ///
    /// [`PaperEventDecodeError::MalformedJson`] on unparseable JSON —
    /// which includes a payload violating a domain type, since the types
    /// refuse in their `Deserialize` impls — and
    /// [`PaperEventDecodeError::KindMismatch`] when the tag and the column
    /// disagree. [`PaperEventDecodeError::InvalidPayload`] is reserved for
    /// a post-parse domain check; `decode` does not construct it today.
    pub fn decode(kind: &str, payload: &str) -> Result<Self, PaperEventDecodeError> {
        let event: Self = serde_json::from_str(payload)
            .map_err(|e| PaperEventDecodeError::MalformedJson(format!("{kind}: {e}")))?;
        let payload_kind = event.kind();
        if payload_kind != kind {
            return Err(PaperEventDecodeError::KindMismatch {
                kind: kind.to_owned(),
                payload_kind: payload_kind.to_owned(),
            });
        }
        Ok(event)
    }

    /// The row payload: the event's JSON with its internal `"type"` tag.
    ///
    /// # Errors
    ///
    /// Infallible for the variant set in practice; surfaces serialization
    /// failure as a string for the adapter to wrap.
    pub fn payload(&self) -> Result<String, String> {
        serde_json::to_string(self).map_err(|e| e.to_string())
    }
}
