//! r2.s2.w1 — DSL schema 1.1.0 contract suite (the RED gate for AC-1).
//!
//! Pins the whole schema-step contract in one place: the identity migration
//! `1.0.0 → 1.1.0` over every committed fixture and an inline document, the new
//! grammar's serde round-trips (a `series`-scoped operand, `IndicatorSpec::Atr`,
//! `ExitRule::AtrStop`), the validation arms and their exact `FieldError` paths,
//! the mutation leaves in lockstep, the series-tagged compilation shapes
//! (`CompiledValue::{Price,Indicator}` carrying `Series::Htf`,
//! `CompiledExit::AtrStop`, `required_htf_indicators`/`needs_htf`), the pure
//! `atr_stop_price` helper, and the repository round-trip of a persisted
//! `1.0.0` document. Behaviour (ATR math, `bar.htf` reads, HTF stepping) is
//! covered by `tests/htf_atr_engine.rs`; this file pins the compile shapes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use pulse::{
    CandidateDsl, Comparator, CompiledExit, Condition, CreatedBy, Db, Direction, ExitRule,
    IndicatorSpec, MIGRATOR, Migrator, Mutation, NewVersion, ParamValue, PriceField, RiskParams,
    SchemaVersion, Series, SqliteStrategyRepo, StrategyDsl, StrategyRepository, SweepableValue,
    ValidationCode, ValueSource, apply, atr_stop_price, compile, sweepable_paths, validate,
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tempfile::TempDir;

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

fn v(major: u16, minor: u16, patch: u16) -> SchemaVersion {
    SchemaVersion {
        major,
        minor,
        patch,
    }
}

/// The canonical `1.0.0` document — the same shape `tests/fixtures/strategies/`
/// carries, written inline so the migration assertions run on a known byte
/// string too.
const INLINE_1_0_0: &str = r#"{
  "schema_version": "1.0.0",
  "name": "RSI Oversold",
  "direction": "long",
  "entry": {
    "type": "Compare",
    "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
    "op": "Lt",
    "rhs": { "type": "Constant", "value": "30" }
  },
  "filters": [],
  "exits": [
    { "type": "StopLoss", "distance_pct": "0.05" },
    { "type": "TakeProfit", "target_r": "2.0" }
  ],
  "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
}"#;

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// A minimal valid strategy builder: caller supplies entry + exits; direction,
/// name and risk are the canonical demo values.
fn dsl_with(entry: Condition, exits: Vec<ExitRule>) -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "schema-1.1 fixture".to_owned(),
        direction: Direction::Long,
        entry,
        filters: vec![],
        exits,
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(dec(1, 2)),
            max_leverage: SweepableValue::Fixed(dec(3, 0)),
        },
    }
}

fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

fn constant(mantissa: i64, scale: u32) -> ValueSource {
    ValueSource::Constant {
        value: dec(mantissa, scale),
    }
}

fn stop_loss() -> ExitRule {
    ExitRule::StopLoss {
        distance_pct: SweepableValue::Fixed(dec(5, 2)),
    }
}

fn take_profit() -> ExitRule {
    ExitRule::TakeProfit {
        target_r: SweepableValue::Fixed(dec(2, 0)),
    }
}

fn atr_stop(period: u32, multiple_mantissa: i64, multiple_scale: u32) -> ExitRule {
    ExitRule::AtrStop {
        period: SweepableValue::Fixed(period),
        multiple: SweepableValue::Fixed(dec(multiple_mantissa, multiple_scale)),
    }
}

