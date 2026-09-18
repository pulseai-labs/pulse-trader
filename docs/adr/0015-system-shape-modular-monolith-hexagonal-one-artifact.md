# 15. System shape: modular monolith, hexagonal, one shippable artifact, zero sidecars

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

PulseTrader must ship as an installable macOS app while running an LLM agent loop, a
deterministic backtest engine, and market-data ingestion. The obvious alternative was a
Rust core plus a Python agent sidecar, which the PulseHive decision eliminated.

## Decision

A **modular monolith** with **hexagonal (ports-and-adapters)** discipline: ports are
traits in the domain layer, adapters implement them, and dependency direction is
always inward.

**The invariant now holds for every external concern; the one named exception is
closed.** `src/domain/port.rs` defines `ExchangeAdapter`, `MarketDataSource`,
`StrategyRepository`, `BacktestRunRepository`, `LlmProvider`, `LlmCallRepository`,
`CoachingRepository` — and, since **r1.s3.w1 (#112)**, `CandleSeriesRepository`.
Candle storage previously had no port: the trait ADR-0002 named was never built and
the use cases imported and constructed the concrete `CandleStore` adapter directly,
which made the candle path the one place that did *not* satisfy this decision. It
now does. `CandleSeriesRepository` is a **deep** domain trait — three semantic
operations (`load_head`, `load_version`, `commit`), with the content hash, Parquet
codec, path layout and atomic-write internals left inside the adapter — and the
fetch, indicator and backtest use cases consume it generically.

**The composition-root qualification stands, and there are two roots.** "Only
composition roots choose a concrete adapter" is the rule, not "no module ever names
one". `src/cli/mod.rs` constructs `CandleStore` (from the default root or a
`--store` / `--base-dir` argument) and hands it to the use cases, exactly as it
constructs `BinanceDataSource` and the `sqlx` repositories. Since **r1.s3.w3**,
`src/tauri/commands.rs`'s `DesktopState` is the desktop's equivalent: it owns the
pool and the candle store for the app's lifetime and injects both into the shared
use case. Naming it here is compliance with this qualification, not a second
exception — a delivery adapter that resolved its own store inside each command would
be the violation. `tests/candle_repository.rs` is the standing guard: it scans the
three use-case modules for a concrete-store mention in code, and asserts the
deterministic engine names neither the adapter nor the port.

**Orchestration lives in an application ring, not in a delivery adapter (r1.s3.w3).**
`src/application/` holds use cases that are generic over the domain ports and names no
infrastructure adapter — no `tauri`, no `specta`, no `sqlx`, no filesystem type. The
one deliberate adapter import is the deterministic engine itself,
`crate::adapters::backtest`: it owns no I/O (its `adapters` address is namespace,
not infrastructure), and the ring's own module doc records that exception at the
import site. The version-id backtest flow lives
there because the debug CLI and the desktop command must run the *same* sequence —
its FR-7 compare-before-insert ordering is a correctness rule that only reads as one
if it exists once. The ring is also where the backtest-only posture is enforced
structurally: its dependency set is strategy, candle, exchange and run repositories,
with no order or broker capability reachable, which is what discharges the risk
gate's kill-switch control by construction rather than by a flag.

**The domain invariant is "zero I/O", not "zero dependencies"** — `src/domain/mod.rs`
states that policy explicitly. Domain types freely use `serde`, `rust_decimal`,
`chrono` and `thiserror`; `rust_decimal::Decimal` appears directly in port
signatures. What the domain must not do is perform I/O or reach an external system.
Recording the stronger dependency-free claim would make compliant changes look like
architecture violations. **One shippable artifact**, **zero sidecar
processes**.

**Agent orchestration is first-party and in-process; PulseHive is the transport.**
`src/agent/composer.rs` is the orchestrator — it drives the model through the builder
tools and finalizes the `StrategyVersion` itself. PulseHive is reached only through
the thin `LlmProvider` port (ADR-0012, ADR-0013); `src/adapters/llm/openai_compat.rs`
states the boundary explicitly: *"Thin transport ONLY: no `HiveMind`/agent/lens
substrate."* The zero-sidecar property comes from orchestrating in-process at all,
not from adopting PulseHive's agent substrate — which this baseline deliberately does
not use.

## Relationship to the ADR-0001 decision queue

