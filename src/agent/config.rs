//! The composer's config seam — the "moat in DATA, not code" loader (VS-1.3.2
//! work-2.03, VS-1.3.1 decision 4).
//!
//! The composer's behavioural content is authored as **data**, not Rust
//! literals:
//!
//! - the system prompt is a versioned `.md` file
//!   ([`prompts/composer.md`](../prompts/composer.md)), compiled in as the
//!   default via [`include_str!`] with an optional
//!   `$PULSE_PROMPT_DIR/composer.md` runtime override (the private-workspace
//!   override — forward-compat to the owner's runtime-private moat);
//! - the per-model price table loads from `config/prices.toml` through the
//!   EXISTING [`PriceTable::from_config`] seam (VS-1.3.1 C5) — this module
//!   carries **no** price VALUES (AC-8), only the wiring that reads them.
//!
//! Config-directory resolution order (README C6):
//! 1. `$PULSE_CONFIG_DIR` (explicit override),
//! 2. the dev default — the canonical repo's `config/` (via
//!    `CARGO_MANIFEST_DIR`),
//! 3. `~/Library/Application Support/PulseTrader/config/` (the app-support dir).
//!
//! All fallible paths return [`ConfigError`] (a `thiserror` enum) with a clear
//! message — **never** a panic.
//!
//! Visibility: the loader is `pub(crate)`. The composition root (2.05, R4) is
//! its first production caller; until then its only callers are this module's
//! unit tests, so the whole seam is `#![allow(dead_code)]` under
//! `deny(warnings)` (the VS-1.3.1 harvested dead-code gotcha). This is an
//! internal seam, deliberately NOT a `pub` re-export on the crate's public API
//! surface.

// The loader is built-but-unwired this slice (2.05 is its first production
// caller). Under `deny(warnings)` a `pub(crate)` fn whose only non-test caller
// does not yet exist is a `dead_code` BUILD error, so the seam is allowed here.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::domain::{ModelPrice, PriceTable};

/// The application name namespacing the app-support config dir
/// (`~/Library/Application Support/PulseTrader/config/` on macOS).
const APP_DIR: &str = "PulseTrader";

/// Env override for the config directory (resolution order #1).
const CONFIG_DIR_ENV: &str = "PULSE_CONFIG_DIR";

/// Env override for the prompt directory (the private-workspace prompt override).
const PROMPT_DIR_ENV: &str = "PULSE_PROMPT_DIR";

/// The price-table file name under the resolved config dir.
const PRICES_FILE: &str = "prices.toml";

/// The composer-prompt file name under the resolved prompt-override dir.
const COMPOSER_FILE: &str = "composer.md";

/// The compiled-in default composer prompt (the versioned `.md`, authored as
/// DATA per `PROMPT_GOVERNANCE` §3 — not a Rust `const` string literal).
const COMPOSER_PROMPT_DEFAULT: &str = include_str!("prompts/composer.md");

/// The coach-prompt file name under the resolved prompt-override dir (r1.s2.w3).
const COACH_FILE: &str = "coach.md";

/// The compiled-in default coach prompt — same DATA discipline as the composer's.
const COACH_PROMPT_DEFAULT: &str = include_str!("prompts/coach.md");

/// The compiled-in default price table — the SHIPPED `config/prices.toml`,
/// embedded verbatim so a relocated or packaged binary is self-contained.
///
/// Without this floor, [`resolve_config_dir`] falls through to an app-support
/// directory that no code in this crate ever populates, so `pulse compose` and
/// `pulse llm-check` would both fail before contacting the provider whenever the
/// compile-time `CARGO_MANIFEST_DIR` checkout is absent. Embedding (rather than
/// carrying Rust price literals) keeps AC-8's grep of this file for a price
/// literal empty — the numbers still live only in the data file.
const PRICES_DEFAULT: &str = include_str!("../../config/prices.toml");