/// Every `ValueSource::Price`/`ValueSource::Indicator` leaf reachable in `dsl`,
/// with its `series` (Constant carries none).
fn operand_series(dsl: &StrategyDsl) -> Vec<Series> {
    fn walk(cond: &Condition, out: &mut Vec<Series>) {
        match cond {
            Condition::Compare { lhs, rhs, .. }
            | Condition::CrossesAbove { lhs, rhs }
            | Condition::CrossesBelow { lhs, rhs } => {
                for src in [lhs, rhs] {
                    match src {
                        ValueSource::Price { series, .. }
                        | ValueSource::Indicator { series, .. } => out.push(*series),
                        ValueSource::Constant { .. } => {}
                    }
                }
            }
            Condition::And { conditions } | Condition::Or { conditions } => {
                for c in conditions {
                    walk(c, out);
                }
            }
            Condition::Not { condition } => walk(condition, out),
        }
    }
    let mut out = Vec::new();
    walk(&dsl.entry, &mut out);
    for f in &dsl.filters {
        walk(f, &mut out);
    }
    for e in &dsl.exits {
        if let ExitRule::SignalExit { condition } = e {
            walk(condition, &mut out);
        }
    }
    out
}

// ---- (a) identity migration over fixtures + the inline document ------------

#[test]
fn every_committed_fixture_migrates_1_0_0_to_1_1_0() {
    let dir = manifest("tests/fixtures/strategies");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixture dir readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "fixture dir must hold at least one json");

    for path in files {
        let json = std::fs::read_to_string(&path).expect("read fixture");
        let loaded = Migrator::v1()
            .load(&json)
            .unwrap_or_else(|e| panic!("{} must load: {e}", path.display()));

        assert!(
            loaded.migrated,
            "{} must report migrated (1.0.0 -> 1.1.0)",
            path.display()
        );
        assert_eq!(
            loaded.from,
            v(1, 0, 0),
            "{} original version",
            path.display()
        );
        assert_eq!(
            loaded.dsl.schema_version,
            v(1, 1, 0),
            "{} migrated version",
            path.display()
        );
        assert_eq!(
            loaded.dsl_original,
            json,
            "dsl_original must be the verbatim input for {}",
            path.display()
        );

        // Every leaf equals the direct (migration-unaware) deserialize of the
        // same bytes — the identity step rewrites only `schema_version`, which
        // is asserted above, so the direct shape is normalized to it here — and
        // every operand reads `series == Primary` by default.
        let mut direct: StrategyDsl = serde_json::from_str(&json).expect("direct deserialize");
        direct.schema_version = SchemaVersion::CURRENT;
        assert_eq!(loaded.dsl, direct, "{} migrated == direct", path.display());
        for s in operand_series(&loaded.dsl) {
            assert_eq!(s, Series::Primary, "{} operand defaulted", path.display());
        }
    }
}

#[test]
fn inline_1_0_0_document_migrates_and_deserializes_equal() {
    let loaded = Migrator::v1().load(INLINE_1_0_0).expect("load inline");
    assert!(loaded.migrated);
    assert_eq!(loaded.from, v(1, 0, 0));
    assert_eq!(loaded.dsl.schema_version, v(1, 1, 0));
    assert_eq!(loaded.dsl_original, INLINE_1_0_0);
    let mut direct: StrategyDsl = serde_json::from_str(INLINE_1_0_0).expect("direct");
    direct.schema_version = SchemaVersion::CURRENT;
    assert_eq!(loaded.dsl, direct);
}

// ---- (b) future versions still reject --------------------------------------

#[test]
fn future_versions_still_reject() {
    for future in ["1.2.0", "2.0.0"] {
        let mut doc: Value = serde_json::from_str(INLINE_1_0_0).unwrap();
        doc["schema_version"] = json!(future);
        let err = Migrator::v1()
            .load(&doc.to_string())
            .expect_err(&format!("{future} must reject"));
        match err {
            pulse::LoadError::FutureVersion { found, current } => {
                assert_eq!(found.to_string(), future);
                assert_eq!(current, SchemaVersion::CURRENT);
            }
            other => panic!("expected FutureVersion for {future}, got {other:?}"),
        }
    }
}

// ---- (c) new grammar round-trips -------------------------------------------

