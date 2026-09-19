//! VS-1.1.2 demo-2: author an RSI-oversold strategy and inspect the compiled
//! evaluator tree.
//!
//! Walks the full DSL read-path the engine uses: author a `StrategyDsl` →
//! serialize to JSON (the form the LLM composer emits, FR-3) → `Migrator::load`
//! (version-detect + migrate, FR-4) → `validate` (→ `ValidatedDsl`) → `compile`
//! (→ executable evaluator tree) → inspect.
//!
//! Run: `cargo run --example compile_rsi_oversold`

use pulse::{
    Comparator, Condition, Direction, ExitRule, IndicatorSpec, Migrator, RiskParams, SchemaVersion,
    Series, StrategyDsl, SweepableValue, ValueSource, compile, validate,
};
use rust_decimal::Decimal;

/// "Enter long when RSI(14) < 30; 5% stop (1R), 2R take-profit; risk 1%/trade."
fn rsi_oversold_strategy() -> StrategyDsl {
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
                distance_pct: SweepableValue::Fixed(Decimal::new(5, 2)), // 0.05 = 5%
            },
            ExitRule::TakeProfit {
                target_r: SweepableValue::Fixed(Decimal::new(2, 0)), // 2R
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(Decimal::new(1, 2)), // 0.01 = 1%
            max_leverage: SweepableValue::Fixed(Decimal::new(3, 0)),
        },
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let strategy = rsi_oversold_strategy();

    // 1. Author → JSON (what the FR-3 composer tools emit).
    let json = serde_json::to_string_pretty(&strategy)?;
    println!("=== Authored DSL (JSON) ===\n{json}\n");

    // 2. Version-safe read-path (FR-4): detect schema_version, migrate to CURRENT.
    let loaded = Migrator::v1()
        .load(&json)
        .map_err(|e| format!("load failed: {e:?}"))?;
    println!(
        "=== Loaded === schema_version={:?}  migrated={}\n",
        loaded.from, loaded.migrated
    );

    // 3. Validate (FR-3 correctable rejection — here the strategy is valid).
    let validated = validate(&loaded.dsl).map_err(|e| format!("invalid DSL: {e:?}"))?;

    // 4. Compile → executable evaluator tree.
    let compiled = compile(&validated).map_err(|e| format!("compile failed: {e:?}"))?;

    // 5. Inspect the compiled evaluator tree.
    println!("=== Compiled evaluator tree ===");
    println!("direction          : {:?}", compiled.direction());
    println!("required indicators: {:?}", compiled.required_indicators());
    println!("exits              : {:?}", compiled.exits());
    println!("\n{}", compiled.describe());

    Ok(())
}
