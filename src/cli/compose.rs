//! `pulse compose "<NL target>"` — the VS-1.3.2 composition root + demo verb
//! (FR-3 / FR-4 / NFR-6, README C8).
//!
//! This is the ONE place the composer's concrete stack is assembled
//! (monomorphized), keeping every layer generic underneath. The full chain the
//! live arm ([`run_compose`]) wires:
//!
//! ```text
//! resolve_llm_api_key()  →  ApiKey (opaque; carries the CredentialSource label)
//!                      →  OpenAiCompatProvider::{new|with_base_url}(key)  (Ollama Cloud, glm-5.3-flash)
//!                      →  RedactingLoggingProvider::new(inner, capturing, clock, redactor, prices)
//!                         .with_key_source(source)  — the audit label, never the key
//!                         where capturing = CapturingRepo<SqliteLlmCallRepo> sharing ONE
//!                         LlmCallCapture buffer + ONE SystemClock (repo owns created_at, #82)
//!                      →  Composer::new(decorator, builder_tool_definitions(), prompt, config, buffer)
//!                      →  compose()  →  a finalized StrategyVersion value (DB-free)
//!                      →  SqliteStrategyRepo::create_strategy + create_version  →  a persisted,
//!                         attributable version (created_by = ComposerLlm, provenance ids)
//! ```
//!
//! while streaming each [`ComposerEvent`] as a visible tool-call step.
//!
//! **Injectable core (mirror `run_llm_check_with`).** [`run_compose_with`] takes the
//! LLM-side deps bundled in a [`ComposeWiring`] plus the [`StrategyRepository`], so
//! the offline e2e (`tests/compose_cli.rs`, demo criterion 1) drives the SAME
//! composition with a FAKE provider over the REAL composer + REAL builder tools + a
//! `tempfile` `SQLite` repo — never a live LLM, never the network/Keychain
//! (MASTER-SPEC §9.4).
//!
//! **Single shared clock (#82).** ONE clock is injected into BOTH the
//! [`RedactingLoggingProvider`] AND the [`SqliteLlmCallRepo`]; the repo's `save_call`
//! overrides `created_at` from its own clock, so a single shared clock keeps the
//! persisted ledger timestamp single-sourced.
//!
//! **One shared `LlmCallCapture` buffer.** ONE `Arc<Mutex<Vec<LlmCallId>>>` is wired
//! into BOTH the [`CapturingRepo`](crate::adapters::llm::capturing::CapturingRepo) (which pushes each
//! minted id as the decorator writes an `LlmCall`) AND [`Composer::new`], so the
//! composer reads its run's provenance ids back after the loop.
//!
//! **Prices from config (2.03).** The price table loads from `config/prices.toml`
//! via `agent::config::load_price_table` — no price VALUE literal lives in
//! `src/cli/` (AC-11).
//!
//! **Transport from config (FIX A, ADR-0013).** The model + base URL load from the
//! SAME `config/prices.toml` `[llm]` table via `agent::config::load_llm_transport`:
//! MODEL resolves config `[llm].model` → [`COMPOSE_MODEL`] const fallback, base URL
//! resolves config `[llm].base_url` → the provider's `const` default. A one-line
//! model swap is DATA (edit the toml), not a Rust edit — the ADR-0013 promise.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context as _;

use crate::adapters::clock::SystemClock;
use crate::adapters::db::{Db, SqliteLlmCallRepo, SqliteStrategyRepo};
use crate::adapters::llm::openai_compat::OpenAiCompatProvider;
use crate::adapters::llm::redacting_logging::RedactingLoggingProvider;
use crate::agent::config::{load_composer_prompt, load_llm_transport, load_price_table};
use crate::agent::{
    ComposeOutcome, Composer, ComposerEvent, LlmCallCapture, builder_tool_definitions,
};
use crate::domain::Redactor;
use crate::domain::strategy::{CreatedBy, NewVersion, Strategy, StrategyVersion};
use crate::domain::{
    ApiKey, Clock, CredentialSource, LlmBackend, LlmCallId, LlmCallRepository, LlmConfig,
    LlmProvider, PriceTable, StrategyRepository,
};

use crate::adapters::llm::capturing::CapturingRepo;

