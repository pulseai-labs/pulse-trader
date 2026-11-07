# PulseTrader — Architecture Overview

> High-level guide to the code in this repository. Describes what exists today;
> it is not a roadmap.

---

## Repository layout

```
pulse-trader/
├── src/
│   ├── domain/        # Pure domain — types, ports, value crates only
│   ├── adapters/      # Binance, SQLite, Parquet, indicator implementations
│   ├── application/   # Use cases shared by every surface, over the domain ports
│   ├── agent/         # Composer and coach model halves, builder tools (PulseHive-backed)
│   ├── cli/           # CLI surface — every `pulse` subcommand
│   ├── server/        # `pulse serve` — the axum server: /api/v1 routes, /mcp, the paper runtime
│   ├── client/        # Thin HTTP client the desktop bus and the `mcp` relay speak through
│   ├── mcp/           # `pulse mcp` — the MCP tools, resources, exports, and the HTTP relay
│   ├── tauri/         # The desktop command bus and its typed event channel (Tauri 2)
│   └── entry.rs       # argv dispatch — no arguments opens the app, any argument runs the CLI
├── ui/                # The desktop shell's React frontend
├── docs/adr/          # Architecture Decision Records
├── migrations/        # sqlx migration files for the SQLite schema
├── tests/             # Integration tests
└── rust-toolchain.toml
```

---

## Hexagonal (ports-and-adapters) layers

PulseTrader follows a strict hexagonal architecture. Dependency direction is
always inward — outer layers depend on inner ones, never the reverse.

### `domain/` — the pure core

Contains all business types (`Candle`, `Pair`, `Timeframe`, `Strategy`,
`StrategyVersion`, `CandleSeries`, indicator traits) and the port traits that
define every external concern:

| Port trait | Purpose |
|---|---|
| `ExchangeAdapter` | Fetch historical candles + live feeds |
| `LlmProvider` | Issue prompts, receive structured responses |
| `StrategyRepository` | Persist and load `Strategy` / `StrategyVersion` |
| `CandleSeriesRepository` | Load the `HEAD` or an exact `data_version` candle snapshot; commit a new one |
| `MarketDataSource` | Abstraction over bulk, REST, and WebSocket data |
| `Clock` | Mockable wall time |
| `Indicator` | Streaming indicator computation |

The domain module depends only on value crates — `rust_decimal`, `serde`,
`serde_json`, `chrono`, `thiserror`, `sha2` — and on no I/O crates (no
sqlx, no reqwest, no Polars). All computation inside `domain/` is
deterministic by construction.

### `adapters/` — concrete implementations

| Sub-module | Implements |
|---|---|
| `adapters/binance/` | `ExchangeAdapter` + `MarketDataSource` via Binance bulk dumps, REST, and WebSocket |
| `adapters/db/` | `StrategyRepository` using sqlx + SQLite WAL |
| `adapters/store/` | `CandleSeriesRepository` using Polars + immutable Parquet snapshots |
| `adapters/indicators/` | EMA, RSI, MACD, ATR, Bollinger streaming implementations |

### `agent/` — agent orchestration

Wraps PulseHive (the author-owned in-Rust multi-agent SDK) to run the
model-facing halves in-process: the Composer loop with its six validated
builder tools (`agent/composer.rs`, `agent/tools.rs`) and the coach's
prompt-and-parse half (`agent/coach.rs`). The coach turn sequence itself —
one call, one validated mutation, one durable session — lives in
`application/coach.rs` (ADR-0021). No Python layer; no sidecar process.

### `cli/` — command-line surface

The `pulse` binary's argument surface. Its `clap` subcommands: data
(`fetch-data`, `indicators`, `import`, `backup`, `backup-verify`,
`restore`, `fixture`), strategies and runs (`strategy`, `backtest`,
`runs`), the agent loop (`compose`, `coach`, `llm-check`), the server and
its administration (`serve`, `mcp`, `token`, `certify`, `watch`), plus one
hidden diagnostic (`keychain-probe`). Each subcommand is a composition
root wiring adapters around the application use cases.

### `tauri/` — the desktop command bus

