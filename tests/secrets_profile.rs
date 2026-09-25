//! r3.s3.w3 AC-2 — the credential-profile suite: the resolver under each
//! profile, the fail-closed refusal, and the process-wide selection cell.
//!
//! **Per-process isolation is REQUIRED here, not incidental.** The profile is a
//! process-global write-once cell (`OnceLock`) and the process environment is
//! process-global too, so every case that selects a profile or mutates the
//! environment must own its whole process. nextest — the repo's runner
//! (`just test`, `just check`) — runs each test in its own process by default,
//! which is exactly the isolation these cases rely on. Under plain `cargo test`
//! (one shared process, many threads) this file is unsupported by design: the
//! process-global cases would select the cell and mutate the environment under
//! each other's feet.
//!
//! The out-of-crate suite cannot read a resolved key's VALUE (`expose()` is
//! `pub(crate)` — the least-privilege control), so every assertion here is
//! about the resolved SOURCE, the refusal, the status read, or the error text.
//! The `.env` values written below are fake, key-SHAPED literals; no assertion
//! message ever prints one.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use pulse::{
    CredentialProfile, CredentialSearch, CredentialSource, CredentialStatus, LlmError,
    credential_profile, llm_credential_status, llm_credential_status_in, resolve_llm_api_key_in,
    set_credential_profile,
};

/// An API-key-SHAPED literal — not a real credential (the same discipline
/// `tests/credential_source.rs` follows, with this item's own marker so a leak
/// is attributable).
const FAKE_KEY: &str = "sk-SECRETPROFILEaa00bb11cc22dd33ee44";

/// Write a `.env` carrying `OLLAMA_API_KEY=<value>` into `dir`, at mode `0600`.
fn write_dotenv(dir: &Path, value: &str) {
    write_dotenv_mode(dir, value, 0o600);
}

/// The same, at an explicit mode (the refusal cases need a loose file).
fn write_dotenv_mode(dir: &Path, value: &str, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(".env");
    std::fs::write(&path, format!("OLLAMA_API_KEY={value}\n")).expect("write .env");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod the .env");
    path
}

// ---- The process-wide selection cell ---------------------------------------

/// A fresh process never selects a profile itself: the default is Desktop,
/// today's behaviour byte for byte, until `pulse serve` says otherwise.
#[test]
fn a_fresh_process_resolves_under_the_desktop_profile() {
    assert_eq!(
        credential_profile(),
        CredentialProfile::Desktop,
        "the recorded default is Desktop"
    );
}

/// The recorded double-set behaviour (r3.s3.w3): a second selection is a
/// programming error — it panics in debug (and test) builds and is ignored in
/// release. This process is a debug test build, so the second set must panic.
#[test]
#[should_panic(expected = "the credential profile is already set")]
fn setting_the_profile_twice_is_a_programming_error_in_debug() {
    set_credential_profile(CredentialProfile::Server);
    set_credential_profile(CredentialProfile::Desktop);
}

// ---- The chains, per profile (hermetic: explicit searches, no env mutation) -

/// The desktop profile regression guard: the FULL chain in today's order,
/// including BOTH dotenv slots — the working directory first, then the
/// compile-time manifest directory — then the app-data dir. Any narrowing here
/// would silently change every non-serve caller.
#[test]
fn the_desktop_profile_keeps_todays_full_chain_and_order() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let cwd_dir = tempfile::tempdir().expect("cwd tempdir");
    let manifest_dir = tempfile::tempdir().expect("manifest tempdir");
    let app_dir = tempfile::tempdir().expect("app-data tempdir");
    write_dotenv(config_dir.path(), "sk-DESKCONFIGaa00bb11cc22dd33ee4");
    write_dotenv(cwd_dir.path(), "sk-DESKCWDDOTENVaa00bb11cc22dd33ee");
    write_dotenv(manifest_dir.path(), "sk-DESKMANIFESTaa00bb11cc22dd33e");
    write_dotenv(app_dir.path(), "sk-DESKAPPDATAaa00bb11cc22dd33ee44");

    let all = CredentialSearch::empty()
        .with_env_key(Some(FAKE_KEY.to_owned()))
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_dotenv_dirs(vec![
            cwd_dir.path().to_path_buf(),
            manifest_dir.path().to_path_buf(),
        ])
        .with_app_data_dir(Some(app_dir.path().to_path_buf()));

    // 1. The process environment outranks every file.
    assert_eq!(
        resolve_llm_api_key_in(&all).expect("env wins").source(),
        CredentialSource::Env
    );

    // 2. $PULSE_CONFIG_DIR/.env next.
    let no_env = all.clone().with_env_key(None);
    assert_eq!(
        resolve_llm_api_key_in(&no_env)
            .expect("config dir wins")
            .source(),
        CredentialSource::ConfigDir
    );

    // 3. Then the cwd slot.
    let no_config = no_env.clone().with_config_dir(None);
    assert_eq!(
        resolve_llm_api_key_in(&no_config)
            .expect("cwd dotenv wins")
            .source(),
        CredentialSource::CwdDotenv
    );

    // 4. Then the manifest slot, answering with the SAME label once the cwd
    //    slot is peeled away.
    let manifest_slot = no_config.with_dotenv_dirs(vec![manifest_dir.path().to_path_buf()]);
    assert_eq!(
        resolve_llm_api_key_in(&manifest_slot)
            .expect("manifest dotenv wins")
            .source(),
        CredentialSource::CwdDotenv
    );

    // 5. Then the app-data dir.
    let only_app = manifest_slot.with_dotenv_dirs(Vec::new());
    assert_eq!(
        resolve_llm_api_key_in(&only_app)
            .expect("app-data wins")
            .source(),
        CredentialSource::AppDataDir
    );

    // 6. And an exhausted search is still an error.
    assert!(resolve_llm_api_key_in(&only_app.with_app_data_dir(None)).is_err());
}

