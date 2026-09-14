//! r1.s1.w2 — the LLM credential resolver on the risk gate's own surface
//! (`src/adapters/secrets.rs`), driven from OUTSIDE the crate.
//!
//! This file is a separate crate, so it sees only the curated `pulse::` surface:
//! the `pub` injectable core (`resolve_llm_api_key_in` / `llm_credential_status_in`)
//! over an explicit [`CredentialSearch`], never the `pub(crate)` zero-arg wrappers
//! the CLI calls. That is deliberate — the search is INJECTED rather than read from
//! the process environment, so every case below is hermetic: no `set_var`, no
//! shared-env lock, no ordering coupling between tests.
//!
//! It also cannot read a resolved key's VALUE: `ApiKey::expose()` is `pub(crate)`
//! (the least-privilege control). Every assertion here is therefore about the
//! resolved SOURCE, the refusal, or the error text — which is exactly the surface a
//! caller outside the crate is allowed to see.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};

use pulse::{
    CredentialSearch, CredentialSource, CredentialStatus, FakeClock, LlmBackend, LlmCallId,
    LlmCallRepository, LlmConfig, LlmError, LlmProvider, LlmResponse, Message, ModelPrice,
    PriceTable, RedactingLoggingProvider, Redactor, SqliteLlmCallRepo, TokenUsage, ToolDefinition,
    llm_credential_status_in, resolve_llm_api_key_in,
};
use rust_decimal::Decimal;

/// An API-key-SHAPED literal. Not a real credential — it exists so a leak would be
/// greppable in an assertion, and so the redaction suite can look for it.
const FAKE_KEY: &str = "sk-CREDSRC1234abcd5678efgh9012ijkl3456";

/// Write a `.env` carrying `OLLAMA_API_KEY=<value>` into `dir`, at mode `0600`.
///
/// Written at `0600` from the start so the precedence case stays valid once the
/// fail-closed permission checks land — a precedence test that only passes on a
/// world-readable file would be testing the wrong thing.
fn write_dotenv(dir: &Path, value: &str) {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(".env");
    std::fs::write(&path, format!("OLLAMA_API_KEY={value}\n")).expect("write .env");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 0600 the .env");
}

// ---- AC-1: the resolver exists on the gate's surface and answers from the env ----

#[test]
fn resolves_the_key_from_the_process_environment() {
    let search = CredentialSearch::empty().with_env_key(Some(FAKE_KEY.to_owned()));

    let key = resolve_llm_api_key_in(&search).expect("an injected env key must resolve");

    assert_eq!(
        key.source(),
        CredentialSource::Env,
        "a key taken from the environment is labelled `Env`"
    );
}

// ---- AC-3: the precedence order itself is the thing under test -----------------

#[test]
fn precedence_is_env_then_config_dir_then_cwd_then_app_data() {
    // Every location is populated at once, then peeled back one at a time. Peeling
    // (rather than four independently-built searches) is what actually proves an
    // ORDER: each step leaves the lower-priority locations untouched and still
    // answering, so the only thing that can change the winner is precedence.
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let cwd_dir = tempfile::tempdir().expect("cwd tempdir");
    let app_dir = tempfile::tempdir().expect("app-data tempdir");
    write_dotenv(config_dir.path(), "sk-CONFIGDIR1234abcd5678efgh9012ijkl");
    write_dotenv(cwd_dir.path(), "sk-CWDDOTENV1234abcd5678efgh9012ijkl");
    write_dotenv(app_dir.path(), "sk-APPDATADIR1234abcd5678efgh9012ijk");

    let all = CredentialSearch::empty()
        .with_env_key(Some(FAKE_KEY.to_owned()))
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_dotenv_dirs(vec![cwd_dir.path().to_path_buf()])
        .with_app_data_dir(Some(app_dir.path().to_path_buf()));

    // 1. The process environment outranks every file.
    assert_eq!(
        resolve_llm_api_key_in(&all).expect("env wins").source(),
        CredentialSource::Env,
        "the process environment is resolution step 1"
    );

    // 2. $PULSE_CONFIG_DIR next — ADR-0014's overlay seam keeps precedence over the
    //    default locations, rather than being bypassed by them.
    let no_env = all.clone().with_env_key(None);
    assert_eq!(
        resolve_llm_api_key_in(&no_env)
            .expect("config dir wins")
            .source(),
        CredentialSource::ConfigDir,
        "$PULSE_CONFIG_DIR/.env is resolution step 2, ahead of cwd and app-data"
    );

    // 3. Then the gitignored working-directory / manifest-directory `.env`.
    let no_config = no_env.clone().with_config_dir(None);
    assert_eq!(
        resolve_llm_api_key_in(&no_config)
            .expect("cwd dotenv wins")
            .source(),
        CredentialSource::CwdDotenv,
        "the cwd/manifest `.env` is resolution step 3"
    );

    // 4. Finally the application data directory — the new, Finder-launchable
    //    fallback that has no shell behind it.
    let only_app = no_config.clone().with_dotenv_dirs(Vec::new());
    assert_eq!(
        resolve_llm_api_key_in(&only_app)
            .expect("app data dir wins")
            .source(),
        CredentialSource::AppDataDir,
        "the application data directory `.env` is resolution step 4"
    );

    // 5. With every location peeled away there is no credential at all.
    let none = only_app.with_app_data_dir(None);
    assert!(
        resolve_llm_api_key_in(&none).is_err(),
        "an exhausted search must be an error, never a silent empty key"
    );
}

