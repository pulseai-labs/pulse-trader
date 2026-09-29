//! Agent ring (outer): `PulseHive` integration, tools, agent definitions.
//!
//! Empty stub for WI-01 to pin the hexagonal layout.

// VS-1.3.2 work-2.03: the composer's "moat in DATA" config seam — loads the
// versioned composer prompt + the nominal price table as DATA (VS-1.3.1
// decision 4). Kept `pub(crate)`: an internal seam whose first production caller
// is the composition root (2.05). Append-only (keep-both with 2.01's re-exports
// at the merge into `slice/VS-1.3.2`).
pub(crate) mod config;

// VS-1.3.2 work-2.02: the FR-3 heart — the six server-validated builder tools +
// the `StrategyBuilder` accumulator they mutate. `mod builder` owns the
// order-independent partial-strategy state machine + `finalize` → whole-document
// `validate()` (reused verbatim); `mod tools` owns the flat-primitive tool fns,
// the isolated flat→tagged mapping seam (README C4 reversibility), and the
// `ToolDefinition` instances. Their first production caller is the composer loop
// (2.04, R3), so the re-exported surface is `#[allow(unused_imports)]` +
// `#![allow(dead_code)]` (the VS-1.3.1 dead-code-under-deny(warnings) gotcha).
// Append-only (keep-both with 2.03's `mod config;` at the merge into slice/VS-1.3.2).
mod builder;
mod tools;

#[allow(unused_imports)]
pub(crate) use builder::StrategyBuilder;
#[allow(unused_imports)]
pub(crate) use tools::{
    ToolOutcome, add_entry_signal, add_filter, builder_tool_definitions, create_strategy,
    finalize_strategy, set_exit_rules, set_risk_params,
};

// VS-1.3.2 work-2.04: the composer agent loop — the orchestrator that drives the
// model through the builder tools (2.02) over the tools-carrying `LlmProvider` port
// (2.01) framed by the config prompt (2.03), and finalizes a `StrategyVersion` value
// with composer provenance (DB-free — 2.05 persists it). Its surface is `pub` (2.05's
// CLI + composition root consume it), mirrored at `src/lib.rs`. Append-only (keep-both
// with 2.01–2.03's re-exports at the merge into `slice/VS-1.3.2`).
mod composer;
pub use composer::{
    ASSISTANT_TEXT_MAX_BYTES, ComposeOutcome, Composer, ComposerError, ComposerEvent,
    LlmCallCapture,
};

// r1.s2.w3 (ADR-0021), RESEALED by r1.s4.w1 (#132): the coach's model-facing half —
// the turn budgets and the routing of one response into one answer or one typed
// failure. `pub(crate)` and deliberately so: the turn SEQUENCE moved to
// `crate::application::coach`, which takes identifiers rather than fragments, so
// there is no longer a public `Coach` any caller can assemble by hand.
mod coach;
pub(crate) use coach::{
    DEFAULT_MAX_DSL_BYTES, DEFAULT_MAX_TURN_BYTES, DEFAULT_TURN_TIMEOUT, TurnAnswer,
    check_turn_budget, classify,
};

// r1.s4.w1 (#131): the coach's two mutually exclusive tool definitions, in
// advertisement order. Consumed by the sealed turn, which both advertises them and
// feeds them to the request fingerprint.
#[allow(unused_imports)]
pub(crate) use tools::coach_tool_definitions;
