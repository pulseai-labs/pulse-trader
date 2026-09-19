//! Version-safe migration read-path for strategy DSL documents (FR-4).
//!
//! 2.02 shipped [`SchemaVersion`] + `CURRENT` + string serde and established that
//! **direct `serde_json::from_str::<StrategyDsl>` is migration-UNAWARE** — it
//! accepts any `schema_version` (even a future one) and performs no migration.
//! This module builds the version-safe loader: a [`Migrator`] that detects a
//! document's `schema_version`, migrates the **JSON** forward to
//! [`SchemaVersion::CURRENT`], preserves the verbatim `dsl_original`, and
//! deserializes into an (unvalidated) current-version [`StrategyDsl`].
//!
//! **Migration operates on `serde_json::Value`, not on a typed `StrategyDsl`**
//! (architect-critic): an old document may not deserialize into the *current*
//! grammar at all, so each migration step rewrites the JSON `Value` and only the
//! final, current-version `Value` is deserialized.
//!
//! **No validation, no compilation.** `load` returns an un-validated
//! `StrategyDsl`; validation is 2.03's and compilation is 2.04's. The caller
//! (VS-1.1.4 / agent layer) composes `load → validate → compile`.
//!
//! **The production registry.** [`Migrator::v1`] registers the real steps —
//! currently one: the `1.0.0 → 1.1.0` identity step (schema 1.1.0 is additive —
//! a new defaulted field and new enum variants — so the step rewrites only the
//! `schema_version` string; every `1.0.0` document deserializes identically at
//! `1.1.0`). Chaining + error paths are additionally proven by synthetic test
//! migrations (see the test module).
//!
//! **No forward-compatibility** (architect-critic): every `schema_version >
//! CURRENT` — including a same-major future minor like `1.2.0` vs `1.1.0` — is
//! rejected as [`LoadError::FutureVersion`]. A newer minor may carry semantics
//! this engine cannot honor, and silently ignoring unknown fields could
//! mis-execute a real-money strategy.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::schema_version::SchemaVersion;
use super::strategy::StrategyDsl;

/// The kind of version bump a migration performs.
///
/// `Minor` steps auto-apply at read; a `Major` step is still applied, but its
/// *presence* in the registry is what an absent path would otherwise block on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MigrationKind {
    /// A backward-compatible additive change (only minor/patch differ).
    Minor,
    /// A breaking grammar change (the major version differs).
    Major,
}

/// A single registered migration step that rewrites a document's JSON `Value`
/// from `from` to `to`.
///
/// The `apply` closure operates on `serde_json::Value` because an old document
/// may not deserialize into the *current* typed grammar — it is migrated as JSON
/// and only the final current-version `Value` is typed-deserialized.
#[derive(Clone)]
pub struct Migration {
    /// The version this step migrates *from* (registry entries are unique by
    /// `from`).
    pub from: SchemaVersion,
    /// The version this step migrates *to*.
    pub to: SchemaVersion,
    /// Whether this is a minor (auto) or major (breaking) step.
    pub kind: MigrationKind,
    /// The transformation applied to the JSON document at this step.
    pub apply: fn(Value) -> Result<Value, MigrationError>,
}

/// The error a [`Migration`]'s `apply` step can raise (e.g. a required field is
/// absent in the old shape). Folded into [`LoadError::MigrationFailed`].
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[error("migration step failed: {0}")]
pub struct MigrationError(pub String);

/// The outcome of a successful version-safe load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    /// The (unvalidated) current-version strategy document.
    pub dsl: StrategyDsl,
    /// The **verbatim** input the document was loaded from, preserved through
    /// migration per FR-4 (NOT a re-serialization).
    pub dsl_original: String,
    /// The `schema_version` the document was originally authored at.
    pub from: SchemaVersion,
    /// Whether any migration step was applied (`false` when the document was
    /// already at `CURRENT`).
    pub migrated: bool,
}

