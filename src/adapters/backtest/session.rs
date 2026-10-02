//! The stepwise engine session (r3.s4.w1): `run_backtest`'s whole-series
//! event loop, owned bar by bar.
//!
//! [`EngineSession`] holds the exact per-run state the whole-series loop kept
//! on the stack — the indicator engines, the regime detector, the trading
//! state, the funding-event index — and exposes [`EngineSession::step`] (feed
//! one closed primary bar plus the higher-timeframe candles that closed by its
//! close) and [`EngineSession::finish`] (the end-of-series bookkeeping).
//! [`run_backtest`](super::engine::run_backtest) is now a fold over this
//! session — whole-series preconditions at their exact positions, then
//! `new` → `step` per bar → `finish` — so the whole-run and stepwise paths
//! cannot drift. The paper session (r3.s4.w3) drives the same seam live.
//!
//! # Step order (the whole-run loop's order, preserved)
//!
//! 1. **Validation before any state change** (the step refusal list): the
//!    primary strictly ascending and gap-free ([`BacktestError::SeriesUnsorted`]
//!    / [`BacktestError::SeriesGap`]), the funding-order rule as bars arrive
//!    ([`BacktestError::FundingGap`]), each higher series strictly ascending
//!    ([`BacktestError::SeriesUnsorted`]), and every handed higher candle
//!    actually closed by this primary bar's close
//!    ([`BacktestError::HigherBarNotClosed`]). A refused step leaves every
//!    field of the session untouched.
//! 2. **Funding append**: the primary candle's `(open_time, rate)` joins the
//!    session's funding index BEFORE the fill/close block reads it — lead-in
//!    bars included, so the index at any close holds exactly the events the
//!    whole-run `build_funding_index` prebuilds, in the same ascending order.
//! 3. The counted-gated fill → excursion → close → hold block.
//! 4. The indicator engines (primary, then each handed closed HTF/D1 candle in
//!    order), the regime detector, and the paired-bar bookkeeping.
//! 5. The signal-exit, time-stop and entry blocks, the running bar count
//!    standing in for the aligned feed's `bar.index`.

use rust_decimal::Decimal;

use super::engine::{
    BacktestConfig, DualSeriesContext, ExitPlan, LoopState, OpenPosition, PendingEntry,
    PendingExit, PreparedFill, StopRule, atr_at_signal, close_end_of_data,
    close_on_bar_open_or_price, commit_pending_fill, prepare_pending_fill, update_excursion,
};
use crate::adapters::backtest::regime::RegimeDetector;
use crate::adapters::broker::BinanceAdapter;
use crate::adapters::indicators::engine::IndicatorEngine;
use crate::domain::{
    BacktestError, BacktestResult, Candle, CompiledStrategy, Direction, ExitReason,
    OpenPositionMark, Pair, Regime, Series, SeriesEnd, SeriesRole, SkippedEntryCounts,
    SymbolFilters, Timeframe, Trade,
};

/// The cadences a session is declared over: the primary timeframe — whose
/// duration defines the step-path gap rule — and the HTF/D1 timeframes in
/// use, when the strategy consumes those series.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionTimeframes {
    /// The primary series' timeframe.
    pub primary: Timeframe,
    /// The higher-timeframe series' timeframe, when supplied.
    pub htf: Option<Timeframe>,
    /// The fixed daily series' timeframe, when supplied.
    pub d1: Option<Timeframe>,
}

/// The pre-mutation snapshot of the session fields the mutation phase can
/// touch before its last fallible point — a dozen `Copy` values, never the
/// trade history. See [`EngineSession::step_frame`].
#[derive(Clone, Copy)]
struct StepFrame {
    pending_entry: Option<PendingEntry>,
    pending_exit: Option<PendingExit>,
    position: Option<OpenPosition>,
    skipped: SkippedEntryCounts,
    funding_len: usize,
    span_start: Option<i64>,
    last_stamp_anchor: Option<i64>,
    next_boundary: i64,
    first_counted_open: Option<i64>,
}

/// One stepwise backtest: the production event loop, owned bar by bar.
///
/// Construct with [`EngineSession::new`], feed every closed primary candle in
/// order with [`EngineSession::step`], and end with
/// [`EngineSession::finish`]. Between steps the read-only accessors
/// ([`EngineSession::closed_trades`], [`EngineSession::open_position_mark`],
/// [`EngineSession::bars_stepped`]) expose the state the paper session (w3)
/// persists.
pub struct EngineSession {
    /// The compiled exit plan — owns its strategy clone, so the session has
    /// no borrowed lifetime.
    plan: ExitPlan,
    direction: Direction,
    config: BacktestConfig,
    filters: SymbolFilters,
    count_from_ms: Option<i64>,
    timeframes: SessionTimeframes,
    funding_interval_ms: i64,
    engine: IndicatorEngine,
    htf_engine: Option<IndicatorEngine>,
    d1_engine: Option<IndicatorEngine>,
    detector: RegimeDetector,
    state: LoopState,
    /// The last closed HTF candle handed to a step — the paired bar the
    /// strategy's `Htf` leaves read (the aligned feed's `bar.htf`).
    paired_htf: Option<Candle>,
    /// The D1 mirror of `paired_htf`.
    paired_d1: Option<Candle>,
    /// Primary bars stepped so far — `bar.index` in the whole-run loop.
    bars: usize,
    /// The first counted bar's `open_time` — the equity curve's leading time.
    first_counted_open: Option<i64>,
    /// The last accepted primary `open_time` (sortedness/gap state).
    last_primary_open: Option<i64>,
    /// The last accepted HTF candle `open_time` (sortedness state).
    last_htf_open: Option<i64>,
    /// The last accepted D1 candle `open_time` (sortedness state).
    last_d1_open: Option<i64>,
    /// The funding-event index — `(open_time, rate)` for every stamped candle
    /// stepped so far, ascending by construction (step 1 guarantees the order).
    funding_index: Vec<(i64, Decimal)>,
    /// The counted span's first counted `open_time` (funding-order state).
    span_start: Option<i64>,
    /// The last in-span stamp anchor: the latest counted stamp's `open_time`,
    /// or the span start when no stamp has landed yet (funding-order state).
    last_stamp_anchor: Option<i64>,
    /// The next funding boundary not yet decided by the boundary rule
    /// (funding-order state).
    next_boundary: i64,
}

