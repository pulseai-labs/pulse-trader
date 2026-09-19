//! Sequential backtest loop over an aligned candle feed.

use rust_decimal::Decimal;

use crate::adapters::backtest::regime::RegimeDetector;
use crate::adapters::indicators::engine::IndicatorEngine;
use crate::domain::{
    BacktestError, BacktestResult, Candle, CandleSeries, CompiledCondition, CompiledExit,
    CompiledStrategy, CompiledValue, Direction, EngineFingerprint, EquityCurve, EvalContext,
    ExitReason, Fill, IndicatorSpec, IntraBarExit, OpenPositionMark, Regime, RegimeBreakdown,
    Series, SeriesEnd, Side, SizingOutcome, SkippedEntryCounts, SummaryStats, SweepableValue,
    SymbolFilters, Trade, TradeSource, align, apply_slippage, atr_stop_price,
    compute_position_size, funding_payment, realized_pnl, realized_r, resolve_intra_bar_exit,
    stop_price, take_profit_price, taker_fee,
};

/// Runtime knobs for the deterministic backtest loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacktestConfig {
    /// Starting account equity used for constant-equity sizing.
    pub starting_equity: Decimal,
    /// Taker fee in basis points.
    pub taker_fee_bps: Decimal,
    /// Adverse fill slippage in basis points.
    pub slippage_bps: Decimal,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            starting_equity: Decimal::new(10_000, 0),
            taker_fee_bps: Decimal::new(4, 0),
            slippage_bps: Decimal::ONE,
        }
    }
}

impl BacktestConfig {
    /// Validate the cost/equity knobs before they reach the sizing + fill math.
    /// Starting equity must be strictly positive (it is the sizing denominator);
    /// the fee and slippage rates must be in `[0, 10_000)` bps — `[0%, 100%)`: a
    /// negative rate would invent a favorable fill or a fee rebate this slice does
    /// not model, and a rate at or above 100% would drive a slipped fill price to
    /// zero or negative.
    ///
    /// # Errors
    ///
    /// [`BacktestError::InvalidConfig`] when any knob is out of range.
    pub fn validate(&self) -> Result<(), BacktestError> {
        let full_pct_bps = Decimal::new(10_000, 0); // 100%
        if self.starting_equity <= Decimal::ZERO {
            return Err(BacktestError::InvalidConfig(format!(
                "starting equity must be positive (got {})",
                self.starting_equity
            )));
        }
        if self.taker_fee_bps < Decimal::ZERO || self.taker_fee_bps >= full_pct_bps {
            return Err(BacktestError::InvalidConfig(format!(
                "taker fee must be in [0, 10000) bps (got {})",
                self.taker_fee_bps
            )));
        }
        if self.slippage_bps < Decimal::ZERO || self.slippage_bps >= full_pct_bps {
            return Err(BacktestError::InvalidConfig(format!(
                "slippage must be in [0, 10000) bps (got {})",
                self.slippage_bps
            )));
        }
        Ok(())
    }
}

/// Run one sequential, deterministic backtest.
///
/// `series_end` says what `primary`'s last bar IS (r2.s1 G1): the caller that
/// sliced the series knows whether it ends because the snapshot did or because
/// a window truncated it. Only [`SeriesEnd::SnapshotEnd`] licenses the
/// `EndOfData` force-close — a position open at a window edge is left open
/// rather than booked as a trade the strategy never chose.
///
/// # Errors
///
/// Returns [`BacktestError`] for strategy preconditions or sizing failures.
pub fn run_backtest(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
    config: &BacktestConfig,
    filters: &SymbolFilters,
    series_end: SeriesEnd,
) -> Result<BacktestResult, BacktestError> {
    config.validate()?;
    let exit_plan = ExitPlan::from_strategy(compiled)?;
    check_htf_inputs(compiled, primary, htf)?;
    let mut engine =
        IndicatorEngine::new(compiled).map_err(|err| BacktestError::EngineInit(err.to_string()))?;
    // The higher-timeframe engine is built only when the strategy carries an
    // `Htf` operand (the guard above guarantees `htf` is `Some` then). It is
    // stepped on each NEWLY aligned closed H4 bar — never once per primary bar.
    let mut htf_engine = if compiled.needs_htf() {
        Some(
            IndicatorEngine::from_specs(compiled.required_htf_indicators())
                .map_err(|err| BacktestError::EngineInit(err.to_string()))?,
        )
    } else {
        None
    };
    // The regime detector is stepped over the PRIMARY M15 series (v1, README C7),
    // independently of the strategy's indicators — so a trade is tagged with the
    // market regime regardless of which indicators the strategy declares.
    let mut detector = RegimeDetector::new();
    let mut state = LoopState::default();
    let direction = compiled.direction();
    // Index into `htf.candles` of the next HTF bar not yet stepped through the
    // HTF engine (r2.s2 round-1 fix F2). A cursor over the source series — not
    // the aligned pairing — is what guarantees every closed HTF candle is
    // stepped exactly once, including the lead-in history `align` jumps past
    // when several H4 closes precede the first primary bar.
    let mut htf_cursor = 0_usize;
    let htf_candles: &[Candle] = htf.map_or(&[][..], |series| series.candles.as_slice());

    // D6 (NFR-1): build the funding-event index ONCE, before the trade loop, so
    // funding accrual is O(trades × (log E + k)) over the ~1095 funding events
    // instead of the old O(trades × candles) full rescan of ~35k bars. The index
    // is `(open_time, rate)` for ONLY the funding-bearing candles; because the
    // store guarantees gap-free chronological-ascending `open_time`, the index is
    // sorted by construction (it preserves source order) — we assert/document
    // this rather than re-sorting. Threaded by borrow through the close chain;
    // built here, never per close.
    let funding_index = build_funding_index(primary);

    for bar in align(primary, htf) {
        // The regime in effect for an entry filling at THIS bar's open is the one
        // determined by already-closed bars (the detector is stepped at the
        // bottom of the loop, mirroring `engine.step`) — the same no-look-ahead
        // discipline the entry signal itself obeys. `current()` is `Unknown` until
        // the EMA200/ADX warm.
        let regime = detector.current();
        fill_pending_entry(
            &mut state,
            bar.primary,
            direction,
            &exit_plan,
            config,
            filters,
            regime,
        )?;
        if let Some(position) = state.position.as_mut() {
            // Fold this bar (the just-opened entry bar, or any held bar including
            // the full exit bar) into the running MFE/MAE before the close reads
            // it. C5: after fill, before close.
            update_excursion(position, bar.primary);
        }
        close_on_bar_open_or_price(&mut state, &funding_index, bar.primary, config)?;

        engine.step(bar.primary);
        // Step the HTF engine with EVERY HTF candle that has closed at or
        // before this primary bar's close (r2.s2 round-1 fix F2), in
        // chronological order, each exactly once. The aligned `bar.htf` stays
        // the last of them, so `current()`/`previous()` remain H4-relative —
        // but the warmup history a `close_time`-jumped pairing would skip
        // (lead-in H4 bars already closed when the run's first primary bar
        // lands) now feeds the engine instead of vanishing.
        if let Some(engine_htf) = htf_engine.as_mut() {
            while let Some(candle) = htf_candles.get(htf_cursor)
                && candle.close_time <= bar.primary.close_time
            {
                engine_htf.step(candle);
                htf_cursor += 1;
            }
        }
        // Advance the regime detector in lock-step with the indicator engine, once
        // per primary bar (README C7). The order vs. `engine.step` is irrelevant
        // (independent state); both step after fill/close so the next bar reads
        // only already-closed information.
        detector.step(bar.primary);

        // The series-routed evaluation context (r2.s2.w2): `Primary` leaves
        // read the primary engine, `Htf` leaves read the HTF engine — a missing
        // HTF engine can only pair with an `Htf`-free strategy (the guard above
        // enforced that), so a `None` here is unreachable for an `Htf` leaf.
        let ctx = DualSeriesContext {
            primary: &engine,
            htf: htf_engine.as_ref(),
        };

        if state.position.is_some()
            && state.pending_exit.is_none()
            // r2.s2 round-1 fix F3: when the strategy needs the higher
            // timeframe a signal exit must not evaluate before a closed HTF
            // bar is actually paired — a `Not(...)` over an absent `Htf`
            // operand otherwise reads `true` and exits against a bar that
            // does not exist yet.
            && (htf_engine.is_none() || bar.htf.is_some())
            && exit_plan.signal_triggered(&ctx)
        {
            state.pending_exit = Some(PendingExit {
                signal_time: bar.primary.close_time,
                reason: ExitReason::Signal,
            });
        }

        if state.position.is_none()
            && state.pending_entry.is_none()
            && bar.index > 0
            && engine.is_warm()
            // Warm gate (r2.s2.w2): when the strategy uses `Htf` operands the
            // HTF engine must be warm too — an `Htf` EMA still seeding must not
            // fire an entry (the same warmup discipline the primary gate has).
            && htf_engine.as_ref().is_none_or(IndicatorEngine::is_warm)
            // r2.s2 round-1 fix F3: `is_warm` is vacuous when the strategy's
            // only `Htf` operand is a Price leaf (no HTF indicator exists to
            // warm), so a paired closed HTF bar must exist before the entry
            // may evaluate — a `Not(...)` over an absent `Htf` operand
            // otherwise reads `true` and fires before any H4 bar exists.
            && (htf_engine.is_none() || bar.htf.is_some())
            && compiled.entry().eval(&ctx)
        {
            // ATR-stop entries additionally require the primary ATR(period)
            // available AT THE SIGNAL BAR (r2.s2.w2): the value is frozen into
            // the pending entry now and turned into the absolute stop at fill —
            // never recomputed there. `is_warm` already covers it when the
            // `AtrStop` registers its ATR, but the explicit `Some` read keeps
            // the gate honest if that registration ever changes.
            let atr_at_signal = match exit_plan.stop {
                StopRule::Atr { period, .. } => ctx.current(&CompiledValue::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Atr {
                        period: SweepableValue::Fixed(period),
                    },
                }),
                StopRule::Pct(_) => None,
            };
            if !matches!(exit_plan.stop, StopRule::Atr { .. }) || atr_at_signal.is_some() {
                state.pending_entry = Some(PendingEntry {
                    signal_time: bar.primary.close_time,
                    atr_at_signal,
                });
            }
        }
    }

    // The force-close fires ONLY when the series genuinely ran out. A window
    // edge is not end-of-data: a position still open at `to` is the strategy's
    // open position, and booking it as a trade fabricates an exit (r2.s1 G1).
    // It must not silently vanish either — the run record carries it as an
    // explicit mark (direction, entry fill, size, last in-window close), never
    // as a trade and never inside the closed-trade statistics.
    let open_position = if series_end == SeriesEnd::SnapshotEnd {
        close_end_of_data(&mut state, primary, &funding_index, config)?;
        None
    } else {
        match (state.position.as_ref(), primary.candles.last()) {
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
    // The leading equity point's time is the run's first primary candle open
    // (README C2 / D5). An empty primary series has no run-start bar; fall back to
    // 0 (the run produced no trades either, so the curve is just the leading point).
    let run_start_time_ms = primary.candles.first().map_or(0, |candle| candle.open_time);
    Ok(state.into_result(config, run_start_time_ms, open_position))
}

/// The engine-side HTF input guards: a strategy carrying an `Htf` operand must
/// never evaluate that operand against primary data (r2.s2.w2 — the
/// application ring checks this first and reports the missing input field),
/// and a supplied "higher" timeframe that is not strictly higher than the
/// primary — compared by `Timeframe::duration_ms`, so the rule holds for any
/// pair — would advance `Series::Htf` operands on the wrong cadence while the
/// DSL renders them as the HTF (r2.s2 round-1 fix F1). Both refusals live at
/// the request boundary too; these are the defence-in-depth copies for callers
/// that construct the series directly.
fn check_htf_inputs(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    htf: Option<&CandleSeries>,
) -> Result<(), BacktestError> {
    if compiled.needs_htf() && htf.is_none() {
        return Err(BacktestError::HtfRequired);
    }
    if let Some(htf_series) = htf
        && htf_series.timeframe.duration_ms() <= primary.timeframe.duration_ms()
    {
        return Err(BacktestError::HtfNotHigher {
            primary: primary.timeframe,
            htf: htf_series.timeframe,
        });
    }
    Ok(())
}

/// How a position's stop is derived (r2.s2.w2): either the fixed-fraction
/// `StopLoss` distance or the `AtrStop`'s `multiple × ATR(period)` frozen at
/// the signal bar.
#[derive(Debug, Clone, Copy)]
enum StopRule {
    /// `entry × (1 ∓ distance_pct)` — the stop distance in price units is
    /// `entry × distance_pct` (defines 1R).
    Pct(Decimal),
    /// `entry ∓ multiple × ATR(period)` — the ATR is read on the primary series
    /// at the signal bar and frozen into the pending entry.
    Atr {
        /// The primary-series ATR lookback.
        period: u32,
        /// The ATR multiple.
        multiple: Decimal,
    },
}

/// The series-routed evaluation context (r2.s2.w2): every [`CompiledValue`]
/// leaf carries its [`Series`] tag, so `current`/`previous` forward `Primary`
/// leaves to the primary engine and `Htf` leaves to the higher-timeframe one.
/// `htf` is `None` only for a strategy with no `Htf` operand — the
/// [`BacktestError::HtfRequired`] guard in [`run_backtest`] makes the
/// alternative unreachable.
struct DualSeriesContext<'a> {
    primary: &'a IndicatorEngine,
    htf: Option<&'a IndicatorEngine>,
}