/// Errors raised by the version-safe loader.
///
/// `thiserror`-derived for ergonomic `Display`/`Error`, and `serde`-serializable
/// so errors can cross the `Tauri` boundary later (mirrors `DataError`).
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[non_exhaustive]
pub enum LoadError {
    /// The input is not valid JSON.
    #[error("could not parse document as JSON: {0}")]
    Parse(String),

    /// The document has no `schema_version` field (or it is not a string).
    #[error("document is missing a string `schema_version` field")]
    MissingVersion,

    /// The `schema_version` string is not a valid `"MAJOR.MINOR.PATCH"` semver.
    #[error("malformed `schema_version`: {0}")]
    BadVersion(String),

    /// The document's version is newer than this engine's `CURRENT`. No
    /// forward-compatibility: all `> CURRENT` are rejected, including a
    /// same-major future minor.
    #[error(
        "document schema_version {found} is newer than this engine's {current}; refusing to load (no forward-compatibility)"
    )]
    FutureVersion {
        /// The version found in the document.
        found: SchemaVersion,
        /// The engine's current version.
        current: SchemaVersion,
    },

    /// No registered migration chains the document's version up to `CURRENT`.
    #[error("no migration path from schema_version {from} to CURRENT")]
    NoMigrationPath {
        /// The version at which the chain dead-ended.
        from: SchemaVersion,
    },

    /// A migration step's `apply` raised, OR the registry is malformed (a step
    /// that does not advance the version, or a cycle).
    #[error("migration from {from} failed: {message}")]
    MigrationFailed {
        /// The version the failing step started from.
        from: SchemaVersion,
        /// What went wrong (the step's error, or the guard that fired).
        message: String,
    },

    /// The migrated (current-version) JSON did not deserialize into a
    /// `StrategyDsl`.
    #[error("could not deserialize the current-version document: {0}")]
    Deserialize(String),
}

impl SchemaVersion {
    /// Classify the bump between two versions (the major-vs-minor logic 2.02
    /// deferred to 2.05).
    ///
    /// - majors differ → [`MigrationKind::Major`]
    /// - only minor/patch differ → [`MigrationKind::Minor`]
    /// - equal → `None`
    #[must_use]
    pub fn bump_kind(from: SchemaVersion, to: SchemaVersion) -> Option<MigrationKind> {
        if from == to {
            None
        } else if from.major == to.major {
            Some(MigrationKind::Minor)
        } else {
            Some(MigrationKind::Major)
        }
    }
}

/// An ordered registry of [`Migration`]s that drives the version-safe read-path.
///
/// A **value** (not a global) so tests can build a custom registry with
/// synthetic migrations. [`Migrator::v1`] is the production registry — it
/// carries the real `1.0.0 → 1.1.0` identity step.
#[derive(Clone, Default)]
pub struct Migrator {
    migrations: Vec<Migration>,
}

/// The `1.0.0 → 1.1.0` step: **identity**. Schema 1.1.0 is purely additive
/// (ADR-0024 — the `series` field defaults to `primary`, `IndicatorSpec::Atr`
/// and `ExitRule::AtrStop` are new variants), so the document's meaning is
/// unchanged; `apply` rewrites only the `schema_version` string and leaves
/// every other byte alone. `dsl_original` is preserved verbatim by [`Loaded`]
/// regardless — the migration never touches it.
fn identity_1_0_0_to_1_1_0(mut value: Value) -> Result<Value, MigrationError> {
    let obj = value
        .as_object_mut()
        .ok_or_else(|| MigrationError("document is not a JSON object".to_owned()))?;
    obj.insert(
        "schema_version".to_owned(),
        serde_json::Value::String("1.1.0".to_owned()),
    );
    Ok(value)
}

