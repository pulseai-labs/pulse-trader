//! Pure read projections for the `pulse mcp` delivery ring (r2.s1.w2).
//!
//! Two unrelated helpers share this file because both are use-case logic shared
//! by every delivery adapter (grill ruling 2):
//!
//! 1. [`parse_indicator_specs`] / [`IndicatorColumn`] — the `<kind>:<period>`
//!    indicator-spec parser moved verbatim out of `src/cli/indicators.rs` so the
//!    MCP `export_indicators` tool and the `pulse indicators` viewer run the
//!    SAME parse/dedup/default behaviour (AC-9 guards the move). The CLI keeps
//!    byte-identical output through a `pub(crate) use` re-export.
//! 2. The wire projections the read tools return: [`VersionEntry`],
//!    [`StrategyEntry`], [`RunListEntry`], [`RunDetail`], [`MfeMaeWire`] — typed
//!    `Serialize` shapes mapped straight off the domain records so `src/mcp/**`
//!    never re-derives its own projection.
//!
//! The application ring may name NO adapter namespace other than
//! `crate::adapters::backtest` (the `tests/tauri_backtest.rs` source scan), so
//! everything here is pure: domain types in, `serde`-serializable structs out.

use std::collections::HashSet;

use rust_decimal::Decimal;
use serde::Serialize;

use crate::domain::strategy::{CreatedBy, Strategy, StrategyVersion};
use crate::domain::{
    BacktestInputs, IndicatorSpec, MfeMaeAggregates, OpenPositionMark, PersistedRun,
    RegimeBreakdown, SkippedEntryCounts, SummaryStats, SweepableValue,
};

/// One column of indicator output: the label a client sees (`<kind>:<period>`,
/// case-normalized) paired with the parsed [`IndicatorSpec`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndicatorColumn {
    /// The display/CSV-header label, e.g. `rsi:14`.
    pub label: String,
    /// The parsed fixed-period spec.
    pub spec: IndicatorSpec,
}

/// Parse `<kind>:<period>` indicator tokens into labelled specs.
///
/// Behaviour is byte-identical to the CLI viewer's (r1.s3.w1): an empty list
/// defaults to `rsi:14, ema:50, adx:14`; duplicate/case-variant tokens dedup to
/// one column each, order preserved; `macd`, unknown kinds, and non-u32 or
/// zero periods are rejected.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] naming the offending token on any malformed
/// spec.
pub fn parse_indicator_specs(raw: &[String]) -> anyhow::Result<Vec<IndicatorColumn>> {
    let tokens = if raw.is_empty() {
        ["rsi:14", "ema:50", "adx:14"]
            .iter()
            .map(|token| (*token).to_owned())
            .collect::<Vec<_>>()
    } else {
        raw.to_vec()
    };

    // The engine dedups specs, but the viewer renders one column per flag — so
    // repeated `--indicator` flags would print duplicate columns. Dedup here,
    // order-preserving, keyed on the case-normalized `kind:period` label (which
    // is 1:1 with the parsed spec for rsi/ema/adx/atr).
    let mut seen = HashSet::new();
    let mut columns = Vec::with_capacity(tokens.len());
    for token in &tokens {
        let column = parse_one_indicator(token)?;
        if seen.insert(column.label.clone()) {
            columns.push(column);
        }
    }
    Ok(columns)
}

fn parse_one_indicator(token: &str) -> anyhow::Result<IndicatorColumn> {
    let (kind, period) = token.split_once(':').ok_or_else(|| {
        anyhow::anyhow!("invalid --indicator {token:?}: expected <kind>:<period>")
    })?;
    let kind = kind.trim().to_ascii_lowercase();
    let period = parse_period(token, period)?;
    let fixed = SweepableValue::Fixed(period);
    let spec = match kind.as_str() {
        "rsi" => IndicatorSpec::Rsi { period: fixed },
        "ema" => IndicatorSpec::Ema { period: fixed },
        "adx" => IndicatorSpec::Adx { period: fixed },
        "atr" => IndicatorSpec::Atr { period: fixed },
        "macd" => anyhow::bail!(
            "invalid --indicator {token:?}: MACD needs fast/slow/signal and is not supported by <kind>:<period>"
        ),
        _ => anyhow::bail!(
            "invalid --indicator {token:?}: unknown kind {kind:?} (expected rsi, ema, adx, or atr)"
        ),
    };
    Ok(IndicatorColumn {
        label: format!("{kind}:{period}"),
        spec,
    })
}