/// A config-loading failure — a missing file or a parse error. Carries the
/// offending path and the underlying error for a clear message; never a panic.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ConfigError {
    /// No platform config directory could be resolved (the `directories` crate
    /// returned `None` and no override/dev-default applied).
    #[error("no platform config directory available")]
    NoConfigDir,
    /// A config file could not be read (most often: it does not exist).
    #[error("reading config file {}: {source}", .path.display())]
    Read {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// A config file was read but could not be parsed as the expected TOML shape.
    #[error("parsing config file {}: {source}", .path.display())]
    Parse {
        /// The path that failed to parse.
        path: PathBuf,
        /// The underlying TOML deserialization error.
        #[source]
        source: toml::de::Error,
    },
}

/// The on-disk `prices.toml` shape. Only `currency` + `models` are consumed by
/// this struct's [`PriceTable::from_config`] seam; the `[llm]` table
/// (`base_url`/`model`) is parsed SEPARATELY by [`load_llm_transport`] (the
/// composition root reads it to drive the model/base-url, slice-close FIX A), so
/// it is intentionally NOT modelled here (serde ignores it), keeping this struct
/// free of any unused-field carry.
///
/// Crucially, the per-model VALUES deserialize DIRECTLY into the domain's
/// [`ModelPrice`] — this module never spells out the per-Mtok price field
/// names, so the price numbers live only in the data file, and AC-8's grep of
/// this file for a price literal stays empty.
#[derive(Debug, Deserialize)]
struct PricesConfig {
    currency: String,
    models: HashMap<String, ModelPrice>,
}

/// The resolved `[llm]` transport pinning read from `prices.toml` (slice-close
/// FIX A, ADR-0013 "config-driven model/base-url"). Both fields are optional: a
/// missing `[llm]` table or a missing field yields `None`, and the composition
/// root falls back to its documented `const` — never an error.
pub(crate) struct LlmTransport {
    /// The OpenAI-compatible base URL override (e.g. `https://ollama.com/v1`), or
    /// `None` to use the provider's `const` default.
    pub(crate) base_url: Option<String>,
    /// The model id override (e.g. `glm-5.2`), or `None` to use the compose
    /// `const` default.
    pub(crate) model: Option<String>,
}

/// The `[llm]` table's on-disk shape (only the two transport-pinning fields). A
/// separate parse struct from [`PricesConfig`] so each loader models exactly what
/// it consumes; toml ignores the sibling `currency`/`[models]` tables here.
#[derive(Debug, Default, Deserialize)]
struct TransportConfig {
    #[serde(default)]
    llm: Option<LlmTable>,
}

/// The `[llm]` table's two optional fields.
#[derive(Debug, Default, Deserialize)]
struct LlmTable {
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// Resolve the config directory per the README C6 order (override → dev default
/// → app-support).
///
/// # Errors
///
/// Returns [`ConfigError::NoConfigDir`] when no override or dev default applies
/// and the platform data directory cannot be determined.
fn resolve_config_dir() -> Result<PathBuf, ConfigError> {
    // 1. Explicit override.
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV) {
        return Ok(PathBuf::from(dir));
    }
    // 2. Dev default: the canonical repo's `config/` (compile-time manifest dir).
    let dev_default = Path::new(env!("CARGO_MANIFEST_DIR")).join("config");
    if dev_default.is_dir() {
        return Ok(dev_default);
    }
    // 3. App-support: ~/Library/Application Support/PulseTrader/config/.
    let dirs = directories::ProjectDirs::from("", "", APP_DIR).ok_or(ConfigError::NoConfigDir)?;
    Ok(dirs.data_dir().join("config"))
}

/// Load the price table from `prices.toml` in the resolved config dir, through
/// the EXISTING [`PriceTable::from_config`] seam (VS-1.3.1 C5).
///
/// # Errors
///
/// Returns [`ConfigError`] if the config dir cannot be resolved, the file
/// cannot be read, or its TOML cannot be parsed.
pub(crate) fn load_price_table() -> Result<PriceTable, ConfigError> {
    load_price_table_from(&resolve_config_dir()?)
}