impl Migrator {
    /// The production registry: the one real step, `1.0.0 → 1.1.0` (identity).
    ///
    /// `to` is pinned to the literal `1.1.0` the step's `apply` stamps —
    /// NOT `SchemaVersion::CURRENT` (r2.s2 round-1 fix F8): a future `CURRENT`
    /// bump must not silently desync the registration from the version the
    /// step actually produces. A test below pins the two to the same literal.
    #[must_use]
    pub fn v1() -> Self {
        Migrator {
            migrations: vec![Migration {
                from: SchemaVersion {
                    major: 1,
                    minor: 0,
                    patch: 0,
                },
                to: SchemaVersion {
                    major: 1,
                    minor: 1,
                    patch: 0,
                },
                kind: MigrationKind::Minor,
                apply: identity_1_0_0_to_1_1_0,
            }],
        }
    }

    /// Build a registry from an explicit set of migrations (used by tests to
    /// inject synthetic migrations).
    #[must_use]
    pub fn with_migrations(migrations: Vec<Migration>) -> Self {
        Migrator { migrations }
    }

    /// Parse a JSON document string and load it version-safely, preserving the
    /// verbatim input as `dsl_original`.
    ///
    /// `parse → detect version → migrate to CURRENT → deserialize`. Returns an
    /// un-validated current-version [`StrategyDsl`] (validation is the caller's).
    ///
    /// # Errors
    ///
    /// See [`LoadError`]: bad JSON, a missing/malformed `schema_version`, a
    /// future version, a missing migration path, a failing/looping migration, or
    /// a final-deserialize failure.
    pub fn load(&self, json: &str) -> Result<Loaded, LoadError> {
        let value: Value =
            serde_json::from_str(json).map_err(|e| LoadError::Parse(e.to_string()))?;
        self.load_value(value, json)
    }

    /// Load a document the caller already holds as a `serde_json::Value`,
    /// preserving `original` verbatim as `dsl_original` (avoids a re-parse — e.g.
    /// VS-1.1.4 persistence reading a column).
    ///
    /// # Errors
    ///
    /// See [`LoadError`] (every case except `Parse`, which only `load` raises).
    pub fn load_value(&self, value: Value, original: &str) -> Result<Loaded, LoadError> {
        let from = read_version(&value)?;
        let current = SchemaVersion::CURRENT;

        if from > current {
            return Err(LoadError::FutureVersion {
                found: from,
                current,
            });
        }

        let migrated = from < current;
        let current_value = if migrated {
            self.resolve_and_apply(value, from)?
        } else {
            value
        };

        let dsl: StrategyDsl = serde_json::from_value(current_value)
            .map_err(|e| LoadError::Deserialize(e.to_string()))?;

        Ok(Loaded {
            dsl,
            dsl_original: original.to_owned(),
            from,
            migrated,
        })
    }

    /// Walk the migration chain from `start` up to `CURRENT`, applying each step.
    ///
    /// Guards (architect-critic): the loop is bounded by the registry length and
    /// tracks visited versions; a step that does not advance the version or
    /// revisits a seen one (cycle / mis-registration) raises
    /// [`LoadError::MigrationFailed`]. Never loops unboundedly.
    fn resolve_and_apply(
        &self,
        mut value: Value,
        start: SchemaVersion,
    ) -> Result<Value, LoadError> {
        let current = SchemaVersion::CURRENT;
        let mut version = start;
        let mut visited: Vec<SchemaVersion> = vec![version];

        // Bound by registry length: each migration may be applied at most once on
        // a well-formed chain, so more steps than entries means a malformed loop.
        for _ in 0..=self.migrations.len() {
            if version == current {
                return Ok(value);
            }

            let migration = self
                .migrations
                .iter()
                .find(|m| m.from == version)
                .ok_or(LoadError::NoMigrationPath { from: version })?;

            value = (migration.apply)(value).map_err(|e| LoadError::MigrationFailed {
                from: version,
                message: e.0,
            })?;

            let next = migration.to;

            // Guard: a step must strictly advance, and must not revisit a seen
            // version (cycle / mis-registration).
            if next == version || visited.contains(&next) {
                return Err(LoadError::MigrationFailed {
                    from: version,
                    message: format!(
                        "migration registry does not advance toward CURRENT (step {version} -> {next} is non-advancing or cyclic)"
                    ),
                });
            }

            visited.push(next);
            version = next;
        }

        // Exhausted the registry-length bound without reaching CURRENT: malformed.
        Err(LoadError::MigrationFailed {
            from: start,
            message: format!(
                "migration chain from {start} did not reach CURRENT within the registry bound (malformed registry)"
            ),
        })
    }
}