impl EngineSession {
    /// Construct a session over `compiled` for `pair`.
    ///
    /// Runs exactly the per-strategy preconditions that need no series, in the
    /// whole-run loop's relative order: `config.validate()`, the funding
    /// interval lookup (`FundingIntervalUnknown`), `ExitPlan::from_strategy`
    /// (`NoStopLoss` / `ImpossibleTakeProfit`) and the indicator-engine builds
    /// (`EngineInit`). The whole-series checks stay in `run_backtest` at their
    /// exact positions; a direct caller drives them itself before stepping.
    ///
    /// # Errors
    ///
    /// [`BacktestError::InvalidConfig`], [`BacktestError::FundingIntervalUnknown`],
    /// [`BacktestError::NoStopLoss`], [`BacktestError::ImpossibleTakeProfit`],
    /// [`BacktestError::EngineInit`].
    pub fn new(
        compiled: &CompiledStrategy,
        pair: &Pair,
        timeframes: SessionTimeframes,
        config: BacktestConfig,
        filters: SymbolFilters,
        count_from_ms: Option<i64>,
    ) -> Result<Self, BacktestError> {
        config.validate()?;
        let funding_interval_ms = BinanceAdapter::new()
            .funding_interval_ms(pair)
            .map_err(|_| BacktestError::FundingIntervalUnknown { pair: pair.clone() })?;
        let plan = ExitPlan::from_strategy(compiled)?;
        let engine = IndicatorEngine::new(compiled)
            .map_err(|err| BacktestError::EngineInit(err.to_string()))?;
        // The higher-timeframe engine is built only when the strategy carries
        // an `Htf` operand; the D1 mirror for a `D1` operand — exactly the
        // whole-run loop's construction.
        let htf_engine = if compiled.needs_htf() {
            Some(
                IndicatorEngine::for_series(compiled, Series::Htf)
                    .map_err(|err| BacktestError::EngineInit(err.to_string()))?,
            )
        } else {
            None
        };
        let d1_engine = if compiled.needs_d1() {
            Some(
                IndicatorEngine::for_series(compiled, Series::D1)
                    .map_err(|err| BacktestError::EngineInit(err.to_string()))?,
            )
        } else {
            None
        };
        Ok(Self {
            plan,
            direction: compiled.direction(),
            config,
            filters,
            count_from_ms,
            timeframes,
            funding_interval_ms,
            engine,
            htf_engine,
            d1_engine,
            detector: RegimeDetector::new(),
            state: LoopState::default(),
            paired_htf: None,
            paired_d1: None,
            bars: 0,
            first_counted_open: None,
            last_primary_open: None,
            last_htf_open: None,
            last_d1_open: None,
            funding_index: Vec::new(),
            span_start: None,
            last_stamp_anchor: None,
            next_boundary: 0,
        })
    }

    /// Feed one closed primary bar, plus every higher-timeframe candle that
    /// closed at or before `primary.close_time`, in chronological order.
    ///
    /// # Errors
    ///
    /// [`BacktestError::SeriesUnsorted`] / [`BacktestError::SeriesGap`] for a
    /// malformed primary or higher feed, [`BacktestError::FundingGap`] when the
    /// counted span reaches past one funding interval with no stamp,
    /// [`BacktestError::HigherBarNotClosed`] for a higher candle still forming,
    /// and the sizing refusals the whole-run loop raises per bar. A refused
    /// step leaves the session's state untouched.
    pub fn step(
        &mut self,
        primary: &Candle,
        closed_htf: &[Candle],
        closed_d1: &[Candle],
    ) -> Result<(), BacktestError> {
        let counted = self.counted(primary);
        // ---- validation + pure preparation, before ANY state change ----
        self.validate_step(primary, closed_htf, closed_d1, counted)?;
        // The fallible half of the counted fill runs PURE — ahead of the
        // funding append and every other mutation — so every Err this step
        // can return leaves the session byte-identical and the identical
        // retry recomputes the same typed error (the step-atomicity
        // guarantee, r3.s4.w1 correction).
        let prepared_fill = if counted {
            let regime = self.detector.current();
            let prepared = prepare_pending_fill(
                &self.state,
                primary,
                self.direction,
                &self.plan,
                &self.config,
                &self.filters,
            )?;
            Some((regime, prepared))
        } else {
            None
        };

        // ---- mutation phase ----
        // The transaction frame restores the handful of `Copy` fields the
        // mutations touch if the residual (unreachable-by-valid-input) close
        // route still refuses — the atomicity guarantee is absolute.
        let frame = self.step_frame();
        let outcome = self.mutate_step(primary, closed_htf, closed_d1, counted, prepared_fill);
        if outcome.is_err() {
            self.restore_step_frame(&frame);
        }
        outcome
    }

