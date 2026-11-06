//! `Setup` — the stateful-setup grammar (schema 1.6.0, ADR-0033).
//!
//! A setup makes a strategy stateful. Each side (`long`, `short`) arms on a
//! higher-timeframe close, waits for its dip, then fires on its trigger, or
//! lapses after `expiry_bars`. The types here are grammar only: validation is in
//! `validate.rs`, compilation in `compile.rs`, and the engine that runs a setup
//! is r6.s2.w2 onward.

use serde::{Deserialize, Serialize};

use super::condition::Condition;
use super::sweepable::SweepableValue;

/// A stateful setup (schema 1.6.0, ADR-0033): each side arms on a higher-timeframe
/// close, waits for its dip, then fires on its trigger, or lapses after `expiry_bars`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Setup {
    /// Primary bars after the arm bar within which the trigger must close. A trigger
    /// on bar arm + k counts for 1 ≤ k ≤ `expiry_bars`. Bounds 1..=2000.
    pub expiry_bars: SweepableValue<u32>,
    /// The long side; absent ⇒ the strategy takes no long setups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long: Option<SetupSide>,
    /// The short side; absent ⇒ the strategy takes no short setups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short: Option<SetupSide>,
}

/// One side of a [`Setup`]. Every condition is written out for this side; the engine
/// never mirrors a long side into a short one (PDR-0045).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct SetupSide {
    /// Evaluated only on a primary bar where at least one new higher-timeframe bar
    /// closed. True arms the side, or re-arms it from scratch. Context rules that are
    /// "checked once, when the setup arms" are conjuncts here.
    pub arm: Condition,
    /// The pullback: the first primary bar after the arm bar where this holds is the dip.
    pub dip: Condition,
    /// The first primary bar after the dip bar where this holds, together with the
    /// strategy's `entry` and every `filter`, fires the side.
    pub trigger: Condition,
    /// The structure stop's anchor: the first primary bar after the arm bar where this
    /// holds starts the stop window (the trigger bar can be that bar). Absent, or never true
    /// from the bar after the arm bar through the trigger bar ⇒ the window starts at the bar
    /// after the arm bar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_anchor: Option<Condition>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Setup, SetupSide};
    use crate::domain::dsl::condition::{Comparator, Condition};
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{PriceField, Series, ValueSource};
    use rust_decimal::Decimal;

    fn close_above(level: i64) -> Condition {
        Condition::Compare {
            lhs: ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            op: Comparator::Gt,
            rhs: ValueSource::Constant {
                value: Decimal::new(level, 0),
            },
        }
    }

    fn side(with_anchor: bool) -> SetupSide {
        SetupSide {
            arm: close_above(1),
            dip: close_above(2),
            trigger: close_above(3),
            stop_anchor: with_anchor.then(|| close_above(4)),
        }
    }

    /// A side with a stop anchor round-trips value-equal and writes the anchor.
    #[test]
    fn setup_side_with_anchor_round_trips() {
        let s = side(true);
        let json = serde_json::to_string(&s).expect("serialize SetupSide");
        assert!(json.contains("\"stop_anchor\""), "json was: {json}");
        let back: SetupSide = serde_json::from_str(&json).expect("deserialize SetupSide");
        assert_eq!(back, s);
    }

    /// An absent stop anchor is omitted on write and reads back as `None`.
    #[test]
    fn setup_side_without_anchor_omits_the_key() {
        let s = side(false);
        let json = serde_json::to_string(&s).expect("serialize SetupSide");
        assert!(!json.contains("stop_anchor"), "json was: {json}");
        let back: SetupSide = serde_json::from_str(&json).expect("deserialize SetupSide");
        assert_eq!(back, s);
    }

    /// A two-sided setup round-trips value-equal.
    #[test]
    fn setup_two_sided_round_trips() {
        let s = Setup {
            expiry_bars: SweepableValue::Fixed(16),
            long: Some(side(true)),
            short: Some(side(false)),
        };
        let json = serde_json::to_string(&s).expect("serialize Setup");
        let back: Setup = serde_json::from_str(&json).expect("deserialize Setup");
        assert_eq!(back, s);
    }

    /// An absent side is omitted on write and reads back as `None`.
    #[test]
    fn setup_one_sided_omits_the_absent_side() {
        let s = Setup {
            expiry_bars: SweepableValue::Fixed(8),
            long: None,
            short: Some(side(false)),
        };
        let json = serde_json::to_string(&s).expect("serialize Setup");
        assert!(!json.contains("\"long\""), "json was: {json}");
        let back: Setup = serde_json::from_str(&json).expect("deserialize Setup");
        assert_eq!(back, s);
    }

    /// A `null` side or anchor reads as absent (the served schema admits `null`).
    #[test]
    fn setup_null_reads_as_absent() {
        let json = r#"{"expiry_bars":4,"long":null,"short":null}"#;
        let s: Setup = serde_json::from_str(json).expect("null sides deserialize");
        assert_eq!(s.long, None);
        assert_eq!(s.short, None);
    }

    /// A missing required condition is refused structurally.
    #[test]
    fn setup_side_requires_arm_dip_and_trigger() {
        let json = serde_json::json!({
            "arm": serde_json::to_value(close_above(1)).unwrap(),
            "dip": serde_json::to_value(close_above(2)).unwrap(),
        });
        assert!(serde_json::from_value::<SetupSide>(json).is_err());
    }
}
