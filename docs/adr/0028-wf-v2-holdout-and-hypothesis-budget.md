# 28. `wf-v2`, the C1 holdout test, and the hypothesis budget

Date: 2026-10-08T00:00:00Z

## Status

Accepted. Amends [ADR-0025](0025-walk-forward-run-kind-and-certification-seam.md) (status line only;
its text stays): `wf-v2` is a second rule name beside the byte-identical `wf-v1`.

## Context

ADR-0025 shipped one verdict rule, `wf-v1`: a fold holds when it has at least 20 trades and its
one-sided expectancy lower bound `mean − 1.645·sqrt(var/n)` is above zero, and the run passes when
at least `⌈2K/3⌉` folds hold and the pooled bound over every out-of-sample trade is above zero.
That rule is a **search-span** rule. The r4.s1 campaign (RELEASE.md r4.s1; SPINE.md rulings D3, Q1,
Q2, C1) proposes strategy versions, refines them on a search span, and must certify at most 12 of
them on data no search run ever saw. Two things are therefore needed before the campaign's first
run, and this ADR is where they are frozen:

1. a **stricter-in-the-right-places verdict rule** for the search span, so a version that is only
   marginally positive does not pass a fold on a wide bound; and
2. a **holdout test** that decides certification on unseen data at a false-certification rate the
   hypothesis budget actually supports.

The planning audit (C1, 2026-10-07) established that the original Q2 model was wrong: a candidate
has been tuned until it passes the search span, so "holdout mean > 0" passes a zero-edge candidate
30–50% of the time, and 12 such attempts make a false certification close to certain. The operator
ruled the C1 test in its place — a family-wise 5% split over the budget, made **before any
candidate is seen**, so it is a tightening, not a loosening.

## Decision

**`wf-v2` is a new rule name beside `wf-v1`; `wf-v1` is untouched.** `wf-v2` holds a fold when it
has at least 20 trades **and a positive mean expectancy** (`n >= 20 && mean_r > 0`) — the fold's
lower bound is still computed and stored for display, but it no longer decides. The run passes
when at least `⌈2K/3⌉` folds hold **and** the pooled one-sided 95% lower bound over every
out-of-sample trade is above zero (`z = 1.645`) — the same pooled bound as `wf-v1`. The rule is
request-selectable: the MCP `run_walk_forward` tool takes an optional `rule` argument (`wf-v1` by
default; an unknown value is refused naming `rule`), the store's `rule` column accepts `wf-v2`, the
save gate re-derives every fold and the run verdict under **the draft's own rule**, and the coach's
certification gate re-runs a candidate under the parent's certifying run's rule. Server ops and the
Tauri command keep their signatures (ADR-0020) and stay on `wf-v1`. Every earlier `wf-v1` verdict
re-derives byte-identically; a persisted `wf-v1` run is never re-assessed under `wf-v2` in storage.

**The C1 holdout test.** A candidate passes its holdout when the one-sided lower confidence bound
of its holdout expectancy in R is **strictly above zero**, at a family-wise 5% split over the
hypothesis budget: `z = z(1 − 0.05/H)`, and `lower_bound = mean − z·sqrt(var/n)` with the mean and
the sample variance (Bessel `N−1`) accumulated in `Decimal` exactly as the walk-forward's fold
verdict does. `holdout_test(rs, h)` in the domain implements it; the quantile is computed by a
documented in-crate inverse-normal approximation (Acklam, |error| < 1.15e-9 — no new crate).
`passes = n >= 2 && lower_bound > 0.0`. This item implements and measures the test; the
certification step that **uses** it is w5.

**The frozen parameters.**

- **Holdout start: 2025-07-01**, all four pairs (BTCUSDT, ETHUSDT, SOLUSDT, XRPUSDT).
- **Search span: 2021-01-01 to 2025-07-01.**
- **Hypothesis budget H = 12** across all four pairs — one hypothesis is one call to the
  certification step. **Lower-only**: H is never raised, and never changed after a candidate has
  been seen. The 13th attempt is refused by name (w5).
- **Freeze lifecycle (F1).** The guard is active only while a freeze is open, and it reads the
  holdout start from the freeze record. A spent holdout is never reused: a later freeze must start
  its holdout after the previous close, with a new budget. The freeze record and its two operator
  commands are **w4's**; the certification record is **w5's**.

**Measured α, the Q2 bound, the power table and the runtime.** Every number below is re-derivable
with the item's measurement harness:

```text
cargo nextest run --run-ignored ignored-only --test wf_v2_measurement
PULSE_WF_V2_DATA_DIR=<data dir> PULSE_ALLOW_PLACEHOLDER_DIST=1 \
  cargo nextest run --release --run-ignored ignored-only --test wf_v2_measurement
```

*α, synthetic.* The zero-edge wf-v2 pass rate over the fixed seed set 1..=200 (K = 6 folds of 100
trades, R ~ N(0, 1.2²) from a seeded splitmix64/Box–Muller stream) is **α = 10/200 = 0.05**. The
Q2 bound `1 − (1 − α/2)^H` at H = 12 is **0.2620 (26.2%)**.