/// Read the `schema_version` string out of a JSON document `Value`.
fn read_version(value: &Value) -> Result<SchemaVersion, LoadError> {
    let raw = value
        .get("schema_version")
        .and_then(Value::as_str)
        .ok_or(LoadError::MissingVersion)?;
    raw.parse::<SchemaVersion>()
        .map_err(|e| LoadError::BadVersion(e.to_string()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{LoadError, Migration, MigrationError, MigrationKind, Migrator};
    use crate::domain::dsl::condition::{Comparator, Condition};
    use crate::domain::dsl::exit::ExitRule;
    use crate::domain::dsl::risk::{Direction, RiskParams};
    use crate::domain::dsl::schema_version::SchemaVersion;
    use crate::domain::dsl::strategy::StrategyDsl;
    use crate::domain::dsl::sweepable::SweepableValue;
    use crate::domain::dsl::value::{IndicatorSpec, Series, ValueSource};
    use rust_decimal::Decimal;
    use serde_json::{Value, json};

    fn v(major: u16, minor: u16, patch: u16) -> SchemaVersion {
        SchemaVersion {
            major,
            minor,
            patch,
        }
    }

    /// The canonical `1.1.0` RSI-oversold strategy (the demo-1/demo-2 shape from
    /// 2.02's `rsi_oversold_strategy`). Used to derive the canonical JSON via
    /// serialization, so the test JSON is guaranteed to match the real serde
    /// shapes (struct-tagged enums, string-encoded decimals) rather than a
    /// hand-written approximation.
    fn canonical_strategy() -> StrategyDsl {
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
            filters: vec![],
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

    /// The canonical `1.1.0` strategy as a JSON string (serialized from the typed
    /// value, so the shape is exactly the current grammar's).
    fn canonical_current_json() -> String {
        serde_json::to_string(&canonical_strategy()).expect("serialize canonical strategy")
    }

    /// AC-5: the canonical `1.1.0` strategy loads via `Migrator::v1().load(json)`
    /// → `Ok(Loaded { migrated: false, .. })`, `dsl_original == json`, and `dsl`
    /// equals the directly-deserialized value.
    #[test]
    fn loads_current_version_without_migration() {
        let json = canonical_current_json();
        let loaded = Migrator::v1()
            .load(&json)
            .expect("load current-version doc");

        assert!(!loaded.migrated, "current-version doc must not be migrated");
        assert_eq!(loaded.from, SchemaVersion::CURRENT);
        assert_eq!(loaded.dsl_original, json, "dsl_original must be verbatim");

        // `dsl` equals the directly-deserialized value (migration-unaware path).
        let direct: crate::domain::dsl::StrategyDsl =
            serde_json::from_str(&json).expect("direct deserialize");
        assert_eq!(loaded.dsl, direct);
    }

    /// AC-6: a doc with `schema_version = "99.0.0"` → `FutureVersion`.
    #[test]
    fn rejects_future_version() {
        let mut value: Value = serde_json::from_str(&canonical_current_json()).unwrap();
        value["schema_version"] = json!("99.0.0");
        let json = value.to_string();

        let err = Migrator::v1()
            .load(&json)
            .expect_err("future version must reject");
        match err {
            LoadError::FutureVersion { found, current } => {
                assert_eq!(found, v(99, 0, 0));
                assert_eq!(current, SchemaVersion::CURRENT);
            }
            other => panic!("expected FutureVersion, got {other:?}"),
        }
    }

    /// AC-6 (companion): a same-major *future minor* is ALSO rejected (no
    /// forward-compatibility, architect-critic C4).
    #[test]
    fn rejects_same_major_future_minor() {
        let mut value: Value = serde_json::from_str(&canonical_current_json()).unwrap();
        value["schema_version"] = json!("1.2.0");
        let json = value.to_string();

        let err = Migrator::v1()
            .load(&json)
            .expect_err("same-major future minor must reject");
        assert!(
            matches!(err, LoadError::FutureVersion { .. }),
            "expected FutureVersion for 1.2.0 vs 1.1.0, got {err:?}"
        );
    }

    /// AC-7: an older version not covered by `v1()`'s registry →
    /// `NoMigrationPath`.
    #[test]
    fn rejects_unknown_old_version_with_no_path() {
        let mut value: Value = serde_json::from_str(&canonical_current_json()).unwrap();
        value["schema_version"] = json!("0.9.0");
        let json = value.to_string();

        let err = Migrator::v1()
            .load(&json)
            .expect_err("old version with empty registry must reject");
        match err {
            LoadError::NoMigrationPath { from } => assert_eq!(from, v(0, 9, 0)),
            other => panic!("expected NoMigrationPath, got {other:?}"),
        }
    }

    /// A synthetic minor migration `0.9.0 → CURRENT` that renames an old field
    /// (`strat_name` → `name`) and stamps the current `schema_version`. Proves
    /// the framework end-to-end against a `from` the real registry does not
    /// cover.
    fn synthetic_minor_0_9_to_current() -> Migration {
        fn apply(mut value: Value) -> Result<Value, MigrationError> {
            let obj = value
                .as_object_mut()
                .ok_or_else(|| MigrationError("document is not a JSON object".to_owned()))?;
            // Old shape used `strat_name`; current grammar uses `name`.
            let name = obj
                .remove("strat_name")
                .ok_or_else(|| MigrationError("old doc missing `strat_name`".to_owned()))?;
            obj.insert("name".to_owned(), name);
            obj.insert(
                "schema_version".to_owned(),
                json!(SchemaVersion::CURRENT.to_string()),
            );
            Ok(value)
        }
        Migration {
            from: v(0, 9, 0),
            to: SchemaVersion::CURRENT,
            kind: MigrationKind::Minor,
            apply,
        }
    }

    /// The canonical strategy in its *old* `0.9.0` shape (uses `strat_name`
    /// instead of `name`). Derived from the canonical current JSON by renaming the
    /// field + downgrading the version, so every other field stays byte-for-byte
    /// the current grammar — the synthetic migration only has to undo the rename.
    fn old_0_9_json() -> String {
        let mut value: Value = serde_json::from_str(&canonical_current_json())
            .expect("parse canonical for old-shape derivation");
        let obj = value.as_object_mut().expect("canonical is an object");
        let name = obj.remove("name").expect("canonical has `name`");
        obj.insert("strat_name".to_owned(), name);
        obj.insert("schema_version".to_owned(), json!("0.9.0"));
        value.to_string()
    }

    /// AC-8: a test `Migrator` with a synthetic minor migration applies it; old
    /// JSON → `Ok(Loaded { migrated: true, .. })`, the resulting `dsl` is the
    /// current shape, AND `dsl_original` is the old input verbatim.
    #[test]
    fn applies_synthetic_minor_migration_and_preserves_original() {
        let old = old_0_9_json();
        let migrator = Migrator::with_migrations(vec![synthetic_minor_0_9_to_current()]);

        let loaded = migrator.load(&old).expect("synthetic migration must load");

        assert!(loaded.migrated, "old doc must be reported as migrated");
        assert_eq!(loaded.from, v(0, 9, 0));
        assert_eq!(
            loaded.dsl_original, old,
            "dsl_original must be the OLD input verbatim"
        );
        // The migrated `dsl` is the current shape, with the renamed `name`.
        assert_eq!(loaded.dsl.schema_version, SchemaVersion::CURRENT);
        assert_eq!(loaded.dsl.name, "RSI Oversold");

        // And it equals the canonical current-version document.
        let canonical: crate::domain::dsl::StrategyDsl =
            serde_json::from_str(&canonical_current_json()).expect("canonical deserialize");
        assert_eq!(loaded.dsl, canonical);
    }

    /// AC-9: `bump_kind` classifies major / minor / none.
    #[test]
    fn bump_kind_classifies_major_minor() {
        assert_eq!(
            SchemaVersion::bump_kind(v(1, 0, 0), v(2, 0, 0)),
            Some(MigrationKind::Major)
        );
        assert_eq!(
            SchemaVersion::bump_kind(v(1, 0, 0), v(1, 1, 0)),
            Some(MigrationKind::Minor)
        );
        assert_eq!(SchemaVersion::bump_kind(v(1, 0, 0), v(1, 0, 0)), None);
    }

    /// AC-10 (architect-critic C3): a cyclic / non-advancing registry →
    /// `MigrationFailed`, and the loop terminates (no hang).
    #[test]
    fn rejects_cyclic_or_stalled_migration_registry() {
        // Identity passthrough so the chain logic — not the transform — is tested.
        // The `Result` wrap is mandatory: the signature must match
        // `Migration.apply: fn(Value) -> Result<Value, MigrationError>`.
        #[allow(clippy::unnecessary_wraps)]
        fn passthrough(value: Value) -> Result<Value, MigrationError> {
            Ok(value)
        }

        // A cycle: 0.8.0 -> 0.9.0 -> 0.8.0, never reaching CURRENT (1.1.0).
        let cyclic = Migrator::with_migrations(vec![
            Migration {
                from: v(0, 8, 0),
                to: v(0, 9, 0),
                kind: MigrationKind::Minor,
                apply: passthrough,
            },
            Migration {
                from: v(0, 9, 0),
                to: v(0, 8, 0),
                kind: MigrationKind::Minor,
                apply: passthrough,
            },
        ]);

        let mut value: Value = serde_json::from_str(&old_0_9_json()).unwrap();
        value["schema_version"] = json!("0.8.0");
        let json = value.to_string();

        let err = cyclic
            .load(&json)
            .expect_err("cyclic registry must reject (and terminate)");
        assert!(
            matches!(err, LoadError::MigrationFailed { .. }),
            "expected MigrationFailed for a cyclic registry, got {err:?}"
        );

        // A non-advancing self-loop: 0.9.0 -> 0.9.0.
        let stalled = Migrator::with_migrations(vec![Migration {
            from: v(0, 9, 0),
            to: v(0, 9, 0),
            kind: MigrationKind::Minor,
            apply: passthrough,
        }]);
        let err = stalled
            .load(&old_0_9_json())
            .expect_err("non-advancing registry must reject (and terminate)");
        assert!(
            matches!(err, LoadError::MigrationFailed { .. }),
            "expected MigrationFailed for a stalled registry, got {err:?}"
        );
    }

    /// r2.s2 round-1 fix F8: the identity step's registered `to` is the literal
    /// `1.1.0` its `apply` stamps — pinned, not `SchemaVersion::CURRENT` — so a
    /// future `CURRENT` bump cannot silently desync the chain: this test fails
    /// if the registration and the stamped literal ever diverge.
    #[test]
    fn v1_terminal_version_matches_the_literal_its_apply_stamps() {
        let migrator = Migrator::v1();
        let step = migrator
            .migrations
            .iter()
            .find(|m| m.from == v(1, 0, 0))
            .expect("v1 registers the 1.0.0 -> 1.1.0 step");

        // The registration pins the literal, not CURRENT.
        assert_eq!(step.to, v(1, 1, 0));

        // And the literal equals what `apply` stamps on a real 1.0.0 document.
        let mut doc: Value =
            serde_json::from_str(&canonical_current_json()).expect("canonical parses");
        doc["schema_version"] = json!("1.0.0");
        let migrated = (step.apply)(doc).expect("identity step applies");
        let stamped: SchemaVersion = migrated["schema_version"]
            .as_str()
            .expect("schema_version stays a string")
            .parse()
            .expect("the stamped version parses");
        assert_eq!(
            stamped, step.to,
            "the registry's terminal `to` must equal the literal `apply` stamps"
        );
    }
}