fn parse_period(token: &str, period: &str) -> anyhow::Result<u32> {
    let period = period
        .trim()
        .parse::<u32>()
        .map_err(|e| anyhow::anyhow!("invalid --indicator {token:?}: period must be u32: {e}"))?;
    if period == 0 {
        anyhow::bail!("invalid --indicator {token:?}: period must be >= 1");
    }
    Ok(period)
}

/// One version row inside [`StrategyEntry::versions`], in `version_tree`
/// (parent-first) order — the `list_strategies` wire shape.
#[derive(Debug, Clone, Serialize)]
pub struct VersionEntry {
    /// The version id.
    pub id: String,
    /// The parent version id, when this version was cloned from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// The `created_by` provenance string (whatever the enum serializes to).
    pub created_by: String,
    /// The adapter-minted creation timestamp (RFC3339).
    pub created_at: String,
    /// The strategy name stored inside the version's DSL document.
    pub dsl_name: String,
}

/// One strategy row of `list_strategies` — meta plus its version subtree.
#[derive(Debug, Clone, Serialize)]
pub struct StrategyEntry {
    /// The strategy id.
    pub id: String,
    /// The human-readable name.
    pub name: String,
    /// Whether the strategy is archived.
    pub archived: bool,
    /// The pinned "canonical" version, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_version_id: Option<String>,
    /// The version subtree in `version_tree` (parent-first) order.
    pub versions: Vec<VersionEntry>,
}

/// The `created_by` wire string — the serde `snake_case` column text
/// (`"human"`, `"composer_llm"`, …). The `Debug` fallback is unreachable:
/// `CreatedBy` is a fieldless enum whose serde repr is always a string.
fn created_by_wire(created_by: CreatedBy) -> String {
    serde_json::to_value(created_by)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{created_by:?}"))
}

/// Project one [`StrategyVersion`] onto its [`VersionEntry`] wire row.
#[must_use]
pub fn version_entry(version: &StrategyVersion) -> VersionEntry {
    VersionEntry {
        id: version.id.as_str().to_owned(),
        parent_id: version
            .parent_version_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        created_by: created_by_wire(version.created_by),
        created_at: version.created_at.to_rfc3339(),
        dsl_name: version.dsl.name.clone(),
    }
}

/// The `get_version` wire shape — the full immutable version record, with the
/// DSL as the current-schema JSON object alongside the verbatim original.
#[derive(Debug, Clone, Serialize)]
pub struct VersionDetail {
    /// The version id.
    pub id: String,
    /// The owning strategy id.
    pub strategy_id: String,
    /// The parent version id, when cloned from one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_version_id: Option<String>,
    /// The `created_by` provenance string.
    pub created_by: String,
    /// The recorded schema version of the stored document (`"MAJOR.MINOR.PATCH"`).
    pub dsl_schema_version: String,
    /// The migrated current DSL document (current-schema JSON object).
    pub dsl: serde_json::Value,
    /// The verbatim pre-migration source bytes.
    pub dsl_original: String,
    /// The adapter-computed content hash.
    pub version_hash: String,
    /// The adapter-minted creation timestamp (RFC3339).
    pub created_at: String,
}

/// Project a [`StrategyVersion`] onto its [`VersionDetail`] wire shape.
#[must_use]
pub fn version_detail(version: &StrategyVersion) -> VersionDetail {
    VersionDetail {
        id: version.id.as_str().to_owned(),
        strategy_id: version.strategy_id.as_str().to_owned(),
        parent_version_id: version
            .parent_version_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        created_by: created_by_wire(version.created_by),
        dsl_schema_version: version.dsl_schema_version.to_string(),
        dsl: serde_json::to_value(&version.dsl).unwrap_or(serde_json::Value::Null),
        dsl_original: version.dsl_original.clone(),
        version_hash: version.version_hash.clone(),
        created_at: version.created_at.to_rfc3339(),
    }
}