impl EvalContext for DualSeriesContext<'_> {
    fn current(&self, value: &CompiledValue) -> Option<Decimal> {
        match value {
            CompiledValue::Const(..)
            | CompiledValue::Price {
                series: Series::Primary,
                ..
            }
            | CompiledValue::Indicator {
                series: Series::Primary,
                ..
            } => self.primary.current(value),
            CompiledValue::Price {
                series: Series::Htf,
                ..
            }
            | CompiledValue::Indicator {
                series: Series::Htf,
                ..
            } => self.htf.and_then(|engine| engine.current(value)),
        }
    }

    fn previous(&self, value: &CompiledValue) -> Option<Decimal> {
        match value {
            CompiledValue::Const(..)
            | CompiledValue::Price {
                series: Series::Primary,
                ..
            }
            | CompiledValue::Indicator {
                series: Series::Primary,
                ..
            } => self.primary.previous(value),
            CompiledValue::Price {
                series: Series::Htf,
                ..
            }
            | CompiledValue::Indicator {
                series: Series::Htf,
                ..
            } => self.htf.and_then(|engine| engine.previous(value)),
        }
    }
}

#[derive(Debug, Clone)]
struct ExitPlan<'a> {
    stop: StopRule,
    take_profit_target_r: Option<Decimal>,
    signal_exits: Vec<&'a CompiledCondition>,
    risk_per_trade_pct: Decimal,
    max_leverage: Decimal,
}

impl<'a> ExitPlan<'a> {
    fn from_strategy(compiled: &'a CompiledStrategy) -> Result<Self, BacktestError> {
        // Unsupported kinds gate FIRST: a `TrailingStop`/`TimeStop` is
        // structurally valid but unmodelled here, so it must surface the typed
        // `UnsupportedExit` — not `NoStopLoss`.
        reject_unsupported(compiled.exits())?;
        let Some(stop) = stop_rule(compiled.exits()) else {
            return Err(BacktestError::NoStopLoss);
        };
        let take_profit_target_r = take_profit_target(compiled.exits());
        // The short-TP impossibility check is plan-time only for the
        // fixed-fraction stop (its distance is known before any bar). For an
        // `AtrStop` the distance is `multiple × ATR` — unknowable until the
        // signal bar — so the same typed check moves to the fill (r2.s2.w2).
        if let StopRule::Pct(stop_distance_pct) = stop {
            reject_impossible_short_tp(
                compiled.direction(),
                take_profit_target_r,
                stop_distance_pct,
            )?;
        }
        Ok(Self {
            stop,
            take_profit_target_r,
            signal_exits: signal_exits(compiled.exits()),
            risk_per_trade_pct: compiled.risk().risk_per_trade_pct,
            max_leverage: compiled.risk().max_leverage,
        })
    }

    fn signal_triggered(&self, ctx: &dyn EvalContext) -> bool {
        self.signal_exits
            .iter()
            .any(|condition| condition.eval(ctx))
    }
}

#[derive(Debug, Default)]
struct LoopState {
    pending_entry: Option<PendingEntry>,
    pending_exit: Option<PendingExit>,
    position: Option<OpenPosition>,
    trades: Vec<Trade>,
    /// Bounded O(1) per-reason tally of entries the exchange-constrained sizer
    /// suppressed over the run (audit C4); surfaced on the result.
    skipped_entries: SkippedEntryCounts,
}