/// The FALLBACK Ollama Cloud model id the composer drives when the config
/// `[llm].model` is absent (README C2/C8). A model-id STRING (not a price literal)
/// — AC-11 greps `src/cli/` only for price VALUE field names.
///
/// `glm-5.3-flash` since ADR-0023 (2026-08-29) — see it for the bare-id evidence
/// and for the composer tool-loop run that qualified this model. That run is the
/// bar THIS const has to clear, because `gpt-oss:120b` once transported cleanly on
/// this endpoint and still failed mid-loop.
///
/// The live model is config-driven per ADR-0013 (`config/prices.toml`
/// `[llm].model`); this const is only the documented fallback (slice-close FIX A),
/// and `agent::config`'s identity test holds the two in agreement (#126).
pub(crate) const COMPOSE_MODEL: &str = "glm-5.3-flash";

/// A conservative sampling temperature for the compose run (wire-level `f32`, never
/// a determinism input — MASTER-SPEC §9.4 / the `LlmConfig` note).
const COMPOSE_TEMPERATURE: f32 = 0.2;

/// The response token cap. GLM is a **reasoning** family whose thinking tokens
/// count against this cap BEFORE its tool calls, so keep generous headroom past the
/// reasoning ([VS-1.3.1 GLM]; the live demo uses 4096).
const COMPOSE_MAX_TOKENS: u32 = 4096;

/// `pulse compose "<NL target>" [--db <path>]` — turn a natural-language strategy
/// target into a persisted, attributable [`StrategyVersion`] via the composer.
#[derive(Debug, clap::Args)]
pub struct ComposeArgs {
    /// The natural-language strategy target to compose (e.g. "RSI oversold bounce
    /// on BTC with a trend filter").
    pub nl_target: String,
    /// `pulse.db` path override (defaults to the platform Application Support db);
    /// `global = true` so it parses in any position (mirror `LlmArgs.db`).
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
}

/// The LLM-side deps of one compose run, bundled so [`run_compose_with`] stays under
/// the argument-count limit and the e2e wires the SAME composition the live arm does.
///
/// `provider` is the (possibly faked) inner [`LlmProvider`]; `llm_repo` the ledger
/// repo the decorator writes through; `clock` the SINGLE shared clock (#82);
/// `prompt` the composer system prompt (2.03); `config` the per-request knobs.
pub struct ComposeWiring<P, R, C> {
    /// The inner provider (live `OpenAiCompatProvider`, or the e2e's fake).
    pub provider: P,
    /// The `LlmCall` ledger repo the redacting decorator writes each row through.
    pub llm_repo: R,
    /// The NFR-6 secret scrubber for the PERSISTED prompt/completion copy.
    pub redactor: Redactor,
    /// The README-C5 cost table (loaded from `config/prices.toml`, 2.03).
    pub prices: PriceTable,
    /// The SINGLE shared clock injected into BOTH the decorator AND `llm_repo` (#82).
    pub clock: C,
    /// The composer system prompt (2.03 `load_composer_prompt`).
    pub prompt: String,
    /// Which credential source supplied the API key, stamped on every persisted
    /// `LlmCall` (r1.s1.w2 — the risk gate's audit-trail control). A LABEL, never
    /// the key: it is `ApiKey::source()`, a type that cannot carry a value.
    /// `None` when provenance was not recorded (a test double, say).
    pub key_source: Option<CredentialSource>,
    /// The per-request chat config (backend / model / temperature / `max_tokens`).
    pub config: LlmConfig,
}

/// The outcome of one compose run: the persisted strategy + its initial version
/// (repo-minted id / hash / `created_at`), the minted `LlmCall` provenance ids, and
/// the streamed [`ComposerEvent`]s (recorded in order).
pub struct ComposeCliOutcome {
    /// The newly-created owning [`Strategy`] (repo-minted id + timestamp).
    pub strategy: Strategy,
    /// The persisted initial [`StrategyVersion`] (`parent_version_id = None`,
    /// `created_by = ComposerLlm`, repo-minted id / `version_hash` / `created_at`).
    pub version: StrategyVersion,
    /// The `LlmCall` ids minted during the run (this version's provenance).
    pub llm_call_ids: Vec<LlmCallId>,
    /// The streamed steps, recorded in order (the CLI renders these live).
    pub events: Vec<ComposerEvent>,
}

/// The refusal message a cancelled compose run carries. A LABEL, never a payload —
/// it names the condition, not anything about the run's data.
///
/// Both cancellation signals end here: a dead sink (which trips the provider guard
/// in `src/tauri/commands.rs`) and an explicit `compose_cancel`. The desktop core
/// decides "cancelled vs failed" from the LATCH rather than from this string, so
/// the text is for humans; it is shared so the two sites cannot drift.
pub const COMPOSE_CANCELLED: &str = "compose run cancelled: the destination channel closed";