// ---- AC-4: least privilege, part 1 — the mode check, fail-closed ---------------

/// `chmod` an existing path.
fn chmod(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

#[test]
fn refuses_group_or_world_readable_credential_file() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let fallback_dir = tempfile::tempdir().expect("fallback tempdir");
    let secret = "sk-LOOSEMODE1234abcd5678efgh9012ijkl";
    write_dotenv(config_dir.path(), secret);
    // A perfectly good lower-priority credential sits underneath, so a refusal that
    // "falls through" would look like a success. It must NOT.
    write_dotenv(fallback_dir.path(), "sk-FALLBACK1234abcd5678efgh9012ijklm");

    let search = CredentialSearch::empty()
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_app_data_dir(Some(fallback_dir.path().to_path_buf()));
    let dotenv = config_dir.path().join(".env");

    for loose in [0o644, 0o640, 0o604, 0o660, 0o606, 0o666] {
        chmod(&dotenv, loose);
        let err = resolve_llm_api_key_in(&search)
            .err()
            .unwrap_or_else(|| panic!("mode {loose:o} must be refused, not accepted"));
        let message = err.to_string();

        assert!(
            message.contains(&dotenv.display().to_string()),
            "the refusal names the offending file; got: {message}"
        );
        assert!(
            message.contains(&format!("{loose:04o}")),
            "the refusal names the offending mode {loose:04o}; got: {message}"
        );
        assert!(
            message.contains("chmod 0600"),
            "the refusal says how to fix it; got: {message}"
        );
        assert!(
            !message.contains(secret),
            "a refusal must never contain the value — the file is never read"
        );
    }

    // Fail-closed means REFUSED, not "skipped": the lower-priority location that
    // would have answered must not rescue the run.
    chmod(&dotenv, 0o644);
    assert!(
        resolve_llm_api_key_in(&search).is_err(),
        "a refused file must abort resolution, never silently downgrade to the next \
         location — otherwise the operator never learns their key file is exposed"
    );

    // 0600 is accepted, and so is anything STRICTER: the rule is "no group or world
    // bits", not "exactly 0600". A strict-equality check would refuse `0400`, a file
    // safer than the one it accepts.
    for tight in [0o600, 0o400] {
        chmod(&dotenv, tight);
        assert_eq!(
            resolve_llm_api_key_in(&search)
                .unwrap_or_else(|e| panic!("mode {tight:o} must be accepted, got: {e}"))
                .source(),
            CredentialSource::ConfigDir,
            "mode {tight:04o} is owner-only and must be accepted"
        );
    }
}

// ---- Codex P2 (src/adapters/secrets.rs read_credential_file): the permission ----
// ---- checks follow a symlink to its TARGET's inode, not its own -----------------

