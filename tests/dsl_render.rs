//! r2.s2.w4 AC-1 — the one ring-owned DSL formatter's pinned vocabulary.
//!
//! `b10` claims every surface that *describes* a strategy prints the same words
//! from one code path: the Library node's `dsl_summary` and the Designer card's
//! `summarize_dsl` are thin adapters over `domain::dsl::render`, the pure
//! formatter that owns the vocabulary — including schema 1.1.0's two tokens,
//! the `h4:` operand prefix and the `atr(14)×2` stop (ledger line `d18` reads
//! them off the Library at the walk).
//!
//! Three parts: (a) the vocabulary pinned token-by-token, (b) the seeded
//! journey document rendered to its expected `Rendered`, (c) the two DTO
//! adapters agreeing on every committed fixture document.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use pulse::render::{self, Rendered};
use pulse::{
    Comparator, Condition, Direction, ExitRule, IndicatorSpec, PriceField, RiskParams,
    SchemaVersion, Series, StrategyDsl, SweepableValue, ValueSource, dsl_summary, summarize_dsl,
};
use rust_decimal::Decimal;

fn dec(mantissa: i64, scale: u32) -> Decimal {
    Decimal::new(mantissa, scale)
}

fn fixed_u32(value: u32) -> SweepableValue<u32> {
    SweepableValue::Fixed(value)
}

fn fixed_dec(mantissa: i64, scale: u32) -> SweepableValue<Decimal> {
    SweepableValue::Fixed(dec(mantissa, scale))
}

fn indicator(series: Series, spec: IndicatorSpec) -> ValueSource {
    ValueSource::Indicator { series, spec }
}

fn price(series: Series, field: PriceField) -> ValueSource {
    ValueSource::Price { series, field }
}

fn constant(mantissa: i64, scale: u32) -> ValueSource {
    ValueSource::Constant {
        value: dec(mantissa, scale),
    }
}

fn compare(lhs: ValueSource, op: Comparator, rhs: ValueSource) -> Condition {
    Condition::Compare { lhs, op, rhs }
}

/// `rsi(14) < 30` on the primary series — the canonical entry predicate.
fn rsi_oversold() -> Condition {
    compare(
        indicator(
            Series::Primary,
            IndicatorSpec::Rsi {
                period: fixed_u32(14),
            },
        ),
        Comparator::Lt,
        constant(30, 0),
    )
}

/// The seeded journey document (schema 1.1.0): an M15 `rsi(14) < 30` entry,
/// an `h4:close > h4:ema(200)` filter, and `atr(14)×2` + `take profit 2R`
/// exits — the shape `d18` composes at the walk. Built typed (not a fixture
/// file): `tests/fixtures/strategies/` holds 1.0.0 documents only, and
/// `dsl_schema_1_1.rs` iterates it asserting each migrates to CURRENT.
fn journey_dsl() -> StrategyDsl {
    StrategyDsl {
        schema_version: SchemaVersion::CURRENT,
        name: "H4-trend ATR-stop long".to_owned(),
        direction: Direction::Long,
        entry: rsi_oversold(),
        filters: vec![compare(
            price(Series::Htf, PriceField::Close),
            Comparator::Gt,
            indicator(
                Series::Htf,
                IndicatorSpec::Ema {
                    period: fixed_u32(200),
                },
            ),
        )],
        exits: vec![
            ExitRule::AtrStop {
                period: fixed_u32(14),
                multiple: fixed_dec(2, 0),
            },
            ExitRule::TakeProfit {
                target_r: fixed_dec(2, 0),
            },
        ],
        risk: RiskParams {
            risk_per_trade_pct: fixed_dec(1, 2),
            max_leverage: fixed_dec(3, 0),
        },
    }
}

// ---- (a) the pinned vocabulary, token by token -----------------------------

#[test]
fn direction_renders_lowercase() {
    assert_eq!(render::direction(Direction::Long), "long");
    assert_eq!(render::direction(Direction::Short), "short");
}

