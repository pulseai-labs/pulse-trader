//! Application ring (r1.s3.w3) — use cases shared by every delivery adapter.
//!
//! **Why a ring at all.** Before this, the version-id backtest flow lived inside
//! `src/cli/backtest.rs`: load version → compile → load snapshots → reject gaps →
//! resolve filters → run → compare fingerprint → persist. The desktop command needs
//! the identical sequence, and a second copy of it is a second place for the order
//! to drift — the FR-7 compare-before-insert ordering alone is a correctness rule
//! that only reads as one if it exists once.
//!
//! **What belongs here.** Orchestration of domain ports, and nothing else. This ring
//! names no infrastructure adapter — no `tauri`, no `specta`, no `sqlx`, and no
//! filesystem type — and is generic over the ports in `crate::domain::port`,
//! returning domain values plus its own typed errors. The one deliberate adapter
//! import is the deterministic engine itself,
//! `crate::adapters::backtest::run_backtest`: it owns no I/O (its `adapters`
//! address is namespace, not infrastructure — it lives there because it owns the
//! concrete `IndicatorEngine`), so running it from the ring breaks no boundary the
//! hexagonal scan enforces. `src/cli/mod.rs` and
//! `src/tauri/commands.rs::DesktopState` are the composition roots that choose
//! implementations (ADR-0015).
//!
//! **What deliberately does not belong here.** Any order, broker or execution
//! capability. r1 is backtest-only, and the risk gate's kill-switch and
//! progressive-exposure controls are discharged by the dependency set being
//! *incapable* of placing an order rather than by a flag that disables one.
//! `tests/tauri_backtest.rs` scans this ring — EVERY file in it, by glob since
//! r1.s4.w2 — for exactly that, and for the ADR-0015 rule that
//! `crate::adapters::backtest` is the ring's ONE deliberate adapter import.

pub(crate) mod backtest;

// r1.s4.w1 (#131 / #132, ADR-0015): the SEALED coach turn. One crate-private entry
// point that takes IDENTIFIERS and ports — a session id and a run id — claims the
// session before any provider I/O, makes exactly one attributed call, and settles
// the claim once. It replaces the `Coach::new` + `Coach::run_turn` fragment surface
// the desktop rail (w3) and the decision module (w2) would otherwise consume.
pub(crate) mod coach;

// r1.s4.w2 (ADR-0010 / ADR-0019 / ADR-0021): the coach DECISION use case. One
// session id and one action — modify, reject or accept — in; one durable outcome
// out. On accept it re-applies the CURRENT mutation, re-runs the backtest on the
// parent run's exact persisted inputs through `backtest::prepare_backtest`, and
// commits child version + run + trades + links in W4's one transaction.
pub(crate) mod coach_decision;

// r2.s1.w2: the MCP read projections — the pure parser moved out of
// `cli::indicators` plus the typed wire shapes the `src/mcp/` delivery ring
// returns, so the CLI viewer and `pulse mcp` share one parse/dedup path and one
// projection seam.
pub(crate) mod mcp_read;

// r2.s1.w3: the external-agent submit use case. One typed request in
// (`SubmitRequest`: target + DSL + hypothesis + agent name), one persisted
// outcome out (version + `agent_submission` row). Validates in the spec's
// fixed order — hypothesis, agent name, target, load, validate, compile — so a
// field failure and a compile failure are distinguishable, and the `dsl` path
// collects EVERY validation error rather than stopping at the first.
pub(crate) mod mcp_write;

// r2.s3.w3 (ADR-0025): the walk-forward use case — `rolling-oos/v1` folds as
// ordinary persisted windowed runs, `wf-v1` verdicts, one blocking task for
// all folds (a13/#201), one transaction for the parent + fold rows + fold runs.
pub(crate) mod walk_forward;

// r2.s3.w5: the walk-forward READ projection — `load_fold_runs` (the L8
// seam's read half: a fold is an ordinary `backtest_run`) plus the one
// `WalkForwardRunDetail` wire shape both MCP walk-forward tools return, so no
// delivery ring re-derives its own read.
pub(crate) mod walk_forward_read;