/// The fix opens the candidate path once and validates the OPEN HANDLE
/// (`File::metadata`, an `fstat`) rather than the path. `File::open` follows a
/// symlink to its target, so the owner/mode checked are the TARGET's — meaning a
/// symlink whose target is group- or world-readable must be refused exactly as a
/// directly loose file would be. This is what proves the fix closes the TOCTOU: a
/// weaker check (e.g. `symlink_metadata`/`lstat` on the path) would validate the
/// link itself, which is typically `0777` and owned by whoever created it, and would
/// never see the exposed target underneath.
#[test]
fn refuses_a_symlink_whose_target_is_group_or_world_readable() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let fallback_dir = tempfile::tempdir().expect("fallback tempdir");
    let target_dir = tempfile::tempdir().expect("target tempdir");

    let target = target_dir.path().join("real-credential");
    let secret = "sk-SYMLINKTARGET1234abcd5678efgh9012i";
    std::fs::write(&target, format!("OLLAMA_API_KEY={secret}\n")).expect("write target");
    chmod(&target, 0o644);

    let link = config_dir.path().join(".env");
    std::os::unix::fs::symlink(&target, &link).expect("create the symlink");

    // A perfectly good lower-priority credential sits underneath, so a refusal that
    // "falls through" would look like a success. It must NOT.
    write_dotenv(fallback_dir.path(), "sk-FALLBACK1234abcd5678efgh9012ijklm");

    let search = CredentialSearch::empty()
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_app_data_dir(Some(fallback_dir.path().to_path_buf()));

    let err = resolve_llm_api_key_in(&search)
        .expect_err("a symlink to a group/world-readable target must be refused");
    let message = err.to_string();

    assert!(
        message.contains(&link.display().to_string()),
        "the refusal names the searched (symlink) path; got: {message}"
    );
    assert!(
        message.contains("0644") && message.contains("chmod 0600"),
        "the refusal names the target's failing mode and the fix; got: {message}"
    );
    assert!(
        !message.contains(secret),
        "a refusal must never contain the value — the file is never read"
    );

    // Fail-closed, not fall-through: the readable lower-priority location must not
    // rescue a run whose higher-priority credential is an exposed symlink target.
    assert!(
        resolve_llm_api_key_in(&search).is_err(),
        "an exposed symlink target must abort resolution, never silently downgrade"
    );
}

// ---- AC-5: least privilege, part 2 — the ownership check, fail-closed ----------

#[test]
fn refuses_credential_file_not_owned_by_running_user() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let fallback_dir = tempfile::tempdir().expect("fallback tempdir");
    let secret = "sk-WRONGOWNER1234abcd5678efgh9012ij";
    write_dotenv(config_dir.path(), secret);
    write_dotenv(fallback_dir.path(), "sk-FALLBACK1234abcd5678efgh9012ijklm");
    let dotenv = config_dir.path().join(".env");

    // The test cannot `chown` a file to another user without privileges, so it moves
    // the OTHER side of the comparison instead: the uid the resolver believes it is
    // running as. That exercises the real check on a real file, and it is the only
    // way to reach this path in an unprivileged suite.
    let owner = file_owner_uid(&dotenv);
    let someone_else = owner.wrapping_add(1);

    let search = CredentialSearch::empty()
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_app_data_dir(Some(fallback_dir.path().to_path_buf()))
        .with_running_uid(someone_else);

    let err = resolve_llm_api_key_in(&search)
        .expect_err("a credential file owned by another user must be refused");
    let message = err.to_string();

    assert!(
        message.contains(&dotenv.display().to_string()),
        "the refusal names the offending file; got: {message}"
    );
    assert!(
        message.contains(&owner.to_string()) && message.contains(&someone_else.to_string()),
        "the refusal names both the file's owner and the running user; got: {message}"
    );
    assert!(
        message.contains("chown"),
        "the refusal says how to fix it; got: {message}"
    );
    assert!(
        !message.contains(secret),
        "a refusal must never contain the value — the file is never read"
    );

    // Fail-closed, not fall-through: the readable lower-priority location must not
    // rescue a run whose higher-priority credential file is owned by someone else.
    assert!(
        resolve_llm_api_key_in(&search).is_err(),
        "a wrongly-owned file must abort resolution, never silently downgrade"
    );

    // The same file resolves cleanly once the running uid is its actual owner —
    // proving the refusal came from the ownership check and nothing else.
    assert_eq!(
        resolve_llm_api_key_in(&search.clone().with_running_uid(owner))
            .expect("the owner may read their own 0600 file")
            .source(),
        CredentialSource::ConfigDir,
    );
}

/// The uid owning `path`, read straight from the filesystem metadata.
fn file_owner_uid(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .expect("stat the credential file")
        .uid()
}

// ---- Codex P2 (src/adapters/secrets.rs read_credential_file): a metadata error --
// ---- must abort resolution, not silently fall through to a lower-priority key ---

