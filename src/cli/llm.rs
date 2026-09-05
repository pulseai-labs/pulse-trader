//! `pulse llm-check` — the VS-1.3.1 composition root + demo verb (FR-23 / FR-24 /
//! NFR-6, README the-full-composition).
//!
//! This is the ONE place the slice's concrete LLM types are assembled
//! (monomorphized), keeping every layer generic underneath. The live arm
//! ([`run_llm_check`]) wires:
//!
//! ```text
//! glm_api_key()  →  ApiKey (opaque; carries the CredentialSource::Keychain label)
//!                →  OpenAiCompatProvider::new(key.expose())
//!                →  RedactingLoggingProvider::new(inner, repo, clock, redactor, prices)
//!                   .with_key_source(Some(key.source()))  — the audit label, never the key
//!                   where repo = SqliteLlmCallRepo over the opened Db
//!                →  .chat()  →  a redacted, cost-logged LlmCall row
//! ```
//!
//! and prints backend / model / tokens / cost+currency + the stored `LlmCall`
//! id, then the model's reply and the persisted (redacted) prompt so a human can
//! confirm the secret was stripped at rest.
//!
//! **Injectable core (audit C2, mirror `run_fetch_data`).**
//! [`run_llm_check_with`] takes the provider + repo + redactor + prices + clock by
//! value, so the offline auto-test (`tests/llm_roundtrip_cli.rs`) drives the SAME
//! composition with a FAKE provider + a tempfile-`Db` repo — never a live
//! `GlmProvider`, never the network/Keychain (MASTER-SPEC §9.4).
//!
//! **Single shared clock (1.04 deferral).** The live arm creates ONE
//! [`SystemClock`] and injects the SAME clock into BOTH the
//! [`RedactingLoggingProvider`] AND the [`SqliteLlmCallRepo`] — the repo's
//! `save_call` overrides `created_at` with its own clock, so a single shared
//! clock keeps the persisted timestamp single-sourced.
//!
//! **Prices from config (2.03 moat seam).** The per-token figures the ledger
//! records are a NOMINAL estimate (Ollama Cloud is flat-rate), loaded from
//! `config/prices.toml` via `agent::config::load_price_table` — DATA, never a
//! hardcoded public-Rust price literal in `src/cli/` (AC-11 retires the old
//! `nominal_price_table` seam).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::adapters::clock::SystemClock;
use crate::adapters::db::{Db, SqliteLlmCallRepo};
use crate::adapters::llm::capturing::CapturingRepo;
use crate::adapters::llm::openai_compat::OpenAiCompatProvider;
use crate::adapters::llm::redacting_logging::RedactingLoggingProvider;
use crate::adapters::secrets::glm_api_key;
use crate::domain::Redactor;
use crate::domain::{
    Clock, CredentialSource, LlmBackend, LlmCall, LlmCallRepository, LlmConfig, LlmProvider,
    LlmResponse, Message, PriceTable,
};

/// The demo model id — `glm-5.3-flash` via Ollama Cloud since ADR-0023, which
/// carries the evidence.
///
/// `llm-check` never reads `[llm].model`, so this const is the ONLY thing choosing
/// its model — while its PRICES still come from the config file. The shipped
/// `[models]` row for this id must therefore exist, or the decorator's preflight
/// fails the verb before the billed call. `agent::config`'s identity + pricing
/// tests hold both halves (#126).
pub(crate) const DEMO_MODEL: &str = "glm-5.3-flash";

/// A conservative sampling temperature for the demo round-trip (wire-level `f32`,
/// never a determinism input — MASTER-SPEC §9.4 / the `LlmConfig` note).
const DEMO_TEMPERATURE: f32 = 0.2;

/// The response token cap for a verb driving the configured reasoning model. GLM
/// is a **reasoning** model whose thinking tokens count against this cap BEFORE
/// the final answer, so a tight cap yields empty `content` (the live VS-1.3.1
/// close demo saw an empty reply at 256 and a real one at ~343). Keep generous
/// headroom past the reasoning.
///
/// `llm-check`'s ONLY — the coach shared it until #164 and no longer does
/// (`adapters::llm::coach_transport::COACH_MAX_TOKENS`, 16 384). This verb sends a
/// one-sentence prompt and the coach sends a whole backtest, so one number was never
/// going to be right for both, and the failure the wrong one buys is quiet: a turn
/// that spends its budget thinking and emits no tool call is recorded as
/// `ZeroCalls`, which reads as a model that declined rather than a cap that was too
/// small (#124's reasoning-burn class).
pub(crate) const REASONING_MAX_TOKENS: u32 = 4096;

