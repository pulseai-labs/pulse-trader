//! Persisted backtest-run domain types (VS-1.2.4 work-4.04, FR-6 / FR-7 / NFR-2).
//!
//! The read-back projections of a persisted backtest run: [`BacktestRunId`] (the
//! opaque run id), [`PersistedRun`] (the run header + its [`SummaryStats`]
//! projection + the integrity / cohort-key fields), and [`RunSummary`] (the typed
//! list projection for the run catalog — explicit fields, NOT a serialized
//! [`BacktestResult`](super::BacktestResult) blob).
//!
//! **Typed projection, never a blob (D1, #68 — README C5).** Both projections
//! carry EXPLICIT typed fields mirrored one-to-one onto the `backtest_run` columns
//! (README C4). Nothing here round-trips a serde-serialized `BacktestResult`, so
//! read-back is independent of serde field-presence and #17 stays a track-forward
//! DSL-grammar concern (no silent-field-drop surface reintroduced).
//!
//! **`engine_target` cohort key (#49, D6).** [`PersistedRun::engine_target`] +
//! [`RunSummary::engine_target`] persist the engine's compiled target triple as
//! the regime cohort key. The per-trade `regime` (persisted on the `trade` row) is
//! **deterministic on the v1 pinned toolchain, NOT byte-portable** (inherits the
//! deferred #29 caveat): it is threshold-on-`f64`-EMA/ADX derived, so two
//! architectures may classify a borderline bar differently. The richer regime
//! provenance (threshold flag + raw indicator snapshot) is DEFERRED to #49.
//!
//! These types carry **`PartialEq` but not `Eq`** (R2 carry-forward): the embedded
//! [`SummaryStats`] holds `sharpe`/`sortino` `f64` fields, so `Eq` is not
//! derivable transitively. Use `PartialEq` only.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::regime::RegimeBreakdown;
use super::stats::SummaryStats;
use crate::domain::Direction;
use crate::domain::pair::Pair;
use crate::domain::sizing::SkippedEntryCounts;
use crate::domain::strategy::VersionId;
use crate::domain::timeframe::Timeframe;
use crate::domain::version::DataVersion;

/// One immutable candle snapshot a run consumed: which timeframe, and which exact
/// content-addressed version of it (r1.s3.w2, #110).
///
/// It names a `data_version`, never `HEAD`. `HEAD` is a mutable pointer that
/// `fetch-data` advances, so recording it would record "whatever is current when
/// someone asks" — which is the bug this type exists to close. ADR-0009's identity
/// is immutable, so this selection resolves to the same bytes forever.
///
/// `Eq` is derivable here (unlike [`PersistedRun`]) — no `f64` anywhere.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSelection {
    /// The candle interval this snapshot holds.
    pub timeframe: Timeframe,
    /// The exact immutable snapshot identity (ADR-0009's content hash).
    pub data_version: DataVersion,
}

/// How a run sourced its funding rates (r1.s3.w2, #110).
///
/// **One variant on purpose, and a real type rather than a string.** The engine
/// reads funding off the loaded snapshot's `funding_rate` and r1 offers no
/// alternative, so this records existing behaviour rather than adding a control the
/// product does not have. It is still a typed discriminant: the column has to carry
/// *something*, and a bare domain `String` is the shape that quietly stays untyped
/// when a second source (a configured flat rate, a funding-free mode) arrives.
/// Fail-closed read decoding rejects any token that is not a known variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingConfig {
    /// Funding accrues from the rates embedded in the loaded candle snapshot.
    SnapshotRates,
}