/// Load the price table from `prices.toml` under an explicit `config_dir` (the
/// testable core of [`load_price_table`]).
///
/// # Errors
///
/// Returns [`ConfigError::Read`] if the file is present but unreadable, or
/// [`ConfigError::Parse`] if its TOML does not match the expected shape. An
/// ABSENT file is not an error — it falls back to [`PRICES_DEFAULT`].
fn load_price_table_from(config_dir: &Path) -> Result<PriceTable, ConfigError> {
    let (text, path) = read_prices_text(config_dir)?;
    let parsed: PricesConfig =
        toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })?;
    // Reuse the domain cost model's loader seam — no price VALUES live here.
    Ok(PriceTable::from_config(parsed.currency, parsed.models))
}

/// Read `prices.toml` from `config_dir`, falling back to the compiled-in
/// [`PRICES_DEFAULT`] when the file is ABSENT (a relocated/packaged binary).
///
/// Returns the TOML text plus the path to blame in a [`ConfigError::Parse`] —
/// the real path when the file was read, else the would-be path (so a malformed
/// SHIPPED default still reports a meaningful location).
///
/// A file that EXISTS but cannot be read stays a hard [`ConfigError::Read`]: an
/// unreadable override is an operator error worth surfacing, not something to
/// paper over with the default.
///
/// # Errors
///
/// Returns [`ConfigError::Read`] when the file exists but cannot be read.
fn read_prices_text(config_dir: &Path) -> Result<(String, PathBuf), ConfigError> {
    let path = config_dir.join(PRICES_FILE);
    if path.is_file() {
        let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        return Ok((text, path));
    }
    Ok((PRICES_DEFAULT.to_owned(), path))
}

/// Load the `[llm]` transport pinning (`base_url` + model) from `prices.toml` in the
/// resolved config dir (slice-close FIX A). Reuses the SAME config-dir resolution +
/// file as [`load_price_table`]; a missing `[llm]` table or field is `None`, never
/// an error.
///
/// # Errors
///
/// Returns [`ConfigError`] if the config dir cannot be resolved, the file cannot be
/// read, or its TOML cannot be parsed (the same failure modes as the price loader).
pub(crate) fn load_llm_transport() -> Result<LlmTransport, ConfigError> {
    load_llm_transport_from(&resolve_config_dir()?)
}

/// Load the `[llm]` transport pinning from `prices.toml` under an explicit
/// `config_dir` (the testable core of [`load_llm_transport`]).
///
/// # Errors
///
/// Returns [`ConfigError::Read`] if the file is present but unreadable, or
/// [`ConfigError::Parse`] if its TOML does not parse. A present file with no
/// `[llm]` table (or empty fields) is `Ok` with `None`s — never an error, and an
/// ABSENT file falls back to [`PRICES_DEFAULT`] (same seam as the price loader).
fn load_llm_transport_from(config_dir: &Path) -> Result<LlmTransport, ConfigError> {
    let (text, path) = read_prices_text(config_dir)?;
    let parsed: TransportConfig =
        toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })?;
    let llm = parsed.llm.unwrap_or_default();
    Ok(LlmTransport {
        base_url: llm.base_url,
        model: llm.model,
    })
}

/// Load the composer system prompt.
///
/// Resolution: if `$PULSE_PROMPT_DIR/composer.md` exists it wins (the
/// private-workspace runtime override); otherwise the compiled-in default
/// ([`prompts/composer.md`](../prompts/composer.md)) is returned.
///
/// # Errors
///
/// Returns [`ConfigError::Read`] only when a `$PULSE_PROMPT_DIR/composer.md`
/// override exists but cannot be read; the compiled-in default path is
/// infallible.
pub(crate) fn load_composer_prompt() -> Result<String, ConfigError> {
    load_composer_prompt_from(prompt_override_dir().as_deref())
}

/// The prompt-overlay directory the operator installed, if any — `$PULSE_PROMPT_DIR`.
///
/// The ONE place the variable is read, so every agent surface resolves the overlay
/// the same way. A composition root that needs to hand the directory onward
/// (`pulse coach`, whose injectable core takes it as a parameter so tests stay
/// hermetic) calls this; one that resolves in place ([`load_composer_prompt`])
/// calls it too.
pub(crate) fn prompt_override_dir() -> Option<PathBuf> {
    std::env::var_os(PROMPT_DIR_ENV).map(PathBuf::from)
}

