//! The paper-session aggregate (r3.s4.w2, ADR-0027).
//!
//! A live paper session is a `paper_session` row plus an append-only
//! `paper_event` log; the materialised candles a shadow check recorded are
//! `paper_bar` rows. Nothing here reads the store or the clock — the aggregate
//! is pure, and the application ring wires it to SQLite and the candle store.

pub(crate) mod fixture;
// r3.s4.w2: the typed event log (`event.rs`), the row-facing value types
// (`session.rs`), and the pure promotion gate (`gate.rs`). The replay state
// machine lands with AC-3 (`state.rs`).
pub(crate) mod event;
pub(crate) mod gate;
pub(crate) mod session;
// r3.s4.w2 AC-3: the replay state machine — state IS the log replayed.
pub(crate) mod state;

// The module's curated surface, re-exported once here so `domain/mod.rs` and
// `lib.rs` can chain the crate and crate-boundary re-exports (an un-re-exported
// public domain type is a `dead_code` BUILD error under `deny(warnings)`).
pub use event::{BarRef, PaperEvent, PaperEventDecodeError, PaperSide, StopActor};
pub use gate::{PromotionDraft, PromotionOverride, PromotionRefused, decide_promotion};
pub use session::{
    CertifiedDataVersion, EmptyTextError, Graduation, MIN_TRADES, NonEmptyLabel, NonEmptyReason,
    NonEmptyText, PaperSession, PaperSessionDraft, PaperSessionId, SLIPPAGE_BPS,
    STARTING_EQUITY_USDT, TAKER_FEE_BPS,
};
pub use state::{
    PaperClosedTrade, PaperPosition, PaperSessionState, PaperSessionStatus, ReplayError,
};
