//! `EngineFingerprint` — the deterministic, build-time identity of the backtest
//! engine (VS-1.2.3 work-3.01, FR-7 / NFR-2).
//!
//! The fingerprint is a sha2-256 hex digest computed in `build.rs` (decision D5)
//! over five inputs that, together, pin a byte-reproducible engine build:
//! - (a) the raw bytes of the workspace `Cargo.lock` (the full resolved
//!   dependency graph);
//! - (b) the *resolved* `rustc -vV` filtered to its `release:` + `commit-hash:`
//!   lines (the `host:` line is excluded — it varies by build host and is not
//!   the property we fingerprint; the **target triple** below covers arch);
//! - (c) the DSL schema-version string (`DSL_SCHEMA_VERSION`, shared via the
//!   `schema_version_const.rs` seam);
//! - (d) the engine source set — a sha2-256 over the `.rs` files under the
//!   eleven locked roots in `build_support/engine_source_set.rs`, folded behind
//!   the `b"engine-source-v1\0"` domain prefix (r2.s3.w1, #155);
//! - (e) the full target triple.
//!
//! The hex digest is baked into the binary by `build.rs` via
//! `cargo:rustc-env=PULSE_ENGINE_FINGERPRINT=<hex>` and the triple via
//! `PULSE_TARGET_TRIPLE`; this type reads them through `env!` at compile time, so
//! [`EngineFingerprint::current()`] is a pure, allocation-only accessor with no
//! runtime hashing.
//!
//! **FR-7 scope (audit C2 — traceability honesty).** This slice delivers the
//! fingerprint *recording* substrate (attached to `BacktestResult` by 3.03) and
//! the cross-fingerprint comparison *mechanism* ([`EngineFingerprint::compare`]) —
//! but `compare` is intentionally **built-but-unwired** here: there is no persisted
//! prior run to compare against until `BacktestRun` persistence lands in VS-1.2.4,
//! which is where the warning is actually surfaced. FR-7 is therefore *partially*
//! delivered by this slice.

use serde::{Deserialize, Serialize};

/// The build-time identity of the backtest engine.
///
/// A newtype over the sha2-256 hex digest of the five D5 inputs (`Cargo.lock`,
/// resolved `rustc`, DSL schema version, engine source set, target triple).
/// Two builds with identical
/// fingerprints are byte-reproducible peers; a differing fingerprint means the
/// engine, its dependency graph, its compiler, or its target changed — and any
/// backtest results carrying different fingerprints are not directly comparable
/// (the FR-7 warning [`compare`](EngineFingerprint::compare) surfaces this).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineFingerprint(String);

/// `Default` is **this build's** fingerprint (VS-1.2.4 work-4.01 / #68).
///
/// Required so `BacktestResult::engine_fingerprint` can carry `#[serde(default)]`
/// (README C5): an old-shape result serialized before the fingerprint field
/// existed deserializes to the fingerprint of the build doing the read — the
/// honest stand-in (the run was produced by *some* engine; absent a recorded one,
/// the current build is the best available identity, and it matches the value
/// `LoopState::into_result` stamps via [`EngineFingerprint::current`]). It is NOT
/// an empty/placeholder string.
impl Default for EngineFingerprint {
    fn default() -> Self {
        Self::current()
    }
}

impl EngineFingerprint {
    /// The fingerprint of *this* build, baked in at compile time by `build.rs`.
    ///
    /// Reads the `PULSE_ENGINE_FINGERPRINT` env var that `build.rs` emitted via
    /// `cargo:rustc-env`; the value is fixed for the lifetime of a build (stable
    /// within a build, validated by `current_is_stable_within_build`).
    #[must_use]
    pub fn current() -> Self {
        Self(env!("PULSE_ENGINE_FINGERPRINT").to_owned())
    }

