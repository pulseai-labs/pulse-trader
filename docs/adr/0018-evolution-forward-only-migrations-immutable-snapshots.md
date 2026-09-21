# 18. Evolution: forward-only migrations on the startup path, immutable content-addressed snapshots, immutable StrategyVersion

Date: 2026-08-23T00:00:00Z

## Status

Accepted

(Accepted at adoption. This decision was made and exercised under the scaffold-dev
stack across sprints 1.1-1.3; `/ossify:adopt` recorded it as a bone on 2026-08-23
against baseline `49f229a`. Per ossify's bones protocol, decisions the adopted
baseline already exercises are minted `Accepted`, not `Proposed` — the baseline
*is* the release that exercised them. Retrospective record: it documents a
standing decision, it does not introduce one.)

## Context

Backtest reproducibility is the product's core claim: a result must be re-derivable
from the same inputs. That is incompatible with mutable history.

## Decision

Schema evolves by **forward-only sqlx migrations on the normal startup path**. Candle
snapshots are **immutable and content-addressed** by `data_version` (ADR-0009).
`StrategyVersion` is **immutable** with provenance (ADR-0010). Correction is a new
version, never an edit.

**Runs record their inputs, not only their outputs (r1.s3.w2, #110).** Immutable
snapshots are only half of re-derivability; the other half is knowing *which* snapshot
a stored result came from. Until `0006`, `backtest_run` persisted `engine_fingerprint`,
`engine_target`, `result_content_hash` and the money totals but nothing about the data
— while the CLI loaded the `HEAD` snapshot and `fetch-data` advanced `HEAD`. Once
`HEAD` moved the old Parquet files might still exist, and nothing in the row said which
of them produced it: the reproducibility claim rested on a link that was not stored.
Migration `0006` adds eight columns — pair, primary and optional HTF
`timeframe`+`data_version`, taker fee and slippage bps, and the funding discriminant —
and `BacktestRunRepository::save_run` requires a typed `inputs` parameter, so a fresh
run with no provenance is unrepresentable rather than merely rejected. The identities
are captured from the `CandleSeries` values the engine actually consumed, never from a
second `HEAD` read, which would record what is current rather than what ran.

**Old rows stay honestly unavailable rather than plausibly wrong.** The eight columns
are nullable, because a row written before `0006` cannot be backfilled truthfully —
nothing stored recovers the snapshot identity it used — and this bone forbids rewriting
immutable records with invented facts. Those rows read back as `inputs: None`, an
explicit "provenance unavailable"; the debug CLI says so in words. A `BEFORE INSERT`
completeness trigger, installed after the columns exist, holds the line for every FRESH
row without touching a single existing one, and a partially-populated row is a read
error rather than a partially-trusted projection. #110 closes when `r1.s3` reaches
`main`; the capability lands here.

**There is one destructive exception and it is currently uncontrolled.** The tree ships
four `*.down.sql` migrations and a publicly exported `undo_to(pool, target_version)`
(`src/adapters/db/migrate.rs`). Calling it applies those down migrations, dropping the
strategy, backtest, trade and LLM-call tables — no backup, no confirmation. No shipped
CLI verb reaches it, so the exception is library-surface only; but "forward-only"
describes the startup path, **not the crate's public API**. Restricting or gating
`undo_to` is open work, not a decided part of this bone.

## Fingerprint inputs

The `engine_fingerprint` baked into a build (D5) is a sha2-256 over, in fold
order: **(a)** the raw bytes of `Cargo.lock` (the full resolved dependency
graph); **(b)** the resolved `rustc -vV` filtered to its `release:` +
`commit-hash:` lines; **(c)** the DSL schema-version string via the
`schema_version_const.rs` seam; **(d)** a sha2-256 over the engine source set —
the `.rs` files under `src/domain/backtest/`, `src/adapters/backtest/`,
`src/adapters/indicators/` and `src/domain/dsl/`, plus `src/domain/indicator.rs`,
`src/domain/series.rs` and `src/domain/candle.rs`, sorted by path bytes and
folded behind the `b"engine-source-v1\0"` domain prefix (r2.s3.w1, #155);
**(e)** the full target triple. `build.rs` emits `cargo:rerun-if-changed` for
every hashed file and every root directory, so an added or removed source file
rebuilds just as an edited one does.

Input (d) is the amendment this section exists for: before it, two builds that
differed in engine code produced the same fingerprint while claiming to differ.
Stored `backtest_run.engine_fingerprint` values are never rewritten — every run
recorded before (d) existed mismatches builds that carry it exactly once, and
both comparison sites treat a mismatch as they did before: the standalone path
warns, the coach's accept path refuses.

## Consequences

Reproducibility holds by construction **on the normal startup path**, and there the
determinism fingerprint means something — and since `0006` it means more than it did:
a stored run now names the exact immutable snapshots it consumed, so it stays
re-derivable after `fetch-data` advances `HEAD`, which is the case that previously
broke the claim silently. The cost is a permanent two-tier read: every consumer of a
persisted run has to handle `inputs: None`, and will for as long as pre-`0006` rows
exist. That is the honest shape — the alternative was a backfill that would have made
every legacy row *look* re-derivable while pointing at a snapshot nobody can prove it
used. It does **not** survive the `undo_to`
exception above: a library caller that runs the down migrations destroys the persisted
inputs a result would be re-derived from, so the guarantee is scoped to a database that
has only ever migrated forward. The cost of the immutability discipline is storage
growth and no cheap way to fix a bad row — a destructive or non-forward migration on
the startup path would break the claim outright, which is why it is the revisit trigger
rather than a routine option, and why gating `undo_to` is open work.
