//! r5.s3.w1 (PDR-0037, d106) — the example-trade mark's value types.
//!
//! A mark is a trader's or an agent's **reference trade**: one candle (pair,
//! timeframe, the candle's open time), a side, a setup label, an optional note
//! and the writer that put it there. It is never an order and carries no
//! credential (PDR-0037, PDR-0015).
//!
//! Pure types, zero I/O like the rest of the ring. The SQLite store lives in
//! `adapters::db::example_mark_repo`; the ONE use case that writes it lives in
//! `application::example_marks`, and `0021`'s triggers hold the same
//! immutability against a raw statement.
//!
//! **What is NOT here.** No stop, no target, no "what happened next": exits come
//! from a rule's risk section, and the position tool belongs with the drawing
//! tools (PDR-0037 Q1). A mark is never edited — it is withdrawn, once.

use serde::{Deserialize, Serialize};

use crate::domain::{DataError, Pair, Timeframe};

/// The longest legal setup label, in characters (Q1).
const SETUP_MAX_CHARS: usize = 64;

/// The longest legal note, in characters (Q1).
const NOTE_MAX_CHARS: usize = 280;

/// Which way the example trade went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkSide {
    /// The example went long.
    Long,
    /// The example went short.
    Short,
}

impl MarkSide {
    /// Parse the wire spelling.
    ///
    /// # Errors
    ///
    /// Returns a `side` field refusal for anything but `long` or `short`.
    pub fn parse(raw: &str) -> Result<Self, MarkError> {
        match raw {
            "long" => Ok(Self::Long),
            "short" => Ok(Self::Short),
            other => Err(MarkError::field(
                "side",
                format!("side must be `long` or `short`, got `{other}`"),
            )),
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Long => "long",
            Self::Short => "short",
        }
    }
}

/// Who wrote a mark: the app's trader, or an agent through MCP.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkWriterKind {
    /// The human surface — the app, named by its token's label.
    Trader,
    /// An MCP client, named by the self-reported agent identity (PDR-0015).
    Agent,
}

impl MarkWriterKind {
    /// Parse the wire spelling.
    ///
    /// # Errors
    ///
    /// Returns a `writer` field refusal for anything but `trader` or `agent`.
    pub fn parse(raw: &str) -> Result<Self, MarkError> {
        match raw {
            "trader" => Ok(Self::Trader),
            "agent" => Ok(Self::Agent),
            other => Err(MarkError::field(
                "writer",
                format!("writer kind must be `trader` or `agent`, got `{other}`"),
            )),
        }
    }

    /// The wire spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trader => "trader",
            Self::Agent => "agent",
        }
    }

    /// Whether this writer's name is the one it reported about itself (PDR-0015)
    /// — true for an agent, false for the token-labelled trader.
    #[must_use]
    pub fn is_self_reported(self) -> bool {
        matches!(self, Self::Agent)
    }
}

/// The mark's writer: which surface, and the name it is stored under.
///
/// A `trader` writer's name is the app token's **label** — never the token
/// (PDR-0037 Q5, G5). An `agent` writer's name is the session's resolved
/// identity, which the agent itself supplied, and the read says so
/// ([`MarkWriterKind::is_self_reported`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkWriter {
    /// Which surface wrote the mark.
    pub kind: MarkWriterKind,
    /// The writer's name as stored.
    pub name: String,
}

impl MarkWriter {
    /// Validate a writer as the use case stores it.
    ///
    /// # Errors
    ///
    /// Returns a `writer` field refusal when the name is empty once trimmed.
    pub fn validate(self) -> Result<Self, MarkError> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err(MarkError::field(
                "writer",
                "the writer's name is empty; a mark cannot be stored without one",
            ));
        }
        Ok(Self {
            kind: self.kind,
            name: name.to_owned(),
        })
    }
}

/// A mark's setup label — the trader's own words for the pattern, trimmed, 1–64
/// characters, no control characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkSetup(String);

impl MarkSetup {
    /// Parse and validate a setup label.
    ///
    /// # Errors
    ///
    /// Returns a `setup` field refusal when the trimmed label is empty, longer
    /// than 64 characters, or carries a control character.
    pub fn parse(raw: &str) -> Result<Self, MarkError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(MarkError::field("setup", "setup must not be empty"));
        }
        let len = trimmed.chars().count();
        if len > SETUP_MAX_CHARS {
            return Err(MarkError::field(
                "setup",
                format!("setup must be at most {SETUP_MAX_CHARS} characters, got {len}"),
            ));
        }
        if trimmed.chars().any(char::is_control) {
            return Err(MarkError::field(
                "setup",
                "setup must not carry a control character",
            ));
        }
        Ok(Self(trimmed.to_owned()))
    }

    /// The stored label.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A mark's free note, at most 280 characters, no control characters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkNote(String);

