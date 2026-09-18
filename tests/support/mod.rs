//! Shared test support for the `pulse mcp` integration suites.
//!
//! `w3` extracted the stdio harness `tests/mcp_stdio.rs` grew (r2.s1.w2) so the
//! write-tool suite (`tests/mcp_write.rs`) drives the identical seeded fixture
//! — same `pulse.db`, same copied candle store, same `spawn_client` /
//! `call_tool` plumbing — instead of forking it.

pub mod mcp;