    /// The mutation phase of [`EngineSession::step`] — everything from the
    /// funding append to the bar count, in the whole-run loop's order. With
    /// the fill prepared up front, its only fallible point is the close
    /// route's `realized_r`, which no valid feed can reach (the fill's
    /// geometry guard refuses a zero-distance stop first); the caller's
    /// transaction frame covers even that residual.
    fn mutate_step(
        &mut self,
        primary: &Candle,
        closed_htf: &[Candle],
        closed_d1: &[Candle],
        counted: bool,
        prepared_fill: Option<(Regime, PreparedFill)>,
    ) -> Result<(), BacktestError> {
        // The funding append precedes the fill/close block: every stamped
        // candle — lead-in included — joins the index, so any close's
        // `(entry_fill, exit_fill]` window is complete when it is read.
        if let Some(rate) = primary.funding_rate {
            self.funding_index.push((primary.open_time, rate));
        }
        if counted {
            if self.first_counted_open.is_none() {
                self.first_counted_open = Some(primary.open_time);
            }
            self.advance_funding_state(primary);
        }

        // The counted trading block — the whole-run loop's order: fill the
        // pending entry at the open, fold the bar into the running excursion,
        // close at the open or on an intra-bar level, fold the closed bar
        // into the trailing state only while the position survived.
        if counted && let Some((regime, prepared)) = prepared_fill {
            self.trade_step(primary, regime, prepared)?;
        }

        // The warm-up steps are NOT gated on `counted`: a lead-in bar warms
        // the engines and the detector but counts for nothing else.
        self.engine.step(primary);
        if let Some(engine_htf) = self.htf_engine.as_mut() {
            for candle in closed_htf {
                engine_htf.step(candle);
            }
        }
        if let Some(engine_d1) = self.d1_engine.as_mut() {
            for candle in closed_d1 {
                engine_d1.step(candle);
            }
        }
        self.detector.step(primary);

        // The paired bars: the last handed closed candle of each higher
        // series, persisting across steps with none handed — the aligned
        // feed's `bar.htf` / `bar.d1` semantics. Validation already proved
        // each handed batch ascending, so the batch's last open is its max.
        if let Some(last) = closed_htf.last() {
            self.paired_htf = Some(last.clone());
            self.last_htf_open = Some(last.open_time);
        }
        if let Some(last) = closed_d1.last() {
            self.paired_d1 = Some(last.clone());
            self.last_d1_open = Some(last.open_time);
        }
        self.last_primary_open = Some(primary.open_time);

        // The signal-exit, time-stop and entry blocks over the stepped state.
        self.evaluate_blocks(primary, counted);

        self.bars += 1;
        Ok(())
    }

    /// The pre-mutation snapshot of every field the mutation phase can touch
    /// before its last fallible point (the close route's `realized_r`, which
    /// no valid feed can reach). A dozen `Copy` values — never the trade
    /// history — restored wholesale when the residual route refuses, so ANY
    /// [`EngineSession::step`] error leaves the session byte-identical.
    fn step_frame(&self) -> StepFrame {
        StepFrame {
            pending_entry: self.state.pending_entry,
            pending_exit: self.state.pending_exit,
            position: self.state.position,
            skipped: self.state.skipped_entries,
            funding_len: self.funding_index.len(),
            span_start: self.span_start,
            last_stamp_anchor: self.last_stamp_anchor,
            next_boundary: self.next_boundary,
            first_counted_open: self.first_counted_open,
        }
    }

    fn restore_step_frame(&mut self, frame: &StepFrame) {
        self.state.pending_entry = frame.pending_entry;
        self.state.pending_exit = frame.pending_exit;
        self.state.position = frame.position;
        self.state.skipped_entries = frame.skipped;
        self.funding_index.truncate(frame.funding_len);
        self.span_start = frame.span_start;
        self.last_stamp_anchor = frame.last_stamp_anchor;
        self.next_boundary = frame.next_boundary;
        self.first_counted_open = frame.first_counted_open;
    }

