//! The Backtest Lab's wire contract (r1.s3.w3) — ring-owned DTOs plus the pure
//! projection that builds them from a [`BacktestOutcome`].
//!
//! **Ring-owned wire types, not `specta` derives on domain types** — the
//! [`LibraryOverview`](super::library::LibraryOverview) pattern. Every crossing type
//! is declared here and built by a pure function, so the domain never grows a
//! serialization concern and the frontend never sees a shape that changed because a
//! domain field moved.
//!
//! **Every analytical and provenance value comes from the read-back.** The outcome
//! this projects was assembled from the saved run, the saved trades and the exact
//! snapshots those inputs name — so what the screen renders is what the database
//! holds, not what memory held a moment before the insert. The single exception is
//! `fingerprint_warning`: the FR-7 comparison must happen *before* the insert (after
//! it the fresh row is its own prior) and has no column, so it is control metadata
//! rather than persisted truth. It is documented as such rather than quietly mixed
//! in.
//!
//! **Decimals cross as exact strings** (NFR-2). The frontend renders them verbatim
//! and does no numeric math, so a rounded or reconstructed figure cannot appear
//! screen-side. `sharpe`/`sortino` stay nullable numbers because they are genuinely
//! `f64`-derived, and `null` is how "not enough trades to compute" is already
//! spelled everywhere else.
//!
//! **`fills` are deliberately absent.** W4 renders no fill-level view, and the inline
//! per-fill JSON is a materially larger payload for data nothing displays.
//!
//! **The projection is fallible, and refuses rather than guesses.** Two things here
//! can fail on a corrupt-but-hash-consistent saved row: an enum token, and narrowing a
//! stored count or schema tag to the wire's `u32`. The first draft fabricated
//! `"unknown"` and clamped to `u32::MAX`; both render a plausible false number for a
//! row the truth-source contract says must refuse. Enum tokens are now exhaustive
//! `match` label functions — infallible by construction and compiler-checked against a
//! new variant — and every narrowing returns a typed post-save failure carrying the
//! saved run id, so the caller can still say which run is affected.
//!
//! **Epoch milliseconds cross as exact decimal strings, not numbers.** Specta refuses
//! to export `i64` at all — it will not risk a silent precision loss in the JS number
//! type — and the two ways round that are a lossy `f64` or an exact string. Every
//! other exact value here is already a string the frontend renders verbatim and
//! converts only for chart geometry, so a string keeps one rule instead of two.
//! `schemaVersion` is a small tag and crosses as `u32`.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::application::backtest::{
    BacktestAppError, BacktestOutcome, Histogram, ReadBackFailure, ReadBackStage,
};
use crate::domain::backtest::{
    EquityCurve, ExitReason, Regime, RegimeBreakdown, RegimeCell, TradeSource,
};
use crate::domain::{
    BacktestInputs, BacktestRunId, Direction, FundingConfig, HoldoutFreeze, PersistedRun, Trade,
};

// ---------------------------------------------------------------------------
// The holdout fence over one projected run (#417, PDR-0038 option A)
// ---------------------------------------------------------------------------

/// What one app run read leaves out while an open freeze's holdout start H is
/// in force.
///
/// The DECISION is not made here — a caller resolves it through the same
/// application-ring guard agent reads use ([`guard_persisted_window`], and its
/// snapshot-loading wrapper `guard_persisted_run`) and hands the answer down.
/// This type is only the answer's shape, so the projection has one spelling of
/// the fence rather than one per caller.
///
/// [`guard_persisted_window`]: crate::application::holdout::guard_persisted_window
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunReadFence {
    /// The open freeze's holdout start, epoch ms.
    pub holdout_start_ms: i64,
    /// Whether the run's COUNTED WINDOW reaches H — `true` withholds every
    /// outcome aggregate and series. `false` leaves them exactly as stored: the
    /// run never reached the holdout, so its totals disclose nothing about it.
    pub withhold_totals: bool,
}

impl RunReadFence {
    /// The fence this read applies under an open freeze, from the window
    /// decision the application ring returned.
    ///
    /// `guard_persisted_run` answers `Ok(())` for a window that clears H and a
    /// `HoldoutRefusal` for one that reaches it — so "refused" is exactly "the
    /// counted window reaches H", which is the ruling's own test. A run with no
    /// readable provenance (`MissingInputs`) cannot be PROVEN clear, so the
    /// caller refuses it before reaching here.
    #[must_use]
    pub fn from_window_decision(holdout_start_ms: i64, window_clears_holdout: bool) -> Self {
        Self {
            holdout_start_ms,
            withhold_totals: !window_clears_holdout,
        }
    }
}

