//! `StrategyDsl` — the top-level strategy document: the thing the LLM composes
//! (FR-3) and the backtester executes.
//!
//! Aggregates the 2.01 leaf+predicate layer and the 2.02 exit/risk vocabulary
//! into one self-contained, serde-round-tripping document: a `schema_version`,
//! a `name`, a [`Direction`], the required `entry` [`Condition`], an optional
//! list of `filters`, the `exits`, and the `risk` params.
//!
//! `entry` and `filters` stay **separate** (grill branch 4): this mirrors FR-3's
//! distinct `add_entry_signal` / `add_filter` builder tools and the coach's
//! first-class "add a filter" one-mutation operation. Convention pinned for
//! 2.04: **effective entry = `entry` ∧ all `filters`**; `entry` is the required
//! single trigger, `filters` defaults empty. They are NOT collapsed into one
//! `Condition::And` despite the logical equivalence — the structure is
//! load-bearing for the composer/coach/UI.
//!
//! **Direct deserialize is migration-UNAWARE (architect-critic C3):** plain
//! `serde_json::from_str::<StrategyDsl>` accepts ANY `schema_version` (even a
//! future `"99.0.0"`) and performs **no** migration — by design, because the
//! version-safe read-path (`load → detect → migrate → validate`) is **2.05's**.
//! 2.02 ships only the round-trip; VS-1.1.4 persistence MUST route loads through
//! 2.05's loader, never raw serde. No version gating is added here.
//!
//! **No semantic validation here.** A document with zero exits, duplicate stops,
//! an all-`Constant` cross, or a `Sweep` value is *representable* and is rejected
//! in **2.03** (the full rule set lives in the slice README contract). 2.02
//! guarantees serde round-trip (value-equality), not sensibility.

use serde::{Deserialize, Serialize};

use super::condition::Condition;
use super::exit::ExitRule;
use super::risk::{Direction, RiskParams};
use super::schema_version::SchemaVersion;