impl LoopState {
    /// Fold the accumulated trade log into the final [`BacktestResult`].
    ///
    /// `config` supplies the constant `starting_equity` base for the equity curve
    /// (D5); `run_start_time_ms` is the run's first primary candle open, the
    /// leading equity point's time. The derived read-only `summary` + `equity_curve`
    /// are computed as pure folds **after** the existing totals loop (D1) — they
    /// read the already-final trade log + totals, never mutate them, and (the HARD
    /// slice invariant, README C3/C8) are EXCLUDED from both content hashes.
    /// `open_position` is the still-open position a `SeriesEnd::WindowEdge` run
    /// left behind (built by the caller, which owns the last candle), or `None`
    /// — it lands on the result verbatim, outside the trade log and every
    /// closed-trade statistic.
    fn into_result(
        self,
        config: &BacktestConfig,
        run_start_time_ms: i64,
        open_position: Option<OpenPositionMark>,
    ) -> BacktestResult {
        let mut regime_breakdown = RegimeBreakdown::new();
        for trade in &self.trades {
            // Aggregate each closed trade into its entry-bar regime cell (FR-5).
            regime_breakdown.record(trade.regime, trade.realized_pnl);
        }
        let mut result = BacktestResult {
            trades: self.trades,
            net_pnl: Decimal::ZERO,
            fees_total: Decimal::ZERO,
            funding_total: Decimal::ZERO,
            slippage_total: Decimal::ZERO,
            regime_breakdown,
            skipped_entries: self.skipped_entries,
            open_position,
            // FR-7 / NFR-2 (3.03): stamp every run with the build-time engine
            // identity. EXCLUDED from the content hash (D4) — it is the cross-run
            // comparison key, not part of the determinism oracle.
            engine_fingerprint: EngineFingerprint::current(),
            // Derived read-only surfaces filled in below, AFTER the totals loop
            // (D1). Default placeholders here so the struct is well-formed; the
            // real values are computed once `net_pnl`/the cost totals are final.
            summary: SummaryStats::default(),
            equity_curve: EquityCurve::default(),
        };
        for trade in &result.trades {
            result.net_pnl += trade.realized_pnl;
            result.fees_total += trade.fees_total;
            result.funding_total += trade.funding_total;
            result.slippage_total += trade.slippage_total;
        }
        // Derived read-only folds over the now-final totals + trade log (D1 /
        // README C1–C3). The equity curve is the single source of truth for
        // `max_drawdown`, so it is built first and handed to the summary. NEITHER
        // value is fed into `result_content_hash`/`money_math_hash` (D2/C8) — the
        // frozen baseline stays frozen by construction (#69 untouched).
        result.equity_curve =
            EquityCurve::from_trades(run_start_time_ms, config.starting_equity, &result.trades);
        result.summary = SummaryStats::from_trades(
            &result.trades,
            result.net_pnl,
            result.fees_total,
            result.funding_total,
            &result.equity_curve,
        );
        result
    }
}

#[derive(Debug, Clone, Copy)]
struct PendingEntry {
    signal_time: i64,
    /// The primary-series ATR(period) read AT THE SIGNAL BAR (r2.s2.w2) —
    /// `Some` iff the exit plan is an `AtrStop`. Frozen here so the fill turns
    /// it into the absolute stop without recomputing (the ATR may have moved
    /// between signal and fill bars).
    atr_at_signal: Option<Decimal>,
}

#[derive(Debug, Clone, Copy)]
struct PendingExit {
    signal_time: i64,
    reason: ExitReason,
}

#[derive(Debug, Clone, Copy)]
struct OpenPosition {
    direction: Direction,
    qty: Decimal,
    entry_price: Decimal,
    stop_price: Decimal,
    take_profit_price: Option<Decimal>,
    entry_signal_time: i64,
    entry_fill_time: i64,
    entry_fee: Decimal,
    entry_slippage: Decimal,
    /// Running maximum favorable excursion in R-multiples (C5). Initialized to 0
    /// at entry; `update_excursion` walks it up over each held bar.
    mfe_r: Decimal,
    /// Running maximum adverse excursion in R-multiples (C5). Initialized to 0 at
    /// entry; `update_excursion` walks it down over each held bar.
    mae_r: Decimal,
    /// The market regime in effect at the entry-fill bar (FR-6), carried to the
    /// `Trade` at close so `RegimeBreakdown` can aggregate it.
    regime: Regime,
}

fn fill_pending_entry(
    state: &mut LoopState,
    candle: &Candle,
    direction: Direction,
    plan: &ExitPlan<'_>,
    config: &BacktestConfig,
    filters: &SymbolFilters,
    regime: Regime,
) -> Result<(), BacktestError> {
    let Some(pending) = state.pending_entry.take() else {
        return Ok(());
    };
    if state.position.is_some() {
        return Ok(());
    }

    let raw_entry = candle.open;
    let entry_price = apply_slippage(raw_entry, config.slippage_bps, direction, Side::Entry);
    // The stop is derived once at the fill and FROZEN on the position — neither
    // stop kind ever recomputes or trails afterwards (r2.s2.w2). For `AtrStop`
    // the distance is `multiple × ATR` where the ATR was frozen into the
    // pending entry at the signal bar.
    let stop = match plan.stop {
        StopRule::Pct(distance_pct) => stop_price(entry_price, distance_pct, direction),
        // `atr_at_signal` is guaranteed `Some` by the entry gate; a `None`
        // reaching the fill means a signal was recorded without its ATR —
        // impossible, so skip rather than invent a stop.
        StopRule::Atr { multiple, .. } => {
            let Some(atr) = pending.atr_at_signal else {
                return Ok(());
            };
            let stop = atr_stop_price(entry_price, atr, multiple, direction);
            // r2.s2 round-1 fix F4: `multiple × ATR >= entry` on a long
            // resolves to a non-positive stop — a level that can never fill.
            // Zero would surface as the generic `NoStopLoss` and a negative
            // would size on its absolute distance, both silently wrong, so the
            // fill refuses with the typed error the short-TP leg already has.
            if stop <= Decimal::ZERO {
                return Err(BacktestError::ImpossibleStop(format!(
                    "ATR stop {multiple} × ATR {atr} from entry {entry_price} \
                     resolves to a non-positive price {stop}"
                )));
            }
            stop
        }
    };
    // The shared exchange-constrained sizer (NFR-3, C8): one sizing path for sim
    // and (future v3) live. `NoStopLoss` (zero stop distance) still propagates
    // fail-fast (G5/#20). A `Skipped` outcome consumes the pending entry (it is
    // NOT retried) and increments the matching `SkippedEntryCounts` cell.
    let qty = match compute_position_size(
        config.starting_equity,
        plan.risk_per_trade_pct,
        entry_price,
        stop,
        plan.max_leverage,
        filters,
    )? {
        SizingOutcome::Sized(qty) => qty,
        SizingOutcome::Skipped(reason) => {
            state.skipped_entries.record(reason);
            return Ok(());
        }
    };
    let entry_fee = taker_fee(qty * entry_price, config.taker_fee_bps);
    // Take-profit uses the same 1R distance (`|entry − stop|`) for BOTH stop
    // kinds (r2.s2.w2): fixed-fraction keeps its closed form, `AtrStop` takes
    // `entry ± target_r × |entry − stop|` — with the short-TP impossibility
    // check moved here from plan time, where the ATR distance was unknowable.
    let take_profit_price = plan
        .take_profit_target_r
        .map(|target| match plan.stop {
            StopRule::Pct(distance_pct) => Ok(take_profit_price(
                entry_price,
                distance_pct,
                target,
                direction,
            )),
            StopRule::Atr { .. } => atr_take_profit_price(entry_price, stop, target, direction),
        })
        .transpose()?;
    state.position = Some(OpenPosition {
        direction,
        qty,
        entry_price,
        stop_price: stop,
        take_profit_price,
        entry_signal_time: pending.signal_time,
        entry_fill_time: candle.open_time,
        entry_fee,
        entry_slippage: (entry_price - raw_entry).abs() * qty,
        mfe_r: Decimal::ZERO,
        mae_r: Decimal::ZERO,
        regime,
    });
    Ok(())
}

/// Fold one held bar into the position's running MFE/MAE (C5). Called in the
/// `run_backtest` loop **after** `fill_pending_entry` and **before**
/// `close_on_bar_open_or_price`, only when a position exists — so it folds the
/// just-opened entry bar and the full exit bar before the close reads the running
/// values.
///
/// Excursion is measured from the entry fill price `E` and normalized by the
/// initial stop distance `D = |E − stop|` (`> 0`, the sizer guarantees it). For a
/// held bar with high `H`, low `L`: long → `fav = (H − E)/D`, `adv = (L − E)/D`;
/// short → `fav = (E − L)/D`, `adv = (E − H)/D`. We keep the running
/// `mfe_r = max(mfe_r, fav)` and `mae_r = min(mae_r, adv)`. The init-0 sample
/// keeps `mfe_r >= 0 ∧ mae_r <= 0` (C5). The full bar range counts (no intra-bar
/// path reconstruction), so `mfe_r >= realized_r >= mae_r` is NOT guaranteed.
fn update_excursion(position: &mut OpenPosition, candle: &Candle) {
    let entry = position.entry_price;
    let stop_distance = (entry - position.stop_price).abs();
    if stop_distance.is_zero() {
        // The sizer refuses a zero stop distance, so this is unreachable in a
        // real run; guard anyway to avoid a divide-by-zero on a degenerate path.
        return;
    }
    let (fav, adv) = match position.direction {
        Direction::Long => (
            (candle.high - entry) / stop_distance,
            (candle.low - entry) / stop_distance,
        ),
        Direction::Short => (
            (entry - candle.low) / stop_distance,
            (entry - candle.high) / stop_distance,
        ),
    };
    if fav > position.mfe_r {
        position.mfe_r = fav;
    }
    if adv < position.mae_r {
        position.mae_r = adv;
    }
}