impl MarkNote {
    /// Parse and trim a note. An empty note is `Ok(None)` — "no note", never an
    /// error: the field is optional on every surface, and a caller that sends
    /// `""` is saying the same thing as one that omits it.
    ///
    /// # Errors
    ///
    /// Returns a `note` field refusal when the trimmed note is longer than 280
    /// characters or carries a control character.
    pub fn parse(raw: &str) -> Result<Option<Self>, MarkError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let len = trimmed.chars().count();
        if len > NOTE_MAX_CHARS {
            return Err(MarkError::field(
                "note",
                format!("note must be at most {NOTE_MAX_CHARS} characters, got {len}"),
            ));
        }
        if trimmed.chars().any(char::is_control) {
            return Err(MarkError::field(
                "note",
                "note must not carry a control character",
            ));
        }
        Ok(Some(Self(trimmed.to_owned())))
    }

    /// The stored note.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One `example_mark` row — a stored reference trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExampleMark {
    /// The row id (a UUID, minted by the store).
    pub id: String,
    /// The pair the marked candle belongs to.
    pub pair: Pair,
    /// The marked candle's timeframe: 15m, 4h or 1d.
    pub timeframe: Timeframe,
    /// The marked candle's open time, epoch ms.
    pub bar_open_ms: i64,
    /// Which way the example trade went.
    pub side: MarkSide,
    /// The setup label the trader gave it.
    pub setup: MarkSetup,
    /// The optional note; `None` when the writer left none.
    pub note: Option<MarkNote>,
    /// Who wrote it.
    pub writer: MarkWriter,
    /// The HEAD snapshot the candle was found in when the mark was written.
    pub data_version: String,
    /// When the mark landed, epoch ms (the injected `Clock`).
    pub created_at_ms: i64,
}

/// A mark as one read returns it: the row, plus whether it has been withdrawn.
///
/// A withdrawn mark is never deleted (`0021`'s trigger refuses the delete), so
/// the list can say a mark was withdrawn while the row stays for the audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredMark {
    /// The stored row.
    pub mark: ExampleMark,
    /// When it was withdrawn, epoch ms; `None` while it stands.
    pub withdrawn_at_ms: Option<i64>,
}

impl StoredMark {
    /// Whether the mark has been withdrawn.
    #[must_use]
    pub fn is_withdrawn(&self) -> bool {
        self.withdrawn_at_ms.is_some()
    }
}

/// Everything one write asks the store to persist; the adapter mints the id and
/// `created_at_ms` from the injected clock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExampleMarkDraft {
    /// The pair the marked candle belongs to.
    pub pair: Pair,
    /// The marked candle's timeframe.
    pub timeframe: Timeframe,
    /// The marked candle's open time, epoch ms.
    pub bar_open_ms: i64,
    /// Which way the example trade went.
    pub side: MarkSide,
    /// The setup label.
    pub setup: MarkSetup,
    /// The optional note.
    pub note: Option<MarkNote>,
    /// Who is writing the mark.
    pub writer: MarkWriter,
    /// The HEAD snapshot the candle was found in.
    pub data_version: String,
}

/// What `list_marks` may narrow by; every member is optional and they narrow
/// together.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkFilter {
    /// Only this pair.
    pub pair: Option<Pair>,
    /// Only this timeframe.
    pub timeframe: Option<Timeframe>,
    /// Only this setup label (exact, as stored).
    pub setup: Option<String>,
}

