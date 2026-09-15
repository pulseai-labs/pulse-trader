//! r1.s1.w2 — the **no-secret-in-log** control of the *LLM credential handling and
//! redaction* risk gate (AC-6).
//!
//! One claim, asserted from outside the crate: **the key's value appears nowhere.**
//! Not in a `Debug` rendering, not in a `Display` rendering (there is none — that is
//! asserted structurally, not assumed), not in any error message including the two
//! refusal paths, and not in any persisted `LlmCall` field.
//!
//! The distinction that matters here is *by construction* versus *by policy*. A
//! comment saying "do not log the key" is policy; a type with no `Display` impl and
//! a hand-written `Debug` that cannot reach the value is construction. This file
//! tests the construction.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::path::Path;

use pulse::{
    ApiKey, CredentialSearch, FakeClock, LlmBackend, LlmConfig, LlmError, LlmProvider, LlmResponse,
    Message, ModelPrice, PriceTable, RedactingLoggingProvider, Redactor, SqliteLlmCallRepo,
    TokenUsage, ToolDefinition, resolve_llm_api_key_in,
};
use rust_decimal::Decimal;

/// An API-key-SHAPED literal, not a real credential. Every assertion below is a
/// substring search for exactly this string.
const FAKE_KEY: &str = "sk-REDACTME1234abcd5678efgh9012ijkl3456";

// ---------------------------------------------------------------------------
// A compile-time probe for "does `T` implement `Display`?".
//
// Inherent associated items take precedence over trait ones, but only when their
// bounds are satisfiable — so `Probe::<T>::IMPLEMENTS_DISPLAY` resolves to the
// inherent `true` when `T: Display` and falls back to the trait's `false` when it
// does not. This turns "ApiKey must not implement Display" from a reviewer's job
// into a failing test the moment someone adds the impl.
// ---------------------------------------------------------------------------

struct Probe<T>(PhantomData<T>);

trait ProbeFallback {
    const IMPLEMENTS_DISPLAY: bool = false;
}

impl<T> ProbeFallback for Probe<T> {}

impl<T: std::fmt::Display> Probe<T> {
    const IMPLEMENTS_DISPLAY: bool = true;
}

/// Sanity check on the probe itself: a type that DOES implement `Display` must
/// probe `true`. Without this, a probe silently stuck at `false` would make the
/// assertion below vacuous — it would "pass" for every type in the language.
struct DefinitelyDisplays;

impl std::fmt::Display for DefinitelyDisplays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("x")
    }
}

/// Write a `.env` carrying `OLLAMA_API_KEY=<value>` into `dir` at `mode`.
fn write_dotenv(dir: &Path, value: &str, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.join(".env");
    std::fs::write(&path, format!("OLLAMA_API_KEY={value}\n")).expect("write .env");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

/// The uid owning `path`.
fn file_owner_uid(path: &Path) -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .expect("stat the credential file")
        .uid()
}

// ---- The key cannot be rendered -----------------------------------------------

/// Both assertions are `const` blocks, so they are evaluated at COMPILE time: adding
/// a `Display` impl to `ApiKey` does not produce a red test, it produces a build
/// failure. That is the strongest form the claim can take, and it is what
/// `clippy::assertions_on_constants` asks for once the condition is a constant.
#[test]
fn the_probe_itself_detects_a_display_impl() {
    const {
        assert!(
            Probe::<DefinitelyDisplays>::IMPLEMENTS_DISPLAY,
            "the Display probe must report true for a type that does implement it — \
             otherwise the ApiKey assertion below proves nothing"
        );
    }
}

#[test]
fn api_key_implements_no_display() {
    const {
        assert!(
            !Probe::<ApiKey>::IMPLEMENTS_DISPLAY,
            "ApiKey must not implement Display: formatting a key with the display \
             placeholder is a leak, and the type must make that impossible rather \
             than merely discouraged"
        );
    }
}

#[test]
fn api_key_debug_never_renders_the_value() {
    let key =
        resolve_llm_api_key_in(&CredentialSearch::empty().with_env_key(Some(FAKE_KEY.to_owned())))
            .expect("env key resolves");

    let rendered = format!("{key:?}");
    assert!(
        !rendered.contains(FAKE_KEY),
        "ApiKey's Debug leaked the value: {rendered}"
    );
    // Not even a fragment: a prefix long enough to identify the credential is still
    // a leak, and a "first 8 characters" style Debug is the usual way this regresses.
    assert!(
        !rendered.contains(&FAKE_KEY[..12]),
        "ApiKey's Debug leaked a fragment of the value: {rendered}"
    );
    assert!(
        rendered.contains("redacted"),
        "ApiKey's Debug should say plainly that the value is withheld: {rendered}"
    );
}

#[test]
fn credential_search_debug_never_renders_the_env_key() {
    let search = CredentialSearch::empty().with_env_key(Some(FAKE_KEY.to_owned()));
    let rendered = format!("{search:?}");
    assert!(
        !rendered.contains(FAKE_KEY),
        "CredentialSearch's Debug leaked the injected key: {rendered}"
    );
}

// ---- No error message carries the value, including both refusal paths ----------