/// Regression guard for the fix: a location whose `.env` genuinely does not exist
/// (`std::fs::metadata` fails with `NotFound`) is still an ORDINARY miss and must
/// keep falling through to the next location — the fix narrows which errors abort
/// resolution, it must not turn every miss into a refusal.
#[test]
fn a_missing_credential_file_still_falls_through_to_the_next_location() {
    let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
    let fallback_dir = tempfile::tempdir().expect("fallback tempdir");
    write_dotenv(fallback_dir.path(), "sk-FALLTHROUGH1234abcd5678efgh9012ijk");

    // `empty_config_dir` exists but holds no `.env` at all, so `metadata()` on the
    // candidate path fails with `NotFound` — the ordinary-miss case.
    let search = CredentialSearch::empty()
        .with_config_dir(Some(empty_config_dir.path().to_path_buf()))
        .with_app_data_dir(Some(fallback_dir.path().to_path_buf()));

    assert_eq!(
        resolve_llm_api_key_in(&search)
            .expect("a NotFound location must fall through, not abort resolution")
            .source(),
        CredentialSource::AppDataDir,
        "the absent config-dir `.env` must not shadow the answering fallback"
    );
}

/// A `RAII` guard that restores a directory's permissions on drop, so a failed
/// assertion (or an early return) still leaves the `TempDir` cleanable — an
/// unrestored `0o000` directory cannot be deleted by its own `Drop`.
struct RestorePerms(PathBuf);

impl Drop for RestorePerms {
    fn drop(&mut self) {
        chmod(&self.0, 0o700);
    }
}

/// Codex P2: `read_credential_file` used to swallow EVERY `std::fs::metadata`
/// failure as `Ok(None)` — an ordinary miss — which silently downgraded to a
/// lower-priority credential on `EACCES`/`EIO`/`ELOOP`/`ENOTDIR`, exactly the
/// "never silently downgraded" property the function's own doc comment promises.
/// This proves the fix: a metadata error that is NOT `NotFound` aborts the whole
/// resolution with `LlmError::Config`, naming the path and no credential material,
/// rather than falling through to the readable fallback underneath it.
///
/// Produced portably (macOS + Linux) by chmod-ing the PARENT directory to `0o000`:
/// with no search permission on that directory, `std::fs::metadata` on a path
/// inside it fails with `PermissionDenied`, regardless of whether the file itself
/// exists.
#[test]
fn a_metadata_error_other_than_not_found_aborts_rather_than_downgrades() {
    let parent = tempfile::tempdir().expect("parent tempdir");
    let locked_dir = parent.path().join("locked");
    std::fs::create_dir(&locked_dir).expect("create the subdirectory to lock");
    let candidate = locked_dir.join(".env");

    let fallback_dir = tempfile::tempdir().expect("fallback tempdir");
    write_dotenv(fallback_dir.path(), "sk-SHOULDNOTWIN1234abcd5678efgh9012ij");

    chmod(&locked_dir, 0o000);
    // Restored on drop (including on an assertion panic) so the outer `TempDir`s
    // can actually be removed at the end of the test.
    let _restore = RestorePerms(locked_dir.clone());

    let search = CredentialSearch::empty()
        .with_config_dir(Some(locked_dir.clone()))
        .with_app_data_dir(Some(fallback_dir.path().to_path_buf()));

    let err = resolve_llm_api_key_in(&search).expect_err(
        "a metadata error other than NotFound must abort resolution, never downgrade \
         to the readable fallback underneath it",
    );
    assert!(
        matches!(&err, LlmError::Config(_)),
        "must be a Config error, got {err:?}"
    );
    let message = err.to_string();
    assert!(
        message.contains(&candidate.display().to_string()),
        "the refusal names the offending path; got: {message}"
    );
    assert!(
        !message.contains("SHOULDNOTWIN"),
        "the refusal must contain no credential material; got: {message}"
    );
}

// ---- AC-7: the audit trail — the ledger records the LABEL, never the value ------

/// A provider double that answers once with a known token usage, so the REAL
/// decorator prices the call and writes a REAL `LlmCall` row. No network.
struct FakeProvider;

impl LlmProvider for FakeProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
        std::future::ready(Ok(LlmResponse {
            content: Some("ok".to_owned()),
            tool_calls: Vec::new(),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }))
    }
}

