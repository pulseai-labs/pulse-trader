//! `pulse indicators` offline indicator-series viewer.
//!
//! Since r1.s3.w1 (#112) it reads candles through the [`CandleSeriesRepository`]
//! port; `src/cli/mod.rs` resolves the fixture/default root and chooses the
//! concrete adapter.

use std::path::PathBuf;

use crate::{CandleSeriesRepository, CompiledValue, EvalContext, IndicatorEngine, Pair, Series};
use rust_decimal::Decimal;

// r2.s1.w2: the `<kind>:<period>` parser moved to `crate::application::mcp_read`
// so `pulse mcp` runs the same use case. The re-export keeps this viewer's call
// sites — and its output — byte-identical.
pub(crate) use crate::application::mcp_read::{IndicatorColumn, parse_indicator_specs};

use super::parse_one_tf;

const BLANK: &str = "—";

/// `pulse indicators --pair BTCUSDT --tf M15 [--indicator rsi:14] [--limit N]`.
#[derive(Debug, clap::Args)]
pub struct IndicatorsArgs {
    /// The trading pair symbol (e.g. `BTCUSDT`).
    #[arg(long)]
    pub pair: String,
    /// Candle timeframe to load (`M15`/`15m` or `H4`/`4h`).
    #[arg(long)]
    pub tf: String,
    /// `CandleStore` root. Defaults to the committed `BTCUSDT` fixture under CWD.
    #[arg(long)]
    pub base_dir: Option<PathBuf>,
    /// Indicator to render as `<kind>:<period>`; repeatable.
    #[arg(long = "indicator")]
    pub indicators: Vec<String>,
    /// Maximum number of candle rows to print. Omitted means all rows.
    #[arg(long)]
    pub limit: Option<usize>,
}

/// Load the `HEAD` snapshot through the repository port, stream the indicator
/// engine over every candle, and print a deterministic tab-separated series.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on invalid args, missing fixture data, or engine
/// construction failure.
pub fn run_indicators<R>(repo: &R, args: &IndicatorsArgs) -> anyhow::Result<()>
where
    R: CandleSeriesRepository,
{
    let pair = Pair::parse(args.pair.clone())
        .map_err(|e| anyhow::anyhow!("invalid pair argument: {e}"))?;
    let tf = parse_one_tf(&args.tf)?;
    let series = repo
        .load_head(&pair, tf)?
        .ok_or_else(|| anyhow::anyhow!("no HEAD snapshot for {pair} {}", tf.binance_interval()))?
        .series;
    let columns = parse_indicator_specs(&args.indicators)?;
    let specs = columns
        .iter()
        .map(|column| column.spec.clone())
        .collect::<Vec<_>>();
    let mut engine = IndicatorEngine::from_specs(&specs)?;
    let mut first_rows = vec![None; columns.len()];

    println!("{}", render_header(&columns));
    for (idx, candle) in series.candles.iter().enumerate() {
        engine.step(candle);
        let values = current_values(&engine, &columns);
        note_first_rows(idx + 1, &values, &mut first_rows);
        if args.limit.is_none_or(|limit| idx < limit) {
            println!("{}", render_row(candle.open_time, &values));
        }
    }
    println!(
        "{}",
        render_summary(series.candles.len(), &columns, &first_rows)
    );
    println!("\u{26a0} Not financial advice \u{2014} hypothetical results. See DISCLAIMER.md");
    Ok(())
}

#[must_use]
pub(crate) fn render_indicator_value(value: Option<Decimal>) -> String {
    value.map_or_else(|| BLANK.to_owned(), |value| value.normalize().to_string())
}

#[must_use]
pub(crate) fn render_header(columns: &[IndicatorColumn]) -> String {
    let mut cells = Vec::with_capacity(columns.len() + 1);
    cells.push("open_time".to_owned());
    cells.extend(columns.iter().map(|column| column.label.clone()));
    cells.join("\t")
}

#[must_use]
pub(crate) fn render_row(open_time: i64, values: &[Option<Decimal>]) -> String {
    let mut cells = Vec::with_capacity(values.len() + 1);
    cells.push(open_time.to_string());
    cells.extend(values.iter().copied().map(render_indicator_value));
    cells.join("\t")
}

fn current_values(engine: &IndicatorEngine, columns: &[IndicatorColumn]) -> Vec<Option<Decimal>> {
    columns
        .iter()
        .map(|column| {
            engine.current(&CompiledValue::Indicator {
                series: Series::Primary,
                spec: column.spec.clone(),
            })
        })
        .collect()
}

fn note_first_rows(row: usize, values: &[Option<Decimal>], first_rows: &mut [Option<usize>]) {
    for (first_row, value) in first_rows.iter_mut().zip(values) {
        if first_row.is_none() && value.is_some() {
            *first_row = Some(row);
        }
    }
}

fn render_summary(
    candle_count: usize,
    columns: &[IndicatorColumn],
    first_rows: &[Option<usize>],
) -> String {
    let mut cells = Vec::with_capacity(columns.len() + 2);
    cells.push("summary".to_owned());
    cells.push(format!("candles={candle_count}"));
    cells.extend(columns.iter().zip(first_rows).map(|(column, first_row)| {
        format!(
            "{}_first_row={}",
            column.label,
            first_row.map_or_else(|| "none".to_owned(), |row| row.to_string())
        )
    }));
    cells.join("\t")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{parse_indicator_specs, render_header, render_indicator_value, render_row};
    use rust_decimal::Decimal;

    #[test]
    fn warmup_rows_render_as_blank() {
        let columns =
            parse_indicator_specs(&["rsi:14".to_owned(), "ema:50".to_owned()]).expect("parse");
        assert_eq!(render_header(&columns), "open_time\trsi:14\tema:50");
        assert_eq!(render_indicator_value(None), "—");
        assert_eq!(
            render_indicator_value(Some(Decimal::new(42_125, 3))),
            "42.125"
        );
        assert_eq!(
            render_row(1_700_000_000_000, &[None, Some(Decimal::new(42_125, 3))]),
            "1700000000000\t—\t42.125"
        );
    }
}