`0001-record-architecture-decisions.md` carries a decision index that listed
**ADR-0002** (hexagonal), **ADR-0003** (Tauri shell, zero sidecars) and **ADR-0004**
(PulseHive as the in-Rust agent framework) as placeholders never written up. That
index is now reconciled and is authoritative: 0002 and 0004 are **superseded**, and
**only ADR-0003 remains `Proposed`**. This ADR is where two of the three land:

- **ADR-0002 (hexagonal)** — written up here. Superseded.
- **ADR-0003 (zero sidecars)** — the zero-sidecar half is written up here and
  superseded. The **Tauri desktop shell** half is *not*: it is unbuilt at this
  baseline. Its Proposed placeholder is **ADR-0003** in ADR-0001's queue, and it needs
  its own dedicated ADR when a slice builds it — **not** ADR-0019, which scopes itself
  to the exercised stack and expressly refuses ownership of the shell.
- **ADR-0004 (PulseHive as the agent framework)** — superseded by **ADR-0012** and
  **ADR-0013**, which chose the opposite of what the queue entry proposed: a thin
  PulseTrader-owned port over PulseHive rather than its agent substrate. The queue
  entry itself even records that alternative ("A2") as the one to revisit; it won.

The index in ADR-0001 is annotated accordingly.

## Consequences

Crate splits stay mechanical because ports live in the domain. No IPC, no process
supervision, no version skew between components. The cost is that everything shares one
address space and one release cadence: a component that genuinely needs independent
deployment would force a revisit, which is this bone's trigger.

With #112 closed the candle path no longer reads as an exception — a contributor can
take any use-case module as the compliant pattern. The one standing exception is the
application ring's deliberate import of the deterministic engine, recorded above and in
the ring's module doc: it is I/O-free and is not a precedent for importing an
infrastructure adapter from an inward layer. The candle path in particular gains a
substitutable seam: `r1.s3.w2` captures a backtest's
exact snapshot inputs through this port rather than threading provenance through a
concrete filesystem adapter, and a future replay or remote candle source becomes an
adapter swap instead of an edit to every consumer. The cost is one more trait to
keep **deep**: re-exposing `content_version`, `snapshot_path`, `write_head` or the
encode helpers on the port would recreate `CandleStore` as a shallow interface and
close nothing. Those stay inherent methods on the adapter, for its own tests and
tooling.

## Amendment — 2026-09-18 (r2.s1.w1, Proposed)

*(Authored `Proposed`; flips `Accepted` at `r2.s1` spine close.)*

**`pulse mcp` is a second long-lived OS process on the same artifact and the
same `pulse.db`.** An external coding agent drives the discovery loop over MCP
by running `pulse mcp` — a process the *agent* owns (spawned by its harness,
lived for its session), not one PulseTrader supervises. Both processes open the
same embedded database: WAL readers never block each other, writes serialize
through SQLite's single-writer discipline, and each composition root migrates
forward on open under ADR-0018's backup-before-migrate and refuses a schema
newer than itself. A second OS process sharing the file is the design WAL was
chosen for, not a breach of it.

**It is not a sidecar runtime, and it opens no listener.** The zero-sidecars
claim was always about PulseTrader not *supervising* a second runtime — no
spawned Python, no service mesh, no version skew to manage. `pulse mcp` is the
same binary on the same release cadence, speaking stdio framing to its parent
agent: there is no socket, no port, no IPC substrate PulseTrader must keep
alive or version against. It is a second *entry point*, not a second
*component*.

**`src/mcp/` is a third composition root beside `src/cli/` and `src/tauri/`.**
The composition-root rule — only roots choose concrete adapters — now closes
over three roots: the CLI, the desktop shell, and the MCP surface. `src/mcp/`
constructs the same domain ports (`SqliteStrategyRepo` et al.) the other two
do and consumes the same application-ring use cases; it introduces no fourth
kind of adapter and no new dependency direction. **The application ring stays
infrastructure-free** — MCP transport types (framing, JSON-RPC envelopes) live
in the root, never in `src/application/` or `src/domain/`.

**The revisit trigger is exercised, not violated.** The Consequences entry
names this bone's trigger: "a component that genuinely needs independent
deployment would force a revisit". `pulse mcp` is not that component — it ships
inside the one artifact, upgrades on the same cadence, and cannot skew against
itself. What changed is the *process* count, which the decision never counted;
what did not change is the *deployment* count, which is what the trigger
measures. Recorded here so a future reader does not mistake a second `main`
for a sidecar: the monolith gained a door, not a satellite.