fn close_on_bar_open_or_price(
    state: &mut LoopState,
    funding_index: &[(i64, Decimal)],
    candle: &Candle,
    config: &BacktestConfig,
) -> Result<(), BacktestError> {
    let Some(position) = state.position else {
        state.pending_exit = None;
        return Ok(());
    };

    // #44 fix (C6): a signal-exit that fired on bar N's close is scheduled to fill
    // at THIS bar's open — it fills at the open and the bar's intra-bar (post-open)
    // SL/TP CANNOT preempt it. The open may itself gap through a level, which we
    // label symmetrically via `open_gap_reason`: gapped through the stop →
    // `StopLoss`; through the TP → `TakeProfit`; inside the channel → `Signal`.
    // The fill price is the open in all three cases; only the `exit_reason` (and
    // the `signal_time`) differs.
    if let Some(pending) = state.pending_exit.take() {
        let exit = match open_gap_reason(candle.open, &position) {
            Some(reason) => ExitFill {
                // A price event at the open, not the prior signal: the timestamp
                // is this bar's open, not the prior bar's close.
                signal_time: candle.open_time,
                fill_time: candle.open_time,
                raw_price: candle.open,
                reason,
            },
            None => ExitFill {
                signal_time: pending.signal_time,
                fill_time: candle.open_time,
                raw_price: candle.open,
                reason: pending.reason,
            },
        };
        close_position(state, funding_index, exit, config)?;
        return Ok(());
    }

    // No pending signal-exit: the existing intra-bar SL/TP resolution runs
    // unchanged.
    if let Some(exit) = price_exit(candle, &position) {
        close_position(state, funding_index, exit, config)?;
    }
    Ok(())
}

/// Resolve whether this bar's **open** itself gapped through a price level for a
/// position whose signal-exit is filling at the open (#44 / C6). Symmetric
/// labeling: an open at/through the stop → `StopLoss`; an open at/through the TP →
/// `TakeProfit`; an open inside the channel → `None` (⇒ the caller labels it
/// `Signal`). This is the ONLY level check on a signal-exit bar — the intra-bar
/// high/low are deliberately ignored, because the position is already closed at
/// the open.
fn open_gap_reason(open: Decimal, position: &OpenPosition) -> Option<ExitReason> {
    match position.direction {
        Direction::Long => {
            if open <= position.stop_price {
                Some(ExitReason::StopLoss)
            } else if position.take_profit_price.is_some_and(|tp| open >= tp) {
                Some(ExitReason::TakeProfit)
            } else {
                None
            }
        }
        Direction::Short => {
            if open >= position.stop_price {
                Some(ExitReason::StopLoss)
            } else if position.take_profit_price.is_some_and(|tp| open <= tp) {
                Some(ExitReason::TakeProfit)
            } else {
                None
            }
        }
    }
}

fn close_end_of_data(
    state: &mut LoopState,
    primary: &CandleSeries,
    funding_index: &[(i64, Decimal)],
    config: &BacktestConfig,
) -> Result<(), BacktestError> {
    let Some(last) = primary.candles.last() else {
        return Ok(());
    };
    if state.position.is_none() {
        return Ok(());
    }
    let exit = ExitFill {
        signal_time: last.close_time,
        fill_time: last.close_time,
        raw_price: last.close,
        reason: ExitReason::EndOfData,
    };
    close_position(state, funding_index, exit, config)
}

#[derive(Debug, Clone, Copy)]
struct ExitFill {
    signal_time: i64,
    fill_time: i64,
    raw_price: Decimal,
    reason: ExitReason,
}

fn price_exit(candle: &Candle, position: &OpenPosition) -> Option<ExitFill> {
    let exit = match position.take_profit_price {
        Some(tp) => resolve_intra_bar_exit(
            candle.open,
            candle.high,
            candle.low,
            position.stop_price,
            tp,
            position.direction,
        ),
        None => stop_only_exit(candle, position),
    }?;
    Some(ExitFill {
        signal_time: candle.open_time,
        fill_time: candle.open_time,
        raw_price: exit.price,
        reason: exit.reason,
    })
}

fn stop_only_exit(candle: &Candle, position: &OpenPosition) -> Option<IntraBarExit> {
    match position.direction {
        Direction::Long if candle.open <= position.stop_price => Some(IntraBarExit {
            reason: ExitReason::StopLoss,
            price: candle.open,
        }),
        Direction::Long if candle.low <= position.stop_price => Some(IntraBarExit {
            reason: ExitReason::StopLoss,
            price: position.stop_price,
        }),
        Direction::Short if candle.open >= position.stop_price => Some(IntraBarExit {
            reason: ExitReason::StopLoss,
            price: candle.open,
        }),
        Direction::Short if candle.high >= position.stop_price => Some(IntraBarExit {
            reason: ExitReason::StopLoss,
            price: position.stop_price,
        }),
        _ => None,
    }
}

fn close_position(
    state: &mut LoopState,
    funding_index: &[(i64, Decimal)],
    exit: ExitFill,
    config: &BacktestConfig,
) -> Result<(), BacktestError> {
    let Some(position) = state.position.take() else {
        return Ok(());
    };
    let exit_price = apply_slippage(
        exit.raw_price,
        config.slippage_bps,
        position.direction,
        Side::Exit,
    );
    let exit_fee = taker_fee(position.qty * exit_price, config.taker_fee_bps);
    let funding_total = funding_between(funding_index, &position, exit.fill_time);
    let fees_total = position.entry_fee + exit_fee;
    let slippage_total =
        position.entry_slippage + (exit.raw_price - exit_price).abs() * position.qty;
    let gross = realized_pnl(
        position.entry_price,
        exit_price,
        position.qty,
        position.direction,
    );
    // `gross` is computed from the *slipped* entry/exit fills, so slippage is
    // already embedded in it. `slippage_total` is a reporting figure only — do
    // NOT subtract it again here (that would double-count it).
    let net = gross + funding_total - fees_total;
    let realized_r = realized_r(
        position.entry_price,
        exit_price,
        position.stop_price,
        position.direction,
    )?;

    state.trades.push(Trade {
        direction: position.direction,
        qty: position.qty,
        entry_price: position.entry_price,
        exit_price,
        entry_signal_time: position.entry_signal_time,
        entry_fill_time: position.entry_fill_time,
        exit_signal_time: exit.signal_time,
        exit_fill_time: exit.fill_time,
        fills: vec![
            Fill {
                price: position.entry_price,
                qty: position.qty,
                time_ms: position.entry_fill_time,
                fee: position.entry_fee,
            },
            Fill {
                price: exit_price,
                qty: position.qty,
                time_ms: exit.fill_time,
                fee: exit_fee,
            },
        ],
        fees_total,
        funding_total,
        slippage_total,
        realized_pnl: net,
        realized_r,
        // The running excursion folded by `update_excursion` over every held bar
        // (entry-fill to exit-fill inclusive). For an `EndOfData` force-close the
        // final bar was already folded in the loop's last iteration before this
        // out-of-loop close runs, so it carries the correct excursion too (C6).
        mfe_r: position.mfe_r,
        mae_r: position.mae_r,
        exit_reason: exit.reason,
        source: TradeSource::Backtest,
        // The market regime captured at the entry-fill bar (FR-6), carried
        // through to the trade record for `RegimeBreakdown` aggregation.
        regime: position.regime,
        // The frozen stop level the position carried (r2.s2.w2) — recorded
        // verbatim so the trade log shows the risk the entry was sized against.
        stop_price: Some(position.stop_price),
    });
    Ok(())
}

