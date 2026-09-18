//! `SweepableValue<T>` — a tunable numeric leaf of the strategy DSL.
//!
//! Wraps every tunable numeric (indicator periods, thresholds) so the schema is
//! stable across the v2 parameter-sweep feature. v1 only ever *constructs* the
//! [`SweepableValue::Fixed`] variant; [`SweepableValue::Sweep`] is an
//! architectural stub that is *representable* now (locking schema stability) but
//! rejected by validation (2.03) and the compiler (2.04). No sweep machinery
//! lives here — this is grammar shape only.

use serde::{Deserialize, Serialize};

/// A numeric leaf that is either a fixed value or a (future) parameter sweep.
///
/// `#[serde(untagged)]` so the common case is terse and unambiguous:
/// [`SweepableValue::Fixed`] serializes as the **bare value** (e.g. `"14"`),
/// while [`SweepableValue::Sweep`] serializes as an **object**
/// (`{"start":…,"end":…,"step":…}`). The two are structurally disjoint (scalar
/// vs object), so untagged deserialization is round-trip-safe.
///
/// Generic over the leaf numeric: `SweepableValue<Decimal>` for thresholds,
/// `SweepableValue<u32>` for indicator periods. No `f64` anywhere (NFR-2
/// determinism).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SweepableValue<T> {
    /// A single fixed value — the only variant v1 constructs.
    Fixed(T),
    /// A parameter sweep over `[start, end]` by `step` — a v2 stub. Rejected by
    /// validation (2.03) and the compiler (2.04) in v1.
    Sweep {
        /// Inclusive lower bound of the sweep.
        start: T,
        /// Inclusive upper bound of the sweep.
        end: T,
        /// Step between successive sweep points.
        step: T,
    },
}

// r2.s1.w2: the `pulse://dsl/schema` resource publishes the ACCEPTED wire
// shape, not the representable one — v1 accepts only `Fixed`, which serializes
// as the bare `T`. The schema therefore delegates to `T` verbatim (inline, same
// name/id) instead of describing the untagged `Fixed | Sweep` union: publishing
// the sweep object shape would tell an external agent a rejected form is valid.
impl<T: schemars::JsonSchema> schemars::JsonSchema for SweepableValue<T> {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> std::borrow::Cow<'static, str> {
        T::schema_name()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        T::schema_id()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        T::json_schema(generator)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::SweepableValue;
    use rust_decimal::Decimal;

    #[test]
    fn fixed_serializes_as_bare_value() {
        // u32 Fixed serializes as a bare number.
        let v: SweepableValue<u32> = SweepableValue::Fixed(14);
        let json = serde_json::to_string(&v).expect("serialize Fixed<u32>");
        assert_eq!(json, "14");
        let back: SweepableValue<u32> =
            serde_json::from_str(&json).expect("deserialize Fixed<u32>");
        assert_eq!(back, v);
    }

    #[test]
    fn fixed_decimal_serializes_as_string() {
        // Decimal (serde-with-str) serializes as a JSON string, bare (no wrapper).
        let v: SweepableValue<Decimal> = SweepableValue::Fixed(Decimal::new(30, 0));
        let json = serde_json::to_string(&v).expect("serialize Fixed<Decimal>");
        assert_eq!(json, "\"30\"");
        let back: SweepableValue<Decimal> =
            serde_json::from_str(&json).expect("deserialize Fixed<Decimal>");
        assert_eq!(back, v);
    }

    #[test]
    fn sweep_serializes_as_object() {
        let v: SweepableValue<u32> = SweepableValue::Sweep {
            start: 5,
            end: 20,
            step: 5,
        };
        let json = serde_json::to_string(&v).expect("serialize Sweep");
        let back: SweepableValue<u32> = serde_json::from_str(&json).expect("deserialize Sweep");
        assert_eq!(back, v);
    }
}