/// A read's holdout decision as the accessors the outcome projection reads
/// through (`#417`, PDR-0038 option A).
///
/// `true` means the totals were KEPT — no freeze is open, or the run's counted
/// window ends before the holdout start. Every accessor then returns its input
/// unchanged, which is what makes the unfenced read byte-identical to the one
/// that predates `#417`. `false` means every outcome is withheld: `None`, or an
/// empty series. It is never a recomputation over the trades that remain — the
/// ruling rejected that by name, because the shown numbers would no longer match
/// the stored run.
#[derive(Debug, Clone, Copy)]
pub struct KeptTotals(bool);

impl KeptTotals {
    /// An exact decimal-string field.
    fn dec(self, value: Decimal) -> Option<String> {
        if self.0 { Some(dec(value)) } else { None }
    }

    /// An already-optional exact decimal-string field.
    fn dec_opt(self, value: Option<Decimal>) -> Option<String> {
        if self.0 { dec_opt(value) } else { None }
    }

    /// An already-optional `f64` field.
    fn ratio(self, value: Option<f64>) -> Option<f64> {
        if self.0 { value } else { None }
    }

    /// A non-optional value that the DTO carries as `Option` (a histogram).
    fn opt<T>(self, value: T) -> Option<T> {
        if self.0 { Some(value) } else { None }
    }

    /// A series the DTO carries as a `Vec` (the equity curve, the regimes).
    fn series<T>(self, value: Vec<T>) -> Vec<T> {
        if self.0 { value } else { Vec::new() }
    }

    /// A stored count, narrowed to the wire's `u32` — and NOT narrowed at all
    /// when withheld, because an absent value has nothing to fit.
    ///
    /// # Errors
    ///
    /// Returns [`BacktestAppError::SavedButReadBackFailed`] when a KEPT count
    /// does not fit `u32`.
    fn count(
        self,
        run_id: &BacktestRunId,
        field: &str,
        value: usize,
    ) -> Result<Option<u32>, BacktestAppError> {
        if self.0 {
            count(run_id, field, value).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// The run's trades as this read sends them, and how many the fence withheld
/// (`#417`, PDR-0038 option A).
///
/// A trade that reaches the holdout start is dropped WHOLE — it is not in the
/// answer at all, so no client can filter it back in. The count is the trades
/// this read withheld, never a total the run does not have.
fn fenced_trades(
    outcome: &BacktestOutcome,
    fence: Option<RunReadFence>,
) -> (Vec<TradeRowDto>, u32) {
    let mut rows = Vec::with_capacity(outcome.trades.len());
    let mut withheld: u32 = 0;
    for trade in &outcome.trades {
        if trade_clears_fence(trade, fence) {
            rows.push(trade_dto(trade));
        } else {
            withheld = withheld.saturating_add(1);
        }
    }
    (rows, withheld)
}

/// Whether a stored trade CLEARS the fence: entry before H, and exit before H.
///
/// This is the Chart tab's `beforeFence` (`ui/src/chart/layers.ts`) and
/// `src/tauri/chart.rs`'s `trade_clears_holdout`, spelled for the run read — ONE
/// rule, so the tab's second-guard filter and the server's cut can never
/// disagree about a stored row.
///
/// A stored [`Trade`] is closed by construction (`exit_fill_time` is not
/// optional, and an open position is the run's own `open_position`, never a
/// trade row), so `beforeFence`'s "or no exit" arm has no stored-row case to
/// cover and this predicate is total. The DTO's own exit fields stay `Option`
/// only because the wire shape allows an open trade.
#[must_use]
pub fn trade_clears_fence(trade: &Trade, fence: Option<RunReadFence>) -> bool {
    fence.is_none_or(|fence| {
        trade.entry_fill_time < fence.holdout_start_ms
            && trade.exit_fill_time < fence.holdout_start_ms
    })
}

use super::coach::SummaryDto;

/// What the desktop asks for: one persisted strategy version.
///
/// Pair, timeframes and costs are **not** here. r1's Backtest Lab runs the fixed
/// BTCUSDT M15+H4 / default-cost request, and a field the product cannot vary would
/// be a control that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunRequest {
    /// The immutable strategy version to run.
    pub version_id: String,
}

/// One `[lower, upper)` histogram bin and its count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HistogramBinDto {
    /// Inclusive lower bound in R, exact decimal string.
    pub lower: String,
    /// Exclusive upper bound in R, exact decimal string.
    pub upper: String,
    /// How many trades landed in this bin.
    pub count: u32,
}

/// A pinned excursion histogram, ready to render without further analysis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct HistogramDto {
    /// The shared bin width in R, exact decimal string (`"0.25"`).
    pub bin_width: String,
    /// The finite bins, ascending, covering `[0, 3)`.
    pub bins: Vec<HistogramBinDto>,
    /// Normalized values below `0` — a sign violation, counted rather than hidden.
    pub underflow: u32,
    /// Normalized values at or above `3R`.
    pub overflow: u32,
}

