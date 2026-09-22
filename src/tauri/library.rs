//! The Strategy Library's read surface (r1.s1.w3) — ring-owned wire DTOs plus
//! the pure projections that build them from domain values.
//!
//! **Ring-owned wire types, not `specta` derives on domain types** (the
//! [`ShellInfo`](super::commands::ShellInfo) pattern): every crossing type is
//! declared here, camelCase on the wire, and built by a pure projection from the
//! domain record. The frontend renders these strings verbatim — it does no
//! numeric math, so a fabricated figure cannot appear screen-side. A version
//! with no persisted run carries `stats: None`, and the screen renders an em
//! dash there (grill A1) — never a zero dressed up as data.
//!
//! The DSL summary renders exactly the fields [`StrategyDsl`] carries (name,
//! direction, entry, filters, exits, risk). The design mock's `pair` and
//! `timeframes` lines are deliberately absent: `StrategyDsl` has neither field,
//! and inventing values for them is the fake this spine's ledger exists to
//! catch. Numbers render as short human text (`5%`, `2R`, `rsi(14) < 30`)
//! derived from the record's own decimals — `rust_decimal` math, no floats.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::domain::StrategyDsl;
use crate::domain::backtest::{RunSummary, SummaryStats};
use crate::domain::dsl::render;

// ---------------------------------------------------------------------------
// Wire DTOs (one per shape the Library screen renders)
// ---------------------------------------------------------------------------

/// The `library_overview` command's whole payload: every strategy, each with its
/// version tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct LibraryOverview {
    /// Every persisted strategy, in the repository's list order.
    pub strategies: Vec<LibraryStrategy>,
}

/// One strategy and its `version_tree`-ordered versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct LibraryStrategy {
    /// The strategy's id.
    pub id: String,
    /// The strategy's name.
    pub name: String,
    /// Creation timestamp (RFC3339 UTC, from the record).
    pub created_at: String,
    /// The pinned version's id, if the strategy has one (FR-11 pin).
    pub pinned_version_id: Option<String>,
    /// All versions, parent-before-child (`version_tree` order).
    pub versions: Vec<LibraryVersion>,
}

/// One version: tree position, DSL summary, stats-or-em-dash, recent runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct LibraryVersion {
    /// The version's id.
    pub id: String,
    /// The parent version's id (`None` for a root).
    pub parent_id: Option<String>,
    /// Creation timestamp (RFC3339 UTC, from the record).
    pub created_at: String,
    /// Who authored this version — the `created_by` label (`"human"`,
    /// `"composer_llm"`, `"coach_llm"`, `"external_agent"`, …). r2.s1.w4 C1.
    pub created_by: String,
    /// For an `external_agent` version, the submission's normalized agent name
    /// (the screen renders `external_agent · <agent name>`). `None` for every
    /// other provenance — and for an agent version whose `agent_submission`
    /// row is absent, which is reported, not guessed at.
    pub agent_name: Option<String>,
    /// For an `external_agent` version, the submission's stated hypothesis.
    /// `None` under the same two rules as [`Self::agent_name`].
    pub hypothesis: Option<String>,
    /// The version's DSL, rendered to summary lines.
    pub dsl: DslSummary,
    /// The latest persisted run's stats, or `None` when no run exists — the
    /// screen renders an em dash on `None` (grill A1).
    pub stats: Option<VersionStats>,
    /// The expectancy delta vs the parent's, formatted (e.g. `"+0.12R"`), when
    /// BOTH this version and its parent carry a run. `None` otherwise.
    pub delta_vs_parent: Option<String>,
    /// This version's run catalog (best-effort — one corrupt row costs its row
    /// here, not the screen), newest first.
    pub recent_runs: Vec<LibraryRunSummary>,
    /// Whether the version's latest walk-forward run passed — the badge signal
    /// (r2.s3.w4); `false` until a run passes AND whenever a newer run failed.
    pub certified: bool,
    /// The certifying run's id — `None` until a walk-forward run is persisted.
    /// The Details pane renders it when present.
    pub latest_walk_forward_run_id: Option<String>,
}

/// The three headline KPIs the screen renders per version, pre-formatted from
/// the persisted run's [`SummaryStats`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct VersionStats {
    /// Mean P&L per trade, e.g. `"+0.42R"`.
    pub expectancy: String,
    /// Fraction of winners, e.g. `"48.3%"`.
    pub win_rate: String,
    /// Completed trades in the run.
    pub trades: u32,
}