/// Load the composer prompt given an optional override directory (the testable
/// core of [`load_composer_prompt`]).
///
/// # Errors
///
/// Returns [`ConfigError::Read`] when `prompt_dir/composer.md` exists but cannot
/// be read.
fn load_composer_prompt_from(prompt_dir: Option<&Path>) -> Result<String, ConfigError> {
    if let Some(dir) = prompt_dir {
        let path = dir.join(COMPOSER_FILE);
        if path.is_file() {
            return fs::read_to_string(&path).map_err(|source| ConfigError::Read { path, source });
        }
    }
    Ok(COMPOSER_PROMPT_DEFAULT.to_owned())
}

/// A resolved agent prompt and the version stamped on every call it drives.
///
/// The pair travels together on purpose (r1.s2 audit C2): `version` is the
/// content hash of the bytes in `text`, so a caller cannot stamp one prompt's
/// version onto another prompt's call.
pub(crate) struct CoachPrompt {
    /// The resolved prompt text — the overlay's if one won, else the compiled-in
    /// default.
    pub(crate) text: String,
    /// SHA-256 hex of `text`'s bytes — what lands in `llm_call.prompt_version`.
    pub(crate) version: String,
}

/// Load the coach prompt given an optional override directory.
///
/// Unlike [`load_composer_prompt`] this does NOT read the environment itself: the
/// coach's composition root resolves [`prompt_override_dir`] and passes it in, so
/// the injectable core (`run_coach_with`) stays hermetic under test rather than
/// picking up a developer's exported `$PULSE_PROMPT_DIR`.
///
/// **The version is computed from the RESOLVED bytes** — whichever source won
/// (audit C2). That is what makes an overlay edit change the recorded version with
/// no release step, and what makes the ledger's `prompt_version` an answer to
/// "which prompt produced this?" rather than "which release was this?".
///
/// # Errors
///
/// Returns [`ConfigError::Read`] when `prompt_dir/coach.md` exists but cannot be
/// read. A broken overlay is an error rather than a silent fall-back to the
/// default: silently coaching with a different prompt than the operator installed
/// is exactly the drift the version hash exists to make visible.
pub(crate) fn load_coach_prompt_from(
    prompt_dir: Option<&Path>,
) -> Result<CoachPrompt, ConfigError> {
    let text = if let Some(dir) = prompt_dir {
        let path = dir.join(COACH_FILE);
        if path.is_file() {
            fs::read_to_string(&path).map_err(|source| ConfigError::Read { path, source })?
        } else {
            COACH_PROMPT_DEFAULT.to_owned()
        }
    } else {
        COACH_PROMPT_DEFAULT.to_owned()
    };
    let version = prompt_version(&text);
    Ok(CoachPrompt { text, version })
}