*α, real-path harness null (C2).* Zero-cost random-entry strategies with fixed seeds and one fixed
exit rule, walked forward under wf-v2 on the real search-span M15 candles of BTCUSDT and ETHUSDT:
**1/200 seeds pass on BTCUSDT (0.005)** and **2/200 on ETHUSDT (0.010)**. This is a **harness
null, not the engine's fill path**: the entries are drawn by the seeded PRNG (one uniform per bar,
entry at the next bar's open when it falls below 1/96), the exit rule is fixed across every seed
and pair (stop `1.0·ATR(14)`, target `1.5·ATR(14)`, the stop taken first when one bar touches
both, the span end closing at that bar's close), ATR is the engine's own `Atr(14)` over the real
candles, and fees, slippage and funding are **zero**. It measures the rule's false-pass behaviour
on real price structure (fat tails, volatility clustering), not a strategy's fill path.
SOLUSDT and XRPUSDT are out: their round-1 archives have holes (w2's finding), and a gapped series
is refused by `load_series` at every entry point until w4's fill lands.

*The C1 test's power.* The pass rate of the C1 test for a planted +0.25R edge, σ = 1.2R, over the
fixed seed set 1..=1000:

| holdout trades | H = 12 | H = 6 |
|---|---|---|
| 100 | 312/1000 = 31.2% | 395/1000 = 39.5% |
| 150 | 455/1000 = 45.5% | 557/1000 = 55.7% |
| 160 | 486/1000 = 48.6% | 592/1000 = 59.2% |
| 200 | 628/1000 = 62.8% | 716/1000 = 71.6% |
| 250 | 734/1000 = 73.4% | 817/1000 = 81.7% |

At H = 12 the 50% crossing sits at about **160 holdout trades** (the threshold mean `z·1.2/√n`
equals +0.25R there). A campaign whose holdout yields fewer than ~160 trades therefore has a
**below-50% chance** of certifying a true +0.25R edge at H = 12, and a negative result is a
planned outcome, not a failure. H is the operator's to lower at the freeze (lower-only); this item
does not change it.

*Runtime.* One wf-v2 walk-forward over the full search span of BTCUSDT M15 + H4 (k = 6, `from`
defaulted to the first fully-warm bar 2021-01-01 03:45 UTC, `to` 2025-07-01, release build,
round-1 scratch store) took **1.34 s** wall (330 out-of-sample trades; 4 of 4 folds holding). The
campaign can iterate well inside its 7-day time-box.

**Flags carried forward (no parameter changed here).** The Q2 bound at H = 12 is 26.2%, above the
20% the original Q2 ruling named — but that bound belongs to the pre-C1 model (untuned candidates);
what actually gates a certification is the C1 test, whose family-wise split is fixed before any
candidate is seen. The power figure is the honest cost of that tightening. Both numbers are the
operator's to weigh at the freeze; H is never raised and never changed after a candidate is seen.

**Known limits.**

- **The operator's memory and the LLM's training knowledge of the 2025–26 market.** The holdout is
  data no *search run* sees; it is not data the humans or models involved have never seen.
- **Agent-scope paper reads (C6).** A paper session shows live bars after 2025-07, which fall
  inside the holdout. The campaign brief forbids outside market data and the campaign seat runs
  without web access; the limit is recorded here rather than claimed away.
- **The contiguity guard is whole-snapshot.** A gap anywhere in a snapshot refuses every entry
  point, not just windows that cross it (w2's finding on SOL/XRP).
- **The power figure itself.** At H = 12 and the campaign's plausible holdout trade counts the test
  is below 50% power against the planted edge; a negative result is expected roughly half the time
  for a genuine +0.25R strategy.
- **The CLI `pulse backtest --dsl` path is unguarded** (accepted at the round-2 plan gate). It opens
  no database by design (README C7), so it has no freeze record to read; the `--version` path is
  guarded. Recorded in w4's report §10.
- **The MCP parquet export is refused while a freeze is open** (accepted at the round-2 plan gate).
  A byte copy of the stored snapshot cannot be cut at the holdout start; the CSV export is cut and
  says so. Recorded in w4's report §10.
- **An errored `certify_version` call writes no certification record and spends no hypothesis.**
  A failure after the search step leaves its wf-v2 search run persisted and the version's
  `latest_walk_forward_run_id` pointing to it, exactly as a `run_walk_forward` call would.
  The token audit trail (grill Q5) sees the call, but the `certification` table has no row for it.
  Recorded at the close review (top amendment 3c) and PR #352 fix round 1.

## Consequences

The walk-forward now has two frozen rules with one persisted shape: `wf-v1` re-derives
byte-identically for every stored run, and `wf-v2` is a distinct name a run carries, a request can
select, and the save gate re-derives under the run's own rule — so a `wf-v2` draft cannot be
checked against `wf-v1` arithmetic, and a `wf-v1` draft is checked exactly as before. The holdout
test is a pure domain function, measured and pinned, so w5 can build the certification record on a
test whose false-positive and power behaviour is already known. The freeze is a lifecycle, not a
constant: the guard exists only while a freeze is open, the holdout start is read from the freeze
record, and a spent holdout is never reused. The campaign starts with the rule, the test, the
holdout start and the budget frozen in writing — and with its own power measured, so the operator
knows the pass chance before the first hypothesis is spent.