/// One regime's persisted trade count and net P&L.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct RegimeCellDto {
    /// `trending_up` / `trending_down` / `ranging` / `unknown`.
    pub regime: String,
    /// Trades that opened in this regime.
    pub trade_count: u32,
    /// Net P&L across them, exact decimal string.
    pub net_pnl: String,
}

/// One point on the reconstructed equity curve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct EquityPointDto {
    /// Epoch milliseconds, exact integer string.
    pub time_ms: String,
    /// Account equity at that point, exact decimal string.
    pub equity: String,
}

/// One persisted trade, exactly as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct TradeRowDto {
    /// `long` / `short`.
    pub direction: String,
    /// Position size, exact decimal string.
    pub qty: String,
    /// Entry fill price.
    pub entry_price: String,
    /// Exit fill price.
    pub exit_price: String,
    /// Epoch ms the entry signalled.
    pub entry_signal_time: String,
    /// Epoch ms the entry filled.
    pub entry_fill_time: String,
    /// Epoch ms the exit signalled.
    pub exit_signal_time: String,
    /// Epoch ms the exit filled.
    pub exit_fill_time: String,
    /// Taker fees on this trade.
    pub fees_total: String,
    /// Signed funding on this trade.
    pub funding_total: String,
    /// Adverse slippage cost.
    pub slippage_total: String,
    /// Realized P&L.
    pub realized_pnl: String,
    /// Realized R multiple.
    pub realized_r: String,
    /// Maximum favourable excursion, in R — exact, unbinned.
    pub mfe_r: String,
    /// Maximum adverse excursion, in R — exact, unbinned, sign preserved.
    pub mae_r: String,
    /// Why the trade closed.
    pub exit_reason: String,
    /// What opened it.
    pub source: String,
    /// The regime at entry.
    pub regime: String,
    /// The recorded per-trade stop, exact decimal string (r2.s2.w2 / migration
    /// 0011). `null` on a row written before the column existed.
    pub stop_price: Option<String>,
}

/// The four exchange symbol filters one run's sizer ran under (r3.s1.w4,
/// #142) — exact decimal strings, rendered as recorded. `BacktestRunDto::
/// symbol_filters` is `None` (rendered "not recorded") exactly for a
/// pre-`0015` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct SymbolFiltersDto {
    /// Quantity granularity (`LOT_SIZE.stepSize`), exact decimal string.
    pub lot_step: String,
    /// Minimum order quantity (`LOT_SIZE.minQty`), exact decimal string.
    pub min_qty: String,
    /// Minimum order notional (`MIN_NOTIONAL`), exact decimal string.
    pub min_notional: String,
    /// Exchange hard max-leverage cap, exact decimal string.
    pub max_leverage: String,
}