    /// The pre-mutation validation half of [`EngineSession::step`] — the step
    /// refusal list in the spec's order. Pure: reads the session, writes
    /// nothing, so a refused step leaves every field untouched.
    fn validate_step(
        &self,
        primary: &Candle,
        closed_htf: &[Candle],
        closed_d1: &[Candle],
        counted: bool,
    ) -> Result<(), BacktestError> {
        // The primary strictly ascending: out-of-order AND duplicate open_times
        // both refuse as `SeriesUnsorted`, then the gap rule — one primary
        // interval, the same rule `CandleSeries::validate` applies whole-series.
        if let Some(last_open) = self.last_primary_open {
            if primary.open_time <= last_open {
                return Err(BacktestError::SeriesUnsorted {
                    series: SeriesRole::Primary,
                    at: primary.open_time,
                });
            }
            let interval = self.timeframes.primary.duration_ms();
            if primary.open_time - last_open > interval {
                return Err(BacktestError::SeriesGap {
                    series: SeriesRole::Primary,
                    expected: last_open + interval,
                    found: primary.open_time,
                });
            }
        }
        // The funding-order rule as bars arrive — pure, and the incoming
        // candle's own stamp is accounted before any absence is judged.
        self.check_funding_order(primary, counted)?;
        // Each higher series strictly ascending across AND within steps;
        // duplicates refuse as `SeriesUnsorted` (the whole-series rule).
        let mut prev_htf_open = self.last_htf_open;
        for candle in closed_htf {
            if let Some(prev) = prev_htf_open
                && candle.open_time <= prev
            {
                return Err(BacktestError::SeriesUnsorted {
                    series: SeriesRole::Htf,
                    at: candle.open_time,
                });
            }
            prev_htf_open = Some(candle.open_time);
        }
        let mut prev_d1_open = self.last_d1_open;
        for candle in closed_d1 {
            if let Some(prev) = prev_d1_open
                && candle.open_time <= prev
            {
                return Err(BacktestError::SeriesUnsorted {
                    series: SeriesRole::D1,
                    at: candle.open_time,
                });
            }
            prev_d1_open = Some(candle.open_time);
        }
        // Every handed higher candle must be CLOSED by this primary bar's
        // close — a still-forming candle would let the strategy read a bar
        // that does not exist yet (the no-look-ahead rule the aligner
        // enforces on the whole-run path).
        for candle in closed_htf {
            if candle.close_time > primary.close_time {
                return Err(BacktestError::HigherBarNotClosed {
                    series: SeriesRole::Htf,
                    close_time: candle.close_time,
                    primary_close: primary.close_time,
                });
            }
        }
        for candle in closed_d1 {
            if candle.close_time > primary.close_time {
                return Err(BacktestError::HigherBarNotClosed {
                    series: SeriesRole::D1,
                    close_time: candle.close_time,
                    primary_close: primary.close_time,
                });
            }
        }
        Ok(())
    }

    /// The counted trading block of [`EngineSession::step`] — commit the
    /// prepared fill, fold the excursion, close, and fold the trailing state,
    /// in the whole-run loop's order. Its fallible half ran pure in `step`
    /// before any mutation; the only residual `?` (the close route's
    /// `realized_r`) is unreachable by valid input and covered by the
    /// caller's transaction frame.
    fn trade_step(
        &mut self,
        primary: &Candle,
        regime: Regime,
        prepared: PreparedFill,
    ) -> Result<(), BacktestError> {
        // The regime in effect for an entry filling at THIS bar's open is the
        // one determined by already-closed bars (the detector is stepped
        // after the engines, mirroring `engine.step`) — captured in `step`
        // before any mutation, the same read with unchanged semantics.
        commit_pending_fill(
            &mut self.state,
            prepared,
            primary,
            self.direction,
            &self.plan,
            regime,
        );
        if let Some(position) = self.state.position.as_mut() {
            // Fold this bar (the just-opened entry bar, or any held bar
            // including the full exit bar) into the running MFE/MAE before
            // the close reads it. C5: after fill, before close.
            update_excursion(position, primary);
        }
        close_on_bar_open_or_price(&mut self.state, &self.funding_index, primary, &self.config)?;
        if let Some(position) = self.state.position.as_mut() {
            position.hold_closed_bar(primary);
        }
        Ok(())
    }

    /// The post-step evaluation blocks of [`EngineSession::step`]: the signal
    /// exit, the time stop and the entry gate — the whole-run blocks verbatim,
    /// with the running bar count standing in for `bar.index` and the paired
    /// bars standing in for `bar.htf`/`bar.d1`.
    fn evaluate_blocks(&mut self, primary: &Candle, counted: bool) {
        // The series-routed evaluation context over the stepped engines.
        let ctx = DualSeriesContext {
            primary: &self.engine,
            htf: self.htf_engine.as_ref(),
            d1: self.d1_engine.as_ref(),
        };

        if counted
            && self.state.position.is_some()
            && self.state.pending_exit.is_none()
            // The F3 clauses: a signal exit must not evaluate before a closed
            // higher bar is actually paired (r2.s2 round-1 fix F3 / r3.s2.w4).
            && (self.htf_engine.is_none() || self.paired_htf.is_some())
            && (self.d1_engine.is_none() || self.paired_d1.is_some())
            && self.plan.signal_triggered(&ctx)
        {
            self.state.pending_exit = Some(PendingExit {
                signal_time: primary.close_time,
                reason: ExitReason::Signal,
            });
        }

        // The time stop: the `max_bars`-th held bar's close schedules the
        // exit for the next bar's open, after the signal block so a
        // same-close tie keeps the `Signal` label.
        if counted
            && self.state.pending_exit.is_none()
            && let Some(max_bars) = self.plan.max_bars
            && self
                .state
                .position
                .as_ref()
                .is_some_and(|position| position.bars_held == max_bars)
        {
            self.state.pending_exit = Some(PendingExit {
                signal_time: primary.close_time,
                reason: ExitReason::TimeStop,
            });
        }

        // The entry gate: the running bar count stands in for `bar.index > 0`
        // (this is bar `self.bars` in the aligned feed's numbering), the
        // engine's own three-conjunct warm gate, the HTF/D1 engine warm
        // gates, and the paired-bar F3 clauses — the whole-run gate verbatim.
        if counted
            && self.state.position.is_none()
            && self.state.pending_entry.is_none()
            && self.bars > 0
            && self.engine.is_warm()
            && self
                .htf_engine
                .as_ref()
                .is_none_or(IndicatorEngine::is_warm)
            && self.d1_engine.as_ref().is_none_or(IndicatorEngine::is_warm)
            && (self.htf_engine.is_none() || self.paired_htf.is_some())
            && (self.d1_engine.is_none() || self.paired_d1.is_some())
            && self.plan.strategy.entry().eval(&ctx)
        {
            let atr_at_signal_value = atr_at_signal(&self.plan, &ctx);
            if !matches!(self.plan.stop, StopRule::Atr { .. }) || atr_at_signal_value.is_some() {
                self.state.pending_entry = Some(PendingEntry {
                    signal_time: primary.close_time,
                    atr_at_signal: atr_at_signal_value,
                });
            }
        }
    }