/// The fixed demo prompt used when the operator gives no prompt argument.
const DEMO_PROMPT: &str = "In one concise sentence, what is a liquidation in crypto futures?";

/// `pulse llm-check [PROMPT] [--db <path>]` — run a GLM chat round-trip through
/// the redacting + cost-logging composition and print the persisted `LlmCall`.
///
/// The verb name derives from the [`Command::LlmCheck`](super::Command) variant
/// (clap kebab-cases it to `llm-check`), so the top-level `--help` lists `llm`.
#[derive(Debug, clap::Args)]
pub struct LlmArgs {
    /// The prompt to send (a fixed demo prompt is used when omitted).
    pub prompt: Option<String>,
    /// `pulse.db` path override (defaults to the platform Application Support db);
    /// `global = true` so it parses in any position (mirror `RunsArgs.db`).
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
}

/// The outcome of one demo round-trip: the persisted (redacted) [`LlmCall`] ledger
/// record and the un-redacted [`LlmResponse`] the caller received. The auto-test
/// asserts against `call`; the live arm prints from both.
pub struct LlmCheckOutcome {
    /// The persisted ledger record — prompt + completion REDACTED, tokens + cost +
    /// currency populated, `created_at` from the shared clock.
    pub call: LlmCall,
    /// The un-redacted response the model returned (OQ-A: the caller sees the real
    /// reply; only the stored copy is scrubbed).
    pub response: LlmResponse,
}

/// The demo chat config (backend = Ollama, model = [`DEMO_MODEL`], nominal knobs).
fn demo_config() -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: DEMO_MODEL.to_owned(),
        temperature: DEMO_TEMPERATURE,
        max_tokens: REASONING_MAX_TOKENS,
    }
}

/// Build the demo prompt: a fixed system framing plus the operator's prompt (or
/// the fixed [`DEMO_PROMPT`] when none was given).
fn build_prompt(args: &LlmArgs) -> Vec<Message> {
    let user = args
        .prompt
        .clone()
        .unwrap_or_else(|| DEMO_PROMPT.to_owned());
    vec![
        Message::system(
            "You are PulseTrader's assistant. Answer concisely. This is not financial advice.",
        ),
        Message::user(user),
    ]
}

/// The injectable, fixture-doubleable core (audit C2, mirror `run_fetch_data`):
/// assemble the redacting + cost-logging decorator over the injected `provider` /
/// `repo` / `redactor` / `prices` / `clock`, run ONE `chat()` over `prompt`, and
/// return the persisted (redacted) [`LlmCall`] plus the un-redacted response.
///
/// The auto-test drives THIS with a FAKE provider + a tempfile-`Db` repo — never a
/// live [`GlmProvider`], never the network/Keychain (MASTER-SPEC §9.4). The same
/// `clock` value should be injected here AND into the `SqliteLlmCallRepo` so
/// `created_at` is single-sourced (the 1.04 deferral).
///
/// A thin, signature-preserving wrapper over [`run_llm_check_core`] with
/// `key_source: None` (provenance not recorded) — its exact 6-argument shape is
/// part of the crate's public API (`tests/llm_roundtrip_cli.rs` calls it directly),
/// so it stays untouched rather than growing a 7th parameter; the LIVE arm
/// ([`run_llm_check`]) calls [`run_llm_check_core`] directly with the real
/// [`CredentialSource`] instead.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] if the provider round-trip fails, the model has no
/// price-table entry (fail-closed cost), the ledger persist fails, or (defensively)
/// the saved row was not captured.
pub async fn run_llm_check_with<P, R, C>(
    provider: P,
    repo: R,
    redactor: Redactor,
    prices: PriceTable,
    clock: C,
    prompt: Vec<Message>,
) -> anyhow::Result<LlmCheckOutcome>
where
    P: LlmProvider + Send + Sync,
    R: LlmCallRepository + Send + Sync,
    C: Clock + Send + Sync,
{
    run_llm_check_core(provider, repo, redactor, prices, clock, prompt, None).await
}