#[test]
fn series_field_round_trips_and_writes_explicitly() {
    // Absent `series` reads as `primary`…
    let src: ValueSource = serde_json::from_str(r#"{"type":"Price","field":"Close"}"#)
        .expect("series-less Price deserializes");
    assert_eq!(
        src,
        ValueSource::Price {
            series: Series::Primary,
            field: PriceField::Close,
        }
    );
    // …and writes ALWAYS serialize `series` explicitly.
    let written = serde_json::to_string(&src).expect("serialize");
    assert!(
        written.contains("\"series\":\"primary\""),
        "writes must be explicit, got {written}"
    );

    let htf = ValueSource::Indicator {
        series: Series::Htf,
        spec: IndicatorSpec::Ema {
            period: SweepableValue::Fixed(200),
        },
    };
    let json = serde_json::to_string(&htf).expect("serialize htf");
    assert!(json.contains("\"series\":\"htf\""), "json was {json}");
    let back: ValueSource = serde_json::from_str(&json).expect("deserialize htf");
    assert_eq!(back, htf);
}

#[test]
fn atr_indicator_and_atr_stop_round_trip() {
    let spec = IndicatorSpec::Atr {
        period: SweepableValue::Fixed(14),
    };
    let spec_json = serde_json::to_string(&spec).expect("serialize Atr");
    assert!(
        spec_json.contains("\"indicator\":\"Atr\""),
        "json was {spec_json}"
    );
    let wire: IndicatorSpec =
        serde_json::from_str(r#"{"indicator":"Atr","period":14}"#).expect("PascalCase tag parses");
    assert_eq!(wire, spec);

    let stop = atr_stop(14, 2, 0);
    let stop_json = serde_json::to_string(&stop).expect("serialize AtrStop");
    assert!(
        stop_json.contains("\"type\":\"AtrStop\""),
        "json was {stop_json}"
    );
    let back: ExitRule = serde_json::from_str(&stop_json).expect("deserialize AtrStop");
    assert_eq!(back, stop);
}

// ---- (d) validation arms + exact paths -------------------------------------

fn field_errors(dsl: &StrategyDsl) -> Vec<pulse::FieldError> {
    validate(dsl)
        .expect_err("fixture must fail validation")
        .into_errors()
}

#[test]
fn atr_period_zero_is_a_field_range_at_the_atr_path() {
    let dsl = dsl_with(
        compare(
            ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Atr {
                    period: SweepableValue::Fixed(0),
                },
            },
            Comparator::Gt,
            constant(1, 0),
        ),
        vec![stop_loss()],
    );
    let errors = field_errors(&dsl);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "entry.lhs.indicator.atr.period"
                && e.code == ValidationCode::FieldRange),
        "expected entry.lhs.indicator.atr.period FieldRange, got {errors:?}"
    );
}

#[test]
fn atr_stop_period_and_multiple_bounds_are_enforced() {
    // period = 0.
    let zero_period = dsl_with(
        compare(constant(1, 0), Comparator::Gt, constant(0, 0)),
        vec![atr_stop(0, 2, 0)],
    );
    let errors = field_errors(&zero_period);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "exits[0].period" && e.code == ValidationCode::FieldRange),
        "expected exits[0].period FieldRange, got {errors:?}"
    );

    // multiple = 0.
    let zero_multiple = dsl_with(
        compare(constant(1, 0), Comparator::Gt, constant(0, 0)),
        vec![atr_stop(14, 0, 0)],
    );
    let errors = field_errors(&zero_multiple);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "exits[0].multiple" && e.code == ValidationCode::FieldRange),
        "expected exits[0].multiple FieldRange, got {errors:?}"
    );

    // multiple = 10.5 breaches the (0, 10] bound.
    let over = dsl_with(
        compare(constant(1, 0), Comparator::Gt, constant(0, 0)),
        vec![atr_stop(14, 105, 1)],
    );
    let errors = field_errors(&over);
    let err = errors
        .iter()
        .find(|e| e.path == "exits[0].multiple" && e.code == ValidationCode::FieldRange)
        .expect("exits[0].multiple FieldRange present");
    assert!(
        err.message.contains("(0, 10]"),
        "message must name the (0, 10] bound, got {:?}",
        err.message
    );
}

