#!/usr/bin/env python3
"""Generate the WI-3.04 pandas-ta reference CSV.

Run from the repository root with a venv OUTSIDE the worktree at the pinned
scratch path (`/tmp` is forbidden by host rule; `mktemp` lands there):

    REFVENV="$HOME/.cache/pulse-scratch/r3.s2.w2/refgen"
    uv venv --python 3.11 "$REFVENV"
    # PIN the versions below — a floating install resolved pandas-ta-classic
    # 0.8.32 on 2026-09-28 and CHANGED the early ADX settling-window values
    # (rows 29-375); the committed CSV's existing columns must stay byte-identical.
    uv pip install --python "$REFVENV/bin/python" pandas-ta-classic==0.6.20 pyarrow==25.0.1
    "$REFVENV/bin/python" tests/fixtures/indicators/gen_reference.py

Resolved on 2026-06-12; regenerated 2026-09-19 for the r2.s2.w2 `atr_14`
column; regenerated 2026-09-28 for the r3.s2.w1 `macd_signal_12_26_9` and
`macd_hist_12_26_9` columns; regenerated 2026-09-29 for the r3.s2.w2
`highest_high_20` and `lowest_low_20` columns:
    python 3.11.16
    numpy 2.4.6
    pandas 3.0.6
    pandas-ta-classic 0.6.20
    pyarrow 25.0.1

The legacy pandas-ta package was attempted first per handoff §4.1, but this
index exposed no Python-3.11-compatible pandas-ta 0.3.14b0 candidate. The
maintained fork is the specified fallback and registers the same df.ta accessor
after `import pandas_ta_classic`.

Indicator calls:
    EMA(50): df.ta.ema(close=close, length=50, adjust=False, sma=False)
    RSI(14): intended df.ta.rsi(close=close, length=14, mamode="ema");
        pandas-ta-classic ignores mamode, so this generator composes the same
        EMA-smoothed RSI from df.ta.ema(..., adjust=False, sma=False) over
        positive/negative close deltas.
    ADX(14): df.ta.adx(high=high, low=low, close=close, length=14)
    MACD line: df.ta.ema(close=close, length=12, adjust=False, sma=False)
        - df.ta.ema(close=close, length=26, adjust=False, sma=False)
    MACD signal (r3.s2.w1): a seeded recursive EMA(9) over the FULL UNBLANKED
        line — both sides seed at candle 1, mirroring ta-rs, whose MACD
        advances the signal EMA from the very first candle
        (`signal_ema.next(macd)` inside `MovingAverageConvergenceDivergence::next`).
        Composing the signal over the blanked line (first valid at index 25)
        would seed 25 bars late and DISAGREE with the engine.
    MACD histogram (r3.s2.w1): line − signal (ta-rs's `histogram = macd - signal`).
    ATR(14): df.ta.atr(high=high, low=low, close=close, length=14,
        mamode="rma") — the Wilder RMA (alpha = 1/period), matching the
        engine's shared WilderRma recursion. pandas-ta-classic SMA-seeds its
        rma over the first `period` true ranges, identically to the engine, so
        the cross-validation needs no settling window (measured at regen: worst
        relative delta ≈2.7e-12 post-warmup, settle_bars = 0).
    highest_high_20 (r3.s2.w2): df["high"].rolling(20).max().shift(1) — plain
        pandas, the prior-N convention: at row k it is the max high of rows
        k-20..k-1, the 20 closed bars BEFORE k, excluding k (Q1). First
        defined at index 20 — the engine's N+1 warm-up for the rolling
        extremes. The same construction gives lowest_low_20 over `low`.

The classic fork emits ADX values before the project ADX warmup boundary.
Rows before index 27 (2 * 14 - 1) are blanked so the committed reference
preserves the VS-1.1.3 ADX warmup contract; comparison still uses the
pandas-ta ADX line after that boundary plus the documented settling window.
pandas-ta's ATR likewise emits before the engine's first-defined index
(candle index 14 — the first `period` real TRs seed its SMA), so rows before
index 14 are blanked for the same warmup-contract reason.

The MACD signal and histogram (r3.s2.w1) blank rows before index
`max(fast, slow) + signal − 1 = 33`: the line first exists at index 25
(0-based; `max(fast, slow) − 1`), and the seeded signal EMA needs `signal`
line values (indices 25..33), so its first fully-defined value is index 33 —
candle 34, the engine's warmup boundary for the selector-bearing slots.
"""

from __future__ import annotations

import csv
import math
from pathlib import Path

import pandas as pd
import pandas_ta_classic  # noqa: F401 - registers the pandas df.ta accessor
from pandas_ta_classic.overlap import ema


ROOT = Path(__file__).resolve().parents[3]
SNAPSHOT_DIR = ROOT / "tests/fixtures/btcusdt-1m-store/candles/BTCUSDT/15m"
OUTPUT = ROOT / "tests/fixtures/indicators/btcusdt-m15-reference.csv"
ADX_FIRST_DEFINED_INDEX = 2 * 14 - 1
ATR_FIRST_DEFINED_INDEX = 14
EMA_FIRST_DEFINED_INDEX = 50 - 1
MACD_FIRST_DEFINED_INDEX = 26 - 1
# line first at 25 (0-based); signal/hist need `signal` line values → 25 + 9 - 1.
MACD_SIGNAL_FIRST_DEFINED_INDEX = 26 - 1 + 9 - 1
# r3.s2.w2: rolling(20).shift(1) is NaN through index 19 by construction; the
# explicit blank pins the warm-up contract (first defined at index 20 = N+1).
HIGHEST_FIRST_DEFINED_INDEX = 20
LOWEST_FIRST_DEFINED_INDEX = 20


def format_value(value: float) -> str:
    if value is None or math.isnan(value):
        return ""
    return f"{value:.12g}"


