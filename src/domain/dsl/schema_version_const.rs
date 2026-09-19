// Single-source DSL schema-version string, shared across the `build.rs` ↔ crate
// seam (VS-1.2.3 work-3.01, decision D5).
//
// `build.rs` runs BEFORE the crate compiles and cannot import crate types, so it
// cannot read `crate::domain::SchemaVersion::CURRENT` directly. This file is the
// single source of truth for the schema-version *string*: it is `include!`'d by
// BOTH `build.rs` (which folds `DSL_SCHEMA_VERSION` into the build-time
// `engine_fingerprint`) and `src/domain/dsl/schema_version.rs` (which adds a
// runtime test asserting `SchemaVersion::CURRENT.to_string() == DSL_SCHEMA_VERSION`).
// Because both sites read the SAME bytes, the structured `SchemaVersion::CURRENT`
// triple and the fingerprint's schema input can never silently drift.
//
// NOTE: this file is `include!`'d as a raw token stream, not loaded as a module —
// it must contain ONLY items valid at the include site (a bare `pub const`), with
// NO inner doc comments (`//!`), `mod`/`use`, or inner attributes that would assume
// a module context. (Inner doc comments here are an E0753 compile error.)

/// The DSL schema version this build writes, as the canonical
/// `"MAJOR.MINOR.PATCH"` string. Kept byte-for-byte in lock-step with
/// `SchemaVersion::CURRENT` via the non-drift test in `schema_version.rs`.
pub const DSL_SCHEMA_VERSION: &str = "1.1.0";
