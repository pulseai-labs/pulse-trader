//! Streaming indicator engine over the VS-1.1.3 indicator adapters.

use super::{adx::Adx, ema::Ema, macd::Macd, rsi::Rsi};
use crate::{
    Candle, CompiledStrategy, CompiledValue, EvalContext, Indicator, IndicatorSpec, PriceField,
    SweepableValue,
};
use rust_decimal::Decimal;

/// Errors produced while building an [`IndicatorEngine`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineError {
    /// A future parameter sweep reached the fixed-only indicator factory.
    #[error("unexpected Sweep value at {field}: an indicator engine requires only Fixed values")]
    UnexpectedSweep {
        /// A human-readable field hint for where the stray sweep was found.
        field: String,
    },
    /// An adapter rejected a fixed period tuple.
    #[error("invalid period for {spec:?}: {detail}")]
    InvalidPeriod {
        /// The spec whose fixed parameters were rejected.
        spec: IndicatorSpec,
        /// Adapter/factory detail explaining the invalid parameter tuple.
        detail: String,
    },
    /// A grammatically-valid spec whose computation lands in a later work item
    /// (r2.s2.w1 compile-only arm — removed by the owning item).
    #[error("{0}")]
    Unsupported(String),
}

/// Composes the concrete indicator adapters needed by one compiled strategy.
///
/// The caller must drive [`IndicatorEngine::step`] with a gap-free candle series
/// in ascending `open_time` order. The engine does not detect or fill missing
/// bars. A strategy driver must also gate entry evaluation on
/// [`IndicatorEngine::is_warm`]; the frozen boolean evaluator is not warmup-safe
/// for `Not`/`Or` over unavailable indicator values.
pub struct IndicatorEngine {
    indicators: Vec<IndicatorSlot>,
    previous_candle: Option<Candle>,
    current_candle: Option<Candle>,
}

struct IndicatorSlot {
    spec: IndicatorSpec,
    indicator: Box<dyn Indicator>,
    previous: Option<Decimal>,
    current: Option<Decimal>,
}

impl IndicatorEngine {
    /// Build the engine from a compiled strategy's required indicator list.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] if any required indicator spec is non-fixed or
    /// has invalid fixed periods.
    pub fn new(strategy: &CompiledStrategy) -> Result<Self, EngineError> {
        Self::from_specs(strategy.required_indicators())
    }

    /// Build the engine from raw indicator specs.
    ///
    /// # Errors
    ///
    /// Returns [`EngineError`] if any spec is non-fixed or has invalid fixed
    /// periods.
    pub fn from_specs(specs: &[IndicatorSpec]) -> Result<Self, EngineError> {
        let mut indicators = Vec::new();
        for spec in specs {
            if indicators
                .iter()
                .any(|slot: &IndicatorSlot| slot.spec == *spec)
            {
                continue;
            }
            indicators.push(IndicatorSlot {
                spec: spec.clone(),
                indicator: build_indicator(spec)?,
                previous: None,
                current: None,
            });
        }
        Ok(Self {
            indicators,
            previous_candle: None,
            current_candle: None,
        })
    }

    /// Number of distinct indicator instances owned by the engine.
    #[must_use]
    pub fn indicator_count(&self) -> usize {
        self.indicators.len()
    }

    /// Whether every owned indicator has a current value available.
    #[must_use]
    pub fn is_warm(&self) -> bool {
        self.indicators
            .iter()
            .all(|slot| slot.current.is_some() && slot.indicator.is_ready())
    }

    /// Advance every owned indicator by one contiguous candle.
    ///
    /// The caller must pass gap-free, ascending candles. This method is the only
    /// mutator and never looks ahead. A driver must not evaluate/fire strategy
    /// entry while [`IndicatorEngine::is_warm`] is false.
    pub fn step(&mut self, candle: &Candle) {
        for slot in &mut self.indicators {
            slot.previous = slot.current;
            slot.current = slot.indicator.next(candle);
        }
        self.previous_candle = self.current_candle.clone();
        self.current_candle = Some(candle.clone());
    }

    fn current_indicator(&self, spec: &IndicatorSpec) -> Option<Decimal> {
        self.indicators
            .iter()
            .find(|slot| slot.spec == *spec)
            .and_then(|slot| slot.current)
    }