#[test]
fn stop_loss_and_atr_stop_are_one_exclusive_family() {
    let dsl = dsl_with(
        compare(constant(1, 0), Comparator::Gt, constant(0, 0)),
        vec![stop_loss(), atr_stop(14, 2, 0)],
    );
    let errors = field_errors(&dsl);
    assert!(
        errors
            .iter()
            .any(|e| e.path == "exits" && e.code == ValidationCode::DuplicateExit),
        "StopLoss + AtrStop must be DuplicateExit, got {errors:?}"
    );
}

#[test]
fn take_profit_with_atr_stop_alone_is_valid() {
    let dsl = dsl_with(
        compare(constant(1, 0), Comparator::Gt, constant(0, 0)),
        vec![atr_stop(14, 2, 0), take_profit()],
    );
    validate(&dsl).expect("AtrStop satisfies the TakeProfit stop requirement");
}

// ---- (e) mutation leaves in lockstep ----------------------------------------

#[test]
fn atr_and_atr_stop_leaves_are_sweepable_and_retunable() {
    let dsl = dsl_with(
        compare(
            ValueSource::Indicator {
                series: Series::Primary,
                spec: IndicatorSpec::Atr {
                    period: SweepableValue::Fixed(14),
                },
            },
            Comparator::Gt,
            constant(1, 0),
        ),
        vec![atr_stop(14, 2, 0)],
    );

    let paths = sweepable_paths(&dsl);
    for leaf in [
        "entry.lhs.indicator.atr.period",
        "exits[0].period",
        "exits[0].multiple",
    ] {
        assert!(
            paths.iter().any(|p| p == leaf),
            "sweepable_paths must contain {leaf}, got {paths:?}"
        );
    }

    // apply() retunes each kind.
    let set_period = |path: &str, value: u32| Mutation::SetParam {
        path: path.to_owned(),
        new_value: ParamValue::Period { value },
    };
    let candidate: CandidateDsl =
        apply(&dsl, &set_period("entry.lhs.indicator.atr.period", 21)).expect("atr.period retunes");
    assert!(matches!(
        candidate.dsl().entry,
        Condition::Compare {
            lhs: ValueSource::Indicator {
                spec: IndicatorSpec::Atr {
                    period: SweepableValue::Fixed(21)
                },
                ..
            },
            ..
        }
    ));

    let candidate = apply(&dsl, &set_period("exits[0].period", 21)).expect("stop period retunes");
    assert!(matches!(
        candidate.dsl().exits[0],
        ExitRule::AtrStop {
            period: SweepableValue::Fixed(21),
            ..
        }
    ));

    let candidate = apply(
        &dsl,
        &Mutation::SetParam {
            path: "exits[0].multiple".to_owned(),
            new_value: ParamValue::Threshold { value: dec(3, 0) },
        },
    )
    .expect("multiple retunes");
    assert!(matches!(
        candidate.dsl().exits[0],
        ExitRule::AtrStop {
            multiple: SweepableValue::Fixed(m),
            ..
        } if m == dec(3, 0)
    ));

    // A `multiple` pushed past 10 is a MutationError::ValidationFailed carrying
    // the same FieldError the validator emits.
    let err = apply(
        &dsl,
        &Mutation::SetParam {
            path: "exits[0].multiple".to_owned(),
            new_value: ParamValue::Threshold { value: dec(105, 1) },
        },
    )
    .expect_err("multiple > 10 must fail");
    match err {
        pulse::MutationError::ValidationFailed { errors, .. } => {
            assert!(
                errors
                    .errors()
                    .iter()
                    .any(|e| e.path == "exits[0].multiple")
            );
        }
        other => panic!("expected ValidationFailed, got {other:?}"),
    }
}