The implemented Tauri command bus: `commands.rs` (the one append-only
registration list, `BUS_COMMANDS`) plus the per-screen read contracts
`library.rs`, `backtest.rs`, `chart.rs`, `coach.rs`, `paper.rs`,
`certification.rs`, `power_review.rs`, `marks.rs`, `walk_forward.rs` and
`streak_card.rs`, with `events.rs` (the typed event channel) and `error.rs`.
Since ADR-0026 ([`0026-client-server-split.md`](docs/adr/0026-client-server-split.md))
the shell is a thin HTTP client of `pulse serve`: the app manages a
`ClientState` (`src/tauri/mod.rs`), and the bus's commands hold
`tauri::State<'_, ClientState>` and reach the server through `src/client/`.

---

## Storage tiers

Four storage tiers, no Postgres, no Redis — and one long-running server
process: `pulse serve` is an always-on axum server that owns the database,
the candle store and the paper runtime (`src/server/mod.rs`).

### 1. SQLite (WAL) — entity store and event log

- Path: `~/Library/Application Support/PulseTrader/pulse.db`
- Managed by sqlx with compile-time-checked queries and `sqlx migrate`.
- Exact version pin: `sqlx = "=0.8.6"` (determinism — see the decision queue
  in [ADR-0001](docs/adr/0001-record-architecture-decisions.md)).
- `StrategyVersion` rows are **immutable by DB trigger** (`BEFORE UPDATE` /
  `BEFORE DELETE` each `RAISE(ABORT, …)`) — see [ADR-0010](docs/adr/0010-strategyversion-identity-immutability-provenance-storage.md).
- Startup protocol: if a schema migration would alter a table that already
  holds rows, the app backs up the database first and refuses to start if the
  backup fails.

### 2. Parquet candle snapshots — immutable time-series store

- Path: `<base>/candles/<PAIR>/<TF>/<data_version>.parquet`
- Each snapshot is **write-once, content-addressed**. `data_version` is a
  truncated SHA-256 over a canonical encoding of `(pair, timeframe,
  CANDLE_SCHEMA_VERSION, candles)` — see [ADR-0009](docs/adr/0009-data-version-content-hash-scheme.md).
- Read via Polars / arrow-rs. Snapshots are never overwritten; a new content
  hash means a new file.

### 3. macOS Keychain — secrets

API keys and other secrets are stored in and retrieved from the macOS Keychain
only. They are never written to disk in plaintext.

### 4. Filesystem — logs and exports

Structured logs and optional verbatim LLM-call archives are written to the
filesystem under the application support directory. These are auxiliary;
the SQLite DB is the system-of-record for all entities.

---

## Determinism stance

Byte-reproducible backtests are a load-bearing requirement (NFR-2). Three
mechanisms enforce it:

