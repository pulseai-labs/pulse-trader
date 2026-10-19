# 32. Certify power review: the pass-chance bar, the count-only holdout read and the certify gate

Date: 2026-10-19

## Status

Accepted. Registers the certify power review — the pass-chance bar, the count-only holdout read it is
computed from, the immutable record it writes, and the gate the certify step gains. Refines
[ADR-0028](0028-wf-v2-holdout-and-hypothesis-budget.md) (the C1 holdout test, `H`, the freeze lifecycle and
the campaign's known limits), [ADR-0010](0010-immutable-strategy-versions.md) /
[ADR-0018](0018-evolution-forward-only-migrations-immutable-snapshots.md) (immutable records, forward-only
migrations), [ADR-0015](0015-hexagonal-one-process.md) (a pure domain module, a port, a SQLite adapter) and
[ADR-0019](0019-stack-rust-core-embedded-sqlite-and-parquet.md) (records in SQLite, candles in Parquet), and
takes its operator-only surface from [ADR-0012](0012-agents-via-mcp-tools.md). Its rulings are PDR-0040 (1, 4,
5), PDR-0026, PDR-0033 and PDR-0007; it is owed by RELEASE r5 criterion 5.

## Context

The r5 campaign certifies **at most `H = 2` hypotheses** under freeze `30f98b21` (PDR-0026; ADR-0028), and each
attempt spends one of them on data no search run may ever see again. ADR-0028 chose the C1 test — the one-sided
lower confidence bound of the holdout expectancy strictly above zero, at a family-wise 5% split
`z = z(1 − 0.05/H)` — and **measured its power**: on the harness null, 312 of 1000 seeds pass at 100 holdout
trades and H = 12, 455 at 150, 486 at 160, 628 at 200, 734 at 250; at H = 6, 395 / 557 / 592 / 716 / 817. The
power table is the fact this ADR turns into a decision procedure: **a candidate whose holdout yields few trades
cannot pass C1 at any edge, and spending one of `H = 2` hypotheses on it is a certain waste.**

Two things the campaign lacked before this decision:

- a number the operator can see **before** an attempt: how many trades this candidate makes in the open freeze's
  holdout window, and the chance C1 passes a true `+0.25R` edge at that count;
- a bar, fixed **before any candidate is seen**, at which an attempt is allowed at all — so the campaign cannot
  talk itself into a low-power attempt after the fact.

The holdout fence (PDR-0007; ADR-0028) bounds the read: knowing *how many* trades a candidate would make is not
an outcome, and it is the minimum the decision needs. Nothing about how those trades went may be read.

## Decision

**The bar is fixed at plan time and moves only by a new decision.** `POWER_BAR = 0.50`, `ASSUMED_EDGE_R = 0.25`,
`MIN_HOLDOUT_TRADES = 20` are named constants in `src/domain/power_review.rs` (PDR-0040 ruling 1). **No request
field, CLI flag, MCP argument or environment variable changes them** (SPINE G4): they are the campaign's
pre-commitment, and a knob would be a way to loosen it after a candidate was seen. `0.25R` is ADR-0028's
reference edge, fixed before any candidate exists; the floor of 20 trades is `wf-v2`'s own per-fold minimum, the
count below which a single trade's R dominates the mean.

**The pass chance is the normal approximation of `P(C1 passes | n, H, μ, σ)`.**

```text
z   = c1_z(H) = holdout_test(&[Decimal::ZERO, Decimal::TWO], H).z     -- C1's own quantile
σ   = the candidate's search-span sample σ in R (Bessel N−1), pooled over every out-of-sample
      trade of the search walk-forward's folds
pass_chance(n, σ) = 1 − Φ(z − μ·√n / σ),  μ = ASSUMED_EDGE_R;  0.0 when n < 2 or σ is not > 0
```

`z` is **read from C1's own code** (`holdout_test`), never re-derived, so the review uses exactly the quantile
the certify step will use at this freeze's `H`. `Φ` is Abramowitz & Stegun **26.2.17** (a rational approximation
of `erf` with `|ε| < 7.5e-8`), implemented in the same module — no new crate, and no second normal-CDF to drift.
The hand check pins it: `n = 20`, `H = 2`, `σ = 1.2` → `0.1519` (z = 1.959964, μ√n/σ = 0.931695), and the same
function reproduces every ADR-0028 power cell within 0.03 at H = 12 and H = 6.

**σ is the candidate's own search-span σ, not a campaign constant.** A candidate that trades often has a tight
R-distribution and more power at the same count; one that trades rarely does not. Recording the candidate's own
σ in the search span — the only data the search has already seen — is what makes the number a statement about
*this* candidate rather than about the harness.