/// Everything a persisted run CONSUMED (r1.s3.w2, #110).
///
/// [`PersistedRun`] has always recorded what a run produced: `engine_fingerprint`
/// pinned the engine and `result_content_hash` detected tampering with the result.
/// Neither pinned the *data*, so once `HEAD` advanced nothing identified which
/// snapshot produced a stored row, and the reproducibility claim rested on a link
/// that was not stored. These are that link.
///
/// One shared [`Pair`] (a run is single-pair), one required primary selection, one
/// optional HTF selection, the exact cost bps the engine ran with — settings, not
/// the `fees_total` / `slippage_total` OUTCOMES already on the run — and the
/// funding discriminant.
///
/// `Eq` is derivable here (unlike [`PersistedRun`]) — every field is `Eq`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BacktestInputs {
    /// The single trading pair the run used.
    pub pair: Pair,
    /// The primary timeframe's exact snapshot.
    pub primary: SnapshotSelection,
    /// The higher timeframe's exact snapshot, when the run used one. `None` is a
    /// genuine single-timeframe run, not missing data — the debug CLI may omit
    /// `--htf`, while the r1 app path always records M15+H4.
    pub htf: Option<SnapshotSelection>,
    /// Taker fee in basis points, exactly as passed to the engine.
    pub taker_fee_bps: Decimal,
    /// Adverse-fill slippage in basis points, exactly as passed to the engine.
    pub slippage_bps: Decimal,
    /// How funding rates were sourced.
    pub funding: FundingConfig,
    /// The candle slice of the snapshots the run consumed, when it was windowed
    /// (r2.s1.w1). `None` is the whole snapshot — every pre-`0009` row, and
    /// every run saved before windowing existed or without one.
    pub window: Option<CandleWindow>,
}

/// The candle slice of a snapshot a run consumed, when it was windowed
/// (r2.s1.w1).
///
/// Both bounds are UTC epoch milliseconds on the candles' `open_time`, and the
/// window is **half-open `[from_ms, to_ms)`** — the `backtest_run.window_from_ms`
/// / `window_to_ms` `INTEGER` columns one-to-one. `NULL`/`NULL` on the columns is
/// the whole snapshot (no `CandleWindow` at all — see
/// [`BacktestInputs::window`]); a half-present or inverted pair is refused by
/// the `0009` `backtest_run_window_pair` trigger, and `Eq` is derivable (two
/// `i64`s, no `f64` anywhere).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandleWindow {
    /// The first `open_time` the window includes (inclusive lower bound).
    pub from_ms: i64,
    /// The `open_time` the window stops before (exclusive upper bound —
    /// `[from, to)`).
    pub to_ms: i64,
}

/// What the LAST bar of the series the engine consumed represents (r2.s1 G1).
///
/// The engine cannot tell a genuine snapshot end from a caller-imposed window
/// edge — the sliced series looks identical either way, so only the caller
/// that performed the slice can say. The distinction decides whether an open
/// position at the last bar is force-closed: a window bounds what the
/// strategy may TRADE inside it, it does not mean the data ran out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesEnd {
    /// The series ended because the snapshot did. A position still open on
    /// the last bar is force-closed at its close with `ExitReason::EndOfData`
    /// — the strategy had no more bars to act on.
    SnapshotEnd,
    /// The series ended because a [`CandleWindow`] truncated it before the
    /// snapshot's real last candle. A position still open at `to` stays open
    /// and produces no trade — the strategy never chose an exit, and an
    /// `EndOfData` close would book a trade the window fabricated, changing
    /// trade counts and P&L against the same run unwindowed.
    WindowEdge,
}

/// The strategy's still-open position at a window edge (r2.s1 G1, ruling
/// condition b).
///
/// A [`SeriesEnd::WindowEdge`] run does not force-close a position still open
/// at `to` — booking it would fabricate an exit the strategy never chose. But
/// a position that never closed must not vanish from the run record either:
/// this mark IS that record — the direction, entry fill and size exactly as
/// the strategy opened them, and the last in-window candle's `close_time` /
/// `close` as the mark price. It is deliberately NOT a [`Trade`]: it produces
/// no trade row and no closed-trade statistics count it — the record says so
/// explicitly rather than by omission.
///
/// `None` on the carrying `Option` means the run ended flat — or ended at the
/// snapshot's real last bar, where `close_end_of_data` already books the
/// position as an `EndOfData` trade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenPositionMark {
    /// The position's side.
    pub direction: Direction,
    /// Position size in base units.
    pub qty: Decimal,
    /// The price the entry filled at.
    pub entry_price: Decimal,
    /// The signal bar's `close_time` that produced the entry (epoch ms).
    pub entry_signal_time: i64,
    /// The bar the entry filled on (`open_time`, epoch ms).
    pub entry_fill_time: i64,
    /// The mark timestamp — the last in-window candle's `close_time`.
    pub mark_time: i64,
    /// The mark price — the last in-window candle's `close`.
    pub mark_price: Decimal,
}