/// Build the once-per-run funding-event index (D6, NFR-1).
///
/// `(open_time, rate)` for ONLY the funding-bearing candles (`funding_rate.is_some()`),
/// in source order. The store guarantees gap-free chronological-ascending
/// `open_time`, so filtering preserves that order ⇒ the index is sorted **by
/// construction** (no re-sort). A `debug_assert!` documents and checks the
/// ascending-`open_time` invariant the windowed binary search in
/// [`funding_between`] relies on. Built ONCE in `run_backtest` before the trade
/// loop and threaded by borrow through the close chain — never rebuilt per close.
fn build_funding_index(primary: &CandleSeries) -> Vec<(i64, Decimal)> {
    let index: Vec<(i64, Decimal)> = primary
        .candles
        .iter()
        .filter_map(|candle| candle.funding_rate.map(|rate| (candle.open_time, rate)))
        .collect();
    debug_assert!(
        index.windows(2).all(|w| w[0].0 <= w[1].0),
        "funding index must be ascending by open_time (store guarantees gap-free \
         chronological candles); windowed binary search depends on it",
    );
    index
}

/// Sum the per-event funding payments accrued over a position's holding window.
///
/// Windowed binary search over the precomputed funding-event index (D6): the
/// half-open `(entry_fill_time, exit_fill_time]` window is located with two
/// `partition_point` probes (`open_time > entry_fill_time` lower bound,
/// `open_time <= exit_fill_time` upper bound) — O(log E + k) instead of the old
/// O(candles) rescan. The fold is **byte-identical by construction**: it visits
/// the identical event set in the identical ascending order and computes
/// `funding_payment(rate, notional, direction)` per event with the SAME per-event
/// rounding as the prior `.filter(..).filter_map(..).map(..).sum()` chain.
/// `notional = qty * entry_price` stays strictly per-trade (entry-notional, G4);
/// it is NOT factored out into a size-scaled prefix-sum — that would reorder the
/// `Decimal` multiply/round/add sequence and break the 3.04 cross-arch hash.
fn funding_between(
    funding_index: &[(i64, Decimal)],
    position: &OpenPosition,
    exit_fill_time: i64,
) -> Decimal {
    let notional = position.qty * position.entry_price;
    // `(entry_fill_time, exit_fill_time]`: lower bound is the first event with
    // `open_time > entry_fill_time` (entry boundary EXCLUDED); upper bound is the
    // first event with `open_time > exit_fill_time` (exit boundary INCLUDED).
    let lo = funding_index.partition_point(|&(open_time, _)| open_time <= position.entry_fill_time);
    let hi = funding_index.partition_point(|&(open_time, _)| open_time <= exit_fill_time);
    funding_index[lo..hi]
        .iter()
        .map(|&(_, rate)| funding_payment(rate, notional, position.direction))
        .sum()
}

fn stop_rule(exits: &[CompiledExit]) -> Option<StopRule> {
    exits.iter().find_map(|exit| match exit {
        CompiledExit::StopLoss { distance_pct } => Some(StopRule::Pct(*distance_pct)),
        CompiledExit::AtrStop { period, multiple } => Some(StopRule::Atr {
            period: *period,
            multiple: *multiple,
        }),
        _ => None,
    })
}

fn take_profit_target(exits: &[CompiledExit]) -> Option<Decimal> {
    exits.iter().find_map(|exit| match exit {
        CompiledExit::TakeProfit { target_r } => Some(*target_r),
        _ => None,
    })
}

fn signal_exits(exits: &[CompiledExit]) -> Vec<&CompiledCondition> {
    exits
        .iter()
        .filter_map(|exit| match exit {
            CompiledExit::SignalExit { condition } => Some(condition),
            _ => None,
        })
        .collect()
}

/// Reject a short take-profit whose geometry resolves to a non-positive price.
///
/// A short TP sits at `entry · (1 − target_r · stop_distance_pct)`; once
/// `target_r · stop_distance_pct ≥ 1` that price is `≤ 0` and can never be reached
/// by positive market data, so the strategy would silently behave as if it had no
/// take-profit. Fail fast instead. (A long TP is `entry · (1 + …)`, always
/// positive, so this only applies to shorts.)
fn reject_impossible_short_tp(
    direction: Direction,
    take_profit_target_r: Option<Decimal>,
    stop_distance_pct: Decimal,
) -> Result<(), BacktestError> {
    if direction != Direction::Short {
        return Ok(());
    }
    let Some(target_r) = take_profit_target_r else {
        return Ok(());
    };
    if target_r * stop_distance_pct >= Decimal::ONE {
        return Err(BacktestError::ImpossibleTakeProfit(format!(
            "short take-profit at {target_r}R × stop {stop_distance_pct} \
             resolves to a non-positive price"
        )));
    }
    Ok(())
}