    /// End the run: `SnapshotEnd` force-closes a still-open position at the
    /// last bar's close (`EndOfData`); `WindowEdge` leaves it open and marks
    /// it against `last` instead. `last` is the primary series' final candle —
    /// `None` for an empty series.
    ///
    /// # Errors
    ///
    /// [`BacktestError`] from the end-of-data close (sizing/refunded-R).
    pub fn finish(
        self,
        series_end: SeriesEnd,
        last: Option<&Candle>,
    ) -> Result<BacktestResult, BacktestError> {
        let mut state = self.state;
        let open_position = if series_end == SeriesEnd::SnapshotEnd {
            close_end_of_data(&mut state, last, &self.funding_index, &self.config)?;
            None
        } else {
            match (state.position.as_ref(), last) {
                (Some(position), Some(last)) => Some(OpenPositionMark {
                    direction: position.direction,
                    qty: position.qty,
                    entry_price: position.entry_price,
                    entry_signal_time: position.entry_signal_time,
                    entry_fill_time: position.entry_fill_time,
                    mark_time: last.close_time,
                    mark_price: last.close,
                }),
                _ => None,
            }
        };
        // The leading equity point's time is the run's first COUNTED primary
        // candle open — 0 when no bar counted (no trades either).
        let run_start_time_ms = self.first_counted_open.unwrap_or(0);
        Ok(state.into_result(&self.config, run_start_time_ms, open_position))
    }

    /// The trades closed so far, in chronological order.
    #[must_use]
    pub fn closed_trades(&self) -> &[Trade] {
        &self.state.trades
    }

    /// The still-open position marked against `last` — the mark a
    /// `SeriesEnd::WindowEdge` finish would record right now.
    #[must_use]
    pub fn open_position_mark(&self, last: &Candle) -> Option<OpenPositionMark> {
        let position = self.state.position.as_ref()?;
        Some(OpenPositionMark {
            direction: position.direction,
            qty: position.qty,
            entry_price: position.entry_price,
            entry_signal_time: position.entry_signal_time,
            entry_fill_time: position.entry_fill_time,
            mark_time: last.close_time,
            mark_price: last.close,
        })
    }

    /// Primary bars stepped so far — the aligned feed's last `bar.index` + 1.
    #[must_use]
    pub fn bars_stepped(&self) -> usize {
        self.bars
    }

    /// The session's declared primary timeframe.
    #[must_use]
    pub fn primary_timeframe(&self) -> Timeframe {
        self.timeframes.primary
    }

    /// The session's declared higher-timeframe, when one is in use.
    #[must_use]
    pub fn htf_timeframe(&self) -> Option<Timeframe> {
        self.timeframes.htf
    }

    /// The session's declared daily timeframe, when one is in use.
    #[must_use]
    pub fn d1_timeframe(&self) -> Option<Timeframe> {
        self.timeframes.d1
    }

    fn counted(&self, primary: &Candle) -> bool {
        self.count_from_ms
            .is_none_or(|from| primary.open_time >= from)
    }