/// Why [`CandleWindow::new`] refused: an empty or backwards window is not a
/// window.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("a candle window requires from_ms < to_ms, got {from_ms} >= {to_ms}")]
pub struct CandleWindowError {
    /// The submitted lower bound.
    pub from_ms: i64,
    /// The submitted upper bound.
    pub to_ms: i64,
}

impl CandleWindow {
    /// Build a window, refusing an empty or inverted one — `from_ms` must be
    /// strictly before `to_ms` (the `0009` pair trigger refuses the same shape
    /// at the schema layer).
    ///
    /// # Errors
    ///
    /// Returns [`CandleWindowError`] when `from_ms >= to_ms`.
    pub fn new(from_ms: i64, to_ms: i64) -> Result<Self, CandleWindowError> {
        if from_ms >= to_ms {
            return Err(CandleWindowError { from_ms, to_ms });
        }
        Ok(Self { from_ms, to_ms })
    }
}

/// Identifier of a persisted [`PersistedRun`] — a `#[serde(transparent)]`
/// `String` newtype (mirror [`StrategyId`](crate::domain::strategy::StrategyId) /
/// [`VersionId`]).
///
/// Holds a UUID-hyphenated value the adapter
/// ([`SqliteBacktestRunRepo`](crate::adapters::db::SqliteBacktestRunRepo)) mints;
/// serializes as a bare JSON string matching the `backtest_run.id` `TEXT` primary
/// key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BacktestRunId(String);

