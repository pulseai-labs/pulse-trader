//! The `pulse://dsl/schema` resource (r2.s1.w2).
//!
//! Publishes the DSL grammar as JSON Schema plus the conventions an external
//! agent needs before it writes a variant — so the schema is the machine-check
//! and the conventions are the semantics it cannot express.

use crate::domain::{DSL_SCHEMA_VERSION, StrategyDsl};

/// The resource URI the agent reads.
pub(crate) const DSL_SCHEMA_URI: &str = "pulse://dsl/schema";

/// The resource's programmatic name.
pub(crate) const DSL_SCHEMA_NAME: &str = "dsl_schema";

/// The resource's MIME type.
pub(crate) const DSL_SCHEMA_MIME: &str = "application/json";

/// The verbatim conventions block, keyed by the field/rule each entry covers.
/// Values are normative strings — the schema cannot express "fraction in
/// (0,1)" or the window-warmup rule, so they live here as sentences.
const CONVENTIONS: &[(&str, &str)] = &[
    ("distance_pct", "fraction in (0,1), e.g. 0.02 = 2%"),
    ("trail_pct", "fraction in (0,1), e.g. 0.015 = 1.5%"),
    (
        "risk_per_trade_pct",
        "fraction in (0,1], e.g. 0.01 = 1% of equity",
    ),
    ("target_r", "R-multiple > 0, e.g. 2.0 = 2R"),
    ("max_leverage", ">= 1"),
    (
        "sweepable_values",
        "Sweep values are rejected: every numeric leaf must be a bare fixed value",
    ),
    ("entry", "effective entry is entry AND all filters"),
    (
        "windows",
        "a windowed backtest warms indicators on the full snapshot history before `from` and counts only `[from, to)` — the run's `inputs.lead_in_from` records where the warm-up began",
    ),
    (
        "series_htf",
        "an `htf` operand is evaluated on the last closed H4 bar; an ATR stop uses the signal bar's ATR",
    ),
    // r3.s2.w4 (Q3): the fixed daily series — same closed-bar rule, its own
    // resolution (no argument) and the fetch that creates a missing snapshot.
    (
        "series_d1",
        "a `d1` operand is evaluated on the last closed daily (1d) bar, never on the forming one; the daily snapshot needs no argument — it resolves automatically for the pair a strategy is run on, and a missing one is refused by name: fetch it with `pulse fetch-data <PAIR> --tf D1`; the `timeframe` argument still selects only `primary` or `htf`, and `1d` is refused there because the daily series is the fixed `d1` slot",
    ),
    // r3.s2.w1 (b2): schema 1.2.0 — MACD's output selector.
    (
        "macd_output",
        "a MACD operand's `output` selects `line`, `signal` or `histogram`; omitting it is the historical line — signal and histogram warm up over max(fast,slow)+signal−1 bars",
    ),
    // r3.s2.w2 (Q1): schema 1.2.0 — the rolling extremes.
    (
        "rolling_extremes",
        "`Highest{period, source}` and `Lowest{period, source}` cover the N closed bars BEFORE the current bar, excluding it (the prior-N convention, rendered `highest(high, 20 prior)`); `source` is any price field and defaults to `high` for `Highest` and `low` for `Lowest`; they warm up over N+1 bars and work on any series (`h4:highest(high, 20)` is the prior 20 closed H4 bars)",
    ),
    // r4.s6.w1/w3 (schema 1.3.0), published by r4.s6.w4: the structure and
    // pattern vocabulary — including the zone-break form the never-true
    // same-bar comparison would violate.
    (
        "structure_indicators",
        "the schema-1.3.0 structure and pattern variants work on any series (an `h4:` operand reads the last closed H4 bar): `PivotHigh{left,right}`/`PivotLow{left,right}` are the most recent confirmed swing high/low, confirmed only once the `right` bars after them have closed, so a value never repaints; `Resistance{left,right,tolerance_pct,min_touches,lookback}`/`Support{…}` are the nearest level strictly above/below the current close, built from the confirmed pivots within the last `lookback` bars: a candidate level is a run of sorted pivot prices whose highest is within `tolerance_pct` of their lowest (for each start pivot, the maximal run within tolerance of that start's own price; runs may overlap), and a run of at least `min_touches` (>= 2) pivots is a candidate whose value is the mean of its pivots — the value is the LOWEST candidate strictly above the close (`Support`: the HIGHEST strictly below it), a pivot counting while it stays inside `lookback` bars (`lookback` > `left + right`); `tolerance_pct` is a decimal fraction in (0, 0.10] (`0.003` = 0.3%); `Body`/`UpperWick`/`LowerWick`/`Range` are the current bar's `abs(close−open)`, `high−max(open,close)`, `min(open,close)−low` and `high−low`; `DoubleTopNeckline{left,right,tolerance_pct}`/`DoubleBottomNeckline{…}` are the lowest low/highest high strictly between the last two confirmed pivot highs/lows within tolerance; `TrendlineLows{left,right}`/`TrendlineHighs{left,right}` are the line through the last two rising/falling confirmed pivots extended to the current bar, its quotient rounded to 8 decimal places half-even; because a `Resistance`/`Support` level sits strictly above/below the close of its own bar by definition, a break compares the close with the PREVIOUS bar's level — `CrossesAbove(close, lag(Resistance{…}, 1))`, the lag counting H4 bars on an `h4:` operand (the mirror is `CrossesBelow` over `lag(Support{…}, 1)`) — never the same bar's level; after warm-up a comparison over a level/pattern operand with no level at that bar is false, so a `Not` over it is true",
    ),
    // r5.s1.w1/w2 (schema 1.4.0), published by r5.s1.w3: the named candlestick
    // patterns, their read-path token, and the support-touch form the example
    // trades.
    (
        "candle_patterns",
        "the schema-1.4.0 named candlestick patterns are read through the comparisons that already existed (`CandlePattern{pattern}`, on any series — an `h4:` operand reads the last closed H4 bar): the value is TA-Lib's own integer on every bar after the pattern's warm-up — +100 bullish, -100 bearish, 0 no pattern, and ±80 for the PARTIAL `Engulfing` and `Harami` forms (a body end coincides with or touches the prior body's edge) — so a rule that wants \"fired\" reads `Gt 0` / `Lt 0` and one that wants only the full form reads `Eq 100` / `Eq -100`; the 17 names and their signs are bullish only `Hammer`, `InvertedHammer`, `Piercing`, `MorningStar`, `ThreeWhiteSoldiers`; bearish only `HangingMan`, `ShootingStar`, `DarkCloudCover`, `EveningStar`, `ThreeBlackCrows`; both signs, by the bar's own colour, `Marubozu`, `SpinningTop`, `Engulfing`; and `Harami`, which also carries both signs but takes the OPPOSITE of the PRIOR bar's colour (+ when the prior bar is black, a bullish harami), never the colour of the bar it fires on; and `Doji`, `DragonflyDoji`, `GravestoneDoji`, which read +100 only and carry no direction. Warm-up is TA-Lib's own lookback (10–13 bars by pattern) and the value is `None` before it — never \"no pattern\"; the pattern is evaluated on the closed bar and never repaints. `export_indicators`, and the CLI `--indicator` flag that shares its parser, carry the same vocabulary as `cdl:<name>` — the name matches ignoring case and underscores, and the column label is always `cdl:<snake_case>` (`cdl:morning_star`), so spelling variants dedup to one column. The checked-in 4h support-rejection example reads a pattern beside a support touch: `Low{h4} <= lag(Support{h4}, 1) × (1 + k)` AND `Close{h4} > lag(Support{h4}, 1)` AND `CandlePattern{…} > 0` — the level the PREVIOUS H4 bar saw, the same lag doctrine the level entry above states for a break, because a `Support` value sits strictly below its own bar's close",
    ),
    // r3.s2.w3 (Q2): value expressions — the semantics the grammar cannot
    // express, keyed by the construct each entry covers.
    (
        "expressions",
        "an `Arith` node computes add/sub/mul/div pointwise over two operand values, `Decimal` only; division by zero, an arithmetic overflow, or any operand without a value gives no value — a comparison over a no-value is false (a `Not` over it is true only behind the warm gate)",
    ),
    (
        "expression_depth",
        "at most 4 `Arith`/`Lag` nodes along any path through one value; a fifth is refused",
    ),
    (
        "lag",
        "`lag(value, bars)` reads the value `bars` bars back on the value's own series (`bars` in 1..=500; an `h4:` operand lags in H4 bars); a lag under a lag and a lag over a value mixing both series are refused — use one lag with a larger `bars`",
    ),
    (
        "rising_falling",
        "`rising`/`falling` mean the value is strictly greater/less than its own value `bars` bars ago on the same series (`bars` in 1..=500, default 1); over a value containing a lag, or mixing both series, they are refused",
    ),
    // r3.s2.w1: schema 1.2.0 — the served version set and its forward rule.
    // r4.s6.w1: moved to 1.3.0 (the structure indicator variants); the served
    // set grows by 1.2.0. r5.s1.w1: moved to 1.4.0 (the named candlestick
    // pattern variant); the served set grows by 1.3.0.
    (
        "schema_version",
        "this engine writes and serves schema 1.4.0 and also loads 1.0.0, 1.1.0, 1.2.0 and 1.3.0 (identity-migrated); any newer version is refused — never write a schema_version other than 1.4.0",
    ),
];

