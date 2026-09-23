# 10. StrategyVersion identity, immutability & provenance storage

Date: 2026-06-15T00:00:00Z

## Status

Accepted

(Reuses ADR-0009's length-prefixed SHA-256 discipline for a per-version integrity hash. Records the `StrategyVersion` storage contract first implemented in VS-1.1.4 — the SQLite persistence slice. Consolidated at slice close per grill gate-2 Q5; the slice-close architect-critic, close depth, added the schema-invariant-placement and JSON→junction-cost decisions below.)

## Context

VS-1.1.4 is the first time PulseTrader writes durable strategy state. It persists two entities into the SQLite tier (`~/Library/Application Support/PulseTrader/pulse.db`, WAL): the mutable **`Strategy`** (name/tags/owner/pin/archive) and the **immutable `StrategyVersion`** — a DSL snapshot in a `parent_version_id` tree that the backtester, paper, and live layers will all read. Because the DB is the **real-money system-of-record**, three storage questions needed a recorded decision rather than an implicit one:

1. **Identity** — what uniquely names a version, and is an identical DSL the *same* version or a new one?
2. **Immutability** — how is "a written version can never be mutated" enforced, and against what?
3. **Provenance** — how are the LLM calls that produced a version recorded before an `LLMCall` entity exists?

These were resolved during the slice's gate-2 grill (Q2/Q3/Q5) and gate-7 audit (C6), then the slice-close adversarial review (close depth, claude + Codex fresh-frame) surfaced two further decisions about where invariants should live and what the eventual provenance-normalization migration costs.

## Decision

**(1) Identity — UUID primary key + a separate `version_hash` content-identity column.**

- The `strategy_version.id` PRIMARY KEY is a **UUID** (hyphenated `TEXT`, adapter-minted; the domain stays uuid-free). Keys are UUIDs across the system.
- `version_hash` is a **separate, non-`UNIQUE`** column carrying a length-prefixed SHA-256 over a fixed-order canonical feed of `strategy_id`, `parent_version_id` (1-byte present/absent tag + value), `dsl_schema_version`, and the verbatim `dsl_original` bytes, emitted as **full 64-char** lowercase hex (the integrity field keeps the whole digest — unlike ADR-0009's `data_version`, which truncates to 16 for a readable file name). It is re-derived on every read and a mismatch is rejected (`DataError::Db`), mirroring `store/mod.rs`'s content-version-mismatch defense.
- A `StrategyVersion` is a **provenance event**, so two creations of an identical `dsl_original` are **two distinct rows** (two UUIDs, same hash) — `version_hash` is therefore explicitly **not** a dedup key (gate-2 Q2).
- `version_hash` is **position-scoped** (it folds in `strategy_id` + `parent`), so the same DSL under two strategies hashes differently. It is a per-version identity/integrity field, **not** a cross-strategy "same logic?" content key (gate-7 C6). A separate pure-`dsl_original` content hash can be added later if cross-strategy dedup-detection is wanted.

**(2) Immutability — dual-guarded, archive-only.**

- **DB triggers:** `BEFORE UPDATE` and `BEFORE DELETE` on `strategy_version` each `RAISE(ABORT, 'strategy_version is immutable')` (SQLite needs the two separate triggers).
- **API shape:** the `StrategyRepository` port exposes versions as **create + read only** — there is no `update_version`/`delete_version` anywhere (immutability is structural in the type, not just enforced at runtime).
- **Strategies** are mutable meta (rename/tags/owner/pin/archive) but **archive-only — there is no hard delete** (`archive_strategy(bool)`), so a strategy's version history is never orphaned.
- `pinned_version_id` is a **nullable circular FK** (`strategy → strategy_version → strategy`) resolved lazily under `foreign_keys=ON` (SQLite checks FK targets at statement end); it starts NULL and `set_pinned_version` validates ownership in a transaction (gate-2 Q3).

**(3) Provenance — denormalized JSON now; normalized later is data-bearing.**

- `creating_llm_call_ids` is stored as a **denormalized JSON-array `TEXT`** column. There is no `LLMCall` table this slice, so there is no referential integrity to enforce yet; v1 is LLM-free at the FR-11 CLI surface (the array is always `[]` there).
- When the `LLMCall` entity lands, the cutover to a junction table (`strategy_version_llm_call`) is a **data-bearing migration**, not an additive schema change — it must parse every stored JSON array and back-fill rows. The cutover trigger is "the first slice that persists `LLMCall`s"; the cost is a one-time data migration + a backup-before-migrate run, which the VS-1.1.4 migration protocol already provides.

**(4) Defense-in-depth invariants belong in the system-of-record.**

The slice already trusts SQL for the load-bearing immutability guarantee (triggers). The recommended hardening direction is to push the *remaining* invariants into the schema rather than relying on the Rust adapter's happy path: `CHECK(archived IN (0,1))`, `json_valid(tags)` / `json_valid(creating_llm_call_ids)`, a `version_hash` length/hex check, and a `parent_version_id ∈ same strategy` guard (today the FK only proves the parent *exists*, not that it belongs to the strategy). This is recorded as a direction, not yet implemented; it is tracked in #39 (persistence integrity hardening).

## Consequences

**Positive**

- **Byte-identical reload is provable.** `dsl_original` is stored verbatim and re-derived through VS-1.1.2's `Migrator::load` on read; the round-trip `==` plus the re-derived `version_hash` match are the NFR-2 demo and the read-time tamper defense.
- **Immutability is defense-in-depth.** A bug or a raw SQL write that bypasses the API still hits the trigger; an API with no mutator can't express a mutation in the first place. The slice-close manual demo confirmed a raw `UPDATE`/`DELETE` is rejected with `strategy_version is immutable`.
- **Provenance is forward-compatible without blocking on `LLMCall`.** The JSON column records call ids today and migrates cleanly when the entity exists.
- **Identity reuses a proven scheme** (ADR-0009's length-prefixed feed), so there is one hashing discipline across the codebase.

**Negative / limitations (tracked)**

- **`version_hash` does not authenticate the executed `.dsl`.** The hash is over `dsl_original`; the executed `.dsl` is re-derived through a loader whose read structs lack `deny_unknown_fields` (#17), so an unknown *field* in a stored `dsl_original` is silently dropped from `.dsl` while the hash still matches — a source-vs-executed divergence the integrity defense does not catch. Elevated at the persistence seam; fix is #17.
- **The denormalized provenance JSON has no referential integrity** until the junction-table cutover (a data-bearing migration, above).
- **Schema invariants are mostly Rust-enforced**, not DB-enforced (#39); a non-API writer could insert rows the adapter would never produce.
- **Single-process assumption.** The persistence + migration design assumes one process; there is no migration lock or cross-process write coordination (#38, relates #7). Safe for the v1 CLI; load-bearing when v2 paper/live runs concurrent processes.

## Alternatives considered

- **`version_hash` as the primary key (content-addressed versions)** — rejected: a `StrategyVersion` is a provenance event, so two identical-DSL creations must be two distinct rows; a content-addressed PK would collapse them and lose provenance.
- **A pure-`dsl_original` content hash (cross-strategy dedup key)** — deferred: not needed for identity/integrity; add a separate column if/when a coach "you've seen this idea before" feature wants cross-strategy dedup. Overloading `version_hash` for both would conflate identity with dedup.
- **A normalized `LLMCall` table + junction now** — rejected for this slice: there is no `LLMCall` entity yet, so it would be a speculative schema with no producer. Denormalized JSON now + a recorded data-bearing cutover is the YAGNI-respecting path.
- **Hard delete of strategies/versions** — rejected: archive-only preserves the immutable version tree and the audit trail the system-of-record exists to keep.
- **All invariants in Rust only** — rejected as the *target* state (kept as the v1 *implementation* for speed): for a real-money system-of-record, critical invariants should be carried by the database so corruption is rejected at write regardless of the writer (#39).

## Amendment — 2026-09-18 (r2.s1.w1, Accepted)

*(Authored `Proposed` by `r2.s1.w1`; accepted at the Release 2 close (tag `r2`,
`d4eed261`), the release that shipped `r2.s1` through its merged PR #178. The
amendment's implementation is recorded with its shipped evidence:
`migrations/0009_external_agent_window_claim` and `tests/migration_0009.rs`.)*

**Provenance gains a sixth kind: `external_agent`.** `CreatedBy` closes over
`human`, `composer_llm`, `coach_llm`, `auto_optimizer`, `migration`, and now
`external_agent` — a version submitted by an agent running OUTSIDE this process
(the `pulse mcp` surface recorded in ADR-0015's same-day amendment). The stored
token is `"external_agent"`, joining the same serde vocabulary the other five
occupy.

**Its ledger is `agent_submission`, normalized from the start.** Decision 3's
"denormalized now, normalized later" bet is not repeated here: where
`creating_llm_call_ids` is a JSON array because `LlmCall` did not yet exist, the
agent-submission shape is known whole on day one, so it lands as a real table —
`id` PK, `version_id` `UNIQUE REFERENCES strategy_version(id)` (exactly one
submission per version), `agent_name` and `hypothesis` (`CHECK`-bounded text),
`created_at`. Two triggers make it append-only (`BEFORE UPDATE` / `BEFORE
DELETE` each `RAISE(ABORT)`), and a third (`agent_submission_version_kind`)
refuses a submission whose referenced version is not `created_by =
'external_agent'` — the schema itself enforces the one-writer rule #39 asks the
system-of-record to carry.

**No `LlmCall` is written, and `creating_llm_call_ids` is `[]` for this kind.**
An `LlmCall` row records an in-app provider call and its cost; an external
agent's cost is external to the app — no call this process made, no tokens it
billed, nothing the ledger can honestly name. The port refuses a non-empty
`creating_llm_call_ids` on an `external_agent` version BEFORE touching the
database (`SqliteStrategyRepo::create_agent_version`), so the denormalized
column's contract — "ids of calls this app made" — is never lied to. Where a
coach version's audit trail is `coaching_sessions` + `llm_call`, an external
version's is `agent_submission` alone.

**Touch surface unchanged.** `StrategyVersion`'s identity, immutability, and
read-back rules are untouched: the version row is written by the existing
`insert_version_row`, defended by the same triggers, and re-derived through the
same `version_hash` check on read. `create_agent_version` is a second writer on
the SAME contract — version and submission in one `BEGIN IMMEDIATE`
transaction, both rows or neither — not a new shape of version.