/// The real body behind [`run_llm_check_with`], additionally carrying `key_source`
/// (r1.s1.w2 — the risk gate's audit-trail control) onto the decorator so it rides
/// onto every ledger row this run writes.
///
/// A LABEL, never the key: `key_source` is an [`ApiKey::source()`](crate::domain::ApiKey::source)
/// value, a type that cannot carry the credential. [`run_llm_check_with`] calls
/// this with `None` (so every existing caller, including the out-of-crate
/// `tests/llm_roundtrip_cli.rs` offline round-trip that never touches the
/// Keychain, keeps its current, honest "provenance not recorded" behavior
/// unchanged); [`run_llm_check`] — the only caller that actually sourced a
/// credential — calls this with `Some(CredentialSource::Keychain)`, the ONLY
/// credential source `llm-check` ever has.
///
/// # Errors
///
/// Same as [`run_llm_check_with`].
async fn run_llm_check_core<P, R, C>(
    provider: P,
    repo: R,
    redactor: Redactor,
    prices: PriceTable,
    clock: C,
    prompt: Vec<Message>,
    key_source: Option<CredentialSource>,
) -> anyhow::Result<LlmCheckOutcome>
where
    P: LlmProvider + Send + Sync,
    R: LlmCallRepository + Send + Sync,
    C: Clock + Send + Sync,
{
    let captured: Arc<Mutex<Option<LlmCall>>> = Arc::new(Mutex::new(None));
    // `llm-check` uses only the single-row `captured` slot; the id buffer is a
    // throwaway here (the composer's provenance buffer is a `compose`-only concern).
    let capturing = CapturingRepo::new(
        repo,
        Arc::clone(&captured),
        Arc::new(Mutex::new(Vec::new())),
    );

    // The composition root: wrap the (already-selected) provider in the redacting +
    // cost-logging decorator over the capturing repo, sharing the single `clock`.
    // r1.s1.w2: which credential source answered rides onto every ledger row, so a
    // call's provenance is reconstructible without the key ever being stored.
    let decorator = RedactingLoggingProvider::new(provider, capturing, clock, redactor, prices)
        .with_key_source(key_source);
    let config = demo_config();

    // No-tool back-compat: the `llm-check` demo advertises no tools (composer tools
    // are 2.04); an empty slice reproduces the VS-1.3.1 behavior exactly.
    let response = decorator
        .chat(prompt, &[], &config)
        .await
        .map_err(|e| anyhow::anyhow!("llm chat round-trip failed: {e}"))?;

    // Recover the persisted row (with its adapter-minted id) from the capture slot.
    let call = {
        let slot = captured
            .lock()
            .map_err(|_| anyhow::anyhow!("internal: llm_call capture lock poisoned"))?;
        slot.clone()
    }
    .ok_or_else(|| anyhow::anyhow!("internal: no llm_call was persisted by the round-trip"))?;

    Ok(LlmCheckOutcome { call, response })
}

/// The LIVE arm (composition root): source the GLM key from the Keychain, build the
/// `GlmProvider` → `RedactingLoggingProvider` → `SqliteLlmCallRepo` composition over
/// the opened `db`, run the round-trip via [`run_llm_check_core`] (passing the
/// resolved [`CredentialSource`] — see that function's doc comment for why this
/// calls the core directly rather than [`run_llm_check_with`]), and print the
/// result. This is the ONLY place the concrete GLM types are assembled.
///
/// `db` is `Some` for this verb (the dispatcher opens a migrated `pulse.db` — the
/// ledger write needs it); it is `Option<&Db>` to mirror the sibling CLI arms.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on an absent db, a missing/unreadable Keychain key
/// (whose message says seeding is not yet supported), a provider/transport
/// failure, or a ledger
/// persist failure — every path a clear message + non-zero exit, never a panic.
pub async fn run_llm_check(db: Option<&Db>, args: &LlmArgs) -> anyhow::Result<()> {
    let db = db.ok_or_else(|| anyhow::anyhow!("internal: llm-check requires an open db"))?;

    // Source the key from the macOS Keychain (READ only — the seed path is
    // VS-1.3.4's `pulse setup-keys`). A missing entry is a clear error, not a panic.
    // `glm_api_key` returns the opaque `ApiKey`, already tagged
    // `CredentialSource::Keychain` — llm-check's ONLY credential source.
    let key = glm_api_key().map_err(|e| anyhow::anyhow!("read GLM API key: {e}"))?;
    // The provenance LABEL, captured up front: it is all that reaches the ledger.
    let key_source = key.source();

    // Tag the live key value as a secret so an accidental echo of it in the prompt
    // is scrubbed from the STORED copy too (structural sk-shaped stripping is always
    // on). `expose()` is the ONE in-crate read of the value; it never leaves this
    // function as a bare String beyond the two consumers below.
    let redactor = Redactor::from_config(vec![key.expose().to_owned()]);
    let provider = OpenAiCompatProvider::new(key.expose().to_owned());

    // SINGLE SHARED CLOCK (1.04 deferral): ONE SystemClock injected into BOTH the
    // repo AND the decorator (via the core), so `created_at` is single-sourced.
    let clock = SystemClock;
    let repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);

    // Prices load from `config/prices.toml` via the 2.03 loader (AC-11 — no price
    // literal in `src/cli/`); the shipped `[models]` row for DEMO_MODEL backs this
    // demo — llm-check is const-driven, so that row must exist or the decorator
    // fails closed before the billed call.
    let prices = crate::agent::config::load_price_table()
        .map_err(|e| anyhow::anyhow!("load price table: {e}"))?;
    let prompt = build_prompt(args);

    // `run_llm_check_core` directly (not `run_llm_check_with`): this is the ONE
    // call site that actually resolved a credential, so it is the one that passes
    // `Some(key_source)` — r1.s1.w2, the audit-trail control.
    let outcome = run_llm_check_core(
        provider,
        repo,
        redactor,
        prices,
        clock,
        prompt,
        Some(key_source),
    )
    .await?;
    print_outcome(&outcome);
    Ok(())
}