/// A complete, self-contained strategy document.
///
/// Plain (untagged) struct — it's the root object. `entry` is required; the
/// other collections carry their own emptiness semantics (see module docs and
/// 2.03 for validation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StrategyDsl {
    /// The DSL document's schema version (a `"MAJOR.MINOR.PATCH"` string in
    /// JSON). Deserialize is migration-unaware — see module docs.
    pub schema_version: SchemaVersion,
    /// Human-readable strategy name.
    pub name: String,
    /// The single trade side this strategy takes.
    pub direction: Direction,
    /// The required entry trigger. Effective entry = `entry` ∧ all `filters`
    /// (convention for 2.04).
    pub entry: Condition,
    /// Optional additional gating conditions, conjoined with `entry`. Defaults
    /// empty.
    pub filters: Vec<Condition>,
    /// The exit rules; any that triggers closes the position.
    pub exits: Vec<ExitRule>,
    /// The risk / sizing inputs.
    pub risk: RiskParams,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::StrategyDsl;
    use crate::domain::dsl::condition::{Comparator, Condition};
    use crate::domain::dsl::exit::ExitRule;
    use crate::domain::dsl::risk::{Direction, RiskParams};
    use crate::domain::dsl::schema_version::SchemaVersion;
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{IndicatorSpec, ValueSource};
    use rust_decimal::Decimal;

    /// The exact demo-1 / demo-2 strategy: "long when RSI(14) < 30, take profit
    /// at 2R, 5% stop, risk 1%/trade". Built once, reused by the round-trip and
    /// the decimal-fraction-convention assertions.
    fn rsi_oversold_strategy() -> StrategyDsl {
        StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "RSI Oversold".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Indicator {
                    spec: IndicatorSpec::Rsi {
                        period: SweepableValue::Fixed(14),
                    },
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant {
                    value: Decimal::new(30, 0),
                },
            },
            filters: vec![],
            exits: vec![
                // 0.05 = 5% stop (defines 1R).
                ExitRule::StopLoss {
                    distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
                },
                // 2.0 = 2R take profit.
                ExitRule::TakeProfit {
                    target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
                },
            ],
            risk: RiskParams {
                // 0.01 = 1% risk per trade.
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        }
    }

    /// AC-5: a complete `StrategyDsl` round-trips value-equal, AND the
    /// decimal-fraction convention is pinned by concrete-value assertions
    /// (architect-critic C1): `distance_pct = 0.05`, `risk_per_trade_pct = 0.01`,
    /// `target_r = 2.0`.
    #[test]
    fn rsi_oversold_strategy_round_trips() {
        let strat = rsi_oversold_strategy();

        // Round-trip value-equal (whole-strategy half of demo-1).
        let json = serde_json::to_string(&strat).expect("serialize StrategyDsl");
        let back: StrategyDsl = serde_json::from_str(&json).expect("deserialize StrategyDsl");
        assert_eq!(back, strat);

        // Pin the decimal-fraction convention by concrete values (NOT prose).
        let distance_pct = match &strat.exits[0] {
            ExitRule::StopLoss { distance_pct } => distance_pct.clone(),
            other => panic!("exits[0] expected StopLoss, was {other:?}"),
        };
        assert_eq!(
            distance_pct,
            SweepableValue::Fixed(Decimal::new(5, 2)),
            "distance_pct must be 0.05 (5% as a decimal fraction)"
        );

        let target_r = match &strat.exits[1] {
            ExitRule::TakeProfit { target_r } => target_r.clone(),
            other => panic!("exits[1] expected TakeProfit, was {other:?}"),
        };
        assert_eq!(
            target_r,
            SweepableValue::Fixed(Decimal::new(2, 0)),
            "target_r must be 2.0 (a plain R-multiple)"
        );

        assert_eq!(
            strat.risk.risk_per_trade_pct,
            SweepableValue::Fixed(Decimal::new(1, 2)),
            "risk_per_trade_pct must be 0.01 (1% as a decimal fraction)"
        );
    }

    /// AC-8: serde structurally rejects (a) a strategy missing a required field
    /// (`risk`), (b) one with an unknown `ExitRule` `"type"`, and (c) a malformed
    /// `schema_version` string. 2.03 owns *semantic* rejection; this is the
    /// *structural* boundary.
    #[test]
    fn rejects_malformed_strategy() {
        // (a) Missing the required `risk` field.
        let missing_risk = r#"{
            "schema_version":"1.0.0",
            "name":"x",
            "direction":"long",
            "entry":{"type":"Compare",
                "lhs":{"type":"Constant","value":"1"},
                "op":"Lt",
                "rhs":{"type":"Constant","value":"2"}},
            "filters":[],
            "exits":[]
        }"#;
        let r: Result<StrategyDsl, _> = serde_json::from_str(missing_risk);
        assert!(r.is_err(), "missing `risk` must be rejected");

        // (b) Unknown ExitRule "type".
        let unknown_exit = r#"{
            "schema_version":"1.0.0",
            "name":"x",
            "direction":"long",
            "entry":{"type":"Compare",
                "lhs":{"type":"Constant","value":"1"},
                "op":"Lt",
                "rhs":{"type":"Constant","value":"2"}},
            "filters":[],
            "exits":[{"type":"Bogus"}],
            "risk":{"risk_per_trade_pct":"0.01","max_leverage":"3"}
        }"#;
        let r: Result<StrategyDsl, _> = serde_json::from_str(unknown_exit);
        assert!(r.is_err(), "unknown ExitRule type must be rejected");

        // (c) Malformed schema_version string.
        let bad_version = r#"{
            "schema_version":"1.0",
            "name":"x",
            "direction":"long",
            "entry":{"type":"Compare",
                "lhs":{"type":"Constant","value":"1"},
                "op":"Lt",
                "rhs":{"type":"Constant","value":"2"}},
            "filters":[],
            "exits":[],
            "risk":{"risk_per_trade_pct":"0.01","max_leverage":"3"}
        }"#;
        let r: Result<StrategyDsl, _> = serde_json::from_str(bad_version);
        assert!(r.is_err(), "malformed schema_version must be rejected");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod prop_tests {
    use super::StrategyDsl;
    use crate::domain::dsl::condition::{Comparator, Condition};
    use crate::domain::dsl::exit::ExitRule;
    use crate::domain::dsl::risk::{Direction, RiskParams};
    use crate::domain::dsl::schema_version::SchemaVersion;
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{IndicatorSpec, PriceField, ValueSource};
    use proptest::prelude::*;
    use rust_decimal::Decimal;

    /// Integer-mantissa `Decimal` (NOT `f64`) so the serde-with-str round-trip is
    /// exact (architect-critic C6 / spec §3 proptest constraint b).
    fn arb_decimal() -> impl Strategy<Value = Decimal> {
        (any::<i64>(), 0u32..=8u32).prop_map(|(m, s)| Decimal::new(m, s))
    }

    fn arb_sweepable_u32() -> impl Strategy<Value = SweepableValue<u32>> {
        prop_oneof![
            any::<u32>().prop_map(SweepableValue::Fixed),
            (any::<u32>(), any::<u32>(), any::<u32>())
                .prop_map(|(start, end, step)| SweepableValue::Sweep { start, end, step }),
        ]
    }

    fn arb_sweepable_decimal() -> impl Strategy<Value = SweepableValue<Decimal>> {
        prop_oneof![
            arb_decimal().prop_map(SweepableValue::Fixed),
            (arb_decimal(), arb_decimal(), arb_decimal())
                .prop_map(|(start, end, step)| SweepableValue::Sweep { start, end, step }),
        ]
    }

    fn arb_indicator_spec() -> impl Strategy<Value = IndicatorSpec> {
        prop_oneof![
            arb_sweepable_u32().prop_map(|period| IndicatorSpec::Rsi { period }),
            arb_sweepable_u32().prop_map(|period| IndicatorSpec::Ema { period }),
            arb_sweepable_u32().prop_map(|period| IndicatorSpec::Adx { period }),
            (
                arb_sweepable_u32(),
                arb_sweepable_u32(),
                arb_sweepable_u32()
            )
                .prop_map(|(fast, slow, signal)| IndicatorSpec::Macd {
                    fast,
                    slow,
                    signal
                }),
        ]
    }

    fn arb_price_field() -> impl Strategy<Value = PriceField> {
        prop_oneof![
            Just(PriceField::Open),
            Just(PriceField::High),
            Just(PriceField::Low),
            Just(PriceField::Close),
            Just(PriceField::Volume),
        ]
    }

    fn arb_value_source() -> impl Strategy<Value = ValueSource> {
        prop_oneof![
            arb_decimal().prop_map(|value| ValueSource::Constant { value }),
            arb_price_field().prop_map(|field| ValueSource::Price { field }),
            arb_indicator_spec().prop_map(|spec| ValueSource::Indicator { spec }),
        ]
    }

    fn arb_comparator() -> impl Strategy<Value = Comparator> {
        prop_oneof![
            Just(Comparator::Gt),
            Just(Comparator::Gte),
            Just(Comparator::Lt),
            Just(Comparator::Lte),
            Just(Comparator::Eq),
        ]
    }

    /// A bounded recursive `Condition` generator (depth ≤ 4, ≤ 32 nodes, ≤ 4
    /// children) — mirrors 2.01's bound (spec §3 constraint a).
    fn arb_condition() -> impl Strategy<Value = Condition> {
        let leaf = prop_oneof![
            (arb_value_source(), arb_comparator(), arb_value_source())
                .prop_map(|(lhs, op, rhs)| Condition::Compare { lhs, op, rhs }),
            (arb_value_source(), arb_value_source())
                .prop_map(|(lhs, rhs)| Condition::CrossesAbove { lhs, rhs }),
            (arb_value_source(), arb_value_source())
                .prop_map(|(lhs, rhs)| Condition::CrossesBelow { lhs, rhs }),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 1..=4)
                    .prop_map(|conditions| Condition::And { conditions }),
                prop::collection::vec(inner.clone(), 1..=4)
                    .prop_map(|conditions| Condition::Or { conditions }),
                inner.prop_map(|c| Condition::Not {
                    condition: Box::new(c),
                }),
            ]
        })
    }

    fn arb_exit_rule() -> impl Strategy<Value = ExitRule> {
        prop_oneof![
            arb_sweepable_decimal().prop_map(|distance_pct| ExitRule::StopLoss { distance_pct }),
            arb_sweepable_decimal().prop_map(|target_r| ExitRule::TakeProfit { target_r }),
            arb_sweepable_decimal().prop_map(|trail_pct| ExitRule::TrailingStop { trail_pct }),
            arb_sweepable_u32().prop_map(|max_bars| ExitRule::TimeStop { max_bars }),
            arb_condition().prop_map(|condition| ExitRule::SignalExit { condition }),
        ]
    }

    fn arb_direction() -> impl Strategy<Value = Direction> {
        prop_oneof![Just(Direction::Long), Just(Direction::Short)]
    }

    fn arb_risk_params() -> impl Strategy<Value = RiskParams> {
        (arb_sweepable_decimal(), arb_sweepable_decimal()).prop_map(
            |(risk_per_trade_pct, max_leverage)| RiskParams {
                risk_per_trade_pct,
                max_leverage,
            },
        )
    }

    fn arb_schema_version() -> impl Strategy<Value = SchemaVersion> {
        (any::<u16>(), any::<u16>(), any::<u16>()).prop_map(|(major, minor, patch)| SchemaVersion {
            major,
            minor,
            patch,
        })
    }

    fn arb_strategy() -> impl Strategy<Value = StrategyDsl> {
        (
            arb_schema_version(),
            ".*",
            arb_direction(),
            arb_condition(),
            prop::collection::vec(arb_condition(), 0..=3),
            prop::collection::vec(arb_exit_rule(), 0..=4),
            arb_risk_params(),
        )
            .prop_map(
                |(schema_version, name, direction, entry, filters, exits, risk)| StrategyDsl {
                    schema_version,
                    name,
                    direction,
                    entry,
                    filters,
                    exits,
                    risk,
                },
            )
    }

    proptest! {
        /// AC-6: `deserialize(serialize(s)) == s` over arbitrary whole
        /// `StrategyDsl` documents (bounded `Condition` recursion;
        /// integer-mantissa `Decimal`s).
        #[test]
        fn prop_strategy_round_trip(strat in arb_strategy()) {
            let json = serde_json::to_string(&strat).expect("serialize arbitrary StrategyDsl");
            let back: StrategyDsl =
                serde_json::from_str(&json).expect("deserialize arbitrary StrategyDsl");
            prop_assert_eq!(back, strat);
        }
    }
}