fn reject_unsupported(exits: &[CompiledExit]) -> Result<(), BacktestError> {
    for exit in exits {
        match exit {
            CompiledExit::TrailingStop { .. } => {
                return Err(BacktestError::UnsupportedExit("TrailingStop".to_owned()));
            }
            CompiledExit::TimeStop { .. } => {
                return Err(BacktestError::UnsupportedExit("TimeStop".to_owned()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// ATR-stop take-profit: `entry ± target_r × |entry − stop|` — the same 1R
/// distance a fixed-fraction stop uses, measured against the frozen ATR stop.
/// The short variant can resolve to a non-positive price (the plan-time
/// `reject_impossible_short_tp` cannot see the ATR distance), so the typed
/// check lives here at fill time (r2.s2.w2).
fn atr_take_profit_price(
    entry_price: Decimal,
    stop: Decimal,
    target_r: Decimal,
    direction: Direction,
) -> Result<Decimal, BacktestError> {
    let distance = (entry_price - stop).abs();
    let tp = match direction {
        Direction::Long => entry_price + target_r * distance,
        Direction::Short => entry_price - target_r * distance,
    };
    if tp <= Decimal::ZERO {
        return Err(BacktestError::ImpossibleTakeProfit(format!(
            "short take-profit at {target_r}R × ATR stop distance {distance} \
             resolves to a non-positive price"
        )));
    }
    Ok(tp)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        BacktestConfig, OpenPosition, build_funding_index, funding_between, run_backtest,
        update_excursion,
    };
    use crate::domain::{
        BacktestError, Candle, CandleSeries, Comparator, CompiledStrategy, Condition, DataVersion,
        Direction, ExitReason, ExitRule, Pair, PriceField, Regime, RiskParams, SchemaVersion,
        Series, SeriesEnd, StrategyDsl, SweepableValue, SymbolFilters, Timeframe, ValueSource,
        compile, realized_pnl, validate,
    };
    use proptest::prelude::*;
    use rust_decimal::Decimal;

    fn d(value: i64) -> Decimal {
        Decimal::new(value, 0)
    }

    fn rate(mantissa: i64, scale: u32) -> Decimal {
        Decimal::new(mantissa, scale)
    }

    fn config() -> BacktestConfig {
        BacktestConfig {
            starting_equity: d(10_000),
            taker_fee_bps: Decimal::ZERO,
            slippage_bps: Decimal::ZERO,
        }
    }

    fn candle(idx: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
        Candle {
            open_time: idx * 60_000,
            close_time: idx * 60_000 + 59_999,
            open: d(open),
            high: d(high),
            low: d(low),
            close: d(close),
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    /// `candle` with `Decimal` OHLC (for the excursion proptest, which builds bars
    /// at sub-integer offsets from the entry price).
    fn candle_dec(idx: i64, open: Decimal, high: Decimal, low: Decimal, close: Decimal) -> Candle {
        Candle {
            open_time: idx * 60_000,
            close_time: idx * 60_000 + 59_999,
            open,
            high,
            low,
            close,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    fn funding_candle(idx: i64, open: i64, high: i64, low: i64, close: i64) -> Candle {
        Candle {
            funding_rate: Some(rate(1, 3)),
            ..candle(idx, open, high, low, close)
        }
    }

    fn series(candles: Vec<Candle>) -> CandleSeries {
        CandleSeries {
            pair: Pair::new("BTCUSDT"),
            timeframe: Timeframe::M15,
            version: DataVersion::new("test"),
            candles,
        }
    }

    fn price_entry() -> Condition {
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        }
    }

    fn never_signal() -> Condition {
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Lt,
            rhs: ValueSource::Constant {
                value: Decimal::ZERO,
            },
        }
    }

    fn signal_on_high_close() -> Condition {
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant { value: d(150) },
        }
    }

    fn stop() -> ExitRule {
        ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(rate(5, 2)),
        }
    }

    fn tp(target_r: i64) -> ExitRule {
        ExitRule::TakeProfit {
            target_r: SweepableValue::Fixed(d(target_r)),
        }
    }

    fn compiled_dir(
        entry: Condition,
        exits: Vec<ExitRule>,
        direction: Direction,
    ) -> CompiledStrategy {
        let dsl = StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "test strategy".to_owned(),
            direction,
            entry,
            filters: vec![],
            exits,
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(rate(1, 2)),
                max_leverage: SweepableValue::Fixed(d(3)),
            },
        };
        compile(&validate(&dsl).unwrap()).unwrap()
    }

    fn compiled(entry: Condition, exits: Vec<ExitRule>) -> CompiledStrategy {
        compiled_dir(entry, exits, Direction::Long)
    }

    fn base_strategy() -> CompiledStrategy {
        compiled(price_entry(), vec![stop(), tp(10)])
    }

    #[test]
    fn entry_fills_at_next_bar_open_not_signal_bar_close() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 110, 112, 108, 111),
            candle(2, 120, 121, 119, 120),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        let trade = &result.trades[0];
        assert_eq!(trade.entry_signal_time, primary.candles[1].close_time);
        assert_eq!(trade.entry_fill_time, primary.candles[2].open_time);
        assert_eq!(trade.entry_price, d(120));
    }

    #[test]
    fn pure_price_strategy_does_not_enter_on_bar_zero() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert!(result.trades.is_empty());
    }

    #[test]
    fn stop_wins_when_entry_bar_straddles_stop_and_take_profit() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 120, 90, 100),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades[0].exit_reason, ExitReason::StopLoss);
        assert_eq!(result.trades[0].exit_price, d(95));
    }

    #[test]
    fn held_position_accrues_one_positive_long_funding_payment() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 101, 99, 100),
            funding_candle(3, 100, 101, 99, 100),
            candle(4, 100, 101, 99, 100),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].funding_total, d(-2));
        assert_eq!(result.funding_total, d(-2));
    }

    #[test]
    fn entry_at_funding_bar_open_excludes_that_boundary() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            funding_candle(2, 100, 101, 99, 100),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].funding_total, Decimal::ZERO);
    }

    #[test]
    fn intra_bar_exit_on_funding_bar_includes_that_boundary() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 101, 99, 100),
            funding_candle(3, 100, 101, 94, 100),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].exit_reason, ExitReason::StopLoss);
        assert_eq!(result.trades[0].funding_total, d(-2));
    }

    /// A funding candle carrying an explicit (per-bar distinct) rate, so the
    /// index-order and per-event-fold assertions below are non-degenerate.
    fn funding_candle_rate(idx: i64, funding: Decimal) -> Candle {
        Candle {
            funding_rate: Some(funding),
            ..candle(idx, 100, 101, 99, 100)
        }
    }

    /// Reference implementation: the PRE-3.05 O(candles) full-rescan fold, exactly
    /// as `funding_between` was written before the index refactor. The new
    /// windowed-binary-search `funding_between` MUST equal this bit-for-bit for any
    /// window — that equality is the byte-identity contract (D6).
    fn funding_between_full_rescan(
        primary: &CandleSeries,
        position: &OpenPosition,
        exit_fill_time: i64,
    ) -> Decimal {
        let notional = position.qty * position.entry_price;
        primary
            .candles
            .iter()
            .filter(|candle| {
                candle.open_time > position.entry_fill_time && candle.open_time <= exit_fill_time
            })
            .filter_map(|candle| candle.funding_rate)
            .map(|rate| super::funding_payment(rate, notional, position.direction))
            .sum()
    }

    fn position_at(entry_fill_time: i64, direction: Direction) -> OpenPosition {
        OpenPosition {
            direction,
            qty: d(3),
            entry_price: d(100),
            stop_price: d(95),
            take_profit_price: None,
            entry_signal_time: 0,
            entry_fill_time,
            entry_fee: Decimal::ZERO,
            entry_slippage: Decimal::ZERO,
            mfe_r: Decimal::ZERO,
            mae_r: Decimal::ZERO,
            regime: Regime::Unknown,
        }
    }

    /// D6 unit coverage: (a) the funding-event index contains EXACTLY the
    /// funding-bearing candles, in ascending `open_time` order; and (b) the new
    /// windowed binary-search `funding_between` equals the old O(candles)
    /// full-rescan fold bit-for-bit for representative `(entry, exit]` windows —
    /// including the boundary-exclusion (entry) / boundary-inclusion (exit) edges,
    /// the empty window, and both directions. This is the in-slice money-math
    /// proof that the perf refactor is byte-identical (NFR-2).
    #[test]
    fn funding_index_contents_and_windowed_fold_match_full_rescan() {
        // open_time = idx * 60_000 (see `candle`). Funding on bars 1, 3, 4, 6;
        // plain bars at 0, 2, 5 must be excluded from the index. Distinct rates
        // (including a negative one) make order + per-event arithmetic load-bearing.
        let r1 = rate(1, 3); // 0.001
        let r3 = rate(2, 3); // 0.002
        let r4 = rate(-5, 4); // -0.0005
        let r6 = rate(3, 3); // 0.003
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            funding_candle_rate(1, r1),
            candle(2, 100, 101, 99, 100),
            funding_candle_rate(3, r3),
            funding_candle_rate(4, r4),
            candle(5, 100, 101, 99, 100),
            funding_candle_rate(6, r6),
        ]);

        // (a) index = exactly the funding-bearing candles, in ascending open_time.
        let index = build_funding_index(&primary);
        assert_eq!(
            index,
            vec![
                (60_000, r1),
                (3 * 60_000, r3),
                (4 * 60_000, r4),
                (6 * 60_000, r6),
            ],
            "index must hold exactly the funding-bearing candles, in source (ascending) order"
        );
        assert!(
            index.windows(2).all(|w| w[0].0 < w[1].0),
            "index open_times must be strictly ascending"
        );

        // (b) windowed fold == full-rescan fold, over representative windows and
        // both directions. Each window is `(entry_fill_time, exit_fill_time]`.
        let windows = [
            (0, 6 * 60_000),          // whole series: all four events
            (60_000, 4 * 60_000), // entry ON a funding bar (1 EXCLUDED), exit ON one (4 INCLUDED): {3,4}
            (3 * 60_000, 6 * 60_000), // {4,6}
            (4 * 60_000, 5 * 60_000), // exit between events, after the last in-range one: {}
            (6 * 60_000, 9 * 60_000), // entry at/after the last event: {} (empty upper tail)
            (0, 0),               // degenerate empty window
        ];
        for direction in [Direction::Long, Direction::Short] {
            for &(entry_fill_time, exit_fill_time) in &windows {
                let position = position_at(entry_fill_time, direction);
                let windowed = funding_between(&index, &position, exit_fill_time);
                let rescan = funding_between_full_rescan(&primary, &position, exit_fill_time);
                assert_eq!(
                    windowed, rescan,
                    "windowed fold must equal full-rescan fold byte-for-byte \
                     (dir={direction:?}, window=({entry_fill_time}, {exit_fill_time}])"
                );
            }
        }

        // Spot-check a concrete value so the test is not purely self-referential:
        // long over the whole series folds -(r1+r3+r4+r6) * notional per event.
        let long_whole = funding_between(&index, &position_at(0, Direction::Long), 6 * 60_000);
        let notional = d(3) * d(100);
        let expected = -(r1 * notional) - (r3 * notional) - (r4 * notional) - (r6 * notional);
        assert_eq!(
            long_whole, expected,
            "long funding folds -rate*notional per event, in order"
        );
    }

    #[test]
    fn stopless_strategy_errors_before_iteration() {
        let primary = series(vec![candle(0, 100, 101, 99, 100)]);
        let strategy = compiled(
            price_entry(),
            vec![ExitRule::SignalExit {
                condition: never_signal(),
            }],
        );

        let err = run_backtest(
            &strategy,
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap_err();
        assert_eq!(err, BacktestError::NoStopLoss);
    }

    #[test]
    fn trailing_and_time_exits_are_rejected() {
        let primary = series(vec![candle(0, 100, 101, 99, 100)]);
        let trailing = compiled(
            price_entry(),
            vec![
                stop(),
                ExitRule::TrailingStop {
                    trail_pct: SweepableValue::Fixed(rate(5, 2)),
                },
            ],
        );
        let time = compiled(
            price_entry(),
            vec![
                stop(),
                ExitRule::TimeStop {
                    max_bars: SweepableValue::Fixed(5),
                },
            ],
        );

        assert!(matches!(
            run_backtest(
                &trailing,
                &primary,
                None,
                &config(),
                &SymbolFilters::unconstrained(),
                SeriesEnd::SnapshotEnd,
            )
            .unwrap_err(),
            BacktestError::UnsupportedExit(_)
        ));
        assert!(matches!(
            run_backtest(
                &time,
                &primary,
                None,
                &config(),
                &SymbolFilters::unconstrained(),
                SeriesEnd::SnapshotEnd,
            )
            .unwrap_err(),
            BacktestError::UnsupportedExit(_)
        ));
    }

    /// r2.s2.w2: an `AtrStop` strategy is modelled — the run no longer refuses
    /// it. A single candle can't warm the ATR (period 14 seeds at index 14), so
    /// the run produces no trades but no error.
    #[test]
    fn atr_stop_runs_without_typed_refusal() {
        let primary = series(vec![candle(0, 100, 101, 99, 100)]);
        let strategy = compiled(
            price_entry(),
            vec![ExitRule::AtrStop {
                period: SweepableValue::Fixed(14),
                multiple: SweepableValue::Fixed(d(2)),
            }],
        );

        let result = run_backtest(
            &strategy,
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .expect("atr stop is modelled since r2.s2.w2");
        assert!(result.trades.is_empty());
    }

    #[test]
    fn open_position_at_series_end_is_force_closed() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 101, 99, 103),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].exit_reason, ExitReason::EndOfData);
        assert_eq!(result.trades[0].exit_price, d(103));
        assert_eq!(
            result.open_position, None,
            "a SnapshotEnd run books the close as a trade — no mark"
        );
    }

    /// G1 ruling (b): the SAME still-open position under `WindowEdge` is not a
    /// trade — but it is not silently dropped either. The run record carries it
    /// as an explicit [`OpenPositionMark`]: direction, entry fill and size as
    /// opened, marked at the last in-window candle's close.
    #[test]
    fn a_position_open_at_a_window_edge_is_marked_not_closed() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 101, 99, 103),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::WindowEdge,
        )
        .unwrap();

        // No fabricated exit: the position never became a trade.
        assert!(result.trades.is_empty(), "trades were: {:?}", result.trades);
        assert_eq!(result.net_pnl, Decimal::ZERO);
        assert_eq!(result.summary.trade_count, 0);

        // The mark says exactly what the strategy is holding at the edge.
        let mark = result
            .open_position
            .expect("a window-edge run that ends holding carries the mark");
        assert_eq!(mark.direction, Direction::Long);
        assert_eq!(mark.qty, d(20), "1% of 10_000 at a 5% stop on 100");
        assert_eq!(mark.entry_price, d(100), "filled at bar 2's open");
        assert_eq!(mark.entry_signal_time, 119_999, "bar 1's close_time");
        assert_eq!(mark.entry_fill_time, 120_000, "bar 2's open_time");
        assert_eq!(mark.mark_time, 179_999, "bar 2's close_time");
        assert_eq!(mark.mark_price, d(103), "bar 2's close");
    }

    /// A `WindowEdge` run that ends FLAT carries no mark — `None` is the honest
    /// "nothing left open", distinguishable from a mark by construction.
    #[test]
    fn a_window_edge_run_ending_flat_carries_no_mark() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
        ]);
        let result = run_backtest(
            &compiled(never_signal(), vec![stop()]),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::WindowEdge,
        )
        .unwrap();
        assert!(result.trades.is_empty());
        assert_eq!(result.open_position, None);
    }

    #[test]
    fn pending_entry_on_final_bar_is_dropped() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert!(result.trades.is_empty());
    }

    #[test]
    fn signal_exit_fills_at_next_bar_open_when_no_price_exit_preempts() {
        let strategy = compiled(
            price_entry(),
            vec![
                stop(),
                tp(10),
                ExitRule::SignalExit {
                    condition: signal_on_high_close(),
                },
            ],
        );
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 101, 99, 160),
            candle(3, 105, 106, 104, 105),
        ]);
        let result = run_backtest(
            &strategy,
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        assert_eq!(result.trades[0].exit_reason, ExitReason::Signal);
        assert_eq!(
            result.trades[0].exit_signal_time,
            primary.candles[2].close_time
        );
        assert_eq!(
            result.trades[0].exit_fill_time,
            primary.candles[3].open_time
        );
        assert_eq!(result.trades[0].exit_price, d(105));
    }

    /// #44 (HIGH) regression — the named scenario (AC-10). A signal-exit fired on
    /// bar N's close is scheduled to fill at bar N+1's open. On bar N+1 the open
    /// sits **inside** the stop/TP channel, but the bar's intra-bar **low later
    /// reaches the stop**. The fix: the position exits as `Signal` at the **open
    /// price**, NOT as `StopLoss` at the stop — the intra-bar post-open SL/TP
    /// cannot preempt a signal-exit filling at the open (C6). Before the fix the
    /// old ordering ran `price_exit` first and mislabeled this as a `StopLoss`.
    #[test]
    fn signal_exit_fills_at_open_even_when_intrabar_stop_is_touched() {
        let strategy = compiled(
            price_entry(),
            vec![
                stop(),
                tp(10),
                ExitRule::SignalExit {
                    condition: signal_on_high_close(),
                },
            ],
        );
        // bar2: entry fills at open=100 → stop=95, tp=150. close=160 fires the
        //       signal-exit (close > 150) without any intra-bar level breach
        //       (high 101 < tp, low 99 > stop). pending_exit is set.
        // bar3: the exit bar. open=100 is inside the channel (95 < 100 < 150), but
        //       the intra-bar low=90 dips through the stop (95). The #44 fix must
        //       fill at the open as Signal, ignoring the intra-bar stop.
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 101, 99, 160),
            candle(3, 100, 101, 90, 100),
        ]);
        let result = run_backtest(
            &strategy,
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        let trade = &result.trades[0];
        assert_eq!(
            trade.exit_reason,
            ExitReason::Signal,
            "a signal-exit at the open is NOT preempted by the bar's intra-bar stop (#44)"
        );
        assert_eq!(
            trade.exit_price,
            d(100),
            "fills at bar N+1's open price, not at the stop"
        );
        assert_eq!(
            trade.exit_fill_time, primary.candles[3].open_time,
            "fills at bar N+1's open time"
        );
        assert_eq!(
            trade.exit_signal_time, primary.candles[2].close_time,
            "the inside-channel signal-exit keeps the prior bar-close signal time"
        );
    }

    /// #44 symmetric gap labeling (C6): when the exit bar's **open** gaps above the
    /// take-profit, a signal-exit filling at the open is labeled `TakeProfit` (the
    /// price event at the open), still at the open price — only the reason differs.
    #[test]
    fn signal_exit_open_gapping_through_tp_is_labeled_take_profit() {
        let strategy = compiled(
            price_entry(),
            vec![
                stop(),
                tp(10),
                ExitRule::SignalExit {
                    condition: signal_on_high_close(),
                },
            ],
        );
        // bar2: entry at open=100 → stop=95, tp=150; close=160 fires the signal.
        // bar3: open=160 gaps above tp (150) → labeled TakeProfit, fills at open.
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 101, 99, 160),
            candle(3, 160, 161, 159, 160),
        ]);
        let result = run_backtest(
            &strategy,
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        let trade = &result.trades[0];
        assert_eq!(trade.exit_reason, ExitReason::TakeProfit);
        assert_eq!(trade.exit_price, d(160), "fills at the gapped-open price");
        assert_eq!(trade.exit_signal_time, primary.candles[3].open_time);
    }

    /// #44 symmetric gap labeling (C6), stop side: an exit-bar **open** gapping
    /// below the stop on a signal-exit is labeled `StopLoss`, filling at the open.
    /// (This is the open gap — distinct from the intra-bar stop the fix ignores.)
    #[test]
    fn signal_exit_open_gapping_through_stop_is_labeled_stop_loss() {
        let strategy = compiled(
            price_entry(),
            vec![
                stop(),
                tp(10),
                ExitRule::SignalExit {
                    condition: signal_on_high_close(),
                },
            ],
        );
        // bar2: entry at open=100 → stop=95; close=160 fires the signal.
        // bar3: open=90 gaps below stop (95) → labeled StopLoss, fills at open=90.
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 101, 99, 160),
            candle(3, 90, 95, 89, 92),
        ]);
        let result = run_backtest(
            &strategy,
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        let trade = &result.trades[0];
        assert_eq!(trade.exit_reason, ExitReason::StopLoss);
        assert_eq!(trade.exit_price, d(90), "fills at the gapped-open price");
        assert_eq!(trade.exit_signal_time, primary.candles[3].open_time);
    }

    /// C6 audit: an `EndOfData` force-closed trade carries non-default
    /// `mfe_r`/`mae_r` (AC-10b). The final bar is folded by `update_excursion`
    /// in the loop's last iteration before the out-of-loop `close_end_of_data`
    /// runs, so the running excursion reaches the recorded trade. Guards against a
    /// future refactor that closes outside the folded path.
    #[test]
    fn end_of_data_trade_carries_running_mfe_and_mae_excursion() {
        // Entry at bar2 open=100 → stop=95, stop_distance=5. The held bars swing
        // up to 110 (fav = (110-100)/5 = 2R) and down to 96 (adv = (96-100)/5 =
        // -0.8R) WITHOUT touching the stop (95) or tp (150), so the position is
        // still open at series end and force-closes as EndOfData.
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 110, 96, 105),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        let trade = &result.trades[0];
        assert_eq!(trade.exit_reason, ExitReason::EndOfData);
        // Non-default (non-zero) excursion proves the EndOfData close folded the
        // running values, not the Trade-literal defaults.
        assert_ne!(trade.mfe_r, Decimal::ZERO, "EndOfData trade carries MFE");
        assert_ne!(trade.mae_r, Decimal::ZERO, "EndOfData trade carries MAE");
        // Exact excursion from the single held (entry+exit) bar.
        assert_eq!(trade.mfe_r, d(2), "(110-100)/5 = 2R favorable");
        assert_eq!(
            trade.mae_r,
            Decimal::new(-8, 1),
            "(96-100)/5 = -0.8R adverse"
        );
    }

    /// G1: a `[from, to)` window edge is not end-of-data. The same series that
    /// force-closes under `SnapshotEnd` must emit NO fabricated trade under
    /// `WindowEdge` — the engine cannot know whether the real market had more
    /// bars, so closing at the window edge would invent a trade, its P&L, and
    /// its exit timestamp.
    #[test]
    fn window_edge_leaves_a_still_open_position_unclosed() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 110, 96, 105),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::WindowEdge,
        )
        .unwrap();
        assert!(
            result.trades.is_empty(),
            "window edge must not fabricate an EndOfData trade"
        );

        // Contrast on the same series: the snapshot-end interpretation still
        // force-closes — `SeriesEnd` is the only input that differs.
        let closed = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();
        assert_eq!(closed.trades[0].exit_reason, ExitReason::EndOfData);
    }

    /// C5 invariant on a real run: every completed trade satisfies
    /// `mfe_r >= 0 ∧ mae_r <= 0` (holds by the init-0 running sample). A direct
    /// engine-level check complementing the golden-fixture assertion.
    #[test]
    fn completed_trades_have_nonneg_mfe_nonpos_mae() {
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 130, 90, 100),
            candle(3, 100, 101, 99, 100),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert!(!result.trades.is_empty());
        for trade in &result.trades {
            assert!(trade.mfe_r >= Decimal::ZERO, "mfe_r must be >= 0");
            assert!(trade.mae_r <= Decimal::ZERO, "mae_r must be <= 0");
        }
    }

    proptest! {
        /// C5 invariant proptest: over randomized synthetic OHLC bars (long and
        /// short), the running excursion sample keeps `mfe_r >= 0 ∧ mae_r <= 0` for
        /// any sequence of held bars (the init-0 sample guarantees it regardless of
        /// price path). Operates directly on `update_excursion` to exercise the
        /// math over arbitrary candles without a full strategy harness.
        #[test]
        fn prop_excursion_invariant_holds_over_arbitrary_bars(
            is_long in any::<bool>(),
            entry_cents in 50_000i64..200_000,
            stop_off in 1i64..40_000,
            bars in proptest::collection::vec(
                (0i64..50_000, 0i64..50_000, 0i64..50_000),
                1..12,
            ),
        ) {
            let entry = Decimal::new(entry_cents, 2);
            let direction = if is_long { Direction::Long } else { Direction::Short };
            // Stop on the losing side; distance is strictly positive.
            let stop = if is_long {
                entry - Decimal::new(stop_off, 2)
            } else {
                entry + Decimal::new(stop_off, 2)
            };
            let mut position = OpenPosition {
                direction,
                qty: Decimal::ONE,
                entry_price: entry,
                stop_price: stop,
                take_profit_price: None,
                entry_signal_time: 0,
                entry_fill_time: 0,
                entry_fee: Decimal::ZERO,
                entry_slippage: Decimal::ZERO,
                mfe_r: Decimal::ZERO,
                mae_r: Decimal::ZERO,
                regime: Regime::Unknown,
            };
            for (lo_off, span, up_off) in &bars {
                // Build a coherent OHLC bar around the entry price: low <= open,
                // close <= high; low <= high by construction. The bar index is
                // irrelevant to the excursion math (it reads OHLC only), so a
                // fixed index is fine here.
                let low = entry - Decimal::new(*lo_off, 2);
                let high = low + Decimal::new(*span, 2) + Decimal::new(*up_off, 2);
                let bar = candle_dec(0, entry, high, low, entry);
                update_excursion(&mut position, &bar);
            }
            prop_assert!(position.mfe_r >= Decimal::ZERO, "mfe_r must be >= 0");
            prop_assert!(position.mae_r <= Decimal::ZERO, "mae_r must be <= 0");
        }
    }

    #[test]
    fn config_validate_rejects_out_of_range_cost_knobs() {
        let ok = BacktestConfig {
            starting_equity: d(10_000),
            taker_fee_bps: d(4),
            slippage_bps: Decimal::ONE,
        };
        assert!(ok.validate().is_ok());
        // Zero/negative equity (the sizing denominator).
        assert!(matches!(
            BacktestConfig {
                starting_equity: Decimal::ZERO,
                ..ok
            }
            .validate(),
            Err(BacktestError::InvalidConfig(_))
        ));
        // Negative fee / slippage, and a rate >= 100% (10_000 bps).
        assert!(
            BacktestConfig {
                taker_fee_bps: d(-1),
                ..ok
            }
            .validate()
            .is_err()
        );
        assert!(
            BacktestConfig {
                slippage_bps: d(10_000),
                ..ok
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn run_backtest_rejects_invalid_config_at_the_engine_boundary() {
        // A non-CLI caller passing a bad config must be rejected by the engine
        // itself, not just by the CLI guard.
        let bad = BacktestConfig {
            starting_equity: Decimal::ZERO,
            taker_fee_bps: Decimal::ZERO,
            slippage_bps: Decimal::ZERO,
        };
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
        ]);
        let err = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &bad,
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap_err();
        assert!(matches!(err, BacktestError::InvalidConfig(_)));
    }

    #[test]
    fn short_take_profit_resolving_nonpositive_is_rejected() {
        // Short, 50% stop (0.5) × 3R target → tp = entry·(1 − 1.5) < 0, a price the
        // market can never reach. The plan must reject it, not silently drop the TP.
        let strategy = compiled_dir(
            price_entry(),
            vec![
                ExitRule::StopLoss {
                    distance_pct: SweepableValue::Fixed(rate(5, 1)), // 0.5 = 50%
                },
                tp(3),
            ],
            Direction::Short,
        );
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
        ]);

        let err = run_backtest(
            &strategy,
            &primary,
            None,
            &config(),
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap_err();
        assert!(matches!(err, BacktestError::ImpossibleTakeProfit(_)));
    }

    #[test]
    fn short_take_profit_with_reachable_target_is_accepted() {
        // Short, 5% stop × 2R target → tp = entry·(1 − 0.10) = 0.9·entry > 0, fine.
        let strategy = compiled_dir(price_entry(), vec![stop(), tp(2)], Direction::Short);
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
        ]);
        // Must not error at plan construction (it may simply produce no trades).
        assert!(
            run_backtest(
                &strategy,
                &primary,
                None,
                &config(),
                &SymbolFilters::unconstrained(),
                SeriesEnd::SnapshotEnd,
            )
            .is_ok()
        );
    }

    #[test]
    fn net_pnl_does_not_double_count_slippage() {
        // Slippage is embedded in the slipped entry/exit fills, so `gross` already
        // reflects it; `net` must NOT subtract `slippage_total` a second time.
        // Invariant under test: net == gross(of the recorded fills) + funding - fees.
        let cfg = BacktestConfig {
            starting_equity: d(10_000),
            taker_fee_bps: d(4),
            slippage_bps: d(10),
        };
        let primary = series(vec![
            candle(0, 100, 101, 99, 100),
            candle(1, 100, 101, 99, 100),
            candle(2, 100, 105, 99, 100),
            funding_candle(3, 100, 105, 99, 103),
        ]);
        let result = run_backtest(
            &base_strategy(),
            &primary,
            None,
            &cfg,
            &SymbolFilters::unconstrained(),
            SeriesEnd::SnapshotEnd,
        )
        .unwrap();

        assert_eq!(result.trades.len(), 1);
        let trade = &result.trades[0];
        // The assertion is only meaningful if slippage is genuinely nonzero.
        assert!(trade.slippage_total > Decimal::ZERO);

        let gross = realized_pnl(
            trade.entry_price,
            trade.exit_price,
            trade.qty,
            trade.direction,
        );
        assert_eq!(
            trade.realized_pnl,
            gross + trade.funding_total - trade.fees_total,
            "net P&L must embed slippage via the fills only, not subtract it twice"
        );
    }
}