/// Print the round-trip result: the ledger header (backend / model / tokens /
/// cost+currency / stored id), the model's un-redacted reply, and the persisted
/// (redacted) prompt so a human can confirm the secret was stripped at rest.
fn print_outcome(outcome: &LlmCheckOutcome) {
    let call = &outcome.call;
    println!(
        "llm-check\tbackend={}\tmodel={}\tinput_tokens={}\toutput_tokens={}\tcost={} {}\tllm_call_id={}",
        backend_label(call.backend),
        call.model,
        call.input_tokens,
        call.output_tokens,
        call.cost.normalize(),
        call.cost_currency,
        call.id.as_str(),
    );
    if let Some(content) = &outcome.response.content {
        println!("response\t{content}");
    }
    println!("persisted_prompt (redacted — confirm no secret leaks at rest):");
    for message in &call.prompt_messages {
        println!("  {}", render_message(message));
    }
}

/// The bare backend tag for display (e.g. `ollama`).
fn backend_label(backend: LlmBackend) -> &'static str {
    match backend {
        LlmBackend::Ollama => "ollama",
    }
}

/// Render one persisted message as `role: content` for the redaction readout.
fn render_message(message: &Message) -> String {
    match message {
        Message::System { content } => format!("system: {content}"),
        Message::User { content } => format!("user: {content}"),
        Message::Assistant { content, .. } => {
            format!("assistant: {}", content.as_deref().unwrap_or(""))
        }
        Message::ToolResult {
            tool_call_id,
            content,
        } => format!("tool[{tool_call_id}]: {content}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{DEMO_MODEL, LlmArgs, build_prompt, demo_config, render_message};
    use crate::cli::{Cli, Command};
    use crate::domain::{LlmBackend, Message};
    use clap::Parser;

    #[test]
    fn parses_llm_check_with_positional_prompt() {
        let cli = Cli::try_parse_from(["pulse", "llm-check", "hello there"]).expect("parse");
        let Command::LlmCheck(args) = cli.command else {
            panic!("expected an llm-check command");
        };
        assert_eq!(args.prompt.as_deref(), Some("hello there"));
    }

    #[test]
    fn parses_llm_check_db_override_globally() {
        let cli =
            Cli::try_parse_from(["pulse", "llm-check", "hi", "--db", "/tmp/x.db"]).expect("parse");
        let Command::LlmCheck(args) = cli.command else {
            panic!("expected an llm-check command");
        };
        assert_eq!(
            args.db.as_deref().and_then(std::path::Path::to_str),
            Some("/tmp/x.db")
        );
    }

    #[test]
    fn build_prompt_uses_demo_prompt_when_absent() {
        let args = LlmArgs {
            prompt: None,
            db: None,
        };
        let prompt = build_prompt(&args);
        assert_eq!(prompt.len(), 2);
        match &prompt[0] {
            Message::System { .. } => {}
            other => panic!("expected a system framing, got {other:?}"),
        }
        // The user turn carries the fixed demo prompt (non-empty).
        match &prompt[1] {
            Message::User { content } => assert!(!content.is_empty()),
            other => panic!("expected a user turn, got {other:?}"),
        }
    }

    #[test]
    fn demo_config_targets_ollama_model() {
        // The demo config targets the pinned Ollama model; the cost table now loads
        // from `config/prices.toml` (2.03), asserted in `agent::config`'s own tests
        // — this seam no longer carries a price table (AC-11).
        let config = demo_config();
        assert_eq!(config.backend, LlmBackend::Ollama);
        assert_eq!(config.model, DEMO_MODEL);
    }

    #[test]
    fn render_message_labels_each_role() {
        assert_eq!(render_message(&Message::system("s")), "system: s");
        assert_eq!(render_message(&Message::user("u")), "user: u");
        assert_eq!(render_message(&Message::assistant("a")), "assistant: a");
    }
}