/// The server profile narrows the chain to `Env` > `ConfigDir` > `AppDataDir`.
/// The cwd and manifest `.env`s are ignored even when they would outrank the
/// app-data dir — and even when they are the ONLY source.
#[test]
fn the_server_profile_searches_env_config_dir_and_app_data_only() {
    set_credential_profile(CredentialProfile::Server);

    let config_dir = tempfile::tempdir().expect("config tempdir");
    let cwd_dir = tempfile::tempdir().expect("cwd tempdir");
    let manifest_dir = tempfile::tempdir().expect("manifest tempdir");
    let app_dir = tempfile::tempdir().expect("app-data tempdir");
    write_dotenv(config_dir.path(), "sk-SERVCONFIGaa00bb11cc22dd33ee4");
    write_dotenv(cwd_dir.path(), "sk-SERVCWDDOTENVaa00bb11cc22dd33ee");
    write_dotenv(manifest_dir.path(), "sk-SERVMANIFESTaa00bb11cc22dd33e");
    write_dotenv(app_dir.path(), "sk-SERVAPPDATAaa00bb11cc22dd33ee44");

    let all = CredentialSearch::empty()
        .with_env_key(Some(FAKE_KEY.to_owned()))
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_dotenv_dirs(vec![
            cwd_dir.path().to_path_buf(),
            manifest_dir.path().to_path_buf(),
        ])
        .with_app_data_dir(Some(app_dir.path().to_path_buf()));

    assert_eq!(
        resolve_llm_api_key_in(&all).expect("env wins").source(),
        CredentialSource::Env
    );

    let no_env = all.clone().with_env_key(None);
    assert_eq!(
        resolve_llm_api_key_in(&no_env)
            .expect("config dir wins")
            .source(),
        CredentialSource::ConfigDir
    );

    // With the config dir peeled but BOTH dotenv slots populated and
    // answering, the server must skip straight to the app-data dir: the cwd
    // and manifest `.env`s would otherwise outrank it.
    let no_config = no_env.clone().with_config_dir(None);
    assert_eq!(
        resolve_llm_api_key_in(&no_config)
            .expect("app-data wins under Server")
            .source(),
        CredentialSource::AppDataDir
    );

    // And when the ignored slots are the ONLY source, there is no credential
    // at all — an exhausted-search error, and a `None` status read.
    let dotenv_only = CredentialSearch::empty().with_dotenv_dirs(vec![
        cwd_dir.path().to_path_buf(),
        manifest_dir.path().to_path_buf(),
    ]);
    let err = resolve_llm_api_key_in(&dotenv_only)
        .expect_err("cwd/manifest `.env`s must not answer under Server");
    assert!(
        matches!(&err, LlmError::Config(_)),
        "an exhausted search is a Config error, got {err:?}"
    );
    assert_eq!(
        llm_credential_status_in(&dotenv_only),
        CredentialStatus::None,
        "the status read agrees with the resolver"
    );
}