#[test]
fn no_refusal_error_contains_the_value() {
    // Mode refusal (step 3's group/world check).
    let loose_dir = tempfile::tempdir().expect("tempdir");
    write_dotenv(loose_dir.path(), FAKE_KEY, 0o644);
    let loose = CredentialSearch::empty().with_config_dir(Some(loose_dir.path().to_path_buf()));
    let mode_error = resolve_llm_api_key_in(&loose)
        .expect_err("a 0644 credential file is refused")
        .to_string();
    assert!(
        !mode_error.contains(FAKE_KEY),
        "the mode refusal leaked the value: {mode_error}"
    );

    // Ownership refusal (step 3's owner check).
    let owned_dir = tempfile::tempdir().expect("tempdir");
    write_dotenv(owned_dir.path(), FAKE_KEY, 0o600);
    let dotenv = owned_dir.path().join(".env");
    let not_me = file_owner_uid(&dotenv).wrapping_add(1);
    let wrong_owner = CredentialSearch::empty()
        .with_config_dir(Some(owned_dir.path().to_path_buf()))
        .with_running_uid(not_me);
    let owner_error = resolve_llm_api_key_in(&wrong_owner)
        .expect_err("a file owned by another user is refused")
        .to_string();
    assert!(
        !owner_error.contains(FAKE_KEY),
        "the ownership refusal leaked the value: {owner_error}"
    );

    // Both refusals happen BEFORE the file is read, which is why they structurally
    // cannot contain the value: prove the accepted path really would have read it.
    let good_dir = tempfile::tempdir().expect("tempdir");
    write_dotenv(good_dir.path(), FAKE_KEY, 0o600);
    assert!(
        resolve_llm_api_key_in(
            &CredentialSearch::empty().with_config_dir(Some(good_dir.path().to_path_buf()))
        )
        .is_ok(),
        "the same file at 0600 and correctly owned resolves — so the refusals above \
         were refusals, not unrelated read failures"
    );
}

// ---- No persisted `LlmCall` field carries the value ---------------------------

/// A provider double whose reply ECHOES the key back — the worst realistic case for
/// at-rest leakage, and the one a "we simply never write the key" argument misses.
struct EchoingProvider;

impl LlmProvider for EchoingProvider {
    fn chat(
        &self,
        _messages: Vec<Message>,
        _tools: &[ToolDefinition],
        _config: &LlmConfig,
    ) -> impl Future<Output = Result<LlmResponse, LlmError>> {
        std::future::ready(Ok(LlmResponse {
            content: Some(format!("your key is {FAKE_KEY}")),
            tool_calls: Vec::new(),
            usage: TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        }))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_persisted_llm_call_field_contains_the_value() {
    let tmp = tempfile::tempdir().expect("db tempdir");
    let db = pulse::Db::with_path(&tmp.path().join("pulse.db"))
        .await
        .expect("open db");
    pulse::MIGRATOR
        .run(db.pool())
        .await
        .expect("run migrations");

    let key_dir = tempfile::tempdir().expect("config tempdir");
    write_dotenv(key_dir.path(), FAKE_KEY, 0o600);
    let key = resolve_llm_api_key_in(
        &CredentialSearch::empty().with_config_dir(Some(key_dir.path().to_path_buf())),
    )
    .expect("credential resolves");

    let mut models = HashMap::new();
    models.insert(
        "glm-5.2".to_owned(),
        ModelPrice {
            input_per_mtok: Decimal::from(2),
            output_per_mtok: Decimal::from(8),
        },
    );
    let clock = FakeClock::at(1_700_000_000_000);
    let provider = RedactingLoggingProvider::new(
        EchoingProvider,
        SqliteLlmCallRepo::with_deps(db.pool().clone(), clock),
        clock,
        Redactor::from_config(vec![FAKE_KEY.to_owned()]),
        PriceTable::from_config("USD", models),
    )
    .with_key_source(Some(key.source()));

    let no_tools: &[ToolDefinition] = &[];
    provider
        .chat(
            // The key rides in on BOTH sides: echoed by the model above, and spoken
            // by the caller here.
            vec![Message::user(format!("here is {FAKE_KEY}, use it"))],
            no_tools,
            &LlmConfig {
                backend: LlmBackend::Ollama,
                model: "glm-5.2".to_owned(),
                temperature: 0.2,
                max_tokens: 256,
                reasoning_effort: None,
            },
        )
        .await
        .expect("the decorated call succeeds");

    // Concatenate EVERY column of the row, including the new `key_source`, and search
    // the lot. Naming the columns individually is what makes this a real assertion:
    // a `SELECT *` dump would silently stop covering a column added later.
    let dump: Vec<String> = sqlx::query_scalar(
        "SELECT id || backend || model || prompt_messages || COALESCE(completion, '') \
         || cost || cost_currency || created_at || created_by || schema_version \
         || COALESCE(key_source, '') FROM llm_call",
    )
    .fetch_all(db.pool())
    .await
    .expect("dump the row");

    assert_eq!(dump.len(), 1, "exactly one ledger row was written");
    assert!(
        !dump[0].contains(FAKE_KEY),
        "a persisted LlmCall field carries the key value: {}",
        dump[0]
    );
    assert!(
        !dump[0].contains(&FAKE_KEY[..12]),
        "a persisted LlmCall field carries a fragment of the key: {}",
        dump[0]
    );
    // The provenance label IS there — redaction must not have simply eaten the row.
    assert!(
        dump[0].contains("config-dir"),
        "the audit label survives redaction: {}",
        dump[0]
    );
}

#[test]
fn the_missing_credential_error_contains_no_value() {
    let empty_dir = tempfile::tempdir().expect("tempdir");
    let search = CredentialSearch::empty().with_config_dir(Some(empty_dir.path().to_path_buf()));
    let message = resolve_llm_api_key_in(&search)
        .expect_err("an exhausted search errors")
        .to_string();
    assert!(
        !message.contains(FAKE_KEY),
        "the missing-credential error leaked a value: {message}"
    );
}