1. **FMA / fast-math disabled (floating-point portability contract, D2).**
   The f64 math paths (indicator / regime / backtest) are restricted to the
   IEEE-754 operations the standard guarantees are *correctly-rounded* —
   `+  -  *  /` and `sqrt` — which are therefore bit-identical on any compliant
   target (aarch64 and x86\_64 alike). Two operation classes are **banned**
   because they break that guarantee:
   - **`mul_add`** — contracts to a fused multiply-add, which rounds once
     instead of twice and so yields a different last bit.
   - **Transcendentals** (`exp` / `ln` / `log` / `log10` / `log2` / `ln_1p` /
     `exp2` / `exp_m1` / `powf` / `powi` / `sin` / `cos` / `tan` / `sin_cos` /
     `sinh` / `cosh` / `tanh` / `asin` / `acos` / `atan` / `atan2` / `cbrt` /
     `hypot`) — not standardized to the last ulp across libm implementations, so
     their results can differ between architectures. (This 23-name list plus
     `mul_add` is the 24-token `BANNED_FP_CALLS` set the determinism guard
     enforces — widened from the original 9 in VS-1.2.4 work-4.02, #70.)

   `sqrt` is **allowed** (correctly-rounded; it is the single transcendental
   permitted, and as of VS-1.2.4 work-4.02 the `SummaryStats` Sharpe/Sortino
   stddev consumes it). The x87 80-bit excess-precision hazard is a non-issue:
   `x86_64` uses SSE2 scalar f64 by default, matching aarch64's IEEE-754
   binary64.

   **Stable-Rust reality (why this is enforced by absence, not a flag).**
   Stable Rust/LLVM does *not* contract `a*b + c` into an FMA unless
   `f64::mul_add` is called explicitly, and stable Rust exposes *no*
   `-ffast-math`-equivalent / fp-contraction flag. So the real levers are:
   (i) never call `mul_add` in the math paths, (ii) never add a
   `target-feature` / `rustflag` that enables fp-contraction or fast-math, and
   (iii) document + guard the above. `.cargo/config.toml` carries the contract
   as a *documented guard surface*, not a magic flag — it deliberately adds no
   `[build] rustflags`.

   **Guard test (advisory tripwire) vs. authoritative gate.**
   `tests/determinism_guard.rs` scans those f64 math source files for the
   banned call-forms (matched word-boundaried and call-form anchored — method,
   path/free-function, and bare forms — so `f64::exp(x)` and `libm::exp(x)`
   are caught while `explain` / `println` are not) **and** for shared-mutable /
   interior-mutable state (`static mut`, `thread_rng`, `lazy_static`,
   `OnceCell` / `OnceLock`, `Mutex` / `RwLock`, `RefCell` / `Cell<`) that would
   make `run_backtest` non-reentrant and flake the parallel arm. This guard is
   an **advisory** pre-filter only: a source scan proves a token is *absent*,
   never that an f64 result is bit-identical. The **authoritative** determinism
   gate is the 100×-both-arches cross-arch CI compare (mechanism #3 below).

   *Resolves issue #29* ("VS-1.1.3 determinism is single-process only —
   establish FMA-off + cross-arch determinism gate"): this FMA-off contract +
   the tracked guard test close the FMA-off / guard half of #29; the cross-arch
   CI matrix half is delivered alongside mechanism #3.

2. **Pinned toolchain.** `rust-toolchain.toml` pins an exact Rust version
   (`1.98.0`), not a floating `stable` channel. The toolchain version feeds
   `engine_fingerprint`. See the comment in `rust-toolchain.toml` — bumps are
   deliberate and ADR-recorded ([ADR-0022](docs/adr/0022-toolchain-bump-1-98.md)
   is the most recent one, `1.92.0` → `1.98.0`).

3. **`engine_fingerprint`.** Every `BacktestRun` record stores a hash of
   (crate versions + Rust toolchain + DSL schema version + target architecture
   triple). CI proves it cross-arch: the `determinism (x86_64)`,
   `determinism (arm64)` and `determinism (arm64-darwin)` jobs each run the
   same strategy 100× sequentially and 100× in parallel in a release build
   and emit one canonical content hash; the `determinism cross-arch compare`
   job then byte-compares the three (`.github/workflows/ci.yml`).

Key dependency pins that protect reproducibility:
- `ta = "=0.5.0"` — indicator library; an upgrade can shift low-decimal outputs.
- `sqlx = "=0.8.6"` — pins the compile-time query cache format.

---

## Architecture Decision Records

All load-bearing architectural decisions are recorded in
[`docs/adr/`](docs/adr/) — every file there, addenda included:

| ADR | Title |
|---|---|
| [0001](docs/adr/0001-record-architecture-decisions.md) | Record architecture decisions |
| [0009](docs/adr/0009-data-version-content-hash-scheme.md) | `data_version` content-hash scheme for candle snapshots |
| [0010](docs/adr/0010-strategyversion-identity-immutability-provenance-storage.md) | StrategyVersion identity, immutability & provenance storage |
| [0011](docs/adr/0011-pulsehive-dual-license-commercial-self-grant.md) | PulseHive dual-license & commercial self-grant for PulseTrader linkage |
| [0012](docs/adr/0012-thin-llmprovider-port-over-pulsehive.md) | Thin PulseTrader-owned LlmProvider port over PulseHive |
| [0013](docs/adr/0013-extend-thin-llmprovider-port-with-tools-over-hivemind.md) | Extend the thin LlmProvider port with tool-calling over PulseHive HiveMind |
| [0014](docs/adr/0014-posture-source-available-with-private-data-overlay.md) | Posture: source-available canonical with a private data-overlay moat |
| [0015](docs/adr/0015-system-shape-modular-monolith-hexagonal-one-artifact.md) | System shape: modular monolith, hexagonal, one shippable artifact, zero sidecars |
| [0016](docs/adr/0016-trust-boundaries-keychain-secrets-redact-before-persist.md) | Trust boundaries: secrets never committed, redact-before-persist, no order-placing path in v1 |
| [0017](docs/adr/0017-failure-visibility-mixed-error-preservation-at-the-cli-edge.md) | Failure visibility: mixed error preservation at the CLI edge; structured tracing deferred |
| [0018](docs/adr/0018-evolution-forward-only-migrations-immutable-snapshots.md) | Evolution: forward-only migrations on the startup path, immutable content-addressed snapshots, immutable StrategyVersion |
| [0019](docs/adr/0019-stack-rust-core-embedded-sqlite-and-parquet.md) | Stack: Rust core with embedded SQLite (WAL) and Parquet storage |
| [0020](docs/adr/0020-desktop-shell-tauri-react.md) | Desktop shell: Tauri v2 + React in WKWebView, one argv-dispatching binary |
| [0021](docs/adr/0021-coach-agent-one-mutation-framework.md) | Coach agent and the one-mutation framework |
| [0022](docs/adr/0022-toolchain-bump-1-98.md) | Toolchain bump: Rust 1.92.0 → 1.98.0 |
| [0023](docs/adr/0023-retain-ollama-cloud-bump-default-model-to-glm-5-3-flash.md) | Retain Ollama Cloud; bump the default model to `glm-5.3-flash` |
| [0024](docs/adr/0024-dsl-schema-evolution-and-indicator-seam.md) | DSL schema evolution and the indicator seam |
| [0024 (addendum)](docs/adr/0024-addendum-structure-indicators-1-3-0.md) | Addendum — Structure indicators under schema 1.3.0 |
| [0024 (addendum)](docs/adr/0024-addendum-candle-patterns-1-4-0.md) | Addendum — Named candlestick patterns under schema 1.4.0 |
| [0024 (addendum)](docs/adr/0024-addendum-cipher-indicators-1-5-0.md) | Addendum — Cipher building blocks (SMA, `max`/`min`, WaveTrend) under schema 1.5.0 |
| [0024 (addendum)](docs/adr/0024-addendum-previous-pivots-1-7-0.md) | Addendum — Previous pivots (`PreviousPivotHigh`, `PreviousPivotLow`) under schema 1.7.0 |
| [0025](docs/adr/0025-walk-forward-run-kind-and-certification-seam.md) | Walk-forward as a run kind and the certification seam |
| [0026](docs/adr/0026-client-server-split.md) | The client/server split |
| [0027](docs/adr/0027-paper-session-event-log.md) | The paper session: a row plus an append-only event log, graduated two ways |
| [0027 (addendum)](docs/adr/0027-addendum-paper-pnl-read-model.md) | Addendum — The paper P&L read model |
| [0028](docs/adr/0028-wf-v2-holdout-and-hypothesis-budget.md) | `wf-v2`, the C1 holdout test, and the hypothesis budget |
| [0029](docs/adr/0029-prod-on-mac-mini-qa-on-draco-desk.md) | Prod on the Mac Mini under launchd |
| [0030](docs/adr/0030-chart-adapter-and-overlay-read.md) | The chart adapter boundary and the overlay read contract |
| [0031](docs/adr/0031-example-trade-marks.md) | Example-trade marks: the store, its writers, the holdout fence and the explain contract |
| [0032](docs/adr/0032-certify-power-review.md) | Certify power review: the pass-chance bar, the count-only holdout read and the certify gate |
| [0033](docs/adr/0033-stateful-setups.md) | Stateful setups: arm, dip, trigger and expiry in the strategy grammar and the engine |