/// The injectable, fixture-doubleable core (mirror `run_llm_check_with`): assemble
/// the redacting + cost-logging decorator over `wiring`'s provider + ledger repo
/// (sharing ONE `LlmCallCapture` buffer + ONE clock), run [`Composer::compose`] over
/// `nl_target` streaming each event to `on_event`, then persist the finalized
/// [`StrategyVersion`] as a NEW strategy + its initial version via `strategy_repo`.
///
/// The e2e drives THIS with a FAKE provider + a `tempfile` `SQLite` repo — never a live
/// LLM, never the network/Keychain (MASTER-SPEC §9.4).
///
/// # Errors
///
/// Returns an [`anyhow::Error`] if the composer loop fails (transport / budget /
/// never-finalized / max-turns), or if persisting the strategy or its version fails.
pub async fn run_compose_with<P, R, S, C>(
    wiring: ComposeWiring<P, R, C>,
    strategy_repo: &S,
    nl_target: &str,
    on_event: &mut (dyn FnMut(ComposerEvent) + Send),
    cancelled: &AtomicBool,
) -> anyhow::Result<ComposeCliOutcome>
where
    P: LlmProvider + Send + Sync,
    R: LlmCallRepository + Send + Sync,
    S: StrategyRepository + Send + Sync,
    C: Clock + Send + Sync,
{
    let ComposeWiring {
        provider,
        llm_repo,
        redactor,
        prices,
        clock,
        prompt,
        config,
        key_source,
    } = wiring;

    // ONE shared LlmCallCapture buffer wired into BOTH the capturing ledger repo
    // (which pushes each minted id) AND the Composer (which reads them back).
    let captured: LlmCallCapture = Arc::new(Mutex::new(Vec::new()));
    let capturing = CapturingRepo::new(llm_repo, Arc::new(Mutex::new(None)), Arc::clone(&captured));

    // The composition root: provider → redacting + cost-logging decorator over the
    // capturing repo, sharing the SINGLE clock (the ledger repo owns created_at, #82).
    // `with_created_by(ComposerLlm)` is load-bearing: every row this decorator writes is
    // provenance-linked from a `StrategyVersion { created_by: ComposerLlm }`, and
    // `llm_call` is trigger-immutable — a row stamped `Human` here could never be fixed.
    let decorator = RedactingLoggingProvider::new(provider, capturing, clock, redactor, prices)
        .with_created_by(CreatedBy::ComposerLlm)
        // r1.s1.w2: which credential source answered rides onto every ledger row, so
        // a call's provenance is reconstructible without the key ever being stored.
        .with_key_source(key_source);
    let composer = Composer::new(
        decorator,
        builder_tool_definitions(),
        prompt,
        config,
        Arc::clone(&captured),
    );

    // `.context(...)` rather than `anyhow!("...: {e}")` on all three: a formatted
    // string is a NEW error with no source, which erases the typed cause. The
    // desktop bus classifies a compose failure by downcasting the anyhow chain
    // (`compose_failure` in `src/tauri/commands.rs`) to tell the Designer which
    // family failed, and a stringified cause makes every one of them `Internal`.
    let outcome: ComposeOutcome = composer
        .compose(nl_target, on_event)
        .await
        .context("compose run failed")?;

    // The last point a cancellation can still be honoured, and the reason this
    // check is HERE rather than only in the provider guard: that guard runs
    // BEFORE each model turn, so a cancel arriving while the final turn is
    // already in flight misses it — the response comes back `Ok`, the composer
    // finalizes, and the two writes below persist a strategy the user cancelled
    // seconds earlier. Refusing between the last turn and the first write closes
    // that window. `COMPOSE_CANCELLED` is what `compose_strategy_core` matches to
    // report the run as cancelled rather than failed.
    if cancelled.load(Ordering::SeqCst) {
        anyhow::bail!(COMPOSE_CANCELLED);
    }

    // Persist the finalized StrategyVersion as a NEW strategy + its initial version
    // (parent_version_id = None); the repo mints id / strategy_id / version_hash /
    // created_at (the composer left those as DB-free placeholders).
    let strategy = strategy_repo
        .create_strategy(&outcome.version.dsl.name, None, &[])
        .await
        .context("persist strategy")?;
    let version = strategy_repo
        .create_version(NewVersion {
            strategy_id: strategy.id.clone(),
            parent_version_id: None,
            dsl_json: outcome.version.dsl_original.clone(),
            created_by: outcome.version.created_by,
            creating_llm_call_ids: outcome.version.creating_llm_call_ids.clone(),
        })
        .await
        .context("persist strategy version")?;

    Ok(ComposeCliOutcome {
        strategy,
        version,
        llm_call_ids: outcome.llm_call_ids,
        events: outcome.events,
    })
}