/// A test price table for the model [`chat_config`] names.
fn test_prices() -> PriceTable {
    let mut models = HashMap::new();
    models.insert(
        "glm-5.2".to_owned(),
        ModelPrice {
            input_per_mtok: Decimal::from(2),
            output_per_mtok: Decimal::from(8),
        },
    );
    PriceTable::from_config("USD", models)
}

/// The per-request chat config for the priced test model.
fn chat_config() -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: "glm-5.2".to_owned(),
        temperature: 0.2,
        max_tokens: 256,
        reasoning_effort: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_call_records_key_source_label_not_value() {
    let tmp = tempfile::tempdir().expect("db tempdir");
    let db = pulse::Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    pulse::MIGRATOR
        .run(db.pool())
        .await
        .expect("run migrations");

    // Resolve a real credential from a real (0600, owned) file, then hand the
    // resolved SOURCE — not the key — to the decorator. This is the whole point of
    // the audit trail: the thing that reaches the ledger is a label.
    let config_dir = tempfile::tempdir().expect("config tempdir");
    write_dotenv(config_dir.path(), FAKE_KEY);
    let key = resolve_llm_api_key_in(
        &CredentialSearch::empty().with_config_dir(Some(config_dir.path().to_path_buf())),
    )
    .expect("credential resolves");
    assert_eq!(key.source(), CredentialSource::ConfigDir);

    let clock = FakeClock::at(1_700_000_000_000);
    let repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let provider = RedactingLoggingProvider::new(
        FakeProvider,
        repo,
        clock,
        // Tag the live key value so an accidental echo would be scrubbed at rest.
        Redactor::from_config(vec![FAKE_KEY.to_owned()]),
        test_prices(),
    )
    .with_key_source(Some(key.source()));

    let no_tools: &[ToolDefinition] = &[];
    provider
        .chat(
            vec![Message::user("size a BTC scalp")],
            no_tools,
            &chat_config(),
        )
        .await
        .expect("the decorated call succeeds");

    // The persisted row carries the kebab-case LABEL for the source that answered.
    let stored: Vec<Option<String>> = sqlx::query_scalar("SELECT key_source FROM llm_call")
        .fetch_all(db.pool())
        .await
        .expect("read key_source column");
    assert_eq!(
        stored,
        vec![Some("config-dir".to_owned())],
        "the ledger records which source answered, as a label"
    );

    // And the whole row — every column, not just the one we added — is free of the
    // key. A `key_source` that recorded the value would satisfy the assertion above
    // if the label happened to match; this is what actually forbids it.
    let dump: Vec<String> = sqlx::query_scalar(
        "SELECT id || backend || model || prompt_messages || COALESCE(completion, '') \
         || cost || cost_currency || created_at || created_by || COALESCE(key_source, '') \
         FROM llm_call",
    )
    .fetch_all(db.pool())
    .await
    .expect("dump the row");
    assert_eq!(dump.len(), 1, "exactly one ledger row was written");
    assert!(
        !dump[0].contains(FAKE_KEY),
        "the persisted LlmCall leaked the key value: {}",
        dump[0]
    );

    // The typed read-back agrees with the column, so provenance is reconstructible
    // through the domain type and not only by raw SQL.
    let id: String = sqlx::query_scalar("SELECT id FROM llm_call")
        .fetch_one(db.pool())
        .await
        .expect("read the row id");
    let repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let call = repo
        .get_call(&LlmCallId::new(id))
        .await
        .expect("get_call")
        .expect("row present");
    assert_eq!(call.key_source, Some(CredentialSource::ConfigDir));
}

// ---- Codex P1 (src/domain/secret.rs CredentialSource): the macOS Keychain is ----
// ---- now a representable, auditable credential source --------------------------

/// `CredentialSource::Keychain`'s serde tag is exactly `keychain` (matching its
/// four siblings' kebab-case tags) and round-trips through `serde_json` — this is
/// the literal string `llm_call.key_source` stores for a `pulse llm-check` row.
#[test]
fn keychain_source_serializes_to_the_kebab_case_tag_and_round_trips() {
    let json = serde_json::to_string(&CredentialSource::Keychain).expect("serialize Keychain");
    assert_eq!(json, "\"keychain\"");
    let back: CredentialSource = serde_json::from_str(&json).expect("deserialize Keychain");
    assert_eq!(back, CredentialSource::Keychain);
}