    /// The funding-order rule as bars arrive — the pure half of the whole-run
    /// `funding_gaps` check over the COUNTED span, applied to the prefix that
    /// ends at `primary`. Both of `funding_gaps`' rules, with its exact slack:
    ///
    /// 1. **Distance.** With the incoming candle's own stamp accounted first
    ///    (it becomes the anchor when it carries one), the distance from the
    ///    last anchor to this bar's close must stay within the funding
    ///    interval plus one primary candle.
    /// 2. **Boundaries.** A funding boundary strictly inside the span is
    ///    decided as soon as the incoming bar's `open_time` passes one candle
    ///    past it — at that point no future stamp (all stamps sit on candle
    ///    opens, ascending) can land within the one-candle coverage window,
    ///    and every stamp that COULD cover it is already in the index. An
    ///    uncovered decided boundary refuses with the same `(from, to)`
    ///    anchors the whole-run rule reports.
    ///
    /// Lead-in bars are not the span's business (their stamps still join the
    /// funding index); a series the whole-run check accepts never refuses
    /// here — every prefix window of an accepted span is itself accepted.
    fn check_funding_order(&self, primary: &Candle, counted: bool) -> Result<(), BacktestError> {
        if !counted {
            return Ok(());
        }
        let candle_ms = self.timeframes.primary.duration_ms();
        let tolerance = self.funding_interval_ms + candle_ms;
        // The effective span start INCLUDING this bar (it may be the first
        // counted one), and the effective anchor INCLUDING this bar's stamp.
        let span_start = self.span_start.unwrap_or(primary.open_time);
        let anchor = if primary.funding_rate.is_some() && primary.open_time > span_start {
            primary.open_time
        } else {
            self.last_stamp_anchor.unwrap_or(span_start)
        };
        // Rule 1 — the growing last window of the distance walk.
        if primary.close_time - anchor > tolerance {
            return Err(BacktestError::FundingGap {
                from: anchor,
                to: primary.close_time,
            });
        }
        // Rule 2 — decided boundaries only, coverage from the stamps seen so
        // far (any covering stamp for a decided boundary is already there).
        let mut boundary = if self.span_start.is_some() {
            self.next_boundary
        } else {
            span_start.div_euclid(self.funding_interval_ms) * self.funding_interval_ms
                + self.funding_interval_ms
        };
        while boundary < primary.close_time && primary.open_time > boundary + candle_ms {
            let covered = {
                let lo = self
                    .funding_index
                    .partition_point(|&(open_time, _)| open_time < boundary - candle_ms);
                self.funding_index[lo..]
                    .iter()
                    .any(|&(open_time, _)| (open_time - boundary).abs() <= candle_ms)
            };
            if !covered {
                return Err(BacktestError::FundingGap {
                    from: span_start.max(boundary - candle_ms),
                    to: boundary + candle_ms,
                });
            }
            boundary += self.funding_interval_ms;
        }
        Ok(())
    }

    /// The mutating half of the funding-order rule: record the span start,
    /// fold the incoming stamp into the anchor, and retire the boundaries the
    /// check just decided. Counted bars only.
    fn advance_funding_state(&mut self, primary: &Candle) {
        let candle_ms = self.timeframes.primary.duration_ms();
        if self.span_start.is_none() {
            self.span_start = Some(primary.open_time);
            self.last_stamp_anchor = Some(primary.open_time);
            self.next_boundary = primary.open_time.div_euclid(self.funding_interval_ms)
                * self.funding_interval_ms
                + self.funding_interval_ms;
        } else if primary.funding_rate.is_some()
            && primary.open_time > self.span_start.unwrap_or(primary.open_time)
        {
            self.last_stamp_anchor = Some(primary.open_time);
        }
        while self.next_boundary < primary.close_time
            && primary.open_time > self.next_boundary + candle_ms
        {
            self.next_boundary += self.funding_interval_ms;
        }
    }
}

#[cfg(test)]
impl EngineSession {
    /// Test-only invalid-state injection (the r3.s4.w1 residual-close-route
    /// rollback proof). HONEST SCOPE: no public feed can construct this
    /// state — the fill's geometry guard refuses a zero-distance stop as
    /// `ImpossibleStop` before any position can exist — so this labeled
    /// method installs an uncloseable position with `initial_stop ==
    /// entry_price` (zero R distance) plus a scheduled signal exit, making
    /// the NEXT step's close route refuse `NoStopLoss` after
    /// `pending_exit.take()`. It exists to prove the transaction frame
    /// restores the session anyway; it proves nothing about public
    /// reachability.
    pub(super) fn inject_uncloseable_position_for_tests(
        &mut self,
        position: OpenPosition,
        pending_exit: PendingExit,
    ) {
        self.state.position = Some(position);
        self.state.pending_exit = Some(pending_exit);
    }