    fn previous_indicator(&self, spec: &IndicatorSpec) -> Option<Decimal> {
        self.indicators
            .iter()
            .find(|slot| slot.spec == *spec)
            .and_then(|slot| slot.previous)
    }
}

impl EvalContext for IndicatorEngine {
    fn current(&self, value: &CompiledValue) -> Option<Decimal> {
        match value {
            CompiledValue::Const(value) => Some(*value),
            CompiledValue::Price { field, .. } => {
                self.current_candle.as_ref().map(|c| price(c, *field))
            }
            CompiledValue::Indicator { spec, .. } => self.current_indicator(spec),
        }
    }

    fn previous(&self, value: &CompiledValue) -> Option<Decimal> {
        match value {
            CompiledValue::Const(value) => Some(*value),
            CompiledValue::Price { field, .. } => {
                self.previous_candle.as_ref().map(|c| price(c, *field))
            }
            CompiledValue::Indicator { spec, .. } => self.previous_indicator(spec),
        }
    }
}

fn build_indicator(spec: &IndicatorSpec) -> Result<Box<dyn Indicator>, EngineError> {
    match spec {
        IndicatorSpec::Rsi { period } => {
            let period = fixed_u32(period, "rsi.period")?;
            Rsi::new(period)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("RSI period {period} is invalid")))
        }
        IndicatorSpec::Ema { period } => {
            let period = fixed_u32(period, "ema.period")?;
            Ema::new(period)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("EMA period {period} is invalid")))
        }
        IndicatorSpec::Adx { period } => {
            let period = fixed_u32(period, "adx.period")?;
            Adx::new(period)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| invalid_period(spec, format!("ADX period {period} is invalid")))
        }
        IndicatorSpec::Macd { fast, slow, signal } => {
            let fast = fixed_u32(fast, "macd.fast")?;
            let slow = fixed_u32(slow, "macd.slow")?;
            let signal = fixed_u32(signal, "macd.signal")?;
            if fast >= slow {
                return Err(invalid_period(
                    spec,
                    format!("MACD fast period {fast} must be less than slow period {slow}"),
                ));
            }
            Macd::new(fast, slow, signal)
                .map(|indicator| Box::new(indicator) as Box<dyn Indicator>)
                .ok_or_else(|| {
                    invalid_period(
                        spec,
                        format!(
                            "MACD periods fast={fast}, slow={slow}, signal={signal} are invalid"
                        ),
                    )
                })
        }
        // r2.s2.w1 compile-only arm: the grammar carries `Atr`; its computation
        // is w2's. A typed refusal, never a silent default.
        IndicatorSpec::Atr { .. } => {
            Err(EngineError::Unsupported("atr lands in r2.s2.w2".to_owned()))
        }
    }
}

fn fixed_u32(value: &SweepableValue<u32>, field: &str) -> Result<u32, EngineError> {
    match value {
        SweepableValue::Fixed(value) => Ok(*value),
        SweepableValue::Sweep { .. } => Err(EngineError::UnexpectedSweep {
            field: field.to_owned(),
        }),
    }
}

fn invalid_period(spec: &IndicatorSpec, detail: String) -> EngineError {
    EngineError::InvalidPeriod {
        spec: spec.clone(),
        detail,
    }
}