/// The complete Backtest Lab response.
///
/// `PartialEq` but not `Eq` — `sharpe`/`sortino` are `f64`, the same reason
/// [`PersistedRun`] carries only `PartialEq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct BacktestRunDto {
    // --- identity -------------------------------------------------------
    /// The freshly minted run id. Every invocation creates a new one.
    pub run_id: String,
    /// The immutable version this run is attributed to.
    pub strategy_version_id: String,
    /// The run-row schema tag.
    pub schema_version: u32,
    /// When the run was recorded (RFC3339 UTC, millisecond precision).
    pub created_at: String,

    // --- provenance (#110) ----------------------------------------------
    /// The pair the run consumed.
    pub pair: String,
    /// The primary timeframe, Binance interval text.
    pub primary_timeframe: String,
    /// The exact immutable primary snapshot identity.
    pub primary_data_version: String,
    /// The HTF timeframe, when the run used one. Paired with the next field.
    pub htf_timeframe: Option<String>,
    /// The exact immutable HTF snapshot identity.
    pub htf_data_version: Option<String>,
    /// The exact immutable D1 snapshot identity, when the run read one
    /// (r3.s2.w4) — `None` for a strategy with no `d1` operand and for every
    /// row persisted before migration `0017`, whose daily snapshot is not
    /// recorded and is therefore never guessed.
    pub d1_data_version: Option<String>,
    /// Whether the run's strategy actually read the HTF (r3.s2.w4, #219).
    ///
    /// NOT derivable from the two fields above: every run records an H4
    /// snapshot even when its DSL has no `htf` operand (`resolve_default_request`
    /// supplies the application default), so the Lab gates its HTF row on this
    /// flag and renders the D1 row on `d1_data_version` alone.
    pub uses_htf: bool,
    /// First candle `open_time` of the **reloaded** primary snapshot, epoch ms.
    pub first_open_time_ms: String,
    /// Last candle `close_time` of the **reloaded** primary snapshot, epoch ms.
    pub last_close_time_ms: String,
    /// The open freeze's holdout start, epoch ms as an exact integer string;
    /// `null` while no freeze is open (r5.s2 close review).
    ///
    /// The boundary travels because the Chart tab fences every trade row at the
    /// current holdout start (Q5), and the run read is its first answer about a
    /// run — without it the tab could not tell where its fence is. `#417`
    /// (PDR-0038 option A) moved the fence itself to this read: the DTO now also
    /// carries `withheldTradeCount` and `totalsWithheld`, so the boundary is not
    /// the only word about what the read left out.
    pub holdout_start_ms: Option<String>,
    /// How many stored trades this read WITHHELD because they reach the open
    /// freeze's holdout start (`#417`, PDR-0038 option A).
    ///
    /// A trade is withheld when the Chart tab's own rule does NOT hold for it —
    /// entry at or after H, or an exit at or after H (`beforeFence`,
    /// `ui/src/chart/layers.ts`; server-side, `trade_clears_holdout`). `0` while
    /// no freeze is open, and `0` while a freeze is open and no trade reaches it.
    /// This counts trades WITHHELD FROM `trades`, never trades the run lacks.
    pub withheld_trade_count: u32,
    /// Whether this read withheld the run's outcome totals (`#417`, PDR-0038
    /// option A).
    ///
    /// `true` exactly when a freeze was open at this read AND the run's counted
    /// window reaches the holdout start — the decision agent reads already make
    /// (`guard_persisted_run`, `src/application/holdout.rs`). When it is `true`,
    /// every outcome aggregate and series below is `null` or empty: the stored
    /// numbers are withheld, NEVER recomputed over the trades that remain. When
    /// it is `false` — no freeze open, or a run whose window ends before H —
    /// every field below carries its stored value exactly as before.
    pub totals_withheld: bool,
    /// Starting equity, exact decimal string.
    pub starting_equity: String,
    /// Taker fee in basis points.
    pub taker_fee_bps: String,
    /// Slippage in basis points.
    pub slippage_bps: String,
    /// How funding was sourced (`snapshot_rates`).
    pub funding: String,
    /// The exchange symbol filters the sizer ran under (r3.s1.w4, #142), exact
    /// decimal strings as recorded. `null` for a pre-`0015` run — the filters
    /// are **not recorded**, never guessed.
    pub symbol_filters: Option<SymbolFiltersDto>,
    /// The `open_time` of the first candle the engines consumed — the lead-in
    /// start (r2.s3.w2), RFC 3339 like `created_at`. `null` for an unwindowed
    /// run and for every row persisted before migration `0012`, whose lead-in
    /// is not recoverable.
    pub lead_in_from: Option<String>,

    // --- engine ---------------------------------------------------------
    /// The recording engine's build fingerprint.
    pub engine_fingerprint: String,
    /// The recording engine's target triple.
    pub engine_target: String,
    /// The stored integrity hash, re-derived and checked on read.
    pub result_content_hash: String,
    /// FR-7 warning from the pre-save comparison against the prior run.
    ///
    /// The one field not read back from storage — the comparison has no column and
    /// must happen before the insert.
    pub fingerprint_warning: Option<String>,

    // --- headline -------------------------------------------------------
    // Every field from here to `mae` is an OUTCOME: a number or series
    // computed over the run's trades, and therefore a number the open freeze
    // withholds when the run's counted window reaches the holdout start
    // (`#417`, PDR-0038 option A). Each is `Option`/empty exactly so the fence
    // has somewhere honest to put "withheld" — never a recomputation over the
    // trades that remain, and never a plausible false zero.
    /// Net P&L; `null` while the totals are withheld.
    pub net_pnl: Option<String>,
    /// Total taker fees; `null` while the totals are withheld.
    pub fees_total: Option<String>,
    /// Total signed funding; `null` while the totals are withheld.
    pub funding_total: Option<String>,
    /// Total adverse slippage; `null` while the totals are withheld.
    pub slippage_total: Option<String>,
    /// Mean P&L per trade; `null` while the totals are withheld.
    pub expectancy: Option<String>,
    /// Fraction of trades that won; `null` while the totals are withheld.
    pub win_rate: Option<String>,
    /// Gross profit over gross loss; `null` when there is no loss to divide by,
    /// and `null` while the totals are withheld.
    pub profit_factor: Option<String>,
    /// Sum of winning P&L; `null` while the totals are withheld.
    pub gross_profit: Option<String>,
    /// Sum of losing P&L; `null` while the totals are withheld.
    pub gross_loss: Option<String>,
    /// Mean winning trade; `null` while the totals are withheld.
    pub avg_win: Option<String>,
    /// Mean losing trade; `null` while the totals are withheld.
    pub avg_loss: Option<String>,
    /// Largest peak-to-trough equity drop; `null` while the totals are withheld.
    pub max_drawdown: Option<String>,
    /// Number of completed trades; `null` while the totals are withheld.
    pub trade_count: Option<u32>,
    /// How many won; `null` while the totals are withheld.
    pub win_count: Option<u32>,
    /// How many lost; `null` while the totals are withheld.
    pub loss_count: Option<u32>,
    /// Longest winning streak; `null` while the totals are withheld.
    pub max_win_streak: Option<u32>,
    /// Longest losing streak; `null` while the totals are withheld.
    pub max_loss_streak: Option<u32>,
    /// Sharpe ratio; `null` when undefined for this run, and while the totals
    /// are withheld.
    pub sharpe: Option<f64>,
    /// Sortino ratio; `null` when undefined for this run, and while the totals
    /// are withheld.
    pub sortino: Option<f64>,

    // --- skipped entries -------------------------------------------------
    /// Entries skipped below the lot step; `null` while the totals are withheld.
    pub skipped_sub_lot: Option<u32>,
    /// Entries skipped below minimum notional; `null` while the totals are withheld.
    pub skipped_sub_notional: Option<u32>,
    /// Entries skipped by the leverage cap; `null` while the totals are withheld.
    pub skipped_leverage_capped: Option<u32>,

    // --- derived read projections ---------------------------------------
    /// The equity curve, rebuilt from the persisted trades. EMPTY while the
    /// totals are withheld — not a shorter curve, no curve.
    pub equity: Vec<EquityPointDto>,
    /// The four regimes, always in this fixed order. EMPTY while the totals
    /// are withheld.
    pub regimes: Vec<RegimeCellDto>,
    /// The pinned MFE histogram; `null` while the totals are withheld.
    pub mfe: Option<HistogramDto>,
    /// The pinned MAE histogram (magnitude); `null` while the totals are withheld.
    pub mae: Option<HistogramDto>,
    /// Every persisted trade that CLEARS the open freeze, in `seq` order, with
    /// exact values. A trade that reaches the holdout start is not in this list
    /// at all (`#417`): it is withheld server-side, so no client can filter it
    /// back in. `withheldTradeCount` says how many were left out.
    pub trades: Vec<TradeRowDto>,

    // --- walk-forward membership (r2.s3.w3, additive) ---------------------
    /// Which `rolling-oos/v1` walk-forward run this run is a fold of — `null`
    /// for a standalone run and for every row persisted before migration
    /// `0013`.
    pub walk_forward: Option<WalkForwardMembershipDto>,
}