/// Project a [`Strategy`] + its already-ordered version subtree onto
/// [`StrategyEntry`].
#[must_use]
pub fn strategy_entry(strategy: &Strategy, versions: Vec<VersionEntry>) -> StrategyEntry {
    StrategyEntry {
        id: strategy.id.as_str().to_owned(),
        name: strategy.name.clone(),
        archived: strategy.archived,
        pinned_version_id: strategy
            .pinned_version_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        versions,
    }
}

/// One `list_runs` row — the headline catalog stats plus the persisted input
/// provenance (which only `get_run` carries, so a row is built off the full
/// [`PersistedRun`], not the lighter [`RunSummary`](crate::domain::RunSummary)).
#[derive(Debug, Clone, Serialize)]
pub struct RunListEntry {
    /// The run id.
    pub run_id: String,
    /// The adapter-minted run timestamp (RFC3339 text on the column).
    pub created_at: String,
    /// Mean P&L per trade.
    pub expectancy: Decimal,
    /// Net P&L across the run.
    pub net_pnl: Decimal,
    /// Number of completed trades.
    pub trade_count: usize,
    /// The persisted inputs, or `null` for a pre-0006 row with no provenance.
    pub inputs: Option<BacktestInputs>,
}

/// Project a full [`PersistedRun`] onto its [`RunListEntry`] wire row.
#[must_use]
pub fn run_list_entry(run: &PersistedRun) -> RunListEntry {
    RunListEntry {
        run_id: run.id.as_str().to_owned(),
        created_at: run.created_at.clone(),
        expectancy: run.summary.expectancy,
        net_pnl: run.net_pnl,
        trade_count: run.summary.trade_count,
        inputs: run.inputs.clone(),
    }
}

/// The `get_run` MFE/MAE wire shape — a three-field projection of
/// [`crate::domain::MfeMaeAggregates`].
#[derive(Debug, Clone, Serialize)]
pub struct MfeMaeWire {
    /// Mean maximum favourable excursion, in R.
    pub mean_mfe_r: Decimal,
    /// Mean maximum adverse excursion, in R.
    pub mean_mae_r: Decimal,
    /// How many trades the aggregate covers.
    pub count: usize,
}

/// The `get_run` wire shape — the persisted-run projection with no inline
/// trades.
#[derive(Debug, Clone, Serialize)]
pub struct RunDetail {
    /// Every [`SummaryStats`] field, as persisted.
    pub summary: SummaryStats,
    /// The per-regime trade-count / net-P&L breakdown, as persisted.
    pub regime_breakdown: RegimeBreakdown,
    /// The counts of entries the sizer skipped, as persisted.
    pub skipped_entries: SkippedEntryCounts,
    /// The MFE/MAE projection over the run's persisted trades.
    pub mfe_mae: MfeMaeWire,
    /// The persisted inputs, or `null` for a pre-0006 row.
    pub inputs: Option<BacktestInputs>,
    /// The still-open position a windowed run ended holding (r2.s1 G1): the
    /// direction, entry fill and size the strategy opened, marked at the last
    /// in-window candle's close. `null` for a run that ended flat or at the
    /// snapshot's real last bar. Never counted in `summary`'s closed-trade
    /// statistics — it is reported explicitly precisely because it is not a
    /// trade.
    pub open_position: Option<OpenPositionMark>,
    /// The recording engine's build-time fingerprint.
    pub engine_fingerprint: String,
    /// The recording engine's compiled target triple.
    pub engine_target: String,
    /// The stored integrity hash (re-derived and checked on read).
    pub result_content_hash: String,
    /// The equity-curve base the run started from.
    pub starting_equity: Decimal,
}

