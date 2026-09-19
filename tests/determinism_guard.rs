//! VS-1.2.3 work-3.02 — determinism guard (NFR-2).
//!
//! ADVISORY defense-in-depth tripwire, NOT the determinism proof. A source
//! scan can only show that a banned token is *absent*; it can never show that
//! an f64 result is bit-identical across CPU architectures. The AUTHORITATIVE
//! cross-arch determinism gate is work-3.04's 100×-both-arches CI compare.
//! This test is the cheap pre-filter that catches the common regression (a new
//! indicator reaching for `powf`, `exp`, or `mul_add`) before it ever gets to
//! that CI run, and protects 3.04's parallel (Rayon) arm from a future hidden
//! shared-mutable cache that would make `run_backtest` non-reentrant.
//!
//! Two scans, both over the f64 math paths only:
//!   1. No fused-multiply-add / no transcendentals (FP portability contract D2).
//!      IEEE-754 `+ - * /` and `sqrt` are correctly-rounded ⇒ bit-portable;
//!      `mul_add` contracts to an FMA (different rounding) and the transcendentals
//!      (the full `BANNED_FP_CALLS` set — `exp`/`ln`/`log`/`log10`/`log2`/`ln_1p`/
//!      `exp2`/`exp_m1`/`powf`/`powi`/`sin`/`cos`/`tan`/`sin_cos`/`sinh`/`cosh`/
//!      `tanh`/`asin`/`acos`/`atan`/`atan2`/`cbrt`/`hypot`) are not standardized
//!      to the last ulp across libm implementations. `sqrt` is therefore ALLOWED;
//!      everything in `BANNED_FP_CALLS` is not.
//!   2. No shared mutable / interior-mutable state (audit C4 reentrancy scan):
//!      `static mut`, `thread_rng`, `lazy_static`, `OnceCell`/`OnceLock`,
//!      `Mutex`/`RwLock`, `RefCell`/`Cell<` — any of which could make
//!      `run_backtest` non-reentrant and flake 3.04's parallel arm.
//!
//! Matching is call-form + word-boundaried, so `f64::exp(x)`, `libm::exp(x)`,
//! `x.exp()`, and a bare `exp(x)` are all caught, while substrings such as
//! `explain`, `println`, `catalog`, or a `log_level` field are NOT — a banned
//! identifier only fires when it stands as a whole word immediately followed
//! by `(` (whitespace permitted). Line comments (`// …`) are stripped before
//! scanning so a banned token in prose (e.g. this module's own doc comment, or
//! `/// explaining the invalid tuple`) cannot false-green or false-red.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::PathBuf;

/// Transcendentals + `mul_add` banned from the f64 math paths (FP contract D2).
/// `sqrt` is intentionally absent — IEEE-754 correctly-rounded ⇒ bit-portable.
///
/// Widened for VS-1.2.4 work-4.02 (#70): the original 9-token v1 set
/// (`mul_add`/`exp`/`ln`/`log`/`powf`/`powi`/`sin`/`cos`/`tan`) missed 15 other
/// non-correctly-rounded f64 methods. The full 24-token ban now also covers
/// `log10`/`log2`/`ln_1p`/`exp2`/`exp_m1`/`sin_cos`/`sinh`/`cosh`/`tanh`/`asin`/
/// `acos`/`atan`/`atan2`/`cbrt`/`hypot`, so the next contributor reaching for any
/// of them in a scanned math path trips the tripwire before CI. `sqrt` stays
/// absent (the ONE allowed transcendental — Sharpe/Sortino's stddev in `stats.rs`
/// consumes it).
const BANNED_FP_CALLS: &[&str] = &[
    "mul_add", "exp", "ln", "log", "powf", "powi", "sin", "cos", "tan", "log10", "log2", "ln_1p",
    "exp2", "exp_m1", "sin_cos", "sinh", "cosh", "tanh", "asin", "acos", "atan", "atan2", "cbrt",
    "hypot",
];

/// Shared-mutable / interior-mutable markers that would break `run_backtest`
/// reentrancy and flake 3.04's parallel (Rayon) determinism arm (audit C4).
/// These are matched as whole-word substrings (no call-form `(` requirement),
/// since several are type names (`Mutex<…>`) or attributes, not calls.
const BANNED_SHARED_STATE: &[&str] = &[
    "static mut",
    "thread_rng",
    "lazy_static",
    "OnceCell",
    "OnceLock",
    "Mutex",
    "RwLock",
    "RefCell",
    "Cell<",
];

