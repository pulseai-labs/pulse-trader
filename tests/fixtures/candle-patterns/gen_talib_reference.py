"""Generate TA-Lib reference outputs for the 17 r5.s1 candlestick patterns.

Usage: python -I gen_talib_reference.py <store_symbol_dir> <out_dir>
  <store_symbol_dir> = .../tests/fixtures/btcusdt-1m-store/candles/BTCUSDT
Writes <out_dir>/talib-reference-15m.csv, talib-reference-4h.csv, lookbacks.json, counts.json.
Default candle settings throughout; CDLDARKCLOUDCOVER penetration 0.5,
CDLMORNINGSTAR / CDLEVENINGSTAR penetration 0.3.
"""
import csv
import json
import os
import sys

import numpy as np
import pyarrow.parquet as pq
import talib
from talib import abstract

PATTERNS = [
    "CDLDOJI", "CDLDRAGONFLYDOJI", "CDLGRAVESTONEDOJI", "CDLHAMMER",
    "CDLINVERTEDHAMMER", "CDLHANGINGMAN", "CDLSHOOTINGSTAR", "CDLMARUBOZU",
    "CDLSPINNINGTOP", "CDLENGULFING", "CDLHARAMI", "CDLPIERCING",
    "CDLDARKCLOUDCOVER", "CDLMORNINGSTAR", "CDLEVENINGSTAR",
    "CDL3WHITESOLDIERS", "CDL3BLACKCROWS",
]
PARAMS = {
    "CDLDARKCLOUDCOVER": {"penetration": 0.5},
    "CDLMORNINGSTAR": {"penetration": 0.3},
    "CDLEVENINGSTAR": {"penetration": 0.3},
}


def lookbacks():
    out = {}
    for p in PATTERNS:
        f = abstract.Function(p)
        if p in PARAMS:
            f.set_parameters(PARAMS[p])
        out[p] = int(f.lookback)
    return out


def run_patterns(o, h, l, c):
    """o,h,l,c: lists of decimal strings. Returns {pattern: np.ndarray[int]}."""
    O = np.array([float(x) for x in o], dtype=np.float64)
    H = np.array([float(x) for x in h], dtype=np.float64)
    L = np.array([float(x) for x in l], dtype=np.float64)
    C = np.array([float(x) for x in c], dtype=np.float64)
    res = {}
    for p in PATTERNS:
        fn = getattr(talib, p)
        r = fn(O, H, L, C, **PARAMS.get(p, {}))
        r = np.asarray(r)
        assert not np.isnan(r.astype(np.float64)).any()
        res[p] = r.astype(np.int64)
    return res


def write_csv(path, open_time, o, h, l, c, res):
    tmp = path + ".tmp"
    with open(tmp, "w", newline="") as fh:
        w = csv.writer(fh, lineterminator="\n")
        w.writerow(["open_time", "open", "high", "low", "close"] + PATTERNS)
        for i in range(len(o)):
            w.writerow([open_time[i], o[i], h[i], l[i], c[i]] + [int(res[p][i]) for p in PATTERNS])
    os.replace(tmp, path)


def counts(res):
    out = {}
    for p in PATTERNS:
        vals, cnt = np.unique(res[p], return_counts=True)
        out[p] = {str(int(v)): int(n) for v, n in zip(vals, cnt) if v != 0}
    return out


def main():
    store, out_dir = sys.argv[1], sys.argv[2]
    os.makedirs(out_dir, exist_ok=True)
    lb = lookbacks()
    summary = {"talib_python": talib.__version__, "talib_c": talib.__ta_version__.decode()
               if isinstance(talib.__ta_version__, bytes) else talib.__ta_version__,
               "lookbacks": lb, "series": {}}
    for tf in ("15m", "4h"):
        head = open(os.path.join(store, tf, "HEAD")).read().strip()
        pqf = os.path.join(store, tf, head + ".parquet")
        t = pq.read_table(pqf, columns=["open_time", "open", "high", "low", "close"])
        d = {k: t.column(k).to_pylist() for k in t.column_names}
        ot = d["open_time"]
        assert all(ot[i] < ot[i + 1] for i in range(len(ot) - 1)), "open_time not strictly increasing"
        res = run_patterns(d["open"], d["high"], d["low"], d["close"])
        write_csv(os.path.join(out_dir, f"talib-reference-{tf}.csv"), ot,
                  d["open"], d["high"], d["low"], d["close"], res)
        summary["series"][tf] = {"snapshot": head, "bars": len(ot), "first_open_time": ot[0],
                                 "last_open_time": ot[-1], "counts": counts(res)}
    with open(os.path.join(out_dir, "summary.json"), "w") as fh:
        json.dump(summary, fh, indent=1)
    print(json.dumps(summary, indent=1))


if __name__ == "__main__":
    main()
