//! The one-sided setup state machine (r6.s2.w2, ADR-0033).
//!
//! A setup side arms on the primary bar where a higher-timeframe bar closed and
//! its `arm` holds, waits for its `dip`, fires on its `trigger` (together with
//! the strategy's shared entry gate), and lapses `expiry_bars` bars after the
//! arm bar. [`step_side`] is the one pure step: it reads the evaluation context
//! of the bar just stepped and advances a [`SideState`]. The engine session
//! calls it on every bar — lead-in bars included — so a walk-forward fold that
//! starts after an arm still enters the same trade as a full run.

use rust_decimal::Decimal;
use serde::Serialize;

use crate::domain::{Candle, CompiledCondition, CompiledSetupSide, Direction, EvalContext};

/// Where a side stands (ADR-0033).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupPhase {
    /// No arm is live.
    #[default]
    Idle,
    /// The arm held; the side waits for its dip.
    Armed,
    /// The dip held; the side waits for its trigger.
    Dipped,
}

/// What happened to a side on the last stepped bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SetupEvent {
    /// The side armed (or re-armed) on this bar.
    Armed,
    /// The dip held on this bar.
    Dipped,
    /// The trigger held on this bar and the side fired.
    Triggered,
    /// The side lapsed on this bar.
    Expired,
}

/// A fired side: everything the fill and the trade record need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupFire {
    /// The side that fired.
    pub direction: Direction,
    /// The close time of the arm bar.
    pub arm_time: i64,
    /// The close time of the dip bar.
    pub dip_time: i64,
    /// The close time of the stop-anchor bar, when one was found.
    pub anchor_time: Option<i64>,
    /// The close time of the trigger bar.
    pub trigger_time: i64,
    /// The stop window's extreme: lowest low (long) or highest high (short), from the
    /// anchor bar when one was found, else from the bar after the arm bar, through the
    /// trigger bar inclusive. w4 turns it into the structure stop.
    pub extreme: Decimal,
}

/// One side's state. `Copy`, so the step frame can restore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SideState {
    /// Where the side stands.
    pub phase: SetupPhase,
    /// The session's index of the arm bar.
    pub arm_bar: usize,
    /// The close time of the arm bar.
    pub arm_time: i64,
    /// The close time of the dip bar, once the dip held.
    pub dip_time: Option<i64>,
    /// The close time of the stop-anchor bar, once the anchor held.
    pub anchor_time: Option<i64>,
    /// The stop window's extreme folded from the anchor bar.
    pub anchor_extreme: Option<Decimal>,
    /// The stop window's extreme folded from the bar after the arm bar.
    pub fallback_extreme: Option<Decimal>,
    /// The last fire of this side; survives a re-arm.
    pub last_trigger: Option<SetupFire>,
    /// What happened on the last stepped bar.
    pub event: Option<SetupEvent>,
}

/// Both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SetupState {
    /// The long side.
    pub long: SideState,
    /// The short side.
    pub short: SideState,
}

/// The stop window's fold: `min` for a long side, `max` for a short side.
fn fold_extreme(direction: Direction, current: Option<Decimal>, x: Decimal) -> Decimal {
    match (direction, current) {
        (_, None) => x,
        (Direction::Long, Some(c)) => c.min(x),
        (Direction::Short, Some(c)) => c.max(x),
    }
}