impl BacktestRunId {
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

/// The read-back header of one persisted backtest run (README C4 / C6).
///
/// The typed projection a `get_run` / `latest_run_for_version` read reconstructs:
/// the run identity + provenance (`strategy_version_id`, `created_at`,
/// `engine_fingerprint`, `engine_target`, `schema_version`), the integrity field
/// (`result_content_hash`, re-derived-and-checked on read per D4), the run-level
/// money totals, the equity-curve base (`starting_equity`), and the derived
/// [`SummaryStats`] projection. **Not** a `BacktestResult` blob (D1).
///
/// `PartialEq` (not `Eq`): the embedded [`SummaryStats`] carries `f64` fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedRun {
    /// The run's opaque id.
    pub id: BacktestRunId,
    /// The `strategy_version` this run was produced against (FK).
    pub strategy_version_id: VersionId,
    /// What the run CONSUMED (r1.s3.w2, #110): the pair, the exact primary and
    /// optional HTF snapshot identities, and the cost/funding configuration.
    ///
    /// `None` **only** for a row written before migration `0006`, whose eight
    /// provenance columns are all NULL. Those rows cannot be backfilled truthfully
    /// and ADR-0018 forbids rewriting immutable records with invented facts, so
    /// they read back as an explicit "provenance unavailable" rather than a guess.
    /// Every fresh save carries `Some`; a partially-populated row is a read error,
    /// never a partially-trusted projection.
    pub inputs: Option<BacktestInputs>,
    /// The run-row schema tag (#68 / D1b) — `RUN_SCHEMA_VERSION` for a v1 row.
    pub schema_version: i64,
    /// The injected-Clock run timestamp (RFC3339 UTC ms text on the column; D7).
    pub created_at: String,
    /// The recording engine's build-time fingerprint (FR-7 prior-run key).
    pub engine_fingerprint: String,
    /// The recording engine's compiled target triple (#49 cohort key; D6).
    pub engine_target: String,
    /// The integrity hash re-derived from the read-back run totals + regime
    /// breakdown + skipped counts + the `seq`-ordered trades, and checked against
    /// the stored column on read (D4 tamper guard).
    pub result_content_hash: String,
    /// The equity-curve base the read-side `EquityCurve::from_trades` reuses (C2).
    pub starting_equity: Decimal,
    /// Net P&L across the run (the engine total — README C4).
    pub net_pnl: Decimal,
    /// Total taker fees across the run.
    pub fees_total: Decimal,
    /// Total signed funding P&L across the run.
    pub funding_total: Decimal,
    /// Total adverse slippage cost across the run.
    pub slippage_total: Decimal,
    /// The derived read-only summary projection (expectancy / win rate / profit
    /// factor / Sharpe / drawdown / streaks / totals — all C4 stat columns).
    pub summary: SummaryStats,
    /// The per-regime trade-count / net-P&L breakdown, as persisted.
    ///
    /// r1.s2.w3: a **pass-through of a stored value**, not a recomputation.
    /// `get_run` already decoded this column to re-derive `result_content_hash`;
    /// it now also surfaces it, because the coach's bounded `CoachContext`
    /// (ADR-0021 decision 8) reads it and the money-math control says the coach
    /// must read persisted results rather than recompute them. Nothing about the
    /// hash input, its feed order, or the query changed.
    pub regime_breakdown: RegimeBreakdown,
    /// The counts of entries the sizer skipped, as persisted. Same pass-through
    /// reasoning as [`regime_breakdown`](Self::regime_breakdown) — and unlike the
    /// regime split, these are **not** derivable from the trade log at all: they
    /// count entries that never became trades.
    pub skipped_entries: SkippedEntryCounts,
    /// The still-open position the window edge left behind, as persisted
    /// (r2.s1 G1, `0010` `backtest_run.open_position`). `None` for every run
    /// that ended flat, every unwindowed run, and every pre-`0010` row — the
    /// column cannot say which, and the distinction does not matter: none of
    /// them have a position to report. The mark is never a [`Trade`] and never
    /// enters the summary's closed-trade statistics.
    pub open_position: Option<OpenPositionMark>,
}

/// The typed list projection of one run for the catalog
/// ([`list_runs_for_version`](crate::domain::port::BacktestRunRepository::list_runs_for_version)).
///
/// Explicit typed fields per README C4 — NOT a [`BacktestResult`](super::BacktestResult)
/// blob (D1). A best-effort catalog row: the headline identity + provenance + the
/// two headline stats most useful in a run list (`net_pnl`, `expectancy`). The
/// full [`SummaryStats`] + trade log are fetched per-run via `get_run` /
/// `get_trades`.
///
/// `PartialEq` (not `Eq`): `expectancy` rides a `Decimal` (Eq-able), but the type
/// is kept consistent with [`PersistedRun`]'s non-`Eq` discipline; only `PartialEq`
/// is derived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    /// The run's opaque id.
    pub id: BacktestRunId,
    /// The `strategy_version` this run was produced against (FK).
    pub strategy_version_id: VersionId,
    /// The run-row schema tag (#68 / D1b).
    pub schema_version: i64,
    /// The injected-Clock run timestamp (RFC3339 UTC ms text; D7).
    pub created_at: String,
    /// The recording engine's build-time fingerprint (FR-7).
    pub engine_fingerprint: String,
    /// The recording engine's compiled target triple (#49 cohort key; D6).
    pub engine_target: String,
    /// The integrity hash stored on the run row (catalog displays it; the
    /// trade-dependent re-derive is a `get_run` concern, D4).
    pub result_content_hash: String,
    /// Net P&L across the run (headline catalog stat).
    pub net_pnl: Decimal,
    /// Mean P&L per trade (headline catalog stat).
    pub expectancy: Decimal,
    /// Number of completed trades in the run.
    pub trade_count: usize,
}