#[test]
fn indicators_render_name_params_without_spaces() {
    assert_eq!(
        render::indicator(&IndicatorSpec::Rsi {
            period: fixed_u32(14)
        }),
        "rsi(14)"
    );
    assert_eq!(
        render::indicator(&IndicatorSpec::Ema {
            period: fixed_u32(200)
        }),
        "ema(200)"
    );
    assert_eq!(
        render::indicator(&IndicatorSpec::Adx {
            period: fixed_u32(14)
        }),
        "adx(14)"
    );
    assert_eq!(
        render::indicator(&IndicatorSpec::Macd {
            fast: fixed_u32(12),
            slow: fixed_u32(26),
            signal: fixed_u32(9)
        }),
        "macd(12,26,9)"
    );
    assert_eq!(
        render::indicator(&IndicatorSpec::Atr {
            period: fixed_u32(14)
        }),
        "atr(14)"
    );
    // A swept leaf renders `{lo..hi}` in the parameter slot.
    assert_eq!(
        render::indicator(&IndicatorSpec::Rsi {
            period: SweepableValue::Sweep {
                start: 5,
                end: 20,
                step: 5
            }
        }),
        "rsi({5..20})"
    );
}

#[test]
fn values_render_constants_prices_and_series_tags() {
    // Constants render normalized (trailing zeros trimmed).
    assert_eq!(render::value(&constant(30, 0)), "30");
    assert_eq!(render::value(&constant(5, 1)), "0.5");
    assert_eq!(render::value(&constant(250, 2)), "2.5");
    // Prices render as their field name; `htf` operands carry the `h4:` prefix.
    assert_eq!(
        render::value(&price(Series::Primary, PriceField::Close)),
        "close"
    );
    assert_eq!(
        render::value(&price(Series::Htf, PriceField::Close)),
        "h4:close"
    );
    assert_eq!(
        render::value(&indicator(
            Series::Primary,
            IndicatorSpec::Ema {
                period: fixed_u32(200)
            }
        )),
        "ema(200)"
    );
    assert_eq!(
        render::value(&indicator(
            Series::Htf,
            IndicatorSpec::Ema {
                period: fixed_u32(200)
            }
        )),
        "h4:ema(200)"
    );
}

#[test]
fn comparators_render_their_symbols() {
    assert_eq!(render::comparator(Comparator::Lt), "<");
    assert_eq!(render::comparator(Comparator::Lte), "<=");
    assert_eq!(render::comparator(Comparator::Gt), ">");
    assert_eq!(render::comparator(Comparator::Gte), ">=");
    assert_eq!(render::comparator(Comparator::Eq), "=");
}

#[test]
fn conditions_render_compare_crosses_and_compounds() {
    assert_eq!(render::condition(&rsi_oversold()), "rsi(14) < 30");
    assert_eq!(
        render::condition(&Condition::CrossesAbove {
            lhs: price(Series::Primary, PriceField::Close),
            rhs: indicator(
                Series::Primary,
                IndicatorSpec::Ema {
                    period: fixed_u32(200)
                }
            ),
        }),
        "close crosses above ema(200)"
    );
    assert_eq!(
        render::condition(&Condition::CrossesBelow {
            lhs: price(Series::Primary, PriceField::Close),
            rhs: indicator(
                Series::Primary,
                IndicatorSpec::Ema {
                    period: fixed_u32(200)
                }
            ),
        }),
        "close crosses below ema(200)"
    );
    // `and`/`or` groups parenthesize each member; `not` wraps its operand.
    assert_eq!(
        render::condition(&Condition::And {
            conditions: vec![
                rsi_oversold(),
                compare(
                    price(Series::Primary, PriceField::Close),
                    Comparator::Gt,
                    indicator(
                        Series::Primary,
                        IndicatorSpec::Ema {
                            period: fixed_u32(200)
                        }
                    )
                ),
            ],
        }),
        "(rsi(14) < 30) and (close > ema(200))"
    );
    assert_eq!(
        render::condition(&Condition::Or {
            conditions: vec![rsi_oversold(), rsi_oversold()],
        }),
        "(rsi(14) < 30) or (rsi(14) < 30)"
    );
    assert_eq!(
        render::condition(&Condition::Not {
            condition: Box::new(rsi_oversold()),
        }),
        "not (rsi(14) < 30)"
    );
    // Nested: a compound member is wrapped like any other member.
    assert_eq!(
        render::condition(&Condition::And {
            conditions: vec![
                rsi_oversold(),
                Condition::Or {
                    conditions: vec![
                        compare(
                            price(Series::Primary, PriceField::Close),
                            Comparator::Gt,
                            constant(100, 0)
                        ),
                        compare(
                            price(Series::Primary, PriceField::Close),
                            Comparator::Lt,
                            constant(50, 0)
                        ),
                    ],
                },
            ],
        }),
        "(rsi(14) < 30) and ((close > 100) or (close < 50))"
    );
}