/// The SHA-256 hex of a resolved prompt's bytes (audit C2).
///
/// `sha2` is the workspace's existing hashing dependency — the same one
/// `BacktestResult::result_content_hash` and `build.rs`'s engine fingerprint use.
/// No new crate.
fn prompt_version(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        COACH_PROMPT_DEFAULT, CONFIG_DIR_ENV, ConfigError, PROMPT_DIR_ENV, load_coach_prompt_from,
        load_composer_prompt_from, load_llm_transport, load_llm_transport_from,
        load_price_table_from, prompt_override_dir, resolve_config_dir,
    };
    use crate::domain::{SchemaVersion, TokenUsage};
    use std::sync::Mutex;

    /// Serializes the one env-mutating test so it cannot race any other test
    /// that reads `$PULSE_CONFIG_DIR` (only this test touches it).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// AC-5: prices load from the config FILE (not a Rust literal). Write a
    /// temp `prices.toml`, load it through the seam, and assert
    /// `PriceTable::cost` returns the configured nominal value.
    /// FR-25 / NFR-10 (cost accounting reads real per-model prices from config).
    #[test]
    fn load_price_table_from_config_reads_configured_nominal_value() {
        // Materialize the SHIPPED nominal price file into an isolated temp
        // config dir (no env mutation → race-free) and load it through the real
        // seam. Sourcing the fixture from the shipped file (rather than an
        // inline literal) also keeps price field names OUT of `config.rs`, so
        // AC-8's grep of the loader stays empty by construction.
        let shipped = include_str!("../../config/prices.toml");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prices.toml"), shipped).unwrap();

        let table = load_price_table_from(dir.path()).expect("load price table");
        assert_eq!(table.currency(), "USD");
        // The shipped nominal: 1_000_000 in @ 0.50/Mtok + 1_000_000 out @
        // 1.50/Mtok = 0.50 + 1.50 = 2.00 (guards the shipped data values too).
        let cost = table
            .cost(
                "gpt-oss:120b",
                &TokenUsage {
                    input_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                },
            )
            .expect("known model");
        assert_eq!(
            cost.normalize(),
            rust_decimal::Decimal::new(2, 0).normalize()
        );
    }

    /// An ABSENT price file falls back to the compiled-in shipped table, so a
    /// relocated/packaged binary stays self-contained (PR #93 Codex P1: nothing
    /// in this crate ever installs `prices.toml` into the app-support dir).
    #[test]
    fn load_price_table_from_missing_file_uses_the_shipped_default() {
        let dir = tempfile::tempdir().unwrap();
        let table = load_price_table_from(dir.path()).expect("absent file falls back");
        // Same nominal the shipped file encodes (guards the embed, not a literal).
        assert_eq!(table.currency(), "USD");
        assert!(
            table
                .cost(
                    "gpt-oss:120b",
                    &TokenUsage {
                        input_tokens: 1_000_000,
                        output_tokens: 1_000_000,
                    },
                )
                .is_ok(),
            "the embedded default must price the shipped models"
        );
    }

    /// The `[llm]` transport pinning also survives an absent file — same seam,
    /// so `pulse compose` still resolves its model/base-url off a packaged binary.
    #[test]
    fn load_llm_transport_from_missing_file_uses_the_shipped_default() {
        let dir = tempfile::tempdir().unwrap();
        let transport = load_llm_transport_from(dir.path()).expect("absent file falls back");
        assert_eq!(transport.model.as_deref(), Some("glm-5.3-flash"));
    }

    /// EVERY model-id site agrees with the shipped `[llm].model` — the guard the
    /// five-site duplication needs (#126).
    ///
    /// One logical value is written in five places: the config `[llm].model`, its
    /// `[models]` price-row key (covered by the sibling test below), and three
    /// compiled-in `const` fallbacks. Nothing else compares them, and the pins that
    /// look like they do are tautologies — `compose`'s own test asserts
    /// `compose_config(None).model == COMPOSE_MODEL`, i.e. the const against itself.
    ///
    /// A bump that updates the config and two consts but misses the third is
    /// therefore invisible to CI and lands as a runtime failure, worst on
    /// `llm-check`: it never reads `[llm].model` at all, so its `DEMO_MODEL` is the
    /// site most easily left behind and the one whose drift an operator meets while
    /// trying to diagnose their configuration.
    #[test]
    fn every_model_id_site_agrees_with_the_shipped_config() {
        let shipped = include_str!("../../config/prices.toml");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prices.toml"), shipped).unwrap();

        let configured = load_llm_transport_from(dir.path())
            .expect("shipped transport loads")
            .model
            .expect("the shipped file pins [llm].model");

        for (site, value) in [
            (
                "adapters::llm::openai_compat::OLLAMA_MODEL_ID",
                crate::adapters::llm::openai_compat::OLLAMA_MODEL_ID,
            ),
            (
                "cli::compose::COMPOSE_MODEL",
                crate::cli::compose::COMPOSE_MODEL,
            ),
            ("cli::llm::DEMO_MODEL", crate::cli::llm::DEMO_MODEL),
        ] {
            assert_eq!(
                value, configured,
                "{site} is {value:?} but config/prices.toml [llm].model is \
                 {configured:?} — a model bump moved one and not the other"
            );
        }
    }

    /// The shipped `[llm].model` MUST have a `[models]` price row in the same
    /// shipped file — the two halves of a model bump, checked against each other.
    ///
    /// This is the coupling guard the model-id duplication needs. `RedactingLoggingProvider`
    /// preflights `PriceTable::cost` BEFORE the billed call, so a `[llm].model` whose
    /// price row is missing or whose key is typo'd does not degrade — every `compose`
    /// and `llm-check` run fails closed with `no price for model …`. Without this
    /// test that lands at runtime with green CI, since nothing else reads the two
    /// tables together.
    ///
    /// Deliberately drives BOTH loaders off the SAME bytes and takes no `[llm]`
    /// fallback: an absent-file run would silently pass on the embedded default even
    /// if the on-disk file were inconsistent.
    #[test]
    fn the_shipped_default_model_is_priced_by_the_shipped_table() {
        let shipped = include_str!("../../config/prices.toml");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prices.toml"), shipped).unwrap();

        let model = load_llm_transport_from(dir.path())
            .expect("shipped transport loads")
            .model
            .expect("the shipped file pins [llm].model");
        let table = load_price_table_from(dir.path()).expect("shipped price table loads");

        table
            .cost(&model, &TokenUsage::default())
            .unwrap_or_else(|e| {
                panic!(
                    "shipped [llm].model {model:?} has no [models.\"{model}\"] price row \
                     — every compose/llm-check run would fail closed before the billed \
                     call: {e}"
                )
            });
    }

    /// Malformed TOML is a clear [`ConfigError::Parse`], never a panic.
    #[test]
    fn load_price_table_from_malformed_toml_is_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("prices.toml"),
            "this is = not valid toml [[[",
        )
        .unwrap();
        let err = load_price_table_from(dir.path()).expect_err("malformed toml errors");
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    /// AC-6: the compiled-in composer prompt encodes the never-emit-raw-JSON
    /// rule (`PROMPT_GOVERNANCE` §2.1 / §7) — a content invariant a future prompt
    /// edit cannot silently drop. NFR-6 (untrusted-input framing).
    #[test]
    fn composer_prompt_forbids_raw_dsl() {
        let prompt = load_composer_prompt_from(None).expect("compiled-in default");
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("never emit raw dsl json"),
            "composer prompt must state the never-emit-raw-DSL-JSON rule"
        );
    }

    /// The composer prompt frontmatter pins `dsl_schema_version` to
    /// `SchemaVersion::CURRENT` (NFR-12 model/schema pinning). A machine guard
    /// so the prompt and the DSL schema can never silently desync.
    #[test]
    fn composer_prompt_frontmatter_pins_current_schema_version() {
        let prompt = load_composer_prompt_from(None).expect("compiled-in default");
        let needle = format!("dsl_schema_version: \"{}\"", SchemaVersion::CURRENT);
        assert!(
            prompt.contains(&needle),
            "frontmatter must carry {needle:?} (SchemaVersion::CURRENT)"
        );
    }

    /// r2.s2.w3 (#160/#163): the shipped composer prompt must TEACH the
    /// schema-1.1.0 vocabulary — the `"timeframe"` operand token and `"atr"` as
    /// an indicator — or a described H4 filter / ATR stop is silently
    /// substituted instead of composed. Same machine-guard shape as the
    /// frontmatter pin above: a prompt edit that drops the vocabulary fails here.
    #[test]
    fn composer_prompt_teaches_the_1_1_0_vocabulary() {
        let prompt = load_composer_prompt_from(None).expect("compiled-in default");
        for needle in ["\"timeframe\"", "\"atr\""] {
            assert!(
                prompt.contains(needle),
                "composer prompt must carry {needle} — the 1.1.0 operand vocabulary"
            );
        }
    }

    /// The `$PULSE_PROMPT_DIR/composer.md` override wins over the compiled-in
    /// default (the private-workspace override path).
    #[test]
    fn composer_prompt_override_dir_wins_over_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("composer.md"), "OVERRIDDEN COMPOSER PROMPT").unwrap();
        let prompt = load_composer_prompt_from(Some(dir.path())).expect("override read");
        assert_eq!(prompt, "OVERRIDDEN COMPOSER PROMPT");
    }

    /// The shipped coach prompt as ONE whitespace-flattened line.
    ///
    /// Every prompt-contract needle below is a phrase, and the prompt is reflowed
    /// prose — matching against the raw text would make a contract depend on where a
    /// line happens to wrap. `scripts/check-adr-0021.sh` settled the same point for
    /// the ADRs; this is that, in Rust.
    fn coach_prompt_flattened() -> String {
        COACH_PROMPT_DEFAULT
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The coach prompt's structural-limit clause is a CONTRACT, not prose
    /// (PR #128, finding C2). `ADR-0021` says the coach must answer a structural
    /// need with a recorded inapplicability, never an approximation — and the
    /// shipped prompt once instructed the opposite, telling the model to reach for
    /// the closest parameter it could find. Nothing else guards prompt CONTENT:
    /// the ledger only hashes whatever text resolved, so a re-edit would ship
    /// unnoticed.
    ///
    /// It asserts the bad instruction is absent and the rule that replaced it is
    /// present. The structural-decline protocol itself is deliberately NOT here —
    /// that is `r1.s4` (pulseai-labs/pulse-trader#131).
    #[test]
    fn the_shipped_coach_prompt_never_asks_for_an_approximated_structural_change() {
        let prompt = coach_prompt_flattened();

        assert!(
            !prompt.contains("closest parameter"),
            "the coach must not be told to approximate a structural change with the nearest parameter (ADR-0021)"
        );
        assert!(
            prompt.contains("do not approximate a structural change"),
            "the prompt must state the no-approximation rule outright, not merely omit the old instruction"
        );
    }

    /// The MUTABLE SURFACE is a contract too (PR #128, finding F4). `sweepable_paths`
    /// visits indicator specs, exit numerics and risk numerics — and never
    /// `ValueSource::Constant`, so the `30` in `RSI(14) < 30` renders as a plain
    /// number in the document the model reads and is nonetheless unaddressable. A
    /// prompt inviting "any numeric leaf" is therefore an instruction a model can
    /// follow faithfully into a deterministic `UnknownPath`.
    #[test]
    fn the_shipped_coach_prompt_names_the_mutable_surface_and_excludes_condition_constants() {
        let prompt = coach_prompt_flattened();

        for family in ["indicator periods", "exit parameters", "risk parameters"] {
            assert!(
                prompt.contains(family),
                "the prompt must name the `{family}` family, which is what it can actually address"
            );
        }
        assert!(
            prompt.contains("cannot change a constant"),
            "the constant exclusion must be an instruction, not an omission"
        );
        assert!(
            prompt.contains("rsi(14) < 30"),
            "the exclusion needs the concrete case a model will meet in the fixture"
        );
    }

    /// r2.s2.w3 (ADR-0021 decision 1, SPINE b7): the shipped coach prompt must
    /// NAME the ATR stop's `multiple` leaf it may retune — `exits[0].multiple` —
    /// so the prompt and the mutate grammar can never silently desync (w1
    /// already registered the leaf; this guard pins the prompt to it). Same
    /// whitespace-flattened shape as the other coach-prompt contracts.
    #[test]
    fn coach_prompt_names_the_atr_numeric_leaves() {
        let prompt = coach_prompt_flattened();
        assert!(
            prompt.contains("exits[0].multiple"),
            "the coach prompt must name exits[0].multiple — an ATR stop's multiple leaf"
        );
    }

    /// The MFE/MAE numbers are POTENTIAL bounds, and the prompt has to say so
    /// (PR #128, finding G3). The engine folds every bar the position was open into
    /// the running excursion, the exit bar included and in full, so a trade that
    /// exits at a bar's open still carries that whole bar's range — movement after
    /// the close included. A coach told only "MFE/MAE aggregates in R" reads them as
    /// profit a tighter stop would have captured, which is a parameter change made
    /// on a number that was never reachable. Known behaviour, tracked as #55 and not
    /// closed here.
    #[test]
    fn the_shipped_coach_prompt_labels_mfe_mae_as_full_bar_potential() {
        let prompt = coach_prompt_flattened();

        for needle in [
            "full-bar potential",
            "entry-through-exit bar ranges",
            "the entire exit bar is folded in even when the trade exits at its open",
            "not an experienced path",
        ] {
            assert!(
                prompt.contains(needle),
                "the prompt must carry {needle:?}, or the excursion numbers read as reachable"
            );
        }
    }

    /// The live coach path: `pulse coach` resolves [`prompt_override_dir`] and
    /// feeds it to [`load_coach_prompt_from`], so an operator's
    /// `$PULSE_PROMPT_DIR/coach.md` really does drive the turn AND the recorded
    /// `prompt_version` (audit C2). Before PR #128 the live arm passed `None` here
    /// and the overlay was silently ignored — the moat installed and inert.
    ///
    /// Env-mutating, so it shares `ENV_LOCK`.
    #[test]
    fn the_live_coach_path_resolves_the_prompt_dir_overlay() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("coach.md"), "OVERRIDDEN COACH PROMPT").unwrap();

        // SAFETY: serialized by ENV_LOCK; no other test reads $PULSE_PROMPT_DIR.
        unsafe {
            std::env::set_var(PROMPT_DIR_ENV, dir.path());
        }
        let resolved = prompt_override_dir();
        let overlay = load_coach_prompt_from(resolved.as_deref()).expect("overlay read");
        // SAFETY: same lock scope; restore the environment before releasing it.
        unsafe {
            std::env::remove_var(PROMPT_DIR_ENV);
        }

        assert_eq!(resolved.as_deref(), Some(dir.path()));
        assert_eq!(overlay.text, "OVERRIDDEN COACH PROMPT");
        let default = load_coach_prompt_from(None).expect("compiled-in default");
        assert_ne!(
            overlay.version, default.version,
            "an overlay edit must change the recorded prompt_version with no release step"
        );

        // And with nothing exported, the compiled-in default is what resolves.
        assert!(
            prompt_override_dir().is_none(),
            "no $PULSE_PROMPT_DIR means no overlay"
        );
    }

    /// Resolution order #1: an explicit `$PULSE_CONFIG_DIR` wins. The sole
    /// env-mutating test, serialized by `ENV_LOCK`.
    #[test]
    fn resolve_config_dir_honors_pulse_config_dir_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized by ENV_LOCK; no other test reads $PULSE_CONFIG_DIR.
        unsafe {
            std::env::set_var(CONFIG_DIR_ENV, dir.path());
        }
        let resolved = resolve_config_dir().expect("env override resolves");
        // SAFETY: same lock scope; restore the environment before releasing it.
        unsafe {
            std::env::remove_var(CONFIG_DIR_ENV);
        }
        assert_eq!(resolved, dir.path());
    }

    /// FIX A: the `[llm]` table is now LIVE data — a `$PULSE_CONFIG_DIR` prices.toml
    /// whose `[llm].model` is `kimi-k2.6` resolves through the SAME config-dir order
    /// as the price table. The sole other env-mutating test shares `ENV_LOCK`.
    #[test]
    fn load_llm_transport_reads_model_from_config_dir_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("prices.toml"),
            "currency = \"USD\"\n[llm]\nbase_url = \"https://example.test/v1\"\nmodel = \"kimi-k2.6\"\n",
        )
        .unwrap();
        // SAFETY: serialized by ENV_LOCK; no other test reads $PULSE_CONFIG_DIR.
        unsafe {
            std::env::set_var(CONFIG_DIR_ENV, dir.path());
        }
        let transport = load_llm_transport();
        // SAFETY: same lock scope; restore the environment before releasing it.
        unsafe {
            std::env::remove_var(CONFIG_DIR_ENV);
        }
        let transport = transport.expect("transport loads from the env config dir");
        assert_eq!(transport.model.as_deref(), Some("kimi-k2.6"));
        assert_eq!(
            transport.base_url.as_deref(),
            Some("https://example.test/v1")
        );
    }

    /// FIX A: a present prices.toml with NO `[llm]` table yields `None`s (never an
    /// error) — the composition root then falls back to its documented `const`s.
    /// Race-free (explicit dir, no env mutation).
    #[test]
    fn load_llm_transport_missing_table_is_none_not_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("prices.toml"), "currency = \"USD\"\n").unwrap();
        let transport =
            load_llm_transport_from(dir.path()).expect("no [llm] table is not an error");
        assert!(transport.model.is_none());
        assert!(transport.base_url.is_none());
    }
}