/// Codex P1: every `llm_call` row `pulse llm-check` writes used to record
/// `key_source = NULL` — indistinguishable from a legacy pre-migration row, on a
/// table that is trigger-immutable (`llm_call_no_update`/`llm_call_no_delete`), so
/// those rows could never be repaired. `llm-check`'s ONLY credential source is the
/// macOS Keychain (`glm_api_key`, never the file/env precedence chain
/// `resolve_llm_api_key_in` walks), so this drives the exact seam its composition
/// root (`run_llm_check_core` in `src/cli/llm.rs`) uses —
/// `RedactingLoggingProvider::with_key_source(Some(CredentialSource::Keychain))` —
/// over a FAKE provider + a tempfile-`Db` repo: never a live provider, never the
/// network, never the real Keychain (MASTER-SPEC §9.4).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_call_records_keychain_key_source_label() {
    let tmp = tempfile::tempdir().expect("db tempdir");
    let db = pulse::Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    pulse::MIGRATOR
        .run(db.pool())
        .await
        .expect("run migrations");

    let clock = FakeClock::at(1_700_000_100_000);
    let repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let provider = RedactingLoggingProvider::new(
        FakeProvider,
        repo,
        clock,
        Redactor::from_config(vec![FAKE_KEY.to_owned()]),
        test_prices(),
    )
    .with_key_source(Some(CredentialSource::Keychain));

    let no_tools: &[ToolDefinition] = &[];
    provider
        .chat(
            vec![Message::user("size a BTC scalp")],
            no_tools,
            &chat_config(),
        )
        .await
        .expect("the decorated call succeeds");

    // The raw column carries the label, as text.
    let stored: Vec<Option<String>> = sqlx::query_scalar("SELECT key_source FROM llm_call")
        .fetch_all(db.pool())
        .await
        .expect("read key_source column");
    assert_eq!(
        stored,
        vec![Some("keychain".to_owned())],
        "the ledger records the keychain provenance label"
    );

    // The typed read-back agrees, so provenance is reconstructible through the
    // domain type and not only by raw SQL.
    let id: String = sqlx::query_scalar("SELECT id FROM llm_call")
        .fetch_one(db.pool())
        .await
        .expect("read the row id");
    let repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let call = repo
        .get_call(&LlmCallId::new(id))
        .await
        .expect("get_call")
        .expect("row present");
    assert_eq!(call.key_source, Some(CredentialSource::Keychain));
}

// ---- AC-8: the error is a diagnosis, not a shrug --------------------------------

#[test]
fn error_names_every_searched_location_and_the_failed_check() {
    // ---- (a) nothing found anywhere ----------------------------------------
    // Three real, EMPTY directories: the search visits each one and finds nothing,
    // so the error has to be able to name a location it looked at and did not use.
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let cwd_dir = tempfile::tempdir().expect("cwd tempdir");
    let manifest_dir = tempfile::tempdir().expect("manifest tempdir");
    let app_dir = tempfile::tempdir().expect("app tempdir");

    let search = CredentialSearch::empty()
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_dotenv_dirs(vec![
            cwd_dir.path().to_path_buf(),
            manifest_dir.path().to_path_buf(),
        ])
        .with_app_data_dir(Some(app_dir.path().to_path_buf()));

    let message = resolve_llm_api_key_in(&search)
        .expect_err("an exhausted search must error")
        .to_string();

    assert!(
        message.contains("OLLAMA_API_KEY"),
        "the error names the environment variable it looked for; got: {message}"
    );
    for dir in [&config_dir, &cwd_dir, &manifest_dir, &app_dir] {
        let expected = dir.path().join(".env");
        assert!(
            message.contains(&expected.display().to_string()),
            "the error must name EVERY location searched — {} is missing from: {message}",
            expected.display()
        );
    }
    assert!(
        message.contains("not yet supported"),
        "the error says plainly that seeding from inside the app is not yet \
         supported, so the operator stops looking for a command that would do it; \
         got: {message}"
    );
    assert!(
        !message.contains("setup-keys"),
        "the error must NEVER name `pulse setup-keys` — that verb does not exist and \
         is not being built, and pointing an operator at it wastes their afternoon; \
         got: {message}"
    );

    // ---- (b) a file was found and REFUSED: which check, and how to fix it ----
    let refused_dir = tempfile::tempdir().expect("refused tempdir");
    write_dotenv(refused_dir.path(), "sk-REFUSED1234abcd5678efgh9012ijklmn");
    let dotenv = refused_dir.path().join(".env");
    let refused = CredentialSearch::empty().with_config_dir(Some(refused_dir.path().to_path_buf()));

    chmod(&dotenv, 0o644);
    let mode_message = resolve_llm_api_key_in(&refused)
        .expect_err("a 0644 file is refused")
        .to_string();
    assert!(
        mode_message.contains("0644") && mode_message.contains("chmod 0600"),
        "a mode refusal names the failing mode AND the fix; got: {mode_message}"
    );
    assert!(
        !mode_message.contains("setup-keys"),
        "no error path may name `pulse setup-keys`; got: {mode_message}"
    );

    chmod(&dotenv, 0o600);
    let owner = file_owner_uid(&dotenv);
    let owner_message = resolve_llm_api_key_in(&refused.clone().with_running_uid(owner + 1))
        .expect_err("a wrongly-owned file is refused")
        .to_string();
    assert!(
        owner_message.contains(&owner.to_string()) && owner_message.contains("chown"),
        "an ownership refusal names the owning uid AND the fix; got: {owner_message}"
    );
    assert!(
        !owner_message.contains("setup-keys"),
        "no error path may name `pulse setup-keys`; got: {owner_message}"
    );

    // The two refusals are DISTINGUISHABLE. An operator who is told only "refused"
    // has to guess between chmod and chown; naming the check is the whole point.
    assert_ne!(
        mode_message, owner_message,
        "the two refusal paths must say different things"
    );
}

