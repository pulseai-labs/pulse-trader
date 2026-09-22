# 25. Walk-forward as a run kind and the certification seam

Date: 2026-09-20T00:00:00Z

## Status

Accepted

## Context

Strategy acceptance has so far been inferred from a single backtest — one run
over one span, judged by eye. That answer is not reproducible as a claim: the
verdict was never persisted, the span it covered was whatever the caller
happened to ask for, and nothing distinguished "the strategy is warm and
counting" from "the strategy's indicators are still filling". Walk-forward
evaluation is the spine's answer: a strategy version is cut into K contiguous
out-of-sample folds, each fold is an ordinary windowed backtest with
full-history lead-in, and a versioned rule decides pass/fail from the fold
verdicts plus one pooled bound. Making it a **run kind** — persisted, hashed,
auditable — rather than a report computed on demand is what lets the next item
(w4) hang certification off it without inventing a second persistence story.

## Decision

**Walk-forward is a persisted run kind.** `walk_forward_run` is the parent
row: strategy version, injected-clock timestamp, the scheme name
`rolling-oos/v1`, the rule name `wf-v1`, the fold count `k` (2..=12, default
6), the counted span `[span_from_ms, span_to_ms)`, a `from_defaulted` flag,
the shared engine fingerprint, and the recorded verdict columns
(`folds_holding`, `folds_required`, `pooled_n`, `pooled_mean_r`,
`pooled_lower_bound`, `pass`). Each `walk_forward_fold` row names its counted
window and the ordinary `backtest_run` it ran as — fold runs are REAL runs:
own window pair (0009), own recorded lead-in start (0012), own trades,
summary and content hash, listed and read by the existing run surfaces.
Membership is recorded both ways (`backtest_run.walk_forward_run_id` +
`fold_index`, set together or not at all under a pair trigger), so a run's
provenance says which walk-forward it belongs to. Parent, fold rows and the
K fold runs persist in ONE transaction; both tables are immutable
(no-update/no-delete triggers); the down migration refuses rather than
falsify any of the three states 0012 cannot express.

**`wf-v1` is the verdict rule, and the constants are the rule.** A fold holds
when `n >= 20` and its one-sided expectancy lower bound
`mean − 1.645·sqrt(var/n)` is strictly above zero — mean and sample variance
(Bessel N−1) accumulated in `Decimal`, one `var/n` conversion to `f64`, one
`sqrt` (the `stats.rs` discipline). The run passes when at least `⌈2K/3⌉`
folds hold AND the pooled bound over every out-of-sample trade (concatenated
in fold order) holds. Changing `1.645`, `20`, `⌈2K/3⌉`, or the fold-cut is a
new rule/scheme name (`wf-v2`, `rolling-oos/v2`), not an edit of the values
persisted rows were judged under.

**The counted span starts where the strategy can count.** `from` defaults to
the first bar at which the entry warm gate is fully satisfied — the same gate
the engine applies, computed by `first_fully_warm_bar_ms` stepping the same
indicator engines over the same loaded series. An explicit `from` earlier
than that refuses (`FromBeforeWarm`, naming the earliest allowed); a snapshot
that never warms refuses (`NeverWarm`); a fold with no counted candle refuses
before any fold runs and before any row persists. `to` defaults to the
snapshot end. All folds run inside one blocking task over one load of the
pinned snapshots; two cold runs over identical inputs persist byte-identical
fold results.

**The certification seam is decided here and implemented in w4.** The run
kind this ADR creates is the acceptance gate: w4 adds
`strategy_version.latest_walk_forward_run_id`, derives `certified` from the
latest persisted verdict's `pass`, and de-certifies on new edits — plus the
coach accept-path guard. None of that lands in this item: no MCP/Tauri
command, no Library badge, no `strategy_version` column, no coach change.

**Revisit triggers:** a second fold scheme (in-sample segments, anchored or
overlapping windows); a verdict rule that cannot be expressed as
fold-level `n`/`mean`/`lower_bound` tallies plus one pooled bound; or a
certification consumer needing per-fold artifacts beyond what the persisted
run already records.

## Consequences

Acceptance becomes a persisted, replayable claim: the verdict on a strategy
version is a row naming the scheme, the rule and the span, judged from runs
that are themselves first-class and inspectable — not a recomputation a
caller must trust. Determinism is structural: the fold cut is integer
arithmetic, the verdict math is `Decimal` up to one pinned `f64` bound, and
the two-cold-runs oracle proves the persisted artifacts agree to the bit.
The ordinary run surfaces stay honest — fold runs carry their membership, so
listing and reading a fold names its parent — and the schema fails closed:
the pair trigger refuses half-set membership, the fold trigger refuses an
unwindowed run, and the down migration refuses every state that would force
it to falsify a record. w4 inherits a seam, not an expedition: it reads the
latest `walk_forward_run.verdict.pass` and never re-runs anything.