/// f64 math source files the guard scans. Paths are relative to the crate
/// manifest dir; every entry is asserted to exist so a future rename surfaces
/// as a loud failure rather than a silently-skipped (false-green) scan.
const SCAN_TARGETS: &[&str] = &[
    "src/adapters/indicators/adx.rs",
    // r2.s2.w2: the ATR adapter + the Wilder smoothing core it shares with ADX
    // are f64 math paths — scanned like every other indicator surface.
    "src/adapters/indicators/atr.rs",
    "src/adapters/indicators/wilder.rs",
    "src/adapters/indicators/convert.rs",
    "src/adapters/indicators/ema.rs",
    "src/adapters/indicators/engine.rs",
    "src/adapters/indicators/macd.rs",
    "src/adapters/indicators/mod.rs",
    "src/adapters/indicators/rsi.rs",
    "src/domain/backtest/regime.rs",
    "src/adapters/backtest/regime.rs",
    "src/adapters/backtest/engine.rs",
    // VS-1.2.4 work-4.02 (#70/D6): the NEW f64-bearing math surface — Sharpe/
    // Sortino's `sqrt` + ratio. Scanned so a future banned call here is not a
    // false-green hole; passes because the only transcendental present is the
    // allowed `sqrt`.
    "src/domain/backtest/stats.rs",
];

fn manifest_relative(rel: &str) -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(rel);
    path
}

/// True if `c` can appear in a Rust identifier (so it forms a word boundary).
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Strip a `//` line comment from `line` so banned tokens living only in prose
/// (doc comments, `//`-comments) never trip either scan. We strip at the first
/// `//` that is not inside a string literal. Cheap state machine over chars:
/// only string-literal context matters for our purposes; `//` inside a string
/// (e.g. a URL) is preserved, `//` outside one ends the code portion.
fn strip_line_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_str = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
        } else if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            return &line[..i];
        }
        i += 1;
    }
    line
}