/// The walk-forward membership a run carries, on the wire (r2.s3.w3 — the
/// `0013` `backtest_run` pair, both-or-neither by trigger, so one `Option`
/// carries them together).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct WalkForwardMembershipDto {
    /// The parent `walk_forward_run` id.
    pub walk_forward_run_id: String,
    /// This run's position in the parent's scheme (`0..k`).
    pub fold_index: u32,
}

/// The membership projection (r2.s3.w3) — kept out of `backtest_run_dto` so
/// that function's line count is unchanged (#204).
fn walk_forward_dto(run: &PersistedRun) -> Option<WalkForwardMembershipDto> {
    run.walk_forward.as_ref().map(|m| WalkForwardMembershipDto {
        walk_forward_run_id: m.run_id.as_str().to_owned(),
        fold_index: u32::from(m.fold_index),
    })
}

/// What `compare_child_run` is asked for: one persisted run id (r2.s1.w4 C3).
///
/// Everything else — the run's version, that version's parent, the parent's
/// latest run — is resolved server-side so the screen never reconstructs a
/// lineage it was already shown.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CompareChildRunRequest {
    /// The child run to compare against its parent's latest run.
    pub child_run_id: String,
}

/// A child run beside its parent's latest run — the Backtest Lab's
/// before/after for ANY child version, not only a coach accept (r2.s1.w4 C3).
///
/// `before` is always the parent's LATEST persisted run; `after` is the run
/// named in the request. `inputs_differ` is `BacktestInputs` equality over the
/// two persisted tuples — window included — and reads `true` when either side
/// predates recorded inputs, with `inputs_note` saying which.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct CompareChildRunDto {
    /// The version the child run belongs to.
    pub child_version_id: String,
    /// The parent version the `before` run belongs to.
    pub parent_version_id: String,
    /// The run named in the request — the `after` half.
    pub child_run_id: String,
    /// The parent's latest persisted run — the `before` half.
    pub parent_run_id: String,
    /// The parent's latest run, in the coach DTO's cell shape.
    pub before: SummaryDto,
    /// The child run, in the same shape.
    pub after: SummaryDto,
    /// `parent.inputs != child.inputs`; `true` when either side is `None`.
    pub inputs_differ: bool,
    /// Why the inputs cannot be proven equal, when a side predates them.
    pub inputs_note: Option<String>,
}

