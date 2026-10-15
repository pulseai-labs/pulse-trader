"""Hand-built candle sequences for patterns that fire < 3 times in the BTCUSDT fixture.

Usage: python -I gen_synthetic.py <out_dir> PATTERN [PATTERN ...]
Writes <out_dir>/talib-synthetic-<pattern>.csv (same columns as the reference CSVs).
Synthetic open_time starts at 1735689600000 and steps 15 minutes.
"""
import os
import sys
from decimal import Decimal as D

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from gen_talib_reference import PATTERNS, run_patterns, write_csv  # noqa: E402

START = D("100000.00")
T0, STEP = 1735689600000, 900000


BODIES = [D(x) for x in ("8.3", "11.7", "9.1", "12.4", "7.6", "10.9", "9.7")]
UPS = [D(x) for x in ("6.2", "9.8", "7.4", "11.3", "8.6")]
DOWNS = [D(x) for x in ("10.4", "7.1", "12.2", "8.9")]


def filler(bars, px, n):
    """n quiet candles alternating white/black with irregular bodies and shadows
    (irregular so no comparison lands exactly on a threshold)."""
    base = len(bars)
    for k in range(n):
        body = BODIES[(base + k) % len(BODIES)]
        o, c = (px, px + body) if k % 2 == 0 else (px, px - body)
        up = UPS[(base + k) % len(UPS)]
        dn = DOWNS[(base + k) % len(DOWNS)]
        bars.append((o, max(o, c) + up, min(o, c) - dn, c))
        px = c
    return px


def block(pattern, p):
    """Return the candles of one occurrence starting from previous close p, and the new close."""
    if pattern == "CDLPIERCING":
        b = [(p, p + 5, p - 205, p - 200),            # long black
             (p - 215, p - 75, p - 220, p - 80)]      # long white: opens below prior low, closes above midpoint
    elif pattern == "CDLDARKCLOUDCOVER":
        b = [(p, p + 205, p - 5, p + 200),            # long white
             (p + 215, p + 220, p + 75, p + 80)]      # black: opens above prior high, closes below midpoint
    elif pattern == "CDL3BLACKCROWS":
        b = [(p, p + 30, p - 10, p + 20),             # white
             (p + 15, p + 20, p - 85, p - 85),        # crow 1: no lower shadow
             (p - 35, p - 30, p - 135, p - 135),      # crow 2 opens inside crow 1 body
             (p - 85, p - 80, p - 185, p - 185)]      # crow 3 opens inside crow 2 body
    else:
        raise SystemExit(f"no synthetic block defined for {pattern}")
    return b, b[-1][3]


def build(pattern, occurrences=3):
    bars = []
    px = filler(bars, START, 14)
    for _ in range(occurrences):
        b, px = block(pattern, px)
        bars.extend(b)
        px = filler(bars, px, 12)
    return bars


def main():
    out_dir = sys.argv[1]
    for pattern in sys.argv[2:]:
        bars = build(pattern)
        o = [f"{x[0]:.2f}" for x in bars]
        h = [f"{x[1]:.2f}" for x in bars]
        lo = [f"{x[2]:.2f}" for x in bars]
        c = [f"{x[3]:.2f}" for x in bars]
        for i in range(len(bars)):
            assert D(lo[i]) <= min(D(o[i]), D(c[i])) and D(h[i]) >= max(D(o[i]), D(c[i]))
        ot = [T0 + STEP * i for i in range(len(bars))]
        res = run_patterns(o, h, lo, c)
        path = os.path.join(out_dir, f"talib-synthetic-{pattern}.csv")
        write_csv(path, ot, o, h, lo, c, res)
        fired = [(int(ot[i]), int(res[pattern][i])) for i in range(len(bars)) if res[pattern][i] != 0]
        others = {p: int((res[p] != 0).sum()) for p in PATTERNS if p != pattern and (res[p] != 0).any()}
        print(pattern, "bars", len(bars), "fires", len(fired), fired, "other patterns firing:", others)
        assert len(fired) >= 3, pattern


if __name__ == "__main__":
    main()