    /// Test-only full-state digest (the r3.s4.w1 step-atomicity proof):
    /// every field of the session — the exit plan, run knobs, funding index
    /// and span state, loop state (pending entry, pending exit, position,
    /// trades, skip tally), paired higher-timeframe candles, metadata
    /// counters — plus the three indicator engines and the regime detector
    /// read through their crate/public surfaces. The engine reads are
    /// BOUNDED: `is_warm` and the lag-0..=3 `Price`/`Indicator` leaf reads
    /// that are exactly what strategy evaluation consumes; they do not
    /// capture the adapters' opaque recursion internals. Those internals are
    /// established unchanged by STRUCTURAL ORDERING — `step` advances the
    /// engines and detector only after every fallible point, so on any Err
    /// they are untouched — and equal bounded digests under that ordering
    /// mean equal later behaviour.
    pub(crate) fn state_digest(&self) -> String {
        let engine_digest =
            |engine: &IndicatorEngine, specs: &[crate::domain::IndicatorSpec], series: Series| {
                let mut reads = Vec::new();
                for lag in 0..=3u32 {
                    reads.push(format!(
                        "close@{lag}={:?}",
                        crate::domain::EvalContext::current(
                            engine,
                            &crate::domain::CompiledValue::Price {
                                series,
                                field: crate::domain::PriceField::Close,
                                lag,
                            },
                        )
                    ));
                }
                for spec in specs {
                    for lag in 0..=3u32 {
                        reads.push(format!(
                            "{spec:?}@{lag}={:?}",
                            crate::domain::EvalContext::current(
                                engine,
                                &crate::domain::CompiledValue::Indicator {
                                    series,
                                    spec: spec.clone(),
                                    lag,
                                },
                            )
                        ));
                    }
                }
                format!("warm={}{reads:?}", engine.is_warm())
            };
        format!(
            "EngineSession{{plan:{:?},direction:{:?},config:{:?},filters:{:?},\
             count_from:{:?},tfs:{:?},interval:{},engine:{},htf:{:?},d1:{:?},\
             detector:{},state:{:?},paired_htf:{:?},paired_d1:{:?},bars:{},\
             first_counted:{:?},last_primary:{:?},last_htf:{:?},last_d1:{:?},\
             funding_index:{:?},span_start:{:?},anchor:{:?},next_boundary:{}}}",
            self.plan,
            self.direction,
            self.config,
            self.filters,
            self.count_from_ms,
            self.timeframes,
            self.funding_interval_ms,
            engine_digest(
                &self.engine,
                self.plan.strategy.required_indicators(),
                Series::Primary,
            ),
            self.htf_engine.as_ref().map(|engine| {
                engine_digest(
                    engine,
                    self.plan.strategy.required_htf_indicators(),
                    Series::Htf,
                )
            }),
            self.d1_engine.as_ref().map(|engine| {
                engine_digest(
                    engine,
                    self.plan.strategy.required_d1_indicators(),
                    Series::D1,
                )
            }),
            self.detector.state_digest(),
            self.state,
            self.paired_htf,
            self.paired_d1,
            self.bars,
            self.first_counted_open,
            self.last_primary_open,
            self.last_htf_open,
            self.last_d1_open,
            self.funding_index,
            self.span_start,
            self.last_stamp_anchor,
            self.next_boundary,
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::domain::{
        Comparator, Condition, ExitRule, PriceField, RiskParams, SchemaVersion, StrategyDsl,
        SweepableValue, ValueSource, compile, validate,
    };

    fn dec(s: &str) -> Decimal {
        s.parse::<Decimal>().expect("decimal literal")
    }

    /// The close-halt probe scenario verbatim: ATR(5)×2 stop, `close > 0`
    /// entry, positive-OHLC candles whose true range is exactly 1.0 — so the
    /// signal bar's ATR is 1.0 and the fill resolves `stop = 1 − 2×1 = −1`,
    /// refusing as `ImpossibleStop`.
    fn probe_session() -> EngineSession {
        let dsl = StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "disposable sizing-refusal probe".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Price {
                    series: Series::Primary,
                    field: PriceField::Close,
                },
                op: Comparator::Gt,
                rhs: ValueSource::Constant { value: dec("0") },
            },
            filters: vec![],
            exits: vec![ExitRule::AtrStop {
                period: SweepableValue::Fixed(5),
                multiple: SweepableValue::Fixed(dec("2")),
            }],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
                max_leverage: SweepableValue::Fixed(dec("3")),
            },
        };
        let compiled_strategy =
            compile(&validate(&dsl).expect("probe dsl validates")).expect("probe dsl compiles");
        EngineSession::new(
            &compiled_strategy,
            &Pair::new("BTCUSDT"),
            SessionTimeframes {
                primary: Timeframe::M15,
                htf: None,
                d1: None,
            },
            BacktestConfig {
                starting_equity: dec("10000"),
                taker_fee_bps: dec("0"),
                slippage_bps: dec("0"),
            },
            SymbolFilters::unconstrained(),
            None,
        )
        .expect("probe session constructs")
    }

    fn probe_bar(i: i64) -> Candle {
        let ms = Timeframe::M15.duration_ms();
        Candle {
            open_time: i * ms,
            close_time: (i + 1) * ms - 1,
            open: dec("1"),
            high: dec("1.5"),
            low: dec("0.5"),
            close: dec("1"),
            volume: dec("1"),
            funding_rate: if i == 0 { Some(Decimal::ZERO) } else { None },
        }
    }

    /// The close-halt regression (`w1-step-sizing-refusal-mutates-state`): a
    /// sizing/geometry refusal raised by the counted fill must leave EVERY
    /// session field untouched — pending entry, funding index and span state,
    /// loop state, paired candles, the indicator engines and the detector —
    /// and the identical retry must repeat the same typed error.
    #[test]
    fn step_sizing_refusal_is_atomic_and_retry_repeats_the_typed_error() {
        let mut session = probe_session();
        for i in 0..7 {
            session
                .step(&probe_bar(i), &[], &[])
                .expect("warm-up steps accept");
        }
        let before = session.state_digest();

        let first = session
            .step(&probe_bar(7), &[], &[])
            .expect_err("the negative-stop ATR fill refuses");
        assert!(
            matches!(first, BacktestError::ImpossibleStop(_)),
            "the refusal is the typed ImpossibleStop: {first:?}"
        );
        let after = session.state_digest();
        assert_eq!(before, after, "a refused sizing step changed session state");

        let retry = session
            .step(&probe_bar(7), &[], &[])
            .expect_err("the identical retry must refuse again");
        assert!(
            matches!(retry, BacktestError::ImpossibleStop(_)),
            "the identical retry is the same typed ImpossibleStop: {retry:?}"
        );
        assert_eq!(
            format!("{first:?}"),
            format!("{retry:?}"),
            "the identical retry must return the same typed error"
        );
        // The durable half of the contract (re-verify survivor): the state is
        // still byte-identical AFTER the identical retry, not only after the
        // first refusal.
        let after_retry = session.state_digest();
        assert_eq!(
            before, after_retry,
            "the identical retry changed session state"
        );
        assert_eq!(session.bars_stepped(), 7, "a refused step never counts");
        assert!(session.closed_trades().is_empty());
        assert!(session.open_position_mark(&probe_bar(7)).is_none());
    }

    /// The re-verify survivor: the close route consumes `pending_exit`
    /// (`engine.rs` `pending_exit.take()`) BEFORE its fallible `realized_r`,
    /// and the transaction frame must restore it. The labeled injection below
    /// is honest: NO public feed can construct this state — the fill's
    /// geometry guard refuses a zero-distance stop as `ImpossibleStop` before
    /// any position can exist — so a `#[cfg(test)]`-only method installs an
    /// uncloseable position (`initial_stop == entry_price`, zero R distance)
    /// plus a scheduled signal exit, and the NEXT step's close route must
    /// refuse `NoStopLoss` with the FULL session state — `pending_exit`
    /// included — unchanged after the first refusal AND the identical retry.
    /// The close-route rollback probe's session: an entry that can never
    /// fire (`close < 0`), so the injected position is the only one on the
    /// board and no fill path interferes.
    fn close_route_probe_session() -> EngineSession {
        let dsl = StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "close-route rollback probe".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Price {
                    series: Series::Primary,
                    field: PriceField::Close,
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant { value: dec("0") },
            },
            filters: vec![],
            exits: vec![ExitRule::StopLoss {
                distance_pct: SweepableValue::Fixed(dec("0.05")),
            }],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(dec("0.01")),
                max_leverage: SweepableValue::Fixed(dec("3")),
            },
        };
        let compiled_strategy =
            compile(&validate(&dsl).expect("probe dsl validates")).expect("probe dsl compiles");
        EngineSession::new(
            &compiled_strategy,
            &Pair::new("BTCUSDT"),
            SessionTimeframes {
                primary: Timeframe::M15,
                htf: None,
                d1: None,
            },
            BacktestConfig {
                starting_equity: dec("10000"),
                taker_fee_bps: dec("0"),
                slippage_bps: dec("0"),
            },
            SymbolFilters::unconstrained(),
            None,
        )
        .expect("probe session constructs")
    }

    fn close_route_bar(i: i64, stamp: Option<Decimal>) -> Candle {
        let ms = Timeframe::M15.duration_ms();
        Candle {
            open_time: i * ms,
            close_time: (i + 1) * ms - 1,
            open: dec("1"),
            high: dec("1.5"),
            low: dec("0.5"),
            close: dec("1"),
            volume: dec("1"),
            funding_rate: stamp,
        }
    }

    #[test]
    fn close_route_refusal_rollback_restores_pending_exit_and_full_state() {
        let mut session = close_route_probe_session();

        let ms = Timeframe::M15.duration_ms();
        for i in 0..2 {
            session
                .step(
                    &close_route_bar(i, if i == 0 { Some(Decimal::ZERO) } else { None }),
                    &[],
                    &[],
                )
                .expect("warm-up steps accept");
        }

        // The labeled injection: an uncloseable position (zero R distance)
        // plus a scheduled signal exit for the next bar's open. Public fill
        // guards make this state unreachable; it exists to prove rollback.
        session.inject_uncloseable_position_for_tests(
            OpenPosition::for_rollback_tests(
                Direction::Long,
                dec("1"),
                dec("1"),
                dec("1"),
                dec("1"),
                ms,
                2 * ms,
                crate::domain::Regime::Unknown,
            ),
            PendingExit {
                signal_time: 2 * ms - 1,
                reason: ExitReason::Signal,
            },
        );
        let before = session.state_digest();
        assert!(session.state.pending_exit.is_some());

        // The close route fires through the scheduled exit: `realized_r`
        // sees a zero stop distance and refuses AFTER `pending_exit.take()`.
        // The incoming stamp exercises the funding-index truncate too.
        let first = session
            .step(&close_route_bar(2, Some(dec("0.001"))), &[], &[])
            .expect_err("the zero-R close refuses");
        assert!(
            matches!(first, BacktestError::NoStopLoss),
            "the refusal is the typed NoStopLoss: {first:?}"
        );
        let after = session.state_digest();
        assert_eq!(
            before, after,
            "a close-route refusal changed session state (pending_exit included)"
        );
        assert!(
            session.state.pending_exit.is_some(),
            "the scheduled exit must survive the refusal"
        );

        let retry = session
            .step(&close_route_bar(2, Some(dec("0.001"))), &[], &[])
            .expect_err("the identical retry must refuse again");
        assert!(
            matches!(retry, BacktestError::NoStopLoss),
            "the identical retry is the same typed NoStopLoss: {retry:?}"
        );
        assert_eq!(
            format!("{first:?}"),
            format!("{retry:?}"),
            "the identical retry must return the same typed error"
        );
        let after_retry = session.state_digest();
        assert_eq!(
            before, after_retry,
            "the identical retry changed session state"
        );
        assert!(session.state.pending_exit.is_some());
        assert_eq!(session.bars_stepped(), 2, "refused steps never count");
        assert!(session.closed_trades().is_empty());
    }
}