/// Build the `pulse://dsl/schema` document body (a compact JSON string).
///
/// Shape: `{ schema_version, json_schema, conventions }` where `json_schema`
/// is `schemars::schema_for!(StrategyDsl)` — the grammar every contained type
/// derives — and `conventions` is the normative prose map above.
pub(crate) fn dsl_schema_json() -> String {
    let conventions: serde_json::Map<String, serde_json::Value> = CONVENTIONS
        .iter()
        .map(|(k, v)| ((*k).to_owned(), serde_json::Value::String((*v).to_owned())))
        .collect();
    let body = serde_json::json!({
        "schema_version": DSL_SCHEMA_VERSION,
        "json_schema": schemars::schema_for!(StrategyDsl),
        "conventions": conventions,
    });
    serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::dsl_schema_json;
    use crate::domain::DSL_SCHEMA_VERSION;

    #[test]
    fn dsl_schema_document_shape() {
        let doc: serde_json::Value =
            serde_json::from_str(&dsl_schema_json()).expect("resource body parses as JSON");
        assert_eq!(doc["schema_version"], DSL_SCHEMA_VERSION);
        let properties = doc["json_schema"]["properties"]
            .as_object()
            .expect("json_schema has properties");
        for field in [
            "entry",
            "filters",
            "exits",
            "risk",
            "direction",
            "name",
            "schema_version",
        ] {
            assert!(properties.contains_key(field), "missing property {field}");
        }
        // F6: `schema_version` publishes the ACCEPTED values, not an
        // unconstrained string — `Migrator::v1()` loads `1.0.0`, `1.1.0`,
        // `1.2.0` and `1.3.0` (identity-migrated) and `CURRENT`, so the schema a
        // validating agent reads must pin exactly that enum.
        assert_eq!(
            properties["schema_version"]["enum"],
            serde_json::json!(["1.0.0", "1.1.0", "1.2.0", "1.3.0", DSL_SCHEMA_VERSION]),
            "schema_version property must publish the accepted enum"
        );
        let conventions = doc["conventions"].as_object().expect("conventions object");
        let window = conventions
            .get("windows")
            .and_then(serde_json::Value::as_str)
            .expect("conventions.windows is a string");
        assert_eq!(
            window,
            "a windowed backtest warms indicators on the full snapshot history before `from` and counts only `[from, to)` — the run's `inputs.lead_in_from` records where the warm-up began"
        );
        // r2.s2.w2 (b13): the schema-1.1.0 semantics the grammar cannot express.
        let htf = conventions
            .get("series_htf")
            .and_then(serde_json::Value::as_str)
            .expect("conventions.series_htf is a string");
        assert_eq!(
            htf,
            "an `htf` operand is evaluated on the last closed H4 bar; an ATR stop uses the signal bar's ATR"
        );
        // r3.s2.w4 (Q3): the fixed daily series — the same closed-bar rule, the
        // resolution that takes no argument, and the fetch that creates the
        // snapshot a `d1` strategy needs.
        let d1 = conventions
            .get("series_d1")
            .and_then(serde_json::Value::as_str)
            .expect("conventions.series_d1 is a string");
        assert_eq!(
            d1,
            "a `d1` operand is evaluated on the last closed daily (1d) bar, never on the forming one; the daily snapshot needs no argument — it resolves automatically for the pair a strategy is run on, and a missing one is refused by name: fetch it with `pulse fetch-data <PAIR> --tf D1`; the `timeframe` argument still selects only `primary` or `htf`, and `1d` is refused there because the daily series is the fixed `d1` slot"
        );
    }
}
