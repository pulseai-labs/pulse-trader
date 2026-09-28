//! Deep-nesting product-safety proof (r3.s2.w2 AC-1, spec §0.4; the #283
//! companion).
//!
//! #283's overflow was diagnosed to proptest **generation** (the test-side
//! machinery), not serde — but the spec still asks the product question: does
//! a pathologically deep DSL document deserialize, or refuse with an error,
//! on a **2 MiB thread** (a server worker's default)? It must never abort the
//! process.
//!
//! Every case here runs on a thread spawned with
//! `std::thread::Builder::new().stack_size(2 * 1024 * 1024)`. A stack overflow
//! on that thread kills the test process, so the suite passing IS the
//! never-aborts proof.
//!
//! w3 extends this file with `Arith` chains — the document builder is named
//! generically ([`nested_entry`]) for exactly that reason.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use pulse::{Migrator, compile, validate};

/// The default depth for the at-limit document: total JSON object/array
/// nesting lands just under `serde_json`'s default 128 recursion limit
/// (each `And` level nests one object + one array; the leaf Compare nests a
/// few more).
const AT_LIMIT_DEPTH: u32 = 50;

/// A depth comfortably past the 128 limit — the parse must refuse it with an
/// error, never abort.
const OVER_LIMIT_DEPTH: u32 = 120;

/// A `Compare` leaf over an indicator and a constant, as JSON.
const LEAF: &str = r#"{"type":"Compare","lhs":{"type":"Indicator","series":"primary","spec":{"indicator":"Rsi","period":14}},"op":"Gt","rhs":{"type":"Constant","value":"30"}}"#;

/// Build an `entry` of `depth` nested `And` chains around `leaf`:
/// `And[And[…And[leaf]…]]`. Named generically so w3 can extend the file with
/// `Arith` chains without reshaping the builder.
fn nested_entry(depth: u32, leaf: &str) -> String {
    let mut entry = leaf.to_owned();
    for _ in 0..depth {
        entry = format!(r#"{{"type":"And","conditions":[{entry}]}}"#);
    }
    entry
}

/// A whole strategy document whose `entry` is the given (possibly deeply
/// nested) condition JSON.
fn strategy_doc(entry: &str) -> String {
    format!(
        r#"{{
  "schema_version": "1.2.0",
  "name": "deep nesting probe",
  "direction": "long",
  "entry": {entry},
  "filters": [],
  "exits": [{{ "type": "StopLoss", "distance_pct": "0.05" }}],
  "risk": {{
    "risk_per_trade_pct": "0.01",
    "max_leverage": "3"
  }}
}}"#
    )
}

/// Run `f` on a thread with a 2 MiB stack and join it. A stack overflow on
/// that thread aborts the whole test process — the test failing loudly is the
/// never-aborts proof.
fn on_2mib_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(f)
        .expect("2 MiB thread spawns")
        .join()
        .expect("2 MiB thread survives the document")
}

/// The deepest document `serde_json`'s default recursion limit still allows
/// either deserializes, or is refused with a named error — it never aborts
/// the 2 MiB thread. (The typed `serde` path is what `Migrator::load` runs
/// after its parse step; this drives it directly.)
#[test]
fn deep_document_deserializes_or_is_refused_never_aborts() {
    let json = strategy_doc(&nested_entry(AT_LIMIT_DEPTH, LEAF));
    let parsed: Result<pulse::StrategyDsl, _> = on_2mib_thread(move || serde_json::from_str(&json));
    match parsed {
        Ok(dsl) => {
            // Accepted: the entry really is the deep And chain.
            let mut seen = 0u32;
            let mut cursor = &dsl.entry;
            while let pulse::Condition::And { conditions } = cursor {
                cursor = &conditions[0];
                seen += 1;
            }
            assert_eq!(
                seen, AT_LIMIT_DEPTH,
                "the accepted document carries the full And chain"
            );
        }
        Err(err) => {
            // Refused: with a real error message, not an abort (reaching this
            // arm at all proves the thread survived).
            let message = err.to_string();
            assert!(!message.is_empty(), "a refusal names what it refused");
        }
    }
}

/// A document nested past `serde_json`'s default 128 recursion limit is
/// REFUSED with an error on the 2 MiB thread — never aborts.
#[test]
fn over_recursion_limit_document_is_refused_not_aborted() {
    let json = strategy_doc(&nested_entry(OVER_LIMIT_DEPTH, LEAF));
    let parsed: Result<pulse::StrategyDsl, _> = on_2mib_thread(move || serde_json::from_str(&json));
    let err = parsed.expect_err("over-limit document must be refused by the recursion limit");
    let message = err.to_string();
    assert!(
        message.to_lowercase().contains("recursion limit")
            || message.to_lowercase().contains("depth"),
        "the refusal names the recursion limit, got: {message}"
    );
}

/// The same at-limit document through the MCP submit path's parse-and-validate
/// (`Migrator::v1().load` → `validate` → `compile`, the exact steps
/// `submit_agent_version` runs before any write) returns a result on the
/// 2 MiB thread — accepted or refused, never aborts.
#[test]
fn deep_document_through_the_submit_parse_and_validate_returns_a_result() {
    let json = strategy_doc(&nested_entry(AT_LIMIT_DEPTH, LEAF));
    // The exact steps `submit_agent_version` runs before any write, with each
    // step's error folded to its message (three distinct error types).
    let outcome: Result<(), String> = on_2mib_thread(move || {
        let loaded = match Migrator::v1().load(&json) {
            Ok(loaded) => loaded,
            Err(err) => return Err(err.to_string()),
        };
        let validated = match validate(&loaded.dsl) {
            Ok(validated) => validated,
            Err(err) => return Err(err.to_string()),
        };
        match compile(&validated) {
            Ok(_compiled) => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    });
    match outcome {
        // Accepted end-to-end: parse → validate → compile all came back.
        Ok(()) => {}
        // Refused somewhere: a named error, and reaching this arm proves the
        // thread survived.
        Err(message) => {
            assert!(!message.is_empty(), "a refusal names what it refused");
        }
    }
}
