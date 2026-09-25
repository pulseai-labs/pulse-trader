//! AC-4 — the no-credential banner's seam (r1.s1.w5, grill G4/A7).
//!
//! The banner this item renders reads `w2`'s value-free credential status through
//! the ONE new command this item adds to `w1`'s bus: `credential_status`. This file
//! proves the seam end to end:
//!
//!   1. the underlying read (`llm_credential_status_in`) behaves correctly over an
//!      injected search -- `None` when nothing resolves (the banner's trigger
//!      condition, and the first-run case G4 exists for), a named source otherwise;
//!   2. the value that would cross IPC carries no key material, structurally;
//!   3. `credential_status` is registered on BOTH halves of the bus's append-only
//!      registration point (`tauri_bus_contract.rs`'s existing tests re-validate the
//!      *shape* of that registration generically; this file is the one place that
//!      proves `credential_status` specifically is present and wired);
//!   4. it is the seam's first production caller -- which is what makes AC-3's
//!      `#[allow(dead_code)]` removal sound rather than a bare grep of convenience.
//!
//! `llm_credential_status_in`/`CredentialSearch` are `w2`'s existing injectable core
//! (already covered by `tests/credential_source.rs`); asserting their behaviour again
//! here is deliberate, not duplication -- it is the exact call the new command makes,
//! so a regression in the read would otherwise show up only as a UI bug nobody's
//! `cargo test` catches.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use pulse::{BUS_COMMANDS, CredentialSearch, CredentialStatus, llm_credential_status_in};

fn manifest_path(rel: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

// ---------------------------------------------------------------------------
// Behaviour: what the banner's data source actually reports
// ---------------------------------------------------------------------------

#[test]
fn no_source_configured_reports_none_the_banners_trigger() {
    // The banner appears exactly when this is `None` -- the first-run case G4
    // exists for: a user with nothing configured yet, and no key anywhere the
    // resolver would look. A user in this state must still be able to open the
    // shell, which is why the banner is non-blocking rather than a gate.
    let status = llm_credential_status_in(&CredentialSearch::empty());
    assert_eq!(
        status,
        CredentialStatus::None,
        "an empty search must report None, the condition the banner renders on"
    );
}

#[test]
fn an_env_credential_reports_its_source_not_its_value() {
    let search = CredentialSearch::empty().with_env_key(Some("shhh-do-not-print-me".to_owned()));
    let status = llm_credential_status_in(&search);
    assert_eq!(
        status,
        CredentialStatus::Env,
        "an env-sourced credential must report the Env source"
    );
}

#[test]
fn a_status_value_serializes_as_a_bare_string_no_value_bearing_field() {
    // CredentialStatus is what the wire carries -- a status enum only, never an
    // object a key could ride inside. A regression here would mean a key-shaped
    // payload started crossing IPC.
    let json = serde_json::to_value(CredentialStatus::Env).unwrap();
    assert!(
        json.is_string(),
        "CredentialStatus must serialize as a bare string discriminant, got {json}"
    );
    assert_eq!(json, serde_json::json!("env"));

    let none_json = serde_json::to_value(CredentialStatus::None).unwrap();
    assert_eq!(none_json, serde_json::json!("none"));
}

// ---------------------------------------------------------------------------
// Wiring: the command exists, is registered on both halves of the bus, and is
// the seam's first production caller.
// ---------------------------------------------------------------------------

#[test]
fn credential_status_is_registered_on_the_bus() {
    assert!(
        BUS_COMMANDS.contains(&"credential_status"),
        "the banner's command must be appended to BUS_COMMANDS"
    );
}

#[test]
fn the_command_is_an_async_command_returning_credential_status() {
    let source = std::fs::read_to_string(manifest_path("src/tauri/commands.rs"))
        .expect("read src/tauri/commands.rs");
    assert!(
        source.contains(
            "pub async fn credential_status(\n    state: tauri::State<'_, ClientState>,\n) -> Result<CredentialStatus, BusError>"
        ),
        "credential_status must be an async command returning CredentialStatus behind the \
         bus's Result shell -- r3.s3.w5 moved the read across the wire, which gave it a \
         failure mode the local read never had: an unreachable server must be a BusError, \
         never a silent \"no credential\" the banner would render as a missing key"
    );
}

#[test]
fn the_command_is_collected_into_the_invoke_handler() {
    let builder_source =
        std::fs::read_to_string(manifest_path("src/tauri/mod.rs")).expect("read src/tauri/mod.rs");
    assert!(
        builder_source.contains("commands::credential_status,"),
        "credential_status must appear in collect_commands! or it is registered in \
         BUS_COMMANDS but unreachable from the frontend"
    );
}

#[test]
fn the_command_calls_the_seams_zero_arg_wrapper() {
    // The point of this item wiring the seam: `llm_credential_status` (the
    // process-environment wrapper `w2` left carrying `#[allow(dead_code)]`) now has
    // a real caller. Asserted structurally because the wrapper is `pub(crate)` and
    // cannot be called directly from this out-of-crate test.
    //
    // r3.s3.w5: the caller is the SERVER's plain route now — the desktop is a
    // thin client and reads the answer over the wire. The production caller
    // survives one ring outward, and that is what keeps AC-3's
    // `#[allow(dead_code)]` removal sound.
    let server_source = std::fs::read_to_string(manifest_path("src/server/routes.rs"))
        .expect("read src/server/routes.rs");
    assert!(
        server_source.contains("llm_credential_status()"),
        "the credential-status route must call the seam's zero-arg wrapper, or AC-3's \
         #[allow(dead_code)] removal is unsound"
    );
    let commands = std::fs::read_to_string(manifest_path("src/tauri/commands.rs"))
        .expect("read src/tauri/commands.rs");
    assert!(
        !commands.contains("llm_credential_status()"),
        "the thin client must not read the credential locally -- the banner's answer \
         comes from the server that would actually use the key"
    );
}

#[test]
fn the_allow_dead_code_marker_is_gone_from_the_seam() {
    // AC-3's own assertion, restated here as a behavioural claim rather than a bare
    // grep: the allow can only be gone because THIS item gave the function a real
    // caller (the previous test).
    let source = std::fs::read_to_string(manifest_path("src/adapters/secrets.rs"))
        .expect("read src/adapters/secrets.rs");
    assert!(
        !source.contains("#[allow(dead_code)]"),
        "the allow(dead_code) marker on llm_credential_status must be removed once \
         this item wires a production caller"
    );
}
