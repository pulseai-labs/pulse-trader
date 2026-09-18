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
        "a windowed backtest slices the series to `[from, to)` and indicators warm up inside the window",
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

    #[test]
    fn dsl_schema_document_shape() {
        let doc: serde_json::Value =
            serde_json::from_str(&dsl_schema_json()).expect("resource body parses as JSON");
        assert_eq!(doc["schema_version"], "1.0.0");
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
        // F6: `schema_version` publishes the ACCEPTED value, not an
        // unconstrained string — `Migrator::v1()` loads exactly `CURRENT`, so
        // the schema a validating agent reads must pin the const.
        assert_eq!(
            properties["schema_version"]["const"], "1.0.0",
            "schema_version property must publish the accepted const"
        );
        let conventions = doc["conventions"].as_object().expect("conventions object");
        let window = conventions
            .get("windows")
            .and_then(serde_json::Value::as_str)
            .expect("conventions.windows is a string");
        assert_eq!(
            window,
            "a windowed backtest slices the series to `[from, to)` and indicators warm up inside the window"
        );
    }
}