/// A mark request's typed refusal — the vocabulary every surface maps.
///
/// One enum for the whole feature: the field refusals the domain's own parsers
/// raise ([`MarkError::Field`]), the fence ([`MarkError::Holdout`]), the
/// candle-existence rule, and the withdraw's three authority answers.
#[derive(Debug, thiserror::Error)]
pub enum MarkError {
    /// A request member the use case would not accept.
    #[error("{path}: {message}")]
    Field {
        /// The member the refusal attaches to.
        path: &'static str,
        /// The correctable description.
        message: String,
    },
    /// The mark's candle reaches an open freeze's holdout — refused before any
    /// candle is looked up (G2).
    #[error(transparent)]
    Holdout(#[from] crate::application::holdout::HoldoutRefusal),
    /// No candle opens at the named time in the HEAD snapshot for that pair and
    /// timeframe (Q2).
    #[error(
        "no {pair} {} candle opens at {bar_open_ms} ms in the HEAD snapshot",
        timeframe.binance_interval()
    )]
    CandleNotFound {
        /// The pair that was looked up.
        pair: Pair,
        /// The timeframe that was looked up.
        timeframe: Timeframe,
        /// The open time that matched no candle.
        bar_open_ms: i64,
    },
    /// The withdrawal named a mark that does not exist.
    #[error("no example mark with id {mark_id} exists")]
    NotFound {
        /// The id that named nothing.
        mark_id: String,
    },
    /// The mark has already been withdrawn; a withdraw is one-way (Q3).
    #[error("example mark {mark_id} is already withdrawn")]
    AlreadyWithdrawn {
        /// The already-withdrawn mark.
        mark_id: String,
    },
    /// The actor is an agent and the mark is not its own (Q3).
    #[error(
        "agent {actor} may withdraw only its own example marks; mark {mark_id} was written by \
         {writer_kind} {writer_name}"
    )]
    NotWriter {
        /// The withdrawing agent's name.
        actor: String,
        /// The mark it asked to withdraw.
        mark_id: String,
        /// The mark's own writer kind.
        writer_kind: &'static str,
        /// The mark's own writer name.
        writer_name: String,
    },
    /// A store failure underneath the request.
    #[error(transparent)]
    Data(#[from] DataError),
}

impl MarkError {
    /// A field refusal.
    #[must_use]
    pub fn field(path: &'static str, message: impl Into<String>) -> Self {
        Self::Field {
            path,
            message: message.into(),
        }
    }

    /// The refusal's wire code — the one string every surface reports.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Field { .. } => "validation",
            Self::Holdout(_) => "holdout",
            Self::CandleNotFound { .. } => "candle_not_found",
            Self::NotFound { .. } => "not_found",
            Self::AlreadyWithdrawn { .. } => "already_withdrawn",
            Self::NotWriter { .. } => "not_writer",
            Self::Data(_) => "data",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{MarkNote, MarkSetup, MarkSide, MarkWriter, MarkWriterKind};

    #[test]
    fn sides_round_trip_through_their_wire_spellings() {
        assert_eq!(MarkSide::parse("long").unwrap(), MarkSide::Long);
        assert_eq!(MarkSide::parse("short").unwrap(), MarkSide::Short);
        assert_eq!(MarkSide::Long.as_str(), "long");
        assert!(MarkSide::parse("Long").is_err());
        assert!(MarkSide::parse("").is_err());
    }

    #[test]
    fn a_setup_is_trimmed_and_bounded() {
        assert_eq!(MarkSetup::parse("  a shelf  ").unwrap().as_str(), "a shelf");
        assert!(MarkSetup::parse("   ").is_err());
        assert!(MarkSetup::parse("x").is_ok());
        assert!(MarkSetup::parse(&"x".repeat(64)).is_ok());
        assert!(MarkSetup::parse(&"x".repeat(65)).is_err());
        assert!(MarkSetup::parse("a\u{7}b").is_err());
    }

    #[test]
    fn a_note_is_trimmed_bounded_and_optional() {
        assert_eq!(
            MarkNote::parse("  the fourth touch  ")
                .unwrap()
                .unwrap()
                .as_str(),
            "the fourth touch"
        );
        assert!(MarkNote::parse("").unwrap().is_none());
        assert!(MarkNote::parse("   ").unwrap().is_none());
        assert!(MarkNote::parse(&"n".repeat(280)).unwrap().is_some());
        assert!(MarkNote::parse(&"n".repeat(281)).is_err());
        assert!(MarkNote::parse("a\nb").is_err());
    }

    #[test]
    fn a_writer_needs_a_name_and_says_whether_it_is_self_reported() {
        let trader = MarkWriter {
            kind: MarkWriterKind::Trader,
            name: "  w2-app  ".to_owned(),
        }
        .validate()
        .unwrap();
        assert_eq!(trader.name, "w2-app");
        assert!(!trader.kind.is_self_reported());

        let agent = MarkWriter {
            kind: MarkWriterKind::Agent,
            name: "test-agent".to_owned(),
        }
        .validate()
        .unwrap();
        assert!(agent.kind.is_self_reported());

        assert!(
            MarkWriter {
                kind: MarkWriterKind::Agent,
                name: "   ".to_owned(),
            }
            .validate()
            .is_err()
        );
    }
}