// ---- (f) compilation shapes -------------------------------------------------

#[test]
fn htf_operand_compiles_with_its_series() {
    // w2 lifts the compile gate: an `Htf` operand is a `CompiledValue` carrying
    // `Series::Htf`, collected into `required_htf_indicators`, and flips
    // `needs_htf()`. Evaluation routes it to the HTF engine (covered by
    // `tests/htf_atr_engine.rs`).
    let dsl = dsl_with(
        compare(
            ValueSource::Indicator {
                series: Series::Htf,
                spec: IndicatorSpec::Ema {
                    period: SweepableValue::Fixed(200),
                },
            },
            Comparator::Gt,
            constant(100, 0),
        ),
        vec![stop_loss()],
    );
    // `series` carries no validation rule — the document validates AND compiles.
    let validated = validate(&dsl).expect("htf operand has no validation rule");
    let compiled = compile(&validated).expect("htf operand must compile");

    let htf_leaf = pulse::CompiledValue::Indicator {
        series: Series::Htf,
        spec: IndicatorSpec::Ema {
            period: SweepableValue::Fixed(200),
        },
    };
    assert!(
        matches!(
            compiled.entry(),
            pulse::CompiledCondition::Compare { lhs, .. } if *lhs == htf_leaf
        ),
        "entry.lhs must be the Htf-series indicator leaf, was {:?}",
        compiled.entry()
    );
    assert!(
        compiled
            .required_htf_indicators()
            .contains(&IndicatorSpec::Ema {
                period: SweepableValue::Fixed(200),
            }),
        "required_htf_indicators must contain Ema(200), got {:?}",
        compiled.required_htf_indicators()
    );
    assert!(
        compiled.required_indicators().is_empty(),
        "an htf operand registers no primary indicator, got {:?}",
        compiled.required_indicators()
    );
    assert!(compiled.needs_htf(), "an htf operand flips needs_htf");
}

#[test]
fn htf_price_operand_compiles_and_needs_htf() {
    // An `Htf` Price leaf compiles too — the gate is gone for both operand
    // kinds. (The engine reads the aligned closed H4 candle for it.)
    let dsl = dsl_with(
        compare(
            ValueSource::Price {
                series: Series::Htf,
                field: PriceField::Close,
            },
            Comparator::Gt,
            constant(100, 0),
        ),
        vec![stop_loss()],
    );
    let compiled = compile(&validate(&dsl).expect("valid")).expect("must compile");
    assert!(matches!(
        compiled.entry(),
        pulse::CompiledCondition::Compare {
            lhs: pulse::CompiledValue::Price {
                series: Series::Htf,
                field: PriceField::Close,
            },
            ..
        }
    ));
    assert!(compiled.needs_htf());
    assert!(compiled.required_htf_indicators().is_empty());
}

#[test]
fn primary_only_strategy_needs_no_htf() {
    let dsl = dsl_with(
        compare(
            ValueSource::Price {
                series: Series::Primary,
                field: PriceField::Close,
            },
            Comparator::Gt,
            constant(100, 0),
        ),
        vec![stop_loss()],
    );
    let compiled = compile(&validate(&dsl).expect("valid")).expect("must compile");
    assert!(!compiled.needs_htf());
    assert!(compiled.required_htf_indicators().is_empty());
}