    /// The fingerprint as its raw hex string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The target triple this engine was compiled for (e.g.
    /// `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`).
    ///
    /// Baked in by `build.rs` via `cargo:rustc-env=PULSE_TARGET_TRIPLE`. This is
    /// the arch tag that lets two arches report distinct, arch-labelled
    /// fingerprints even when 3.04's cross-arch determinism makes their *content*
    /// hashes identical (NFR-2).
    #[must_use]
    pub fn target() -> &'static str {
        env!("PULSE_TARGET_TRIPLE")
    }

    /// Test-only constructor for an arbitrary fingerprint value.
    ///
    /// Used by sibling-module determinism tests (e.g. `result::tests`) that must
    /// build two results differing ONLY in their fingerprint to prove the content
    /// hash excludes it (D4). Not part of the public surface — `current()` is the
    /// sole production constructor for *this build's* fingerprint.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_raw_for_test(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// Reconstruct a fingerprint value from a **persisted** hex string (VS-1.2.4
    /// work-4.05, FR-7 read-back).
    ///
    /// `current()` mints *this build's* fingerprint; this constructor rehydrates a
    /// fingerprint that was recorded by a possibly-different build — the
    /// [`PersistedRun`](crate::PersistedRun)'s stored `engine_fingerprint` `TEXT`
    /// column — so the FR-7 [`compare`](EngineFingerprint::compare) can run the
    /// new run's fingerprint against the prior persisted one. The stored value is
    /// already a validated hex digest on write (4.04); this is a pure wrap.
    #[must_use]
    pub fn from_stored(hex: impl Into<String>) -> Self {
        Self(hex.into())
    }

    /// FR-7 cross-fingerprint comparison.
    ///
    /// Returns [`None`] when `self` and `other` are byte-equal (the runs share an
    /// engine build and are directly comparable), and `Some(<non-empty warning>)`
    /// when they differ — the substrate VS-1.2.4 surfaces when a re-run's
    /// fingerprint differs from a stored prior run's ("you are comparing runs from
    /// different engine builds"). Built-but-unwired this slice (see module docs).
    #[must_use]
    pub fn compare(&self, other: &Self) -> Option<String> {
        if self.0 == other.0 {
            None
        } else {
            Some(format!(
                "engine fingerprint mismatch: comparing runs from different engine \
                 builds ({} vs {}); results may not be directly comparable",
                self.0, other.0
            ))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::EngineFingerprint;

    /// AC-7 / demo AC#2: the build-time fingerprint hex is non-empty.
    #[test]
    fn current_is_non_empty() {
        let fp = EngineFingerprint::current();
        assert!(
            !fp.as_str().is_empty(),
            "engine fingerprint must be a non-empty hex digest"
        );
    }

    /// The fingerprint is a sha2-256 hex digest: 64 lowercase hex chars.
    #[test]
    fn current_is_sha256_hex() {
        let fp = EngineFingerprint::current();
        let hex = fp.as_str();
        assert_eq!(hex.len(), 64, "sha2-256 hex digest is 64 chars");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "fingerprint must be lowercase hex, got {hex:?}"
        );
    }

    /// NFR-2: the engine reports its compiled target arch (the triple is non-empty
    /// and carries the build host's architecture).
    #[test]
    fn target_embeds_arch() {
        let triple = EngineFingerprint::target();
        assert!(!triple.is_empty(), "target triple must be non-empty");
        // The first triple component is the arch (e.g. aarch64 / x86_64). It must
        // be present and non-empty; this guards against an empty/garbled triple.
        let arch = triple.split('-').next().unwrap_or("");
        assert!(
            !arch.is_empty(),
            "target triple must lead with an arch component, got {triple:?}"
        );
    }

    /// FR-7: `compare()` returns `None` for two equal fingerprints.
    #[test]
    fn compare_none_on_equal() {
        let a = EngineFingerprint::current();
        let b = EngineFingerprint::current();
        assert_eq!(a, b, "two current() values are byte-equal within a build");
        assert!(
            a.compare(&b).is_none(),
            "compare() of equal fingerprints must be None"
        );
    }

    /// FR-7: `compare()` returns a non-empty warning for differing fingerprints.
    #[test]
    fn compare_some_on_differ() {
        let current = EngineFingerprint::current();
        // A hand-constructed differing fingerprint (clearly not equal to current()).
        let other = EngineFingerprint("0".repeat(64));
        assert_ne!(current, other);
        let warning = current
            .compare(&other)
            .expect("compare() of differing fingerprints must be Some");
        assert!(
            !warning.is_empty(),
            "the cross-fingerprint warning must be non-empty"
        );
    }

    /// Stable-within-build: repeated `current()` calls in the same build return the
    /// identical fingerprint (the env value is fixed at compile time).
    #[test]
    fn current_is_stable_within_build() {
        let first = EngineFingerprint::current();
        let second = EngineFingerprint::current();
        assert_eq!(
            first, second,
            "current() must be stable within a single build"
        );
    }
}
