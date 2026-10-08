//! Shared test support for the `pulse mcp` integration suites.
//!
//! `w3` extracted the stdio harness `tests/mcp_stdio.rs` grew (r2.s1.w2) so the
//! write-tool suite (`tests/mcp_write.rs`) drives the identical seeded fixture
//! — same `pulse.db`, same copied candle store, same `spawn_client` /
//! `call_tool` plumbing — instead of forking it.

pub mod mcp;
// r4.s1.w5: the certification suites' shared world — a migrated DB, a
// synthetic candle store with `HEAD`, one version and an open H = 12 freeze,
// driven by `tests/certification_holdout.rs` (AC-1) and
// `tests/mcp_certify_version.rs` (AC-2) so the two cannot drift.
pub mod certification;
// r4.s1.w3: the wf-v2 calibration's deterministic generator, shared by the
// calibration test and the measurement harness so both draw one stream.
pub mod rng;
// r3.s4.w3: the live-runtime harness (migrated DB + real repositories + a
// scripted `ClosedBarSource` + a hand-advanced clock) shared by the five
// `paper_*` suites.
pub mod paper;
// r3.s3.w2: the shared in-process server harness the command-surface suites
// (`tests/server_stream.rs`, `tests/server_routes.rs`) drive — real router,
// fixture store under the server's own data dir, token CLI, sweep + compose
// seams.
pub mod server;