/// The LIVE arm (composition root): source the Ollama key from the dev `.env`, build
/// the `OpenAiCompatProvider` → decorator → `SqliteLlmCallRepo` composition over the
/// opened `db`, run the composer via [`run_compose_with`], and print the streamed
/// steps + the persisted version. This is the ONLY place the concrete stack is
/// assembled.
///
/// `db` is `Some` for this verb (the dispatcher opens a migrated `pulse.db` — the
/// ledger + version writes need it); it is `Option<&Db>` to mirror the sibling arms.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on an absent db, a missing API key, a config-load
/// failure, a composer/transport failure, or a persist failure — every path a clear
/// message + non-zero exit, never a panic.
pub async fn run_compose(db: Option<&Db>, args: &ComposeArgs) -> anyhow::Result<()> {
    let db = db.ok_or_else(|| anyhow::anyhow!("internal: compose requires an open db"))?;

    // Transport pinning from config (FIX A, ADR-0013): base URL + model load from
    // `config/prices.toml` `[llm]`; a missing table/field falls back to the const.
    let transport = load_llm_transport().map_err(|e| anyhow::anyhow!("load llm transport: {e}"))?;

    let key = ollama_api_key()?;
    // The provenance LABEL, captured up front: it is all that reaches the ledger.
    let key_source = key.source();
    // Tag the live key as a secret so an accidental echo is scrubbed at rest too
    // (structural api-key-shaped stripping is always on). `expose()` is the ONE
    // in-crate read of the value; it never leaves this function as a bare String
    // beyond the two consumers below.
    let redactor = Redactor::from_config(vec![key.expose().to_owned()]);
    // Base URL: config `[llm].base_url` → the provider's const default.
    let provider = match transport.base_url {
        Some(base_url) => OpenAiCompatProvider::with_base_url(key.expose().to_owned(), base_url),
        None => OpenAiCompatProvider::new(key.expose().to_owned()),
    };

    // SINGLE SHARED CLOCK (#82): ONE SystemClock into the ledger repo AND the
    // decorator, so the persisted LlmCall.created_at is single-sourced. SystemClock
    // is a zero-sized Copy value, so the strategy repo's clock is the same clock.
    let clock = SystemClock;
    let llm_repo = SqliteLlmCallRepo::with_deps(db.pool().clone(), clock);
    let strategy_repo = SqliteStrategyRepo::new(db.pool().clone());

    let prices = load_price_table().map_err(|e| anyhow::anyhow!("load price table: {e}"))?;
    let prompt =
        load_composer_prompt().map_err(|e| anyhow::anyhow!("load composer prompt: {e}"))?;

    let wiring = ComposeWiring {
        provider,
        llm_repo,
        redactor,
        prices,
        clock,
        prompt,
        // Model: config `[llm].model` → the COMPOSE_MODEL const fallback (FIX A).
        config: compose_config(transport.model.as_deref()),
        // r1.s1.w2: the LABEL for wherever the key came from — the audit trail.
        key_source: Some(key_source),
    };

    let mut on_event = |event: ComposerEvent| println!("{}", render_event(&event));
    // The CLI has no cancellation channel: a `pulse compose` run ends when it
    // ends, or when the operator kills the process. A never-tripped latch keeps
    // one signature for both surfaces rather than two compose cores.
    let never_cancelled = AtomicBool::new(false);
    let outcome = run_compose_with(
        wiring,
        &strategy_repo,
        &args.nl_target,
        &mut on_event,
        &never_cancelled,
    )
    .await?;
    print_outcome(&outcome);
    Ok(())
}

/// The compose chat config (backend = Ollama, generous reasoning-headroom
/// `max_tokens`). MODEL resolves the config override → the [`COMPOSE_MODEL`] const
/// fallback: `model_override` is the config `[llm].model` when present (FIX A).
///
/// `pub(crate)` (r1.s1.w4): the Tauri ring's `compose_strategy` command builds
/// the SAME config the CLI live arm does — one source for the fallback model
/// and reasoning headroom, so the two surfaces cannot drift on transport knobs.
pub(crate) fn compose_config(model_override: Option<&str>) -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: model_override.unwrap_or(COMPOSE_MODEL).to_owned(),
        temperature: COMPOSE_TEMPERATURE,
        max_tokens: COMPOSE_MAX_TOKENS,
        reasoning_effort: None,
    }
}