/// Exact decimal text — the same `.normalize()`d form the database stores.
/// `pub(crate)` so `walk_forward.rs`'s fold/verdict projections render under
/// the identical rule (r2.s3.w5).
pub(crate) fn dec(value: Decimal) -> String {
    value.normalize().to_string()
}

fn dec_opt(value: Option<Decimal>) -> Option<String> {
    value.map(dec)
}

/// Epoch milliseconds as an exact integer string (see the module docs).
/// `pub(crate)` so `chart.rs`'s markers render the identical text (r5.s2.w1).
pub(crate) fn ms(value: i64) -> String {
    value.to_string()
}

// --- exhaustive enum labels ------------------------------------------------
//
// Each returns the SAME `snake_case` token the column stores, and each is an
// exhaustive `match` rather than a serde round-trip. Infallible by construction: a
// new variant is a compile error here, not an `"unknown"` on someone's screen.
// `pub(crate)` (r5.s2.w1) so the chart read's trade markers use the one spelling
// the Lab and the database already use.

pub(crate) fn direction_label(direction: Direction) -> &'static str {
    match direction {
        Direction::Long => "long",
        Direction::Short => "short",
    }
}

pub(crate) fn exit_reason_label(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::StopLoss => "stop_loss",
        ExitReason::TakeProfit => "take_profit",
        ExitReason::Signal => "signal",
        ExitReason::EndOfData => "end_of_data",
        ExitReason::TrailingStop => "trailing_stop",
        ExitReason::TimeStop => "time_stop",
    }
}

fn trade_source_label(source: TradeSource) -> &'static str {
    match source {
        TradeSource::Backtest => "backtest",
    }
}

fn regime_label(regime: Regime) -> &'static str {
    match regime {
        Regime::TrendingUp => "trending_up",
        Regime::TrendingDown => "trending_down",
        Regime::Ranging => "ranging",
        Regime::Unknown => "unknown",
    }
}

fn funding_label(funding: FundingConfig) -> &'static str {
    match funding {
        FundingConfig::SnapshotRates => "snapshot_rates",
    }
}

/// A projection failure that still names the saved run.
fn projection_failed(run_id: &BacktestRunId, reason: String) -> BacktestAppError {
    BacktestAppError::SavedButReadBackFailed {
        run_id: run_id.clone(),
        stage: ReadBackStage::Projection,
        failure: ReadBackFailure::Projection(reason),
    }
}

/// Narrow a stored count to the wire's `u32`, refusing rather than clamping.
fn count(run_id: &BacktestRunId, field: &str, value: usize) -> Result<u32, BacktestAppError> {
    u32::try_from(value).map_err(|_| {
        projection_failed(
            run_id,
            format!("stored `{field}` = {value} does not fit the wire's u32"),
        )
    })
}

/// The same for the `i64` schema tag.
fn narrow_i64(run_id: &BacktestRunId, field: &str, value: i64) -> Result<u32, BacktestAppError> {
    u32::try_from(value).map_err(|_| {
        projection_failed(
            run_id,
            format!("stored `{field}` = {value} does not fit the wire's u32"),
        )
    })
}

fn histogram_dto(histogram: &Histogram) -> HistogramDto {
    HistogramDto {
        bin_width: dec(histogram.bin_width),
        bins: histogram
            .bins
            .iter()
            .map(|bin| HistogramBinDto {
                lower: dec(bin.lower),
                upper: dec(bin.upper),
                count: bin.count,
            })
            .collect(),
        underflow: histogram.underflow,
        overflow: histogram.overflow,
    }
}

fn equity_dto(curve: &EquityCurve) -> Vec<EquityPointDto> {
    curve
        .0
        .iter()
        .map(|point| EquityPointDto {
            time_ms: ms(point.time_ms),
            equity: dec(point.equity),
        })
        .collect()
}

