//! `scripts/check-signing-config.sh` — the failure cases (r4.s3.w1, AC-1 / AC-4).
//!
//! The gate asserts content, so the way to trust it is to WATCH IT FAIL: every test
//! below stages a temp copy of the four files the script reads, applies ONE mutation
//! the gate is supposed to catch, and asserts the non-zero exit NAMES the right
//! check. `an_unmutated_copy_passes` is the non-vacuity control — without it, a
//! mutation case would still "pass" if the staged copy failed for an unrelated
//! reason (a missing file, a broken python3, a bad copy).
//!
//! The copy's root is the temp directory the script's `BASH_SOURCE`-derived
//! `repo_root` resolves to, so the real worktree is never mutated by these tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The files the gate reads, copied whole into the staged temp root.
const CHECKED_FILES: [&str; 4] = ["tauri.conf.json", "justfile", "package.json", "Cargo.toml"];

/// The script under test, copied under `<temp>/scripts/`.
const GATE: &str = "scripts/check-signing-config.sh";

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn stage_copy(dir: &Path) {
    let root = repo_root();
    for rel in CHECKED_FILES {
        fs::copy(root.join(rel), dir.join(rel)).unwrap_or_else(|e| panic!("copy {rel}: {e}"));
    }
    fs::create_dir_all(dir.join("scripts")).expect("create scripts/");
    fs::copy(root.join(GATE), dir.join(GATE)).unwrap_or_else(|e| panic!("copy {GATE}: {e}"));
}

fn run_gate(dir: &Path) -> Output {
    Command::new("bash")
        .arg(dir.join(GATE))
        .current_dir(dir)
        .output()
        .expect("run the signing-config gate")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// Replace `from` with `to` in one staged file. The anchor must exist, or the test
/// is wrong about the file it is mutating.
fn replace_in(dir: &Path, rel: &str, from: &str, to: &str) {
    let path = dir.join(rel);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    assert!(
        text.contains(from),
        "{rel} must contain the mutation anchor {from:?}"
    );
    fs::write(&path, text.replace(from, to)).unwrap_or_else(|e| panic!("write {rel}: {e}"));
}

fn append_to(dir: &Path, rel: &str, line: &str) {
    let path = dir.join(rel);
    let mut text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    text.push('\n');
    text.push_str(line);
    text.push('\n');
    fs::write(&path, text).unwrap_or_else(|e| panic!("write {rel}: {e}"));
}

fn edit_json(dir: &Path, rel: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = dir.join(rel);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"));
    let mut value: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {rel}: {e}"));
    edit(&mut value);
    let edited = serde_json::to_string_pretty(&value).expect("serialise the edited copy");
    fs::write(&path, edited).unwrap_or_else(|e| panic!("write {rel}: {e}"));
}

/// Run the staged gate and assert it fails, naming `needle` on stderr.
fn expect_failure(dir: &Path, needle: &str) {
    let out = run_gate(dir);
    let err = stderr(&out);
    assert!(
        !out.status.success(),
        "the gate must refuse this copy; stderr:\n{err}"
    );
    assert!(
        err.contains(needle),
        "the refusal must name {needle:?}; stderr:\n{err}"
    );
}

/// A staged temp copy of the item's four checked files + the gate script.
fn temp_copy() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    stage_copy(dir.path());
    dir
}

/// AC-1's own command, run where the buyer of this gate runs it: the worktree root.
#[test]
fn the_gate_passes_at_the_item_head() {
    let out = run_gate(&repo_root());
    assert!(
        out.status.success(),
        "`bash scripts/check-signing-config.sh` must exit 0 at the item head; stderr:\n{}",
        stderr(&out)
    );
}

/// The control: an untouched copy must pass, so a failing mutation case means the
/// mutation — not the staging — broke it.
#[test]
fn an_unmutated_copy_passes() {
    let dir = temp_copy();
    let out = run_gate(dir.path());
    assert!(
        out.status.success(),
        "an unmutated copy must pass the gate; stderr:\n{}",
        stderr(&out)
    );
}

#[test]
fn hardened_runtime_off_is_refused() {
    let dir = temp_copy();
    edit_json(dir.path(), "tauri.conf.json", |config| {
        config["bundle"]["macOS"]["hardenedRuntime"] = serde_json::json!(false);
    });
    expect_failure(dir.path(), "hardenedRuntime");
}

#[test]
fn a_missing_macos_section_is_refused() {
    let dir = temp_copy();
    edit_json(dir.path(), "tauri.conf.json", |config| {
        config["bundle"]
            .as_object_mut()
            .expect("bundle is an object")
            .remove("macOS");
    });
    expect_failure(dir.path(), "bundle.macOS");
}

#[test]
fn a_committed_identity_in_the_config_is_refused() {
    let dir = temp_copy();
    edit_json(dir.path(), "tauri.conf.json", |config| {
        config["bundle"]["macOS"]["signingIdentity"] = serde_json::json!("synthetic-identity");
    });
    expect_failure(dir.path(), "signingIdentity");
}

#[test]
fn a_certificate_name_in_the_justfile_is_refused() {
    let dir = temp_copy();
    append_to(
        dir.path(),
        "justfile",
        "# pinned certificate: Local Signing",
    );
    expect_failure(dir.path(), "Local Signing");
}

#[test]
fn a_team_id_in_package_json_is_refused() {
    let dir = temp_copy();
    edit_json(dir.path(), "package.json", |package| {
        package["teamId"] = serde_json::json!("SYNTHETICTEAM");
    });
    expect_failure(dir.path(), "teamId");
}

#[test]
fn a_missing_passthrough_is_refused() {
    let dir = temp_copy();
    replace_in(
        dir.path(),
        "justfile",
        "PULSE_SIGNING_IDENTITY",
        "SIGNING_IDENTITY_DROPPED",
    );
    expect_failure(dir.path(), "PULSE_SIGNING_IDENTITY");
}

#[test]
fn apple_native_in_the_main_dependencies_is_refused() {
    let dir = temp_copy();
    replace_in(
        dir.path(),
        "Cargo.toml",
        "keyring = \"3\"",
        "keyring = { version = \"3\", features = [\"apple-native\"] }",
    );
    expect_failure(dir.path(), "apple-native");
}

#[test]
fn apple_native_missing_is_refused() {
    let dir = temp_copy();
    replace_in(
        dir.path(),
        "Cargo.toml",
        "features = [\"apple-native\"]",
        "features = []",
    );
    expect_failure(dir.path(), "apple-native");
}

#[test]
fn a_missing_apple_anchor_test_in_the_bundle_recipe_is_refused() {
    let dir = temp_copy();
    replace_in(
        dir.path(),
        "justfile",
        "if ! codesign --verify -R='anchor apple generic' \"$app\"; then",
        "if false; then",
    );
    expect_failure(dir.path(), "Apple-issued anchor");
}

#[test]
fn a_missing_392_warning_in_the_bundle_recipe_is_refused() {
    let dir = temp_copy();
    replace_in(
        dir.path(),
        "justfile",
        "prompts on every new build (#392)",
        "nothing to see here",
    );
    expect_failure(dir.path(), "does not warn on stderr");
}

#[test]
fn a_committed_apple_identity_value_in_the_justfile_is_refused() {
    let dir = temp_copy();
    append_to(
        dir.path(),
        "justfile",
        "# identity: Apple Development: Jane Roe (ABCDE12345)",
    );
    expect_failure(dir.path(), "Apple Development:");
}