/// One row of a version's run catalog (`list_runs_for_version`'s projection).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct LibraryRunSummary {
    /// The run's id.
    pub id: String,
    /// Run timestamp (RFC3339 UTC, from the record).
    pub created_at: String,
    /// Mean P&L per trade, e.g. `"+0.42R"`.
    pub expectancy: String,
    /// Completed trades in the run.
    pub trades: u32,
}

/// A version's DSL rendered to summary lines — exactly the fields
/// [`StrategyDsl`] carries, nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DslSummary {
    /// The document's name.
    pub name: String,
    /// `"long"` or `"short"`.
    pub direction: String,
    /// The entry trigger, one line (the DSL's `entry` is a single condition).
    pub entry: Vec<String>,
    /// The gating filters, one line each.
    pub filters: Vec<String>,
    /// The exit rules, one line each.
    pub exits: Vec<String>,
    /// The risk parameters, one line each.
    pub risk: Vec<String>,
}

// ---------------------------------------------------------------------------
// Projections (pure: domain values in, wire values out)
// ---------------------------------------------------------------------------

/// Render a [`StrategyDsl`] to its summary lines. The vocabulary lives in the
/// domain ring's [`render`] — this is the thin DTO adapter over it, so this
/// summary and the Designer's card can never drift apart again (the `atr stop
/// 14 x2` / `atr_stop 14 x2` split r2.s2.w4 deletes).
#[must_use]
pub fn dsl_summary(dsl: &StrategyDsl) -> DslSummary {
    let rendered = render::strategy(dsl);
    DslSummary {
        name: dsl.name.clone(),
        direction: rendered.direction,
        entry: vec![rendered.entry],
        filters: rendered.filters,
        exits: rendered.exits,
        risk: rendered.risk,
    }
}

/// Project a persisted run's [`SummaryStats`] into the screen's three KPIs.
#[must_use]
pub fn version_stats(summary: &SummaryStats) -> VersionStats {
    VersionStats {
        expectancy: format_expectancy(summary.expectancy),
        win_rate: format_win_rate(summary.win_rate),
        trades: u32::try_from(summary.trade_count).unwrap_or(u32::MAX),
    }
}

/// Project one [`RunSummary`] catalog row.
#[must_use]
pub fn recent_run_summary(run: &RunSummary) -> LibraryRunSummary {
    LibraryRunSummary {
        id: run.id.as_str().to_owned(),
        created_at: run.created_at.clone(),
        expectancy: format_expectancy(run.expectancy),
        trades: u32::try_from(run.trade_count).unwrap_or(u32::MAX),
    }
}

/// Format an expectancy (or a delta of one) as signed R-multiples, e.g.
/// `"+0.42R"` / `"-0.04R"`.
#[must_use]
pub fn format_expectancy(value: Decimal) -> String {
    let rounded = value.round_dp(2).normalize();
    if rounded.is_sign_negative() {
        format!("{rounded}R")
    } else {
        format!("+{rounded}R")
    }
}