/// Step one side over the bar just stepped. `bar` is the session's index of this bar
/// (`EngineSession::bars` before its increment); `htf_closed` is `!closed_htf.is_empty()`;
/// `warm` is `entry_gate_warm()`; `gate` is the strategy's `entry ∧ filters`.
#[allow(clippy::too_many_arguments)]
pub fn step_side(
    state: &mut SideState,
    side: &CompiledSetupSide,
    gate: &CompiledCondition,
    ctx: &dyn EvalContext,
    primary: &Candle,
    bar: usize,
    htf_closed: bool,
    warm: bool,
    expiry_bars: u32,
    direction: Direction,
) -> Option<SetupFire> {
    // 1. The event describes this bar only.
    state.event = None;

    // 2. Arm. A re-arm drops the dip, the anchor and both extremes; the arm
    // bar never counts as a dip, an anchor or a stop-window bar.
    if htf_closed && warm && side.arm.eval(ctx) {
        *state = SideState {
            phase: SetupPhase::Armed,
            arm_bar: bar,
            arm_time: primary.close_time,
            last_trigger: state.last_trigger,
            event: Some(SetupEvent::Armed),
            ..SideState::default()
        };
        return None;
    }

    // 3. Nothing is live.
    if state.phase == SetupPhase::Idle {
        return None;
    }

    // 4. Expiry: the bar `arm_bar + expiry_bars` still triggers; the next one lapses.
    if bar > state.arm_bar + expiry_bars as usize {
        state.phase = SetupPhase::Idle;
        state.event = Some(SetupEvent::Expired);
        return None;
    }

    // 5. Stop window.
    let x = match direction {
        Direction::Long => primary.low,
        Direction::Short => primary.high,
    };
    state.fallback_extreme = Some(fold_extreme(direction, state.fallback_extreme, x));
    if state.anchor_time.is_some() {
        state.anchor_extreme = Some(fold_extreme(direction, state.anchor_extreme, x));
    } else if let Some(anchor) = side.stop_anchor.as_ref()
        && anchor.eval(ctx)
    {
        state.anchor_time = Some(primary.close_time);
        state.anchor_extreme = Some(x);
    }

    // 6. Dip. The trigger must come on a later bar.
    if state.phase == SetupPhase::Armed {
        if side.dip.eval(ctx) {
            state.phase = SetupPhase::Dipped;
            state.dip_time = Some(primary.close_time);
            state.event = Some(SetupEvent::Dipped);
        }
        return None;
    }

    // 7. Trigger (`phase == Dipped`).
    if side.trigger.eval(ctx) && gate.eval(ctx) {
        let dip_time = state.dip_time?;
        let extreme = state.anchor_extreme.or(state.fallback_extreme)?;
        let fire = SetupFire {
            direction,
            arm_time: state.arm_time,
            dip_time,
            anchor_time: state.anchor_time,
            trigger_time: primary.close_time,
            extreme,
        };
        state.phase = SetupPhase::Idle;
        state.last_trigger = Some(fire);
        state.event = Some(SetupEvent::Triggered);
        return Some(fire);
    }

    // 8.
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::domain::{Comparator, CompiledValue, PriceField, Series};

    fn dec(s: &str) -> Decimal {
        s.parse::<Decimal>().expect("decimal literal")
    }

    /// A fake context: every `Price` leaf reads the bar's close.
    struct CloseCtx(Decimal);

    impl EvalContext for CloseCtx {
        fn current(&self, value: &CompiledValue) -> Option<Decimal> {
            match value {
                CompiledValue::Price { .. } => Some(self.0),
                CompiledValue::Const(k) => Some(*k),
                _ => None,
            }
        }
        fn previous(&self, _value: &CompiledValue) -> Option<Decimal> {
            None
        }
    }

    fn close_is(op: Comparator, k: &str) -> CompiledCondition {
        CompiledCondition::Compare {
            lhs: CompiledValue::Price {
                series: Series::Primary,
                field: PriceField::Close,
                lag: 0,
            },
            op,
            rhs: CompiledValue::Const(dec(k)),
        }
    }

    /// arm: close == 100, dip: close == 98, trigger: close == 102, anchor: close == 97.
    fn side(with_anchor: bool) -> CompiledSetupSide {
        CompiledSetupSide {
            arm: close_is(Comparator::Eq, "100"),
            dip: close_is(Comparator::Eq, "98"),
            trigger: close_is(Comparator::Eq, "102"),
            stop_anchor: with_anchor.then(|| close_is(Comparator::Eq, "97")),
        }
    }

    fn gate() -> CompiledCondition {
        close_is(Comparator::Gt, "0")
    }

    fn candle(i: i64, close: &str, low: &str, high: &str) -> Candle {
        let ms = 900_000;
        Candle {
            open_time: i * ms,
            close_time: (i + 1) * ms - 1,
            open: dec(close),
            high: dec(high),
            low: dec(low),
            close: dec(close),
            volume: dec("1"),
            funding_rate: None,
        }
    }

    fn close_time(i: i64) -> i64 {
        (i + 1) * 900_000 - 1
    }

    /// Step `state` over one bar at index `i` with `close` and a flat range.
    #[allow(clippy::too_many_arguments)]
    fn step(
        state: &mut SideState,
        side: &CompiledSetupSide,
        gate_condition: &CompiledCondition,
        i: usize,
        close: &str,
        low: &str,
        high: &str,
        htf_closed: bool,
        warm: bool,
        expiry_bars: u32,
        direction: Direction,
    ) -> Option<SetupFire> {
        let primary = candle(i64::try_from(i).unwrap(), close, low, high);
        step_side(
            state,
            side,
            gate_condition,
            &CloseCtx(dec(close)),
            &primary,
            i,
            htf_closed,
            warm,
            expiry_bars,
            direction,
        )
    }

    /// A long-side step with the default flags: HTF closed, warm, expiry 8.
    fn long(
        state: &mut SideState,
        side: &CompiledSetupSide,
        i: usize,
        close: &str,
        low: &str,
    ) -> Option<SetupFire> {
        step(
            state,
            side,
            &gate(),
            i,
            close,
            low,
            "110",
            false,
            true,
            8,
            Direction::Long,
        )
    }

    fn arm_long(state: &mut SideState, side: &CompiledSetupSide, i: usize) {
        let fire = step(
            state,
            side,
            &gate(),
            i,
            "100",
            "99",
            "110",
            true,
            true,
            8,
            Direction::Long,
        );
        assert!(fire.is_none());
    }

    #[test]
    fn setup_side_step_arms_only_when_htf_closed() {
        let side = side(false);
        let mut state = SideState::default();
        let no_htf = step(
            &mut state,
            &side,
            &gate(),
            5,
            "100",
            "99",
            "110",
            false,
            true,
            8,
            Direction::Long,
        );
        assert!(no_htf.is_none());
        assert_eq!(state.phase, SetupPhase::Idle);
        assert_eq!(state.event, None);
        arm_long(&mut state, &side, 6);
        assert_eq!(state.phase, SetupPhase::Armed);
        assert_eq!(state.event, Some(SetupEvent::Armed));
        assert_eq!(state.arm_bar, 6);
        assert_eq!(state.arm_time, close_time(6));
    }

    #[test]
    fn setup_side_step_arm_needs_warm() {
        let side = side(false);
        let mut state = SideState::default();
        let fire = step(
            &mut state,
            &side,
            &gate(),
            5,
            "100",
            "99",
            "110",
            true,
            false,
            8,
            Direction::Long,
        );
        assert!(fire.is_none());
        assert_eq!(state.phase, SetupPhase::Idle);
        assert_eq!(state.event, None);
    }

    #[test]
    fn setup_side_step_arm_bar_is_not_a_dip() {
        // The arm and the dip both hold on bar 5: the side arms and nothing more.
        let mut side = side(false);
        side.dip = close_is(Comparator::Gt, "0");
        let mut state = SideState::default();
        arm_long(&mut state, &side, 5);
        assert_eq!(state.phase, SetupPhase::Armed);
        assert_eq!(state.dip_time, None);
        // The next bar is the first that can dip.
        long(&mut state, &side, 6, "98", "97");
        assert_eq!(state.phase, SetupPhase::Dipped);
        assert_eq!(state.dip_time, Some(close_time(6)));
        assert_eq!(state.event, Some(SetupEvent::Dipped));
    }

    #[test]
    fn setup_side_step_dip_bar_is_not_a_trigger() {
        // The dip and the trigger both hold on bar 6: the side dips, then fires on bar 7.
        let mut side = side(false);
        side.dip = close_is(Comparator::Gt, "0");
        side.trigger = close_is(Comparator::Gt, "0");
        let mut state = SideState::default();
        arm_long(&mut state, &side, 5);
        let on_dip_bar = long(&mut state, &side, 6, "98", "97");
        assert!(on_dip_bar.is_none());
        assert_eq!(state.phase, SetupPhase::Dipped);
        let on_next = long(&mut state, &side, 7, "102", "101");
        let fire = on_next.expect("the trigger fires on the bar after the dip");
        assert_eq!(fire.dip_time, close_time(6));
        assert_eq!(fire.trigger_time, close_time(7));
        assert_eq!(fire.arm_time, close_time(5));
        assert_eq!(state.phase, SetupPhase::Idle);
        assert_eq!(state.event, Some(SetupEvent::Triggered));
        assert_eq!(state.last_trigger, Some(fire));
    }

    #[test]
    fn setup_side_step_expiry_bounds_the_trigger() {
        let side = side(false);
        // Trigger on arm + expiry_bars (13) still fires.
        let mut state = SideState::default();
        arm_long(&mut state, &side, 5);
        long(&mut state, &side, 6, "98", "97");
        let on_limit = long(&mut state, &side, 13, "102", "101");
        assert!(on_limit.is_some(), "arm + expiry_bars still triggers");

        // Trigger on arm + expiry_bars + 1 (14) lapses with the event.
        let mut state = SideState::default();
        arm_long(&mut state, &side, 5);
        long(&mut state, &side, 6, "98", "97");
        let after_limit = long(&mut state, &side, 14, "102", "101");
        assert!(after_limit.is_none());
        assert_eq!(state.phase, SetupPhase::Idle);
        assert_eq!(state.event, Some(SetupEvent::Expired));
    }

    #[test]
    fn setup_side_step_rearm_clears_dip_and_anchor() {
        let side = side(true);
        let mut state = SideState::default();
        arm_long(&mut state, &side, 5);
        long(&mut state, &side, 6, "97", "96");
        long(&mut state, &side, 7, "98", "97.5");
        assert_eq!(state.phase, SetupPhase::Dipped);
        assert_eq!(state.anchor_time, Some(close_time(6)));
        assert!(state.dip_time.is_some());
        arm_long(&mut state, &side, 9);
        assert_eq!(state.phase, SetupPhase::Armed);
        assert_eq!(state.arm_bar, 9);
        assert_eq!(state.arm_time, close_time(9));
        assert_eq!(state.dip_time, None);
        assert_eq!(state.anchor_time, None);
        assert_eq!(state.anchor_extreme, None);
        assert_eq!(state.fallback_extreme, None);
        assert_eq!(state.event, Some(SetupEvent::Armed));
    }

    #[test]
    fn setup_side_step_rearm_keeps_the_last_trigger() {
        let side = side(false);
        let mut state = SideState::default();
        arm_long(&mut state, &side, 5);
        long(&mut state, &side, 6, "98", "97");
        let fire = long(&mut state, &side, 7, "102", "101").expect("fires");
        arm_long(&mut state, &side, 9);
        assert_eq!(state.last_trigger, Some(fire));
    }

    #[test]
    fn setup_side_step_anchor_extreme_folds_from_the_anchor_bar() {
        let side = side(true);
        let mut state = SideState::default();
        arm_long(&mut state, &side, 5);
        // Bar 6 has a low of 90 but no anchor yet: only the fallback sees it.
        long(&mut state, &side, 6, "98", "90");
        assert_eq!(state.anchor_time, None);
        assert_eq!(state.fallback_extreme, Some(dec("90")));
        // Bar 7 is the anchor bar (close 97), low 95.
        long(&mut state, &side, 7, "97", "95");
        assert_eq!(state.anchor_time, Some(close_time(7)));
        assert_eq!(state.anchor_extreme, Some(dec("95")));
        // Bar 8 triggers with low 96; the anchor extreme keeps 95, not 90.
        let fire = long(&mut state, &side, 8, "102", "96").expect("fires");
        assert_eq!(fire.extreme, dec("95"));
        assert_eq!(fire.anchor_time, Some(close_time(7)));
    }

    #[test]
    fn setup_side_step_fallback_extreme_folds_from_the_bar_after_the_arm() {
        let side = side(false);
        let mut state = SideState::default();
        // The arm bar's own low (50) is not in the window.
        let fire = step(
            &mut state,
            &side,
            &gate(),
            5,
            "100",
            "50",
            "110",
            true,
            true,
            8,
            Direction::Long,
        );
        assert!(fire.is_none());
        assert_eq!(state.fallback_extreme, None);
        long(&mut state, &side, 6, "98", "96");
        long(&mut state, &side, 7, "99", "97");
        let fire = long(&mut state, &side, 8, "102", "99").expect("fires");
        assert_eq!(fire.extreme, dec("96"), "lowest low from bar 6 through 8");
        assert_eq!(fire.anchor_time, None);
    }

    #[test]
    fn setup_side_step_short_side_folds_highs_with_max() {
        let side = side(false);
        let mut state = SideState::default();
        let mut short = |i: usize, close: &str, high: &str, htf_closed: bool| {
            step(
                &mut state,
                &side,
                &gate(),
                i,
                close,
                "90",
                high,
                htf_closed,
                true,
                8,
                Direction::Short,
            )
        };
        assert!(short(5, "100", "999", true).is_none());
        assert!(short(6, "98", "104", false).is_none());
        assert!(short(7, "99", "108", false).is_none());
        let fire = short(8, "102", "106", false).expect("fires");
        assert_eq!(fire.direction, Direction::Short);
        assert_eq!(
            fire.extreme,
            dec("108"),
            "highest high from bar 6 through 8"
        );
    }

    #[test]
    fn setup_side_step_gate_false_blocks_the_trigger() {
        let side = side(false);
        let closed_gate = close_is(Comparator::Lt, "0");
        let mut state = SideState::default();
        arm_long(&mut state, &side, 5);
        long(&mut state, &side, 6, "98", "97");
        let blocked = step(
            &mut state,
            &side,
            &closed_gate,
            7,
            "102",
            "101",
            "110",
            false,
            true,
            8,
            Direction::Long,
        );
        assert!(blocked.is_none());
        assert_eq!(state.phase, SetupPhase::Dipped, "the side stays dipped");
        assert_eq!(state.last_trigger, None);
        // The gate opens on a later bar and the same side fires.
        let fire = long(&mut state, &side, 8, "102", "101");
        assert!(fire.is_some());
    }
}