/// The four-regime vec, fixed order (extracted from `backtest_run_dto` so the
/// r2.s3.w3 membership field does not grow it past its r2.s3.w2 count, #204):
/// a chart that reorders its categories between runs is unreadable, and an
/// absent regime is a zero, not a gap.
fn regimes_dto(
    run_id: &BacktestRunId,
    breakdown: &RegimeBreakdown,
) -> Result<Vec<RegimeCellDto>, BacktestAppError> {
    Ok(vec![
        regime_dto(run_id, "trending_up", breakdown.trending_up())?,
        regime_dto(run_id, "trending_down", breakdown.trending_down())?,
        regime_dto(run_id, "ranging", breakdown.ranging())?,
        regime_dto(run_id, "unknown", breakdown.unknown())?,
    ])
}

fn regime_dto(
    run_id: &BacktestRunId,
    regime: &str,
    cell: RegimeCell,
) -> Result<RegimeCellDto, BacktestAppError> {
    Ok(RegimeCellDto {
        regime: regime.to_owned(),
        trade_count: count(run_id, "regime.trade_count", cell.trade_count)?,
        net_pnl: dec(cell.net_pnl),
    })
}

fn trade_dto(trade: &Trade) -> TradeRowDto {
    TradeRowDto {
        direction: direction_label(trade.direction).to_owned(),
        qty: dec(trade.qty),
        entry_price: dec(trade.entry_price),
        exit_price: dec(trade.exit_price),
        entry_signal_time: ms(trade.entry_signal_time),
        entry_fill_time: ms(trade.entry_fill_time),
        exit_signal_time: ms(trade.exit_signal_time),
        exit_fill_time: ms(trade.exit_fill_time),
        fees_total: dec(trade.fees_total),
        funding_total: dec(trade.funding_total),
        slippage_total: dec(trade.slippage_total),
        realized_pnl: dec(trade.realized_pnl),
        realized_r: dec(trade.realized_r),
        mfe_r: dec(trade.mfe_r),
        mae_r: dec(trade.mae_r),
        exit_reason: exit_reason_label(trade.exit_reason).to_owned(),
        source: trade_source_label(trade.source).to_owned(),
        regime: regime_label(trade.regime).to_owned(),
        stop_price: trade.stop_price.map(dec),
    }
}