#[test]
fn atr_stop_compiles_to_data_and_registers_primary_atr() {
    let dsl = dsl_with(
        compare(constant(1, 0), Comparator::Gt, constant(0, 0)),
        vec![atr_stop(14, 2, 0)],
    );
    let compiled = compile(&validate(&dsl).expect("valid")).expect("must compile");
    assert_eq!(
        compiled.exits()[0],
        CompiledExit::AtrStop {
            period: 14,
            multiple: dec(2, 0),
        }
    );
    // The stop reads the PRIMARY-series ATR: required_indicators carries it.
    assert!(
        compiled
            .required_indicators()
            .contains(&IndicatorSpec::Atr {
                period: SweepableValue::Fixed(14),
            }),
        "required_indicators must contain Atr(14), got {:?}",
        compiled.required_indicators()
    );
    assert!(
        compiled.required_htf_indicators().is_empty(),
        "no htf operand means no htf requirement"
    );
    assert!(
        !compiled.needs_htf(),
        "no htf operand means needs_htf is false"
    );
}

// ---- (g) pure exit geometry -------------------------------------------------

#[test]
fn atr_stop_price_is_direction_relative() {
    // Long: entry − multiple×atr → 100 − 2×3 = 94.
    assert_eq!(
        atr_stop_price(dec(100, 0), dec(3, 0), dec(2, 0), Direction::Long),
        dec(94, 0)
    );
    // Short: entry + multiple×atr → 100 + 2×3 = 106.
    assert_eq!(
        atr_stop_price(dec(100, 0), dec(3, 0), dec(2, 0), Direction::Short),
        dec(106, 0)
    );
}

// ---- (h) repository round-trip of a persisted 1.0.0 document ----------------

async fn repo() -> (SqliteStrategyRepo<pulse::SystemClock>, SqlitePool, TempDir) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    MIGRATOR.run(db.pool()).await.expect("run migrations");
    let pool = db.pool().clone();
    (SqliteStrategyRepo::new(pool.clone()), pool, tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_1_0_0_document_reads_back_at_1_1_0() {
    let (repo, _pool, _tmp) = repo().await;
    let s = repo
        .create_strategy("MigrateMe", Some("alice"), &["btc".to_owned()])
        .await
        .expect("create strategy");
    let created = repo
        .create_version(NewVersion {
            strategy_id: s.id.clone(),
            parent_version_id: None,
            dsl_json: INLINE_1_0_0.to_owned(),
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .expect("create version");

    let fetched = repo
        .get_version(&created.id)
        .await
        .expect("get version")
        .expect("version exists");
    assert_eq!(
        fetched.dsl_schema_version,
        v(1, 1, 0),
        "persisted version must read back at CURRENT"
    );
    assert_eq!(
        fetched.dsl_original, INLINE_1_0_0,
        "dsl_original stays byte-verbatim"
    );
}

// ---- (i) the machine schema the MCP resource serves -------------------------

/// The generated JSON Schema publishes the 1.1.0 grammar — the `series` tag,
/// `Atr`, `AtrStop`, and the accepted-version enum — while `SweepableValue`'s
/// manual impl renders the accepted v1 wire form (the bare scalar), NOT the
/// rejected sweep-object shape an auto-derive would publish.
#[test]
fn the_served_schema_carries_the_1_1_0_grammar() {
    let schema =
        serde_json::to_value(schemars::schema_for!(StrategyDsl)).expect("schema serializes");
    let text = schema.to_string();

    // The new vocabulary is present.
    for needle in [
        "\"Atr\"",
        "\"AtrStop\"",
        "\"series\"",
        "\"primary\"",
        "\"htf\"",
        "\"multiple\"",
    ] {
        assert!(text.contains(needle), "schema must contain {needle}");
    }
    // The accepted schema_version enum: 1.0.0 (identity-migrated) + CURRENT.
    assert!(text.contains("\"1.0.0\""), "schema pins the 1.0.0 input");
    assert!(text.contains("\"1.1.0\""), "schema pins CURRENT");

    // A sweep object is a rejected wire form; the manual `SweepableValue`
    // schema must not advertise it — no `start`/`end`/`step` triple anywhere.
    for banned in ["\"start\"", "\"end\"", "\"step\""] {
        assert!(
            !text.contains(banned),
            "served schema must not publish the sweep-object form ({banned})"
        );
    }
}