**The count is the one new holdout reader, and it returns the count and nothing else (G1).** The review reaches
its `n` **through the same code certify runs**: the freeze checks, the pair/timeframe resolution, the persisted
`wf-v2` search walk-forward, the recorded search inputs, `validate` + `compile`, and the holdout window and
series load — extracted into one shared function (`prepare_attempt` + `holdout_trades` in
`src/application/certification.rs`) that both callers use. The review uses `trades.len()`, the window end and the
recorded inputs; it never calls `holdout_test` on the holdout's trades and computes nothing else from them. The
new table, DTO, CLI print-out and every error carry the **count only** — no R, mean, bound, P&L, win rate, trade
time or verdict (SPINE G1). **`certify_version`'s outcome, refusals and record are unchanged by the
extraction** (G2).

**A review spends nothing and writes one immutable row; a refused review writes nothing (G3).** The read runs
under the same operator-only surfaces as `pulse certify freeze`: the `pulse certify power` CLI arm in this item,
and w3's app-scope route. **No power read and no holdout count reaches an agent over MCP** — no new tool, no
existing tool's description (ADR-0012; SPINE G6). The row is immutable in the 0020 pattern: `no_update` and
`no_delete` triggers, and `0022`'s down file refuses over any row rather than erasing the account of a read
(ADR-0018).

**The verdict is one ordered decision** (a pure function): the search span's `wf-v2` verdict fails →
`search_span_fails`; fewer than `MIN_HOLDOUT_TRADES` holdout trades → `too_few_holdout_trades`; pass chance
below `POWER_BAR` → `below_bar`; otherwise the attempt is allowed. The first failing reason is stored, so a
record says why it was not allowed rather than only that it was not.

**The gate (w2 implements it against this record).** `certify_version` spends nothing unless:

- the latest power review for `(version, pair, the open freeze)` has `allowed = true`; **and**
- that review's `holdout_inputs` equal the data versions the certify call would pin **now** — a review of a
  stale snapshot is not a review of the data the attempt would test.

Its refusal names the reason and **no number**: an agent learns that the attempt is not allowed, never the
holdout count, the pass chance or the bar (G1/G5/G6).

**The engine is untouched (G7).** `src/domain/power_review.rs` is deliberately outside the engine-fingerprint
roots (`build_support/engine_source_set.rs`); C1, `wf-v2`, `H`, the freeze tables and the fence rule are not
changed by this ADR, and no golden, pinned hash, paper-replay digest or certified-fixture fingerprint moves.

## Consequences

- **One new table, one new migration.** `0022_power_review` adds `power_review` (23 columns, four CHECK-held
  laws, the two immutability triggers, and an index on `(version_id, pair, freeze_id, created_at)` for the read
  the gate and the app both make). The embedded max moves 21 → 22. The down file restores the exact 0021 shape.
- **The touch surface** joins the bones registry at the r5.s5 close: `src/domain/power_review.rs`,
  `src/application/power_review.rs`, `src/adapters/db/power_review_repo.rs`, `migrations/0022_power_review.*`.
- **The most likely outcome is an honest negative result.** At `H = 2` the C1 quantile is `1.96` and the bar is a
  50% pass chance; a candidate needs roughly 90–100 holdout trades before a `+0.25R` edge clears it. If no
  candidate reaches the bar inside the time-box, r5 criterion 5 fails **on a written negative result**, and the
  gate is never loosened (SPINE Journey; PDR-0040 ruling 2).
- **Revisit when** a later freeze opens, `H` changes, or a candidate's trade rate makes the 50% bar reachable —
  any of the three makes this ADR's arithmetic the wrong arithmetic.
- **`H` and the fence are unchanged.** The review reads the freeze's `holdout_start_ms` and `h`; it does not open,
  close or alter anything about a freeze, and a refusal leaves the store exactly as it was.

## Sources

PDR-0040 (rulings 1, 4, 5); PDR-0026; PDR-0033; PDR-0007; ADR-0028 (C1, `H`, the power table and the campaign's
known limits); ADR-0010, ADR-0012, ADR-0015, ADR-0018, ADR-0019; RELEASE r5 criterion 5;
`docs/specs/r5/r5.s5-certify-power-review-and-one-certify-attempt-under-freeze-30f98b21/SPINE.md` (G1–G7 and the
risk posture); Abramowitz & Stegun, *Handbook of Mathematical Functions*, 26.2.17; r5.s5.w1.