fn price(candle: &Candle, field: PriceField) -> Decimal {
    match field {
        PriceField::Open => candle.open,
        PriceField::High => candle.high,
        PriceField::Low => candle.low,
        PriceField::Close => candle.close,
        PriceField::Volume => candle.volume,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{EngineError, IndicatorEngine};
    use crate::{
        Candle, Comparator, Condition, Direction, EvalContext, ExitRule, IndicatorSpec, PriceField,
        RiskParams, SchemaVersion, Series, StrategyDsl, SweepableValue, ValueSource, compile,
        validate,
    };
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn d(value: &str) -> Decimal {
        Decimal::from_str(value).unwrap()
    }

    fn candle(idx: i64, close: &str) -> Candle {
        let close = d(close);
        Candle {
            open_time: idx * 60_000,
            close_time: idx * 60_000 + 59_999,
            open: close,
            high: close + Decimal::ONE,
            low: close - Decimal::ONE,
            close,
            volume: Decimal::ONE,
            funding_rate: None,
        }
    }

    fn stop_exit() -> ExitRule {
        ExitRule::StopLoss {
            distance_pct: SweepableValue::Fixed(d("0.05")),
        }
    }

    fn risk() -> RiskParams {
        RiskParams {
            risk_per_trade_pct: SweepableValue::Fixed(d("0.01")),
            max_leverage: SweepableValue::Fixed(Decimal::from(3)),
        }
    }

    fn compiled(entry: Condition, filters: Vec<Condition>) -> crate::CompiledStrategy {
        let dsl = StrategyDsl {
            schema_version: SchemaVersion::CURRENT,
            name: "engine fixture".to_owned(),
            direction: Direction::Long,
            entry,
            filters,
            exits: vec![stop_exit()],
            risk: risk(),
        };
        compile(&validate(&dsl).expect("fixture validates")).expect("fixture compiles")
    }

    fn indicator_value(spec: IndicatorSpec) -> ValueSource {
        ValueSource::Indicator {
            series: Series::Primary,
            spec,
        }
    }

    fn constant(value: &str) -> ValueSource {
        ValueSource::Constant { value: d(value) }
    }

    fn price(field: PriceField) -> ValueSource {
        ValueSource::Price {
            series: Series::Primary,
            field,
        }
    }

    fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
        Condition::Compare { lhs, op, rhs }
    }

    fn rsi(period: u32) -> IndicatorSpec {
        IndicatorSpec::Rsi {
            period: SweepableValue::Fixed(period),
        }
    }

    fn ema(period: u32) -> IndicatorSpec {
        IndicatorSpec::Ema {
            period: SweepableValue::Fixed(period),
        }
    }

    #[test]
    fn engine_builds_one_indicator_per_distinct_spec() {
        let rsi = rsi(2);
        let ema = ema(2);
        let strategy = compiled(
            compare(indicator_value(rsi.clone()), Comparator::Lt, constant("30")),
            vec![
                compare(indicator_value(rsi.clone()), Comparator::Gt, constant("10")),
                compare(
                    indicator_value(ema.clone()),
                    Comparator::Gt,
                    constant("100"),
                ),
            ],
        );

        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");

        assert_eq!(engine.indicator_count(), 2);
        for (idx, close) in (0_i64..).zip(["100", "99", "98"]) {
            engine.step(&candle(idx, close));
        }
        let first = engine.current(&crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec: rsi.clone(),
        });
        let second = engine.current(&crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec: rsi,
        });
        assert_eq!(first, second);
        assert!(
            engine
                .current(&crate::CompiledValue::Indicator {
                    series: Series::Primary,
                    spec: ema,
                })
                .is_some()
        );
    }

    #[test]
    fn engine_current_and_previous_resolve_price_const_indicator() {
        let spec = ema(2);
        let strategy = compiled(
            compare(
                indicator_value(spec.clone()),
                Comparator::Gt,
                constant("100"),
            ),
            vec![],
        );
        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
        for (idx, close) in (0_i64..).zip(["100", "102", "104"]) {
            engine.step(&candle(idx, close));
        }

        let const_value = crate::CompiledValue::Const(d("7"));
        let close_value = crate::CompiledValue::Price {
            series: Series::Primary,
            field: PriceField::Close,
        };
        let indicator = crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec,
        };

        assert_eq!(engine.current(&const_value), Some(d("7")));
        assert_eq!(engine.previous(&const_value), Some(d("7")));
        assert_eq!(engine.current(&close_value), Some(d("104")));
        assert_eq!(engine.previous(&close_value), Some(d("102")));
        assert!(engine.current(&indicator).is_some());
        assert!(engine.previous(&indicator).is_some());
    }

    #[test]
    fn engine_previous_is_none_on_first_bar() {
        let spec = ema(2);
        let strategy = compiled(
            compare(
                indicator_value(spec.clone()),
                Comparator::Gt,
                constant("100"),
            ),
            vec![],
        );
        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
        engine.step(&candle(0, "100"));

        let close_value = crate::CompiledValue::Price {
            series: Series::Primary,
            field: PriceField::Close,
        };
        let indicator = crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec,
        };
        assert_eq!(engine.current(&close_value), Some(d("100")));
        assert_eq!(engine.previous(&close_value), None);
        assert_eq!(engine.previous(&indicator), None);
    }

    #[test]
    fn engine_indicator_is_none_during_warmup() {
        let spec = rsi(3);
        let strategy = compiled(
            compare(
                indicator_value(spec.clone()),
                Comparator::Lt,
                constant("30"),
            ),
            vec![],
        );
        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
        let value = crate::CompiledValue::Indicator {
            series: Series::Primary,
            spec,
        };

        for (idx, close) in (0_i64..).zip(["100", "99", "98"]) {
            engine.step(&candle(idx, close));
            assert_eq!(engine.current(&value), None);
            assert!(!engine.is_warm());
        }
        engine.step(&candle(3, "97"));
        assert!(engine.current(&value).is_some());
        assert!(engine.is_warm());
    }

    #[test]
    fn entry_cannot_fire_during_warmup_then_can() {
        let spec = rsi(3);
        let entry = Condition::Not {
            condition: Box::new(compare(
                indicator_value(spec),
                Comparator::Gt,
                constant("70"),
            )),
        };
        let strategy = compiled(entry, vec![]);
        let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");

        for (idx, close) in (0_i64..).zip(["100", "99", "98"]) {
            engine.step(&candle(idx, close));
            assert!(
                strategy.entry().eval(&engine),
                "pins the Not(warmup) hazard"
            );
            let gated_entry = engine.is_warm() && strategy.entry().eval(&engine);
            assert!(!gated_entry, "the readiness gate suppresses warmup entry");
        }

        engine.step(&candle(3, "97"));
        assert!(engine.is_warm());
        assert!(strategy.entry().eval(&engine));
    }

    #[test]
    fn factory_rejects_sweep_payload() {
        let result = IndicatorEngine::from_specs(&[IndicatorSpec::Rsi {
            period: SweepableValue::Sweep {
                start: 2,
                end: 10,
                step: 1,
            },
        }]);
        let Err(err) = result else {
            panic!("sweep payload must be rejected");
        };

        assert!(matches!(err, EngineError::UnexpectedSweep { .. }));
    }

    #[test]
    fn engine_step_is_deterministic_across_repeated_runs() {
        let rsi = rsi(3);
        let ema = ema(2);
        let strategy = compiled(
            compare(indicator_value(rsi.clone()), Comparator::Lt, constant("30")),
            vec![compare(
                indicator_value(ema.clone()),
                Comparator::Gt,
                price(PriceField::Close),
            )],
        );
        let candles = ["100.5", "99.25", "98.75", "97.5", "99.0"];
        let run = || {
            let mut engine = IndicatorEngine::new(&strategy).expect("engine builds");
            let rsi_value = crate::CompiledValue::Indicator {
                series: Series::Primary,
                spec: rsi.clone(),
            };
            let ema_value = crate::CompiledValue::Indicator {
                series: Series::Primary,
                spec: ema.clone(),
            };
            (0_i64..)
                .zip(candles)
                .map(|(idx, close)| {
                    engine.step(&candle(idx, close));
                    (
                        engine.current(&rsi_value),
                        engine.previous(&rsi_value),
                        engine.current(&ema_value),
                        engine.previous(&ema_value),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(run(), run(), "NFR-2: repeated engine runs match exactly");
    }

    #[test]
    fn factory_rejects_invalid_period() {
        let result = IndicatorEngine::from_specs(&[IndicatorSpec::Rsi {
            period: SweepableValue::Fixed(0),
        }]);
        let Err(err) = result else {
            panic!("invalid period must be rejected");
        };

        assert!(matches!(err, EngineError::InvalidPeriod { .. }));
    }

    /// r2.s2.w1 compile-only arm: `Atr` is grammar — the factory refuses it
    /// with a typed `Unsupported` until w2 lands the computation.
    #[test]
    fn factory_rejects_atr_with_typed_unsupported() {
        let result = IndicatorEngine::from_specs(&[IndicatorSpec::Atr {
            period: SweepableValue::Fixed(14),
        }]);
        let Err(err) = result else {
            panic!("atr must be rejected until r2.s2.w2");
        };

        assert_eq!(
            err,
            EngineError::Unsupported("atr lands in r2.s2.w2".to_owned())
        );
    }
}