/// The permission check itself is unchanged — but under the server profile its
/// failure is what startup will refuse on, so the error must name the path and
/// never the value, and the status read must agree.
#[test]
fn a_refused_file_under_the_server_profile_is_an_error_naming_the_path() {
    set_credential_profile(CredentialProfile::Server);

    let config_dir = tempfile::tempdir().expect("config tempdir");
    let app_dir = tempfile::tempdir().expect("app-data tempdir");
    let dotenv = write_dotenv_mode(config_dir.path(), "sk-SERVERLOOSE0644aa0", 0o644);
    // A perfectly good lower-priority credential sits underneath: the refusal
    // must abort, not fall through to it.
    write_dotenv(app_dir.path(), FAKE_KEY);

    let search = CredentialSearch::empty()
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_app_data_dir(Some(app_dir.path().to_path_buf()));

    let err = resolve_llm_api_key_in(&search)
        .expect_err("a group/world-readable credential file is refused under Server too");
    assert!(
        matches!(&err, LlmError::Config(_)),
        "the refusal is a Config error, got {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains(dotenv.display().to_string().as_str()),
        "the refusal names the offending file; got: {message}"
    );
    assert!(
        !message.contains("SERVERLOOSE"),
        "the refusal must never contain the value; got: {message}"
    );
    assert_eq!(
        llm_credential_status_in(&search),
        CredentialStatus::None,
        "a refused file reads as None, as the desktop's banner already does"
    );
}

// ---- The process-level pair -------------------------------------------------
//
// The two cases below drive the REAL process path (`from_process_env` through
// the zero-arg status read) with the environment pointed at temp dirs, to pin
// the differential the item exists for: the SAME cwd-only layout answers
// `CwdDotenv` under the desktop profile and `None` under the server profile.
// Each runs in its own nextest process — see the module docs for why that is
// required, not incidental.

/// Isolates the credential-relevant environment (`HOME`, `XDG_DATA_HOME`,
/// `OLLAMA_API_KEY`, `PULSE_CONFIG_DIR`) into an empty temp dir and restores the
/// original values and working directory on drop.
struct EnvGuard {
    _dir: tempfile::TempDir,
    original_home: Option<OsString>,
    original_xdg: Option<OsString>,
    original_cwd: PathBuf,
}

impl EnvGuard {
    fn isolated() -> Self {
        let dir = tempfile::tempdir().expect("env-guard tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create the isolated home");
        let original_home = std::env::var_os("HOME");
        let original_xdg = std::env::var_os("XDG_DATA_HOME");
        let original_cwd = std::env::current_dir().expect("record the cwd");
        // SAFETY: this test process is single-threaded and dedicated — nextest
        // runs each test in its own process, and this file spawns no threads —
        // so no other code can observe a half-mutated environment.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("XDG_DATA_HOME", &home);
            std::env::remove_var("OLLAMA_API_KEY");
            std::env::remove_var("PULSE_CONFIG_DIR");
        }
        Self {
            _dir: dir,
            original_home,
            original_xdg,
            original_cwd,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_cwd);
        // SAFETY: as in `isolated` — a dedicated, single-threaded process.
        unsafe {
            match &self.original_home {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
            match &self.original_xdg {
                Some(xdg) => std::env::set_var("XDG_DATA_HOME", xdg),
                None => std::env::remove_var("XDG_DATA_HOME"),
            }
        }
    }
}

/// The server process, a cwd-only `.env`: `llm_credential_status()` reads
/// `None` — AC-1(c)'s status half, since w2's credential-status route has not
/// landed on this branch yet.
#[test]
fn the_server_process_reports_none_when_only_a_cwd_dotenv_exists() {
    set_credential_profile(CredentialProfile::Server);
    let _guard = EnvGuard::isolated();
    let cwd = tempfile::tempdir().expect("cwd tempdir");
    write_dotenv(cwd.path(), FAKE_KEY);
    std::env::set_current_dir(cwd.path()).expect("move the process cwd");

    assert_eq!(
        llm_credential_status(),
        CredentialStatus::None,
        "the server profile ignores a cwd-only `.env`"
    );
}

/// The SAME layout under the untouched desktop default answers `CwdDotenv` —
/// the process-level proof that this item did not narrow the desktop's chain.
#[test]
fn the_desktop_process_still_answers_from_a_cwd_dotenv() {
    // No profile selected: the process default is Desktop.
    let _guard = EnvGuard::isolated();
    let cwd = tempfile::tempdir().expect("cwd tempdir");
    write_dotenv(cwd.path(), FAKE_KEY);
    std::env::set_current_dir(cwd.path()).expect("move the process cwd");

    assert_eq!(
        llm_credential_status(),
        CredentialStatus::CwdDotenv,
        "the desktop chain still reads the cwd `.env`"
    );
}