#[test]
fn exits_render_each_rule_in_the_pinned_vocabulary() {
    assert_eq!(
        render::exit(&ExitRule::StopLoss {
            distance_pct: fixed_dec(5, 2)
        }),
        "stop 5%"
    );
    assert_eq!(
        render::exit(&ExitRule::StopLoss {
            distance_pct: fixed_dec(15, 3)
        }),
        "stop 1.5%"
    );
    // `target_r` normalizes: `2.0` on the wire reads `2R`.
    assert_eq!(
        render::exit(&ExitRule::TakeProfit {
            target_r: fixed_dec(20, 1)
        }),
        "take profit 2R"
    );
    assert_eq!(
        render::exit(&ExitRule::TrailingStop {
            trail_pct: fixed_dec(1, 2)
        }),
        "trailing 1%"
    );
    assert_eq!(
        render::exit(&ExitRule::TimeStop {
            max_bars: fixed_u32(20)
        }),
        "time stop 20 bars"
    );
    assert_eq!(
        render::exit(&ExitRule::SignalExit {
            condition: rsi_oversold()
        }),
        "signal exit (rsi(14) < 30)"
    );
    // The `d18` token: `AtrStop` renders `atr(<period>)×<multiple>` — U+00D7,
    // the multiple normalized (`2.0` → `2`, `2.5` → `2.5`).
    assert_eq!(
        render::exit(&ExitRule::AtrStop {
            period: fixed_u32(14),
            multiple: fixed_dec(2, 0)
        }),
        "atr(14)×2"
    );
    assert_eq!(
        render::exit(&ExitRule::AtrStop {
            period: fixed_u32(14),
            multiple: fixed_dec(25, 1)
        }),
        "atr(14)×2.5"
    );
}

#[test]
fn risk_lines_render_the_two_fields() {
    assert_eq!(
        render::risk(&RiskParams {
            risk_per_trade_pct: fixed_dec(1, 2),
            max_leverage: fixed_dec(3, 0),
        }),
        vec!["risk 1% per trade".to_owned(), "max leverage 3x".to_owned()]
    );
}

// ---- (b) the seeded journey document ---------------------------------------

#[test]
fn the_seeded_journey_document_renders_its_expected_lines() {
    let rendered = render::strategy(&journey_dsl());
    assert_eq!(
        rendered,
        Rendered {
            direction: "long".to_owned(),
            entry: "rsi(14) < 30".to_owned(),
            filters: vec!["h4:close > h4:ema(200)".to_owned()],
            exits: vec!["atr(14)×2".to_owned(), "take profit 2R".to_owned()],
            risk: vec!["risk 1% per trade".to_owned(), "max leverage 3x".to_owned()],
        }
    );
}

// ---- (c) the two DTO adapters agree by construction -------------------------

fn manifest(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// For every JSON under `tests/fixtures/strategies/` (plus the journey
/// document — a 1.1.0 document cannot live there), the Library's `DslSummary`
/// and the Designer's `ComposeDslSummary` carry identical lines. Before this
/// item the two renderers disagreed (`stop loss 5%` vs `stop_loss 5%`,
/// `atr stop 14 x2` vs `atr_stop 14 x2`) — the drift b10 names.
#[test]
fn both_dsl_adapters_agree_on_every_committed_fixture() {
    let dir = manifest("tests/fixtures/strategies");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixture dir readable")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "fixture dir must hold at least one json");

    let mut docs: Vec<(String, StrategyDsl)> = files
        .into_iter()
        .map(|path| {
            let json = std::fs::read_to_string(&path).expect("read fixture");
            let dsl: StrategyDsl = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("{} must parse as a StrategyDsl: {e}", path.display()));
            (path.display().to_string(), dsl)
        })
        .collect();
    docs.push(("the journey document".to_owned(), journey_dsl()));

    for (label, dsl) in docs {
        let library = dsl_summary(&dsl);
        let compose = summarize_dsl(&dsl);
        assert_eq!(
            library.direction, compose.direction,
            "{label}: direction lines disagree"
        );
        assert_eq!(
            library.entry,
            vec![compose.entry.clone()],
            "{label}: entry lines disagree"
        );
        assert_eq!(
            library.filters, compose.filters,
            "{label}: filter lines disagree"
        );
        assert_eq!(library.exits, compose.exits, "{label}: exit lines disagree");
        assert_eq!(library.risk, compose.risk, "{label}: risk lines disagree");
    }
}