/// Find a call-form, word-boundaried occurrence of `ident` in `code`: the
/// identifier must stand as a whole word (the char before its start, if any,
/// must not be an identifier char) and be immediately followed — after
/// optional whitespace — by `(`. This catches `f64::exp(x)`, `libm::exp(x)`,
/// `x.exp()`, and bare `exp(x)`, while rejecting `explain`, `expr`, `catalog`,
/// and a bare `log` with no following `(`.
fn contains_call_form(code: &str, ident: &str) -> bool {
    let mut search_from = 0;
    while let Some(rel) = code[search_from..].find(ident) {
        let start = search_from + rel;
        let end = start + ident.len();
        // Left boundary: preceding char must not be an identifier char.
        let left_ok = code[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        // Right boundary: next non-whitespace char must be '(', and the char
        // immediately after the ident must not extend the identifier (so
        // `exp` does not match inside `expr`/`explain`).
        let rest = &code[end..];
        let right_ident_ok = rest.chars().next().is_none_or(|c| !is_ident_char(c));
        let followed_by_paren = rest.trim_start().starts_with('(');
        if left_ok && right_ident_ok && followed_by_paren {
            return true;
        }
        search_from = end;
    }
    false
}

/// Whole-word (no call-form) occurrence of `needle` in `code`. Used for the
/// shared-state markers, several of which are type names or attributes rather
/// than calls. Word-boundaried on both sides so `Cell<` still matches (the `<`
/// is a boundary) while e.g. `RefCellGuard` would require an explicit entry.
/// Markers containing non-ident chars (`static mut`, `Cell<`) are matched as
/// plain substrings since they already carry their own boundary.
fn contains_shared_state(code: &str, needle: &str) -> bool {
    let needle_is_wordish = needle.chars().all(is_ident_char);
    if !needle_is_wordish {
        return code.contains(needle);
    }
    let mut search_from = 0;
    while let Some(rel) = code[search_from..].find(needle) {
        let start = search_from + rel;
        let end = start + needle.len();
        let left_ok = code[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_ident_char(c));
        let right_ok = code[end..].chars().next().is_none_or(|c| !is_ident_char(c));
        if left_ok && right_ok {
            return true;
        }
        search_from = end;
    }
    false
}

fn read_target(rel: &str) -> (PathBuf, String) {
    let path = manifest_relative(rel);
    assert!(
        path.exists(),
        "determinism-guard scan target is missing: {} — a rename must be \
         reflected in tests/determinism_guard.rs::SCAN_TARGETS, never silently \
         dropped (a missing target is a false-green hole).",
        path.display()
    );
    let source = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read scan target {}: {e}", path.display()));
    (path, source)
}

/// Iterate the code portion of each line (line comments stripped) of `source`,
/// calling `f(line_number, code_without_comment)`.
fn for_each_code_line(source: &str, mut f: impl FnMut(usize, &str)) {
    for (idx, raw) in source.lines().enumerate() {
        let code = strip_line_comment(raw);
        f(idx + 1, code);
    }
}

#[test]
fn no_transcendentals_or_fma_in_f64_math_paths() {
    let mut violations: Vec<String> = Vec::new();
    for rel in SCAN_TARGETS {
        let (path, source) = read_target(rel);
        for_each_code_line(&source, |line_no, code| {
            for &banned in BANNED_FP_CALLS {
                if contains_call_form(code, banned) {
                    violations.push(format!(
                        "{}:{line_no}: banned f64 call `{banned}(` — transcendentals \
                         and `mul_add` are not bit-portable across libm/FMA; use \
                         IEEE-754 `+ - * / sqrt` only (FP contract D2). Offending \
                         line: {}",
                        path.display(),
                        code.trim()
                    ));
                }
            }
        });
    }
    assert!(
        violations.is_empty(),
        "determinism guard: {} banned f64-call violation(s) in the math paths \
         (NFR-2). The cross-arch hash (3.03) and 100×-both-arches gate (3.04) \
         rely on these paths being contraction-free and transcendental-free:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

#[test]
fn no_shared_mutable_state_in_f64_math_paths() {
    let mut violations: Vec<String> = Vec::new();
    for rel in SCAN_TARGETS {
        let (path, source) = read_target(rel);
        for_each_code_line(&source, |line_no, code| {
            for &marker in BANNED_SHARED_STATE {
                if contains_shared_state(code, marker) {
                    violations.push(format!(
                        "{}:{line_no}: shared/interior-mutable state `{marker}` — \
                         would make `run_backtest` non-reentrant and flake 3.04's \
                         parallel (Rayon) determinism arm (audit C4). Offending \
                         line: {}",
                        path.display(),
                        code.trim()
                    ));
                }
            }
        });
    }
    assert!(
        violations.is_empty(),
        "determinism guard: {} shared-mutable-state violation(s) in the math \
         paths (audit C4 reentrancy). Keep these paths free of process-global \
         and interior-mutable state so the parallel determinism arm stays \
         flake-free:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

// --- Self-tests for the matcher: prove call-form + word-boundary semantics so
// the guard cannot silently rot into a false-green (always-pass) or false-red
// (substring-tripped) state. These run against in-memory fixtures, never the
// real source, so they add no coupling to the scanned files' contents.

#[test]
fn matcher_catches_all_banned_call_forms() {
    // Method form, path/free-function form, and bare call form must all fire.
    assert!(contains_call_form("let y = x.exp();", "exp"));
    assert!(contains_call_form("let y = f64::exp(x);", "exp"));
    assert!(contains_call_form("let y = libm::exp(x);", "exp"));
    assert!(contains_call_form("let y = exp(x);", "exp"));
    assert!(contains_call_form("let y = base.powf(2.0);", "powf"));
    assert!(contains_call_form("let y = a.mul_add(b, c);", "mul_add"));
    // Whitespace between ident and paren is permitted.
    assert!(contains_call_form("let y = x.ln ();", "ln"));
}

#[test]
fn matcher_rejects_substring_and_non_call_false_positives() {
    // Substring of a longer identifier — must NOT fire.
    assert!(!contains_call_form("let reason = explain(err);", "exp"));
    assert!(!contains_call_form("println!(\"hi\");", "ln")); // `ln` inside `println`
    assert!(!contains_call_form("let p = catalog(items);", "log"));
    assert!(!contains_call_form("self.log_level = 3;", "log")); // no following `(`
    assert!(!contains_call_form("let e = expr_node;", "exp"));
    // sqrt is allowed and not in the banned set, but verify a bare `sqrt(`
    // is not accidentally matched by any banned entry.
    for &banned in BANNED_FP_CALLS {
        assert!(!contains_call_form("let r = value.sqrt();", banned));
    }
}

#[test]
fn comment_stripping_prevents_prose_false_positives() {
    // A banned token living only in a `//` comment must be stripped.
    let code = strip_line_comment("let z = a + b; // exp(x) is banned here");
    assert!(!contains_call_form(code, "exp"));
    // …but real code on the same line before the comment is still scanned.
    let code = strip_line_comment("let z = exp(a); // trailing note");
    assert!(contains_call_form(code, "exp"));
    // `//` inside a string literal is NOT treated as a comment start.
    let code = strip_line_comment(r#"let url = "http://x"; let z = exp(a);"#);
    assert!(contains_call_form(code, "exp"));
}

#[test]
fn shared_state_matcher_semantics() {
    assert!(contains_shared_state(
        "static mut COUNTER: u64 = 0;",
        "static mut"
    ));
    assert!(contains_shared_state(
        "let m: Mutex<u8> = Mutex::new(0);",
        "Mutex"
    ));
    assert!(contains_shared_state(
        "use std::cell::Cell;\nlet c: Cell<u8>;",
        "Cell<"
    ));
    assert!(contains_shared_state("let r = thread_rng();", "thread_rng"));
    // Must not false-positive on a longer identifier embedding the marker.
    assert!(!contains_shared_state("struct MutexGuardLike;", "Mutex"));
    assert!(!contains_shared_state("let once_cellish = 1;", "OnceCell"));
}

#[test]
fn scan_targets_all_exist() {
    for rel in SCAN_TARGETS {
        let path = manifest_relative(rel);
        assert!(
            path.exists(),
            "scan target missing (false-green hole): {}",
            path.display()
        );
    }
}