def recursive_ema(series: pd.Series, length: int) -> pd.Series:
    return ema(series, length=length, adjust=False, sma=False)


def recursive_ema_rsi(close: pd.Series, length: int) -> pd.Series:
    delta = close.diff()
    positive = delta.copy()
    negative = -delta.copy()
    positive[positive < 0] = 0
    negative[negative < 0] = 0
    positive.iloc[0] = 0.1
    negative.iloc[0] = 0.1

    positive_avg = recursive_ema(positive, length)
    negative_avg = recursive_ema(negative, length)
    rsi = 100 * positive_avg / (positive_avg + negative_avg)
    # RSI is delta-based: RSI(length) needs `length` price *deltas* = `length + 1`
    # candles, so the engine's port emits its first defined RSI on candle
    # `length + 1` (0-based index `length`), NOT candle `length`. This is the
    # principled convention the VS-1.1.3 adapter pins (distinct from EMA, which is
    # prices-based and first-defines at candle `length`). Blank indices 0..length-1
    # so the reference's first non-blank RSI row aligns with the engine.
    # NOTE: the absolute EMA-RSI warmup boundary is a convention, not yet confirmed
    # against an independent real pandas-ta (the fork here ignored mamode, so this
    # generator composes EMA-over-deltas) — see the slice follow-up issue.
    rsi.iloc[:length] = math.nan
    return rsi


def snapshot_path() -> Path:
    paths = sorted(SNAPSHOT_DIR.glob("*.parquet"))
    if len(paths) != 1:
        raise SystemExit(f"expected exactly one M15 parquet snapshot, found {len(paths)}")
    return paths[0]


def main() -> None:
    df = pd.read_parquet(snapshot_path()).sort_values("open_time").reset_index(drop=True)
    required = {"open_time", "open", "high", "low", "close", "volume", "funding_rate"}
    missing = required.difference(df.columns)
    if missing:
        raise SystemExit(f"snapshot missing required columns: {sorted(missing)}")

    close = df["close"].astype(float)
    high = df["high"].astype(float)
    low = df["low"].astype(float)

    rsi = recursive_ema_rsi(close, 14)
    ema = recursive_ema(close, 50)
    adx = df.ta.adx(high=high, low=low, close=close, length=14)["ADX_14"].copy()
    # Compose the FULL line (both EMAs seeded at candle 1 — no NaNs), then the
    # signal as a seeded recursive EMA over it, mirroring ta-rs's internal
    # `signal_ema.next(macd)` from the very first candle. Blanking happens
    # AFTER composition, on copies.
    macd = (recursive_ema(close, 12) - recursive_ema(close, 26)).copy()
    macd_signal = recursive_ema(macd, 9).copy()
    macd_hist = (macd - macd_signal).copy()
    atr = df.ta.atr(high=high, low=low, close=close, length=14, mamode="rma")

    # r3.s2.w2: the rolling extremes are exact prior-N window aggregates over
    # the raw price fields (Q1's convention), not a pandas-ta call.
    highest_high_20 = high.rolling(20).max().shift(1)
    lowest_low_20 = low.rolling(20).min().shift(1)

    ema.iloc[:EMA_FIRST_DEFINED_INDEX] = math.nan
    adx.iloc[:ADX_FIRST_DEFINED_INDEX] = math.nan
    macd.iloc[:MACD_FIRST_DEFINED_INDEX] = math.nan
    macd_signal.iloc[:MACD_SIGNAL_FIRST_DEFINED_INDEX] = math.nan
    macd_hist.iloc[:MACD_SIGNAL_FIRST_DEFINED_INDEX] = math.nan
    atr.iloc[:ATR_FIRST_DEFINED_INDEX] = math.nan
    highest_high_20.iloc[:HIGHEST_FIRST_DEFINED_INDEX] = math.nan
    lowest_low_20.iloc[:LOWEST_FIRST_DEFINED_INDEX] = math.nan

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with OUTPUT.open("w", newline="") as handle:
        writer = csv.writer(handle)
        writer.writerow(
            [
                "open_time",
                "rsi_14",
                "ema_50",
                "adx_14",
                "macd_12_26_9",
                "atr_14",
                "macd_signal_12_26_9",
                "macd_hist_12_26_9",
                "highest_high_20",
                "lowest_low_20",
            ]
        )
        for idx, row in df.iterrows():
            writer.writerow(
                [
                    int(row["open_time"]),
                    format_value(float(rsi.iloc[idx])),
                    format_value(float(ema.iloc[idx])),
                    format_value(float(adx.iloc[idx])),
                    format_value(float(macd.iloc[idx])),
                    format_value(float(atr.iloc[idx])),
                    format_value(float(macd_signal.iloc[idx])),
                    format_value(float(macd_hist.iloc[idx])),
                    format_value(float(highest_high_20.iloc[idx])),
                    format_value(float(lowest_low_20.iloc[idx])),
                ]
            )

    print(f"wrote {OUTPUT.relative_to(ROOT)} rows={len(df)}")
    print(
        "first-defined "
        f"rsi={rsi.first_valid_index()} "
        f"ema={EMA_FIRST_DEFINED_INDEX} "
        f"adx={ADX_FIRST_DEFINED_INDEX} "
        f"macd={MACD_FIRST_DEFINED_INDEX} "
        f"macd_signal={MACD_SIGNAL_FIRST_DEFINED_INDEX} "
        f"macd_hist={MACD_SIGNAL_FIRST_DEFINED_INDEX} "
        f"atr={ATR_FIRST_DEFINED_INDEX} "
        f"highest_high_20={highest_high_20.first_valid_index()} "
        f"lowest_low_20={lowest_low_20.first_valid_index()}"
    )


if __name__ == "__main__":
    main()