// ---- AC-10: the banner seam reports the source and nothing else -----------------

#[test]
fn credential_status_reports_source_without_the_value() {
    let config_dir = tempfile::tempdir().expect("config tempdir");
    let app_dir = tempfile::tempdir().expect("app tempdir");
    write_dotenv(config_dir.path(), FAKE_KEY);
    write_dotenv(app_dir.path(), FAKE_KEY);

    // Each source reports itself, and reports it from the SAME precedence chain the
    // resolver uses — a banner that disagreed with the resolver would be worse than
    // no banner at all.
    let cases = [
        (
            CredentialSearch::empty().with_env_key(Some(FAKE_KEY.to_owned())),
            CredentialStatus::Env,
        ),
        (
            CredentialSearch::empty().with_config_dir(Some(config_dir.path().to_path_buf())),
            CredentialStatus::ConfigDir,
        ),
        (
            CredentialSearch::empty().with_dotenv_dirs(vec![config_dir.path().to_path_buf()]),
            CredentialStatus::CwdDotenv,
        ),
        (
            CredentialSearch::empty().with_app_data_dir(Some(app_dir.path().to_path_buf())),
            CredentialStatus::AppDataDir,
        ),
        (CredentialSearch::empty(), CredentialStatus::None),
    ];

    for (search, expected) in cases {
        let status = llm_credential_status_in(&search);
        assert_eq!(status, expected, "wrong source reported for {search:?}");

        // The status carries no key material — not the value, not a fragment of it.
        // This is what makes it safe to render in a UI and to send across the Tauri
        // IPC boundary that `r1.s1.w5` will put it behind.
        let rendered = format!("{status:?}");
        assert!(
            !rendered.contains(FAKE_KEY) && !rendered.contains(&FAKE_KEY[..12]),
            "the status leaked key material: {rendered}"
        );
        let serialized = serde_json::to_string(&status).expect("status serializes");
        assert!(
            !serialized.contains(FAKE_KEY) && !serialized.contains(&FAKE_KEY[..12]),
            "the serialized status leaked key material: {serialized}"
        );
    }

    // A credential file that EXISTS but is refused reports `None`, not a usable
    // source: a banner saying "credential found" over a file the resolver will
    // refuse would send the operator hunting the wrong problem.
    let loose_dir = tempfile::tempdir().expect("loose tempdir");
    write_dotenv(loose_dir.path(), FAKE_KEY);
    chmod(&loose_dir.path().join(".env"), 0o644);
    assert_eq!(
        llm_credential_status_in(
            &CredentialSearch::empty().with_config_dir(Some(loose_dir.path().to_path_buf()))
        ),
        CredentialStatus::None,
        "a refused credential file is not a usable credential"
    );
}

