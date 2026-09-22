// Engine source-set root list — `engine_fingerprint` input (d)
// (r2.s3.w1, #155, SPINE.md ruling L2 — locked).
//
// `include!`'d by BOTH `build.rs` (which hashes this set and watches it via
// `cargo:rerun-if-changed`) and `tests/engine_fingerprint_source.rs` (which
// asserts this exact list), so the two sites can never drift.
//
// NOTE: this file is `include!`'d as a raw token stream, not loaded as a
// module — bare items only, no `//!` inner doc comments, `mod`/`use`, or inner
// attributes (same seam rules as the schema-version const file).
// `#[rustfmt::skip]` is load-bearing: an AC-2 `grep -c` counts matching LINES,
// so the one-root-per-line layout must survive formatting — do not name a
// grepped root in a comment here either.

/// The engine source-set roots hashed into `engine_fingerprint` input (d):
/// four directories (walked recursively for `.rs` files) and five single
/// files. This is the whole of "engine code" for fingerprint purposes — not
/// the compiled artifact, not all of `src/`.
#[rustfmt::skip]
pub const ENGINE_SOURCE_ROOTS: &[&str] = &[
    "src/domain/backtest",
    "src/adapters/backtest",
    "src/adapters/indicators",
    "src/domain/dsl",
    "src/domain/indicator.rs",
    "src/domain/series.rs",
    "src/domain/candle.rs",
    "src/domain/sizing.rs",
    "src/adapters/broker/mod.rs",
];