/// Render one streamed [`ComposerEvent`] as a single line (kept single-line so a
/// multi-line render can never truncate mid-field — the R1 harvested bug).
fn render_event(event: &ComposerEvent) -> String {
    match event {
        ComposerEvent::ToolCallStarted {
            name,
            arguments_preview,
        } => format!("  -> {name} {arguments_preview}"),
        ComposerEvent::ToolCallResult { name, outcome } => format!("     {name}: {outcome}"),
        ComposerEvent::Finalized { version_summary } => format!("  finalized: {version_summary}"),
    }
}

/// Print the persisted result: the strategy + version ids, a one-line DSL summary,
/// the provenance (`created_by` + minted `LlmCall` count).
fn print_outcome(outcome: &ComposeCliOutcome) {
    let version = &outcome.version;
    println!(
        "compose\tstrategy_id={}\tversion_id={}\tname={}\tdirection={:?}\tfilters={}\texits={}\tcreated_by={:?}\tcreating_llm_call_ids={}",
        outcome.strategy.id.as_str(),
        version.id.as_str(),
        version.dsl.name,
        version.dsl.direction,
        version.dsl.filters.len(),
        version.dsl.exits.len(),
        version.created_by,
        version.creating_llm_call_ids.len(),
    );
}

/// Source the LLM API key for the LIVE compose run (the `user:` demo).
///
/// **This function keeps NO resolution logic of its own (r1.s1.w2 step 1).** It is a
/// one-line adapter onto
/// [`resolve_llm_api_key`](crate::adapters::secrets::resolve_llm_api_key), which
/// lives on the *LLM credential handling and redaction* risk gate's registered
/// surface — `src/adapters/secrets.rs`. The precedence chain, the fail-closed
/// permission validation and the error text all belong to the resolver; the search
/// order, the `.env` reader (`parse_dotenv`) and the location list moved there with
/// it rather than being duplicated here.
///
/// The move is also what makes the key reachable from `src/tauri/` (`r1.s1.w4`):
/// `mod cli` is private in `src/lib.rs`, so a private `fn` in this file could never
/// be called from the Tauri ring.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] carrying the resolver's [`LlmError`] message, which
/// names every location searched and — on a refusal — which permission check failed.
///
/// [`LlmError`]: crate::domain::LlmError
fn ollama_api_key() -> anyhow::Result<ApiKey> {
    crate::adapters::secrets::resolve_llm_api_key()
        .map_err(|e| anyhow::anyhow!("resolve LLM API key: {e}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{COMPOSE_MODEL, compose_config};
    use crate::cli::{Cli, Command};
    use crate::domain::LlmBackend;
    use clap::Parser;

    #[test]
    fn parses_compose_with_positional_target() {
        let cli = Cli::try_parse_from(["pulse", "compose", "RSI oversold on BTC"]).expect("parse");
        let Command::Compose(args) = cli.command else {
            panic!("expected a compose command");
        };
        assert_eq!(args.nl_target, "RSI oversold on BTC");
        assert!(args.db.is_none());
    }

    #[test]
    fn parses_compose_db_override_globally() {
        let cli =
            Cli::try_parse_from(["pulse", "compose", "hi", "--db", "/tmp/x.db"]).expect("parse");
        let Command::Compose(args) = cli.command else {
            panic!("expected a compose command");
        };
        assert_eq!(
            args.db.as_deref().and_then(std::path::Path::to_str),
            Some("/tmp/x.db")
        );
    }

    #[test]
    fn compose_config_targets_the_pinned_ollama_model() {
        // No config override → the COMPOSE_MODEL const fallback (FIX A).
        let config = compose_config(None);
        assert_eq!(config.backend, LlmBackend::Ollama);
        assert_eq!(config.model, COMPOSE_MODEL);
        // Reasoning headroom (GLM thinking tokens count against the cap).
        assert!(
            config.max_tokens >= 4096,
            "generous cap for reasoning tokens"
        );
    }

    /// FIX A: a config `[llm].model` (e.g. `kimi-k2.6`) drives the composed config's
    /// model; the const is only the fallback used when the config field is absent.
    /// The config-dir read is proven race-free in `agent::config`'s own tests; here
    /// we prove the compose seam PREFERS that value over the const.
    #[test]
    fn compose_config_prefers_config_model_over_const_fallback() {
        assert_eq!(compose_config(Some("kimi-k2.6")).model, "kimi-k2.6");
        assert_eq!(compose_config(None).model, COMPOSE_MODEL);
    }

    // `parse_dotenv_reads_key_ignoring_comments_and_quotes` moved with the reader
    // itself to `adapters::secrets` (r1.s1.w2 step 1) — `src/cli/compose.rs` keeps
    // no resolution logic, and therefore no test of resolution logic either.
}