/// Migration `0007` adds `key_source` as a NULLABLE column and the row-schema tag
/// stays at 1. That is not a cosmetic choice: `get_call` fail-closes on any stored
/// tag that is not the current constant, so bumping the tag would make every
/// pre-migration row permanently unreadable — data stranding, and the opposite of
/// ADR-0018's forward-only intent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pre_migration_row_without_a_key_source_still_reads_back() {
    let tmp = tempfile::tempdir().expect("db tempdir");
    let db = pulse::Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    pulse::MIGRATOR
        .run(db.pool())
        .await
        .expect("run migrations");

    // A row exactly as VS-1.3.1 wrote them: schema_version 1, no key_source.
    sqlx::query(
        "INSERT INTO llm_call \
         (id, backend, model, prompt_messages, completion, input_tokens, output_tokens, \
          cost, cost_currency, created_at, created_by, schema_version) \
         VALUES ('legacy-1', 'ollama', 'glm-5.2', '[]', NULL, 1, 1, '0', 'USD', \
                 '2026-06-30T00:00:00.000Z', 'human', 1)",
    )
    .execute(db.pool())
    .await
    .expect("seed a pre-migration row");

    let repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), FakeClock::at(0));
    let call = repo
        .get_call(&LlmCallId::new("legacy-1"))
        .await
        .expect("a pre-migration row must still be readable")
        .expect("row present");
    assert_eq!(
        call.key_source, None,
        "a row written before 0007 has no recorded provenance, which is `None`, not \
         an error"
    );
}

/// A FIFO at a searched location is an ordinary miss, not a hang.
///
/// The resolver validates the OPEN HANDLE rather than the path (the fix for a
/// check-then-read race), which moved the open BEFORE `is_file()` can reject a
/// non-regular file. Opening a FIFO for reading blocks until a writer appears, so
/// without `O_NONBLOCK` a named pipe left at an operator-controlled overlay
/// location would hang the resolver forever — and `llm_credential_status_in` walks
/// this same chain from the credential banner's paint, so it would hang the UI,
/// producing neither an error nor a miss.
///
/// The test completing at all is the assertion.
#[test]
fn a_fifo_at_a_searched_location_is_a_miss_rather_than_a_hang() {
    use std::os::unix::ffi::OsStrExt as _;

    let config_dir = tempfile::tempdir().expect("config tempdir");
    let fallback_dir = tempfile::tempdir().expect("fallback tempdir");

    let fifo = config_dir.path().join(".env");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).expect("path has no NUL");
    // SAFETY: `mkfifo` takes a NUL-terminated path and a mode. The path stays owned
    // by the `CString` for the whole call, and the mode is a plain constant.
    let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    assert_eq!(
        made,
        0,
        "mkfifo failed: {}",
        std::io::Error::last_os_error()
    );

    // A real credential sits at a LOWER-priority location, so a correct run finds it
    // and a hang is unambiguous.
    write_dotenv(fallback_dir.path(), FAKE_KEY);

    let search = CredentialSearch::empty()
        .with_config_dir(Some(config_dir.path().to_path_buf()))
        .with_app_data_dir(Some(fallback_dir.path().to_path_buf()));

    let key = resolve_llm_api_key_in(&search).expect("a FIFO must not stall the resolution");
    assert_eq!(
        key.source(),
        CredentialSource::AppDataDir,
        "a named pipe is a miss, so resolution falls through to the real credential"
    );

    // The banner's read walks the same chain and must not hang either.
    assert_eq!(
        llm_credential_status_in(&search),
        CredentialStatus::AppDataDir
    );
}

/// The "no credential" message names the real environment variable, not the Rust
/// const that holds its name.
///
/// A reviewer read `write!(message, "... ${PULSE_CONFIG_DIR_ENV} ...")` as having no
/// format arguments and concluded the literal identifier `PULSE_CONFIG_DIR_ENV`
/// reached the user, who would then export the wrong variable and go in circles.
/// It does not — that is an inline format capture and it renders the const's VALUE.
/// This test is the standing proof, so the next reader does not have to re-derive it
/// and nobody "fixes" the capture away.
#[test]
fn the_missing_credential_message_names_the_real_environment_variable() {
    // No config dir configured, so the message appends the overlay note.
    let search = CredentialSearch::empty();
    let message = resolve_llm_api_key_in(&search)
        .expect_err("an empty search resolves nothing")
        .to_string();

    assert!(
        message.contains("${PULSE_CONFIG_DIR}"),
        "the note must name the variable a user can actually export: {message}"
    );
    assert!(
        !message.contains("PULSE_CONFIG_DIR_ENV"),
        "the Rust const's NAME must never reach the user: {message}"
    );
}