/// Project a completed outcome onto the wire.
///
/// Pure over its four inputs and nothing else. Every provenance and analytical
/// value below traces to a persisted column or to the reloaded snapshot the
/// persisted inputs name — except two the caller passes in: `uses_htf`, which no
/// column carries (read from the run's own strategy,
/// [`run_uses_htf`](crate::application::backtest::run_uses_htf)), and `fence`,
/// which the caller resolves through the application ring's own holdout decision
/// because that decision needs the run's counted window (and, for an unwindowed
/// run, its snapshot).
///
/// `fence` is `None` while no freeze is open, and then every field below is the
/// stored value: the projection is byte-identical to the read that predates
/// `#417`. With a fence in force a trade that reaches the holdout start is
/// dropped from `trades` and counted in `withheldTradeCount`, and a run whose
/// counted window reaches H has every outcome aggregate and series withheld.
///
/// # Errors
///
/// Returns [`BacktestAppError::SavedButReadBackFailed`] with
/// [`ReadBackStage::Projection`] when a stored count or schema tag does not fit the
/// wire's `u32`. The run is saved, so the error still names it — refusing is the
/// point: a clamped count would render a plausible false number for a row that
/// should not be reported at all. A withheld count is not narrowed at all: the
/// value is absent, so nothing has to fit.
pub fn backtest_run_dto(
    outcome: &BacktestOutcome,
    uses_htf: bool,
    holdout: Option<HoldoutFreeze>,
    fence: Option<RunReadFence>,
) -> Result<BacktestRunDto, BacktestAppError> {
    let run: &PersistedRun = &outcome.run;
    // Proven `Some` by the use case, which refuses a fresh read-back without inputs
    // — so the outcome carries them non-optionally and this projection is total.
    let inputs: &BacktestInputs = &outcome.inputs;
    let run_id = &run.id;
    let summary = &run.summary;

    // #417 (PDR-0038 option A): the fence's two halves. The totals read through
    // `kept` and the rows through `trade_clears_fence`, so a withheld run has
    // ONE reason to be empty rather than a dozen. See `KeptTotals`.
    let kept = KeptTotals(!fence.is_some_and(|fence| fence.withhold_totals));
    let (trades, withheld_trade_count) = fenced_trades(outcome, fence);

    Ok(BacktestRunDto {
        run_id: run.id.as_str().to_owned(),
        strategy_version_id: run.strategy_version_id.as_str().to_owned(),
        schema_version: narrow_i64(run_id, "schema_version", run.schema_version)?,
        created_at: run.created_at.clone(),

        pair: inputs.pair.as_str().to_owned(),
        primary_timeframe: inputs.primary.timeframe.binance_interval().to_owned(),
        primary_data_version: inputs.primary.data_version.as_str().to_owned(),
        htf_timeframe: inputs
            .htf
            .as_ref()
            .map(|htf| htf.timeframe.binance_interval().to_owned()),
        htf_data_version: inputs
            .htf
            .as_ref()
            .map(|htf| htf.data_version.as_str().to_owned()),
        d1_data_version: inputs
            .d1
            .as_ref()
            .map(|d1| d1.data_version.as_str().to_owned()),
        uses_htf,
        // From the RELOADED snapshot: the truthful range of the data this run
        // consumed, which survives HEAD moving afterwards.
        first_open_time_ms: ms(outcome
            .primary
            .candles
            .first()
            .map_or(0, |candle| candle.open_time)),
        last_close_time_ms: ms(outcome
            .primary
            .candles
            .last()
            .map_or(0, |candle| candle.close_time)),
        // The fence as THIS read saw it: the Chart tab's trade rows are fenced at
        // this instant, so a run read with no freeze open says so with `null`.
        holdout_start_ms: holdout.map(|freeze| ms(freeze.holdout_start_ms)),
        // What this read left out, said plainly (#417 step 3).
        withheld_trade_count,
        totals_withheld: fence.is_some_and(|fence| fence.withhold_totals),
        starting_equity: dec(run.starting_equity),
        taker_fee_bps: dec(inputs.taker_fee_bps),
        slippage_bps: dec(inputs.slippage_bps),
        funding: funding_label(inputs.funding).to_owned(),
        // r3.s1.w4 (#142): the recorded filters, exact decimal strings as
        // persisted — `None` renders "not recorded" at the Lab.
        symbol_filters: inputs.symbol_filters.as_ref().map(|f| SymbolFiltersDto {
            lot_step: dec(f.lot_step),
            min_qty: dec(f.min_qty),
            min_notional: dec(f.min_notional),
            max_leverage: dec(f.max_leverage),
        }),
        // Same RFC 3339 millisecond shape `BacktestInputs::lead_in_from` puts
        // on the MCP wire — one timestamp spelling across the surfaces.
        lead_in_from: inputs.lead_in_from_ms.and_then(|ms| {
            chrono::DateTime::from_timestamp_millis(ms)
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        }),

        engine_fingerprint: run.engine_fingerprint.clone(),
        engine_target: run.engine_target.clone(),
        result_content_hash: run.result_content_hash.clone(),
        fingerprint_warning: outcome.fingerprint_warning.clone(),

        net_pnl: kept.dec(run.net_pnl),
        fees_total: kept.dec(run.fees_total),
        funding_total: kept.dec(run.funding_total),
        slippage_total: kept.dec(run.slippage_total),
        expectancy: kept.dec(summary.expectancy),
        win_rate: kept.dec(summary.win_rate),
        profit_factor: kept.dec_opt(summary.profit_factor),
        gross_profit: kept.dec(summary.gross_profit),
        gross_loss: kept.dec(summary.gross_loss),
        avg_win: kept.dec(summary.avg_win),
        avg_loss: kept.dec(summary.avg_loss),
        max_drawdown: kept.dec(summary.max_drawdown),
        trade_count: kept.count(run_id, "trade_count", summary.trade_count)?,
        win_count: kept.count(run_id, "win_count", summary.win_count)?,
        loss_count: kept.count(run_id, "loss_count", summary.loss_count)?,
        max_win_streak: kept.count(run_id, "max_win_streak", summary.max_win_streak)?,
        max_loss_streak: kept.count(run_id, "max_loss_streak", summary.max_loss_streak)?,
        sharpe: kept.ratio(summary.sharpe),
        sortino: kept.ratio(summary.sortino),

        skipped_sub_lot: kept.count(run_id, "skipped_sub_lot", run.skipped_entries.sub_lot)?,
        skipped_sub_notional: kept.count(
            run_id,
            "skipped_sub_notional",
            run.skipped_entries.sub_notional,
        )?,
        skipped_leverage_capped: kept.count(
            run_id,
            "skipped_leverage_capped",
            run.skipped_entries.leverage_capped,
        )?,

        equity: kept.series(equity_dto(&outcome.equity_curve())),
        regimes: kept.series(regimes_dto(run_id, &run.regime_breakdown)?),
        mfe: kept.opt(histogram_dto(&outcome.mfe)),
        mae: kept.opt(histogram_dto(&outcome.mae)),
        trades,
        walk_forward: walk_forward_dto(run),
    })
}