/// Project a [`PersistedRun`] + its [`MfeMaeAggregates`] onto [`RunDetail`].
#[must_use]
pub fn run_detail(run: &PersistedRun, mfe_mae: &MfeMaeAggregates) -> RunDetail {
    RunDetail {
        summary: run.summary.clone(),
        regime_breakdown: run.regime_breakdown,
        skipped_entries: run.skipped_entries,
        mfe_mae: MfeMaeWire {
            mean_mfe_r: mfe_mae.avg_mfe_r,
            mean_mae_r: mfe_mae.avg_mae_r,
            count: mfe_mae.trade_count,
        },
        inputs: run.inputs.clone(),
        open_position: run.open_position.clone(),
        engine_fingerprint: run.engine_fingerprint.clone(),
        engine_target: run.engine_target.clone(),
        result_content_hash: run.result_content_hash.clone(),
        starting_equity: run.starting_equity,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::parse_indicator_specs;
    use crate::{IndicatorSpec, SweepableValue};

    fn fixed_period(spec: &IndicatorSpec) -> u32 {
        match spec {
            IndicatorSpec::Rsi { period }
            | IndicatorSpec::Ema { period }
            | IndicatorSpec::Adx { period }
            | IndicatorSpec::Atr { period } => match period {
                SweepableValue::Fixed(period) => *period,
                SweepableValue::Sweep { .. } => panic!("CLI specs must be fixed"),
            },
            IndicatorSpec::Macd { .. } => {
                panic!("MACD is not part of kind:period parsing")
            }
        }
    }

    #[test]
    fn parses_indicator_flag_kind_and_period() {
        let one = parse_indicator_specs(&["rsi:14".to_owned()]).expect("rsi:14 parses");
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].label, "rsi:14");
        assert!(matches!(one[0].spec, IndicatorSpec::Rsi { .. }));
        assert_eq!(fixed_period(&one[0].spec), 14);

        let repeated =
            parse_indicator_specs(&["ema:50".to_owned(), "adx:14".to_owned()]).expect("parse");
        assert_eq!(repeated.len(), 2);
        assert_eq!(repeated[0].label, "ema:50");
        assert!(matches!(repeated[0].spec, IndicatorSpec::Ema { .. }));
        assert_eq!(fixed_period(&repeated[0].spec), 50);
        assert_eq!(repeated[1].label, "adx:14");
        assert!(matches!(repeated[1].spec, IndicatorSpec::Adx { .. }));
        assert_eq!(fixed_period(&repeated[1].spec), 14);

        // `atr:<period>` parses too (r2.s2 round-1 fix F5): the engine builds
        // `IndicatorSpec::Atr`, so `pulse indicators` / MCP `export_indicators`
        // can inspect the ATR an `AtrStop` sizes against.
        let atr = parse_indicator_specs(&["atr:14".to_owned()]).expect("atr:14 parses");
        assert_eq!(atr.len(), 1);
        assert_eq!(atr[0].label, "atr:14");
        assert!(matches!(atr[0].spec, IndicatorSpec::Atr { .. }));
        assert_eq!(fixed_period(&atr[0].spec), 14);

        let defaults = parse_indicator_specs(&[]).expect("defaults parse");
        assert_eq!(
            defaults
                .iter()
                .map(|column| column.label.as_str())
                .collect::<Vec<_>>(),
            ["rsi:14", "ema:50", "adx:14"]
        );

        // Repeated / case-variant specs dedup to one column each, order preserved
        // (the viewer renders one column per surviving spec).
        let deduped = parse_indicator_specs(&[
            "rsi:14".to_owned(),
            "RSI:14".to_owned(),
            "ema:50".to_owned(),
            "rsi:14".to_owned(),
        ])
        .expect("dedup parses");
        assert_eq!(
            deduped
                .iter()
                .map(|column| column.label.as_str())
                .collect::<Vec<_>>(),
            ["rsi:14", "ema:50"],
            "duplicate / case-variant --indicator flags dedup, order preserved"
        );

        assert!(parse_indicator_specs(&["macd:12".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["bogus:14".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["rsi".to_owned()]).is_err());
        assert!(parse_indicator_specs(&["rsi:0".to_owned()]).is_err());
    }
}