/// Format a `[0, 1]` win rate as a percentage, e.g. `"48.3%"`.
#[must_use]
pub fn format_win_rate(rate: Decimal) -> String {
    format!("{}%", (rate * Decimal::from(100)).round_dp(1).normalize())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{DslSummary, dsl_summary, format_expectancy, format_win_rate, version_stats};
    use crate::domain::backtest::SummaryStats;
    use crate::domain::dsl::render;
    use crate::domain::{
        Comparator, Condition, Direction, ExitRule, IndicatorSpec, PriceField, RiskParams,
        SchemaVersion, Series, StrategyDsl, SweepableValue, ValueSource,
    };
    use rust_decimal::Decimal;

    /// The canonical RSI-oversold document — the same shape the integration
    /// test seeds, so the unit expectations pin the exact screen lines.
    fn canonical_dsl() -> StrategyDsl {
        StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "RSI Oversold".to_owned(),
            direction: Direction::Long,
            entry: Condition::Compare {
                lhs: ValueSource::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Rsi {
                        period: SweepableValue::Fixed(14),
                    },
                },
                op: Comparator::Lt,
                rhs: ValueSource::Constant {
                    value: Decimal::new(30, 0),
                },
            },
            filters: vec![Condition::Compare {
                lhs: ValueSource::Price {
                    series: Series::Primary,
                    field: PriceField::Close,
                },
                op: Comparator::Gt,
                rhs: ValueSource::Indicator {
                    series: Series::Primary,
                    spec: IndicatorSpec::Ema {
                        period: SweepableValue::Fixed(200),
                    },
                },
            }],
            exits: vec![
                ExitRule::StopLoss {
                    distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)),
                },
                ExitRule::TakeProfit {
                    target_r: SweepableValue::Fixed(Decimal::new(2, 0)),
                },
            ],
            risk: RiskParams {
                risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)),
                max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
            },
        }
    }

    #[test]
    fn canonical_dsl_renders_its_own_fields_and_no_invented_ones() {
        let DslSummary {
            name,
            direction,
            entry,
            filters,
            exits,
            risk,
        } = dsl_summary(&canonical_dsl());

        assert_eq!(name, "RSI Oversold");
        assert_eq!(direction, "long");
        assert_eq!(entry, vec!["rsi(14) < 30".to_owned()]);
        assert_eq!(filters, vec!["close > ema(200)".to_owned()]);
        assert_eq!(
            exits,
            vec!["stop 5%".to_owned(), "take profit 2R".to_owned()]
        );
        assert_eq!(
            risk,
            vec!["risk 1% per trade".to_owned(), "max leverage 3x".to_owned()]
        );
        // The mock's `pair`/`timeframes` lines have no field to come from — the
        // summary struct cannot even carry them.
    }

    #[test]
    fn compound_conditions_parenthesize() {
        let compound = Condition::Not {
            condition: Box::new(Condition::Or {
                conditions: vec![
                    Condition::Compare {
                        lhs: ValueSource::Indicator {
                            series: Series::Primary,
                            spec: IndicatorSpec::Adx {
                                period: SweepableValue::Fixed(14),
                            },
                        },
                        op: Comparator::Gt,
                        rhs: ValueSource::Constant {
                            value: Decimal::new(25, 0),
                        },
                    },
                    Condition::CrossesAbove {
                        lhs: ValueSource::Indicator {
                            series: Series::Primary,
                            spec: IndicatorSpec::Macd {
                                fast: SweepableValue::Fixed(12),
                                slow: SweepableValue::Fixed(26),
                                signal: SweepableValue::Fixed(9),
                            },
                        },
                        rhs: ValueSource::Constant {
                            value: Decimal::ZERO,
                        },
                    },
                ],
            }),
        };
        let text = render::condition(&compound);
        assert_eq!(
            text,
            "not ((adx(14) > 25) or (macd(12,26,9) crosses above 0))"
        );
    }

    #[test]
    fn every_comparator_renders_a_symbol() {
        assert_eq!(render::comparator(Comparator::Gt), ">");
        assert_eq!(render::comparator(Comparator::Gte), ">=");
        assert_eq!(render::comparator(Comparator::Lt), "<");
        assert_eq!(render::comparator(Comparator::Lte), "<=");
        assert_eq!(render::comparator(Comparator::Eq), "=");
    }

    #[test]
    fn expectancy_renders_signed_r_multiples() {
        assert_eq!(format_expectancy(Decimal::new(42, 2)), "+0.42R");
        assert_eq!(format_expectancy(Decimal::new(-4, 2)), "-0.04R");
        assert_eq!(format_expectancy(Decimal::new(12, 2)), "+0.12R");
        assert_eq!(format_expectancy(Decimal::new(2, 0)), "+2R");
    }

    #[test]
    fn win_rate_renders_as_a_percentage() {
        assert_eq!(format_win_rate(Decimal::new(483, 3)), "48.3%");
        assert_eq!(format_win_rate(Decimal::new(462, 3)), "46.2%");
        assert_eq!(format_win_rate(Decimal::ZERO), "0%");
    }

    #[test]
    fn version_stats_carries_the_persisted_run_numbers() {
        let summary = SummaryStats {
            expectancy: Decimal::new(420, 3),
            win_rate: Decimal::new(483, 3),
            trade_count: 64,
            ..SummaryStats::default()
        };

        let stats = version_stats(&summary);
        assert_eq!(stats.expectancy, "+0.42R");
        assert_eq!(stats.win_rate, "48.3%");
        assert_eq!(stats.trades, 64);
    }
}
