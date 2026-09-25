//! Secret access: the macOS Keychain reader (VS-1.3.1 work-1.03, FR-1 / NFR-5,
//! decision 5) **and** the LLM credential resolver (r1.s1.w2).
//!
//! This file is a registered touch surface of the *LLM credential handling and
//! redaction* risk gate, which is why r1.s1.w2 moved credential resolution here out
//! of `src/cli/compose.rs`. The second half of the file
//! ([`resolve_llm_api_key`] and friends) carries all three of the gate's controls:
//! **least privilege** (a credential file is used only if owned by the running user
//! and free of group/world bits, and its bytes are never read before those checks
//! pass), **no-secret-in-log** (the resolver returns an opaque
//! [`ApiKey`](crate::domain::ApiKey) with no `Display` and a value-free `Debug`),
//! and **audit trail** (the [`CredentialSource`] label rides onto every persisted
//! `LlmCall`). The move is also what makes the credential reachable from
//! `src/tauri/` — `mod cli` is private in `src/lib.rs`.
//!
//! The READ half of secret handling: [`glm_api_key`] fetches the GLM API key from
//! the macOS Keychain (the data-protection keychain `keyring` binds to — see the
//! seeding-reality note below) so the composition root (1.05) can inject it into
//! [`OpenAiCompatProvider`](crate::adapters::llm::openai_compat::OpenAiCompatProvider)
//! as a ctor arg — the key
//! never lives in a committed config file, an env var baked into the binary, or
//! plaintext on disk (NFR-5). A **missing** entry returns a clear
//! [`LlmError::Config`] saying plainly that seeding this path is not yet
//! supported, NEVER a panic (and never naming `pulse setup-keys`, a verb that
//! does not exist — see `read_secret`). Like the
//! resolver below, it returns an opaque [`ApiKey`](crate::domain::ApiKey) tagged
//! [`CredentialSource::Keychain`](crate::domain::CredentialSource::Keychain) —
//! the audit-trail control, so a `pulse llm-check` ledger row records where its
//! key came from too (r1.s1.w2, closing a gap the Keychain path used to leave
//! open: it returned a bare `String` and no `LlmCall` this composition root wrote
//! ever recorded provenance).
//!
//! READ path ONLY — the `set_password` / verify half (`pulse setup-keys`) is
//! VS-1.3.4. Pinned to `keyring` 3.x, whose `Entry::new` + `get_password` bind
//! DIRECTLY to the platform keychain (no store registration). `keyring` 4.x split
//! into `keyring-core` + store crates whose lazy default-store init returned
//! `NoDefaultStore` at runtime for our unsigned dev binary (found by the live
//! user-demo at VS-1.3.1 slice close).
//!
//! **Seeding reality (macOS data-protection keychain — found at slice close):**
//! `keyring` reads/writes the modern *data-protection* keychain, which is
//! access-group-scoped by code identity. Two consequences: (a) a key written by
//! the `security add-generic-password` CLI lands in the *file-based* login
//! keychain and is INVISIBLE to `keyring` — the CLI stopgap does NOT work; (b) an
//! unsigned dev binary gets a distinct ad-hoc identity, so only the `pulse` binary
//! ITSELF (via VS-1.3.4 `pulse setup-keys`, which calls `set_password`) can seed a
//! key it can later read back. Until `setup-keys` + code-signing (v1.5) land, the
//! read path is exercised end-to-end only from a same-identity binary; the live
//! GLM transport is validated by injecting the key directly (VS-1.3.1 close demo).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use keyring::Entry;

use crate::adapters::db::default_data_dir;
use crate::domain::{ApiKey, CredentialSource, CredentialStatus, LlmError};

/// The environment variable / `.env` key naming the LLM API key.
const OLLAMA_API_KEY_VAR: &str = "OLLAMA_API_KEY";

/// The data-overlay directory override (ADR-0014). Must stay in step with
/// `agent::config::CONFIG_DIR_ENV`, which names the same variable for the price /
/// prompt overlay — the two rings deliberately do not import each other, so this is
/// the one literal that is repeated rather than shared.
const PULSE_CONFIG_DIR_ENV: &str = "PULSE_CONFIG_DIR";

/// The credential file's name in every searched directory.
const DOTENV_FILE: &str = ".env";

/// The Keychain service name `PulseTrader` stores its secrets under.
const KEYCHAIN_SERVICE: &str = "PulseTrader";

/// The Keychain account (item name) for the GLM API key.
const GLM_API_KEY_ACCOUNT: &str = "glm_api_key";

/// Read the GLM API key from the macOS Keychain (FR-1 / NFR-5).
///
/// Returns an opaque [`ApiKey`] tagged [`CredentialSource::Keychain`] (r1.s1.w2
/// step 7, the audit-trail control) rather than a bare `String` — so a caller
/// stamps the ledger with the SAME provenance label this function used to mint the
/// value, via [`ApiKey::source()`], instead of a second, separately-hardcoded
/// literal that could drift out of step with it.
///
/// # Errors
///
/// Returns [`LlmError::Config`] when the Keychain entry is absent or the platform
/// store cannot be reached (e.g. the key was never seeded) — with a message that
/// says so plainly rather than naming an unbuilt provisioning verb. Never panics.
pub fn glm_api_key() -> Result<ApiKey, LlmError> {
    read_secret(KEYCHAIN_SERVICE, GLM_API_KEY_ACCOUNT)
        .map(|value| ApiKey::new(value, CredentialSource::Keychain))
}

// ---------------------------------------------------------------------------
// r1.s1.w2 — the LLM credential resolver (the risk gate's registered surface).
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// r3.s3.w3 — the server credential profile (R3): WHAT a process searches and
// WHAT a refusal does, selected once per process.
// ---------------------------------------------------------------------------

/// How THIS process resolves the LLM credential (r3.s3.w3, R3).
///
/// The default is [`Desktop`](CredentialProfile::Desktop) — the desktop and
/// CLI behaviour, byte for byte. [`pulse serve`](crate::server::bind::serve)
/// selects [`Server`](CredentialProfile::Server) once, at its Step 3, before
/// the listener accepts a request; every later resolution in the server
/// process — the ordinary resolver's, and any handler's — then follows the
/// server search. Nothing else selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProfile {
    /// The desktop/CLI search: the full chain — env, `$PULSE_CONFIG_DIR/.env`,
    /// the cwd and manifest `.env`s, the app-data `.env`.
    Desktop,
    /// The server search: env, `$PULSE_CONFIG_DIR/.env`, the app-data `.env` —
    /// never the cwd/manifest dotenv, never the Keychain. A found-but-refused
    /// file stops `pulse serve`'s startup instead of reading as "absent".
    Server,
}

/// The process-wide profile cell: written once by `serve`'s Step 3, read by
/// every resolution.
static CREDENTIAL_PROFILE: OnceLock<CredentialProfile> = OnceLock::new();

/// Select this process's credential profile — the write-once cell `pulse
/// serve` sets at its Step 3 (r3.s3.w3, R3). Nothing else sets it.
///
/// Setting it twice is a programming error: it **panics in debug builds**
/// (test builds included) and **is ignored in release**, where the first
/// selection stands — recorded behaviour, asserted by
/// `tests/secrets_profile.rs::setting_the_profile_twice_is_a_programming_error_in_debug`.
pub fn set_credential_profile(profile: CredentialProfile) {
    let selection = CREDENTIAL_PROFILE.set(profile);
    debug_assert!(
        selection.is_ok(),
        "the credential profile is already set; it is selected once per process"
    );
    // Release build: the second selection is deliberately ignored (the first
    // stands), and `selection` is consumed in both profiles.
    let _ = selection;
}

/// The credential profile this process resolves under —
/// [`CredentialProfile::Desktop`] unless [`set_credential_profile`] ran.
#[must_use]
pub fn credential_profile() -> CredentialProfile {
    *CREDENTIAL_PROFILE.get_or_init(|| CredentialProfile::Desktop)
}

/// The set of places one credential resolution will look, as DATA.
///
/// Injecting the search (rather than reading `std::env` deep inside the resolver)
/// is what makes the precedence order, the permission refusals and the error text
/// testable without a single `set_var`: Rust 2024 makes `set_var` `unsafe` and it
/// races every other test in the binary, so an env-mutating suite would be both
/// unsound and order-coupled.
///
/// [`from_process_env`](Self::from_process_env) is the production build and stays
/// `pub(crate)` — an out-of-crate caller must not be able to harvest the real
/// environment's key into a struct it controls.
///
/// No derived `Debug`: `env_key` holds a raw credential, and a derived `Debug`
/// would print it.
#[derive(Clone)]
pub struct CredentialSearch {
    env_key: Option<String>,
    config_dir: Option<PathBuf>,
    dotenv_dirs: Vec<PathBuf>,
    app_data_dir: Option<PathBuf>,
    running_uid: u32,
}

impl Default for CredentialSearch {
    fn default() -> Self {
        Self {
            env_key: None,
            config_dir: None,
            dotenv_dirs: Vec::new(),
            app_data_dir: None,
            running_uid: running_uid(),
        }
    }
}

/// The uid of the user this process is running as.
///
/// There is no `std` API for it, so this is the one `libc` call in the crate. A
/// direct `getuid()` cannot fail and has no error return — POSIX specifies it as
/// always successful — so there is nothing to map into an error here.
fn running_uid() -> u32 {
    // SAFETY: `getuid()` is a POSIX call that takes no arguments, touches no memory,
    // and is documented as always succeeding. It has no failure mode to handle and
    // no invariant a caller can violate.
    unsafe { libc::getuid() }
}

/// Hand-written `Debug` that never prints the environment-sourced key VALUE (the
/// same discipline `Redactor`'s `Debug` follows). The directory paths are not
/// secret and stay legible — they are exactly what a diagnostic needs.
impl std::fmt::Debug for CredentialSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSearch")
            .field(
                "env_key",
                &self.env_key.as_ref().map_or("none", |_| "<redacted>"),
            )
            .field("config_dir", &self.config_dir)
            .field("dotenv_dirs", &self.dotenv_dirs)
            .field("app_data_dir", &self.app_data_dir)
            .field("running_uid", &self.running_uid)
            .finish()
    }
}

impl CredentialSearch {
    /// A search that will look nowhere — the base every test case builds on.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Set the `OLLAMA_API_KEY` value the process environment would have supplied
    /// (`None` = the variable is unset or empty).
    #[must_use]
    pub fn with_env_key(mut self, key: Option<String>) -> Self {
        self.env_key = key.filter(|k| !k.is_empty());
        self
    }

    /// Set the `$PULSE_CONFIG_DIR` override directory (resolution step 2).
    #[must_use]
    pub fn with_config_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.config_dir = dir;
        self
    }

    /// Set the working-directory / manifest-directory `.env` locations, searched in
    /// the given order (resolution step 3).
    #[must_use]
    pub fn with_dotenv_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.dotenv_dirs = dirs;
        self
    }

    /// Set the application data directory — the same directory `pulse.db` lives in
    /// (resolution step 4).
    #[must_use]
    pub fn with_app_data_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.app_data_dir = dir;
        self
    }

    /// Override the uid the ownership check compares a credential file against
    /// (defaults to the real running uid).
    ///
    /// This exists because an unprivileged test cannot `chown` a file to another
    /// user; moving the OTHER side of the comparison is the only way to exercise the
    /// wrong-owner path (AC-5) on a real file. It weakens nothing: the caller still
    /// receives an opaque [`ApiKey`] it cannot read, and any caller able to set this
    /// could already have read the file itself.
    #[must_use]
    pub fn with_running_uid(mut self, uid: u32) -> Self {
        self.running_uid = uid;
        self
    }

    /// The production search, read from the real process environment.
    ///
    /// `pub(crate)`: an out-of-crate caller must not be able to harvest the running
    /// environment's key into a struct it holds.
    ///
    /// The app-data location comes from [`default_data_dir`], the SAME
    /// `directories::ProjectDirs` helper that resolves `pulse.db` — the credential
    /// file sits beside the database rather than in a second invented location.
    ///
    /// **The manifest directory is included only when it exists.** `CARGO_MANIFEST_DIR`
    /// is baked in at COMPILE time, so in a shipped `.app` it names a path on the
    /// BUILD machine. Searching it there is merely useless; listing it is worse —
    /// `missing_credential_message` names every searched location verbatim, and that
    /// message is what the Designer renders when composition is blocked, so an
    /// end user would be sent hunting for a directory they cannot create and have
    /// no way to reason about. On a developer's machine the directory exists and is
    /// searched exactly as before.
    pub(crate) fn from_process_env() -> Self {
        // The server profile (r3.s3.w3, R3) narrows the FILE search to the two
        // deliberate locations: no cwd `.env`, no compile-time manifest `.env`
        // (which on a deployed server names a build machine's path anyway).
        // The exhausted-search message is built from the same locations, so it
        // never advertises a place the server would not read.
        let mut dotenv_dirs = Vec::new();
        if credential_profile() == CredentialProfile::Desktop {
            if let Ok(cwd) = std::env::current_dir() {
                dotenv_dirs.push(cwd);
            }
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            if manifest_dir.is_dir() && !dotenv_dirs.contains(&manifest_dir) {
                dotenv_dirs.push(manifest_dir);
            }
        }

        Self::empty()
            .with_env_key(std::env::var(OLLAMA_API_KEY_VAR).ok())
            .with_config_dir(std::env::var_os(PULSE_CONFIG_DIR_ENV).map(PathBuf::from))
            .with_dotenv_dirs(dotenv_dirs)
            .with_app_data_dir(default_data_dir().ok())
    }

    /// The FILE locations this search will visit, in precedence order, each paired
    /// with the [`CredentialSource`] label it would produce.
    ///
    /// The process environment is deliberately absent here — it is not a file, so it
    /// has no path to name in an error and no permissions to validate.
    fn file_locations(&self) -> Vec<(PathBuf, CredentialSource)> {
        let mut locations = Vec::new();
        if let Some(dir) = &self.config_dir {
            locations.push((dir.join(DOTENV_FILE), CredentialSource::ConfigDir));
        }
        for dir in &self.dotenv_dirs {
            locations.push((dir.join(DOTENV_FILE), CredentialSource::CwdDotenv));
        }
        if let Some(dir) = &self.app_data_dir {
            locations.push((dir.join(DOTENV_FILE), CredentialSource::AppDataDir));
        }
        locations
    }
}

/// Resolve the LLM API key from an explicit [`CredentialSearch`] — the injectable
/// core `tests/credential_source.rs` drives.
///
/// Resolution order (step 2, and the thing AC-3 tests): the process environment
/// `OLLAMA_API_KEY` → `$PULSE_CONFIG_DIR/.env` → the working-directory / manifest
/// `.env` → the application data directory's `.env`. `$PULSE_CONFIG_DIR` sits ahead
/// of both defaults so ADR-0014's overlay seam is honoured rather than bypassed.
///
/// Under the SERVER profile (r3.s3.w3, R3) the cwd and manifest `.env` slots are
/// dropped from every search that reaches this core — injected searches included —
/// so the guarantee "every call through the ordinary resolver in a server process
/// follows the server profile" is structural, not caller-dependent: w2's compose
/// and coach handlers inherit it without a new call of their own. Under the
/// desktop profile this is a no-op and the behaviour is unchanged byte for byte.
///
/// # Errors
///
/// Returns [`LlmError::Config`] when no location supplied a credential, naming
/// every location that was searched, and when a found credential file was
/// refused — the refusal text is [`CredentialFileRefusal::desktop_message`],
/// byte for byte what this module has always said.
pub fn resolve_llm_api_key_in(search: &CredentialSearch) -> Result<ApiKey, LlmError> {
    let filtered;
    let search = if credential_profile() == CredentialProfile::Server {
        filtered = CredentialSearch {
            dotenv_dirs: Vec::new(),
            ..search.clone()
        };
        &filtered
    } else {
        search
    };
    match resolve_llm_api_key_inner(search) {
        Resolution::Found(key) => Ok(key),
        Resolution::Absent => Err(LlmError::Config(missing_credential_message(search))),
        Resolution::Refused(refusal) => Err(LlmError::Config(refusal.desktop_message())),
    }
}

/// The tri-state heart of one resolution over one [`CredentialSearch`] — found,
/// genuinely absent everywhere, or a REFUSED file (the fail-closed abort
/// [`read_credential_file`] promises). [`resolve_llm_api_key_in`] words the last
/// two for the desktop surfaces; [`resolve_credential_for_startup`] words the
/// refusal for `pulse serve`'s startup line.
enum Resolution {
    Found(ApiKey),
    Absent,
    Refused(CredentialFileRefusal),
}

fn resolve_llm_api_key_inner(search: &CredentialSearch) -> Resolution {
    if let Some(key) = &search.env_key {
        return Resolution::Found(ApiKey::new(key.clone(), CredentialSource::Env));
    }
    for (path, source) in search.file_locations() {
        match read_credential_file(&path, search.running_uid) {
            Ok(Some(value)) => return Resolution::Found(ApiKey::new(value, source)),
            Ok(None) => {}
            Err(refusal) => return Resolution::Refused(refusal),
        }
    }
    Resolution::Absent
}

/// The production resolver: search the real process environment.
///
/// This is the seam `src/cli/compose.rs` and (from `r1.s1.w4`) the Tauri ring call.
/// It exists as a zero-arg `pub(crate)` wrapper over [`resolve_llm_api_key_in`]
/// because an out-of-crate integration test cannot call a `pub(crate)` item — the
/// same injectable-core idiom as `run_compose_with`/`run_compose` and
/// `Db::with_path`/`Db::open_default`.
///
/// # Errors
///
/// Returns [`LlmError::Config`] when no location supplied a credential, or when a
/// credential file was found but refused by the permission checks.
pub(crate) fn resolve_llm_api_key() -> Result<ApiKey, LlmError> {
    resolve_llm_api_key_in(&CredentialSearch::from_process_env())
}

/// The outcome of `pulse serve`'s once-per-process startup resolution — Step 3
/// (r3.s3.w3, R3). `Found`'s value is dropped at once (handlers resolve per
/// call, as the desktop always has); `Absent` starts the server anyway;
/// `Refused` stops the startup with [`CredentialFileRefusal`]'s one-line
/// rendering.
pub(crate) enum StartupCredential {
    /// A credential answered; carry the label to the startup line, drop the
    /// value.
    Found(ApiKey),
    /// Nothing answered anywhere — start without a credential.
    Absent,
    /// A file was found and REFUSED — the startup must not continue.
    Refused(CredentialFileRefusal),
}

/// Resolve the credential ONCE through the production search — which
/// [`set_credential_profile`] has already narrowed to `Server` when called from
/// `serve`'s Step 3. The distinction the startup acts on (absent versus
/// refused) is structural, so `bind.rs` never parses error text.
pub(crate) fn resolve_credential_for_startup() -> StartupCredential {
    match resolve_llm_api_key_inner(&CredentialSearch::from_process_env()) {
        Resolution::Found(key) => StartupCredential::Found(key),
        Resolution::Absent => StartupCredential::Absent,
        Resolution::Refused(refusal) => StartupCredential::Refused(refusal),
    }
}

/// Report WHICH credential source would answer, without returning the key — the
/// injectable core behind the no-credential banner.
///
/// It runs the very same precedence chain [`resolve_llm_api_key_in`] does, rather
/// than a parallel "does a file exist" check. That matters: a banner computed by a
/// second, looser rule would cheerfully report "credential found" over a file the
/// resolver is about to refuse, and send the operator hunting the wrong problem.
/// The resolved key is dropped immediately and never leaves this function.
///
/// A file that exists but fails the permission checks reports
/// [`CredentialStatus::None`] — not usable is not usable, which is what a banner
/// needs to say. The *reason* belongs in the resolver's error, which the operator
/// sees when they actually try to run.
///
/// Performs no LLM request and touches no network: it reads at most four small
/// files (the overlay, working, manifest and app-data `.env`s), each opened
/// non-blocking, so it is safe to call on a UI paint.
#[must_use]
pub fn llm_credential_status_in(search: &CredentialSearch) -> CredentialStatus {
    resolve_llm_api_key_in(search).map_or(CredentialStatus::None, |key| {
        CredentialStatus::from(key.source())
    })
}

/// The production banner read: report which source would answer, from the real
/// process environment.
///
/// This is the seam `r1.s1.w5` renders its no-credential banner from, and —
/// since r3.s3.w3 — the value-free read the credential-profile suite and the
/// `server_auth` credential cases drive from OUTSIDE the crate: it returns the
/// [`CredentialStatus`] label enum and nothing else, so publishing it cannot
/// leak credential material (the zero-arg [`resolve_llm_api_key`] stays
/// `pub(crate)` for exactly that harvest reason). It consults the process's
/// [`credential_profile`], so in a `pulse serve` process it reports the
/// SERVER's status — the reading AC-1(c) pins for a cwd-only `.env`.
#[must_use]
pub fn llm_credential_status() -> CredentialStatus {
    llm_credential_status_in(&CredentialSearch::from_process_env())
}

/// The permission bits that must be clear on a credential file: every group and
/// world bit. A file with any of them set is readable (or worse) by some other local
/// process, which is exactly the exposure this check exists to refuse.
///
/// Deliberately a MASK test rather than `mode == 0o600`: an equality check would
/// refuse `0400`, a file strictly safer than the one it accepts.
const GROUP_AND_WORLD_BITS: u32 = 0o077;

/// WHY a found credential file was refused — the structured shape of the
/// fail-closed check (r3.s3.w3).
///
/// Two renderings hang off this one type: [`Self::desktop_message`] is the
/// resolver's error text, BYTE FOR BYTE the strings this module has always
/// returned (the desktop surfaces them unchanged — operator ruling, r3.s3.w3),
/// and the [`Display`](std::fmt::Display) impl is `pulse serve`'s one-line
/// startup refusal body, which `ServeError::CredentialRefused` (in
/// `crate::server::bind`) frames with its `pulse serve: refusing to start: `
/// lead-in. The credential VALUE is not part of the type — the file's bytes
/// are never read before the checks pass — so no rendering of a refusal can
/// leak one.
#[derive(Debug)]
pub(crate) enum CredentialFileRefusal {
    /// The file could not be OPENED for a reason other than not-found.
    Open {
        /// The searched location (as the operator knows it).
        path: PathBuf,
        /// The open failure, rendered only through its kind on the serve line.
        error: std::io::Error,
    },
    /// Opened, but its metadata could not be read from the handle.
    Metadata {
        /// The searched location.
        path: PathBuf,
        /// The metadata failure.
        error: std::io::Error,
    },
    /// Owned by a uid other than the running one.
    Owner {
        /// The searched location.
        path: PathBuf,
        /// The file's actual owner.
        owner: u32,
        /// The uid the check compared against.
        running_uid: u32,
    },
    /// Group or world bits set (`mode & 0o077 != 0`).
    LooseMode {
        /// The searched location.
        path: PathBuf,
        /// The offending permission mode, already masked to `0o777`.
        mode: u32,
    },
    /// Passed the checks but the bytes could not be read.
    Read {
        /// The searched location.
        path: PathBuf,
        /// The read failure.
        error: std::io::Error,
    },
}

impl CredentialFileRefusal {
    /// The resolver's refusal text: byte for byte what this module returned
    /// before the profile existed (operator ruling, r3.s3.w3 — the desktop
    /// message does not change).
    fn desktop_message(&self) -> String {
        match self {
            Self::Open { path, error } => format!(
                "refusing to fall through past the credential file {}: it could not be \
                 opened ({error}) — a credential location that errors is not the same as \
                 one that is absent, and silently using a lower-priority key would hide it. \
                 The file was never read. Fix: make {} readable, or remove it.",
                path.display(),
                path.display(),
            ),
            Self::Metadata { path, error } => format!(
                "refusing to read the credential file {}: its metadata could not be read after \
                 opening it ({error}) — the file was opened but never read. Fix: make {} \
                 accessible, or remove it.",
                path.display(),
                path.display(),
            ),
            Self::Owner {
                path,
                owner,
                running_uid,
            } => format!(
                "refusing to read the credential file {}: it is owned by uid {owner} but this \
                 process is running as uid {running_uid} — a credential file must be owned by \
                 the user reading it, and this one was never read. Fix: chown {running_uid} {}",
                path.display(),
                path.display(),
            ),
            Self::LooseMode { path, mode } => format!(
                "refusing to read the credential file {}: its mode is {mode:04o}, which grants \
                 access to group or others — a credential file must be reachable only by its \
                 owner, and this one was never read. Fix: chmod 0600 {}",
                path.display(),
                path.display(),
            ),
            Self::Read { path, error } => format!(
                "the credential file {} passed its permission checks but could not be read: {error}",
                path.display(),
            ),
        }
    }
}

/// `pulse serve`'s Step-3 refusal body (operator ruling, r3.s3.w3): one line
/// naming the path and the reason, never the value. The io failures render
/// their KIND only — an OS error string can quote paths, never credential
/// bytes, but the kind is what the operator needs.
impl std::fmt::Display for CredentialFileRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LooseMode { path, mode } => write!(
                f,
                "{} is group/world-readable (mode {mode:04o}); chmod 600 it",
                path.display()
            ),
            Self::Owner {
                path,
                owner,
                running_uid,
            } => write!(
                f,
                "{} is owned by uid {owner} but the server runs as uid {running_uid}; chown {running_uid} it",
                path.display()
            ),
            Self::Open { path, error } => {
                write!(
                    f,
                    "{} could not be opened ({})",
                    path.display(),
                    error.kind()
                )
            }
            Self::Metadata { path, error } => write!(
                f,
                "{} could not be inspected ({})",
                path.display(),
                error.kind()
            ),
            Self::Read { path, error } => write!(
                f,
                "{} passed its checks but could not be read ({})",
                path.display(),
                error.kind()
            ),
        }
    }
}

/// Validate one candidate file fail-closed, then hand back its vetted bytes.
///
/// `Ok(None)` means "this location does not answer" — the file genuinely does not
/// exist (`File::open` fails with [`std::io::ErrorKind::NotFound`]), or it exists but
/// carries no `OLLAMA_API_KEY` line, or it is a directory rather than a file. All
/// three are ordinary misses that fall through to the next location.
///
/// `Err` means the file was found and **REFUSED** — including when it could not even
/// be opened, or its metadata could not be determined (`EACCES` on a parent
/// directory, `EIO`, `ELOOP`, `ENOTDIR`, …). A failure that is NOT `NotFound` is
/// deliberately treated as a refusal rather than an ordinary miss: `Ok(None)` there
/// would silently downgrade to a lower-priority credential, which is exactly the
/// "the file is absent" lie this function exists to never tell. A refusal aborts the
/// whole resolution rather than falling through to the next location: silently
/// downgrading to a lower-priority credential would leave the operator with a
/// working `pulse` and an exposed key file they were never told about. That is the
/// "never read, never silently downgraded" half of the least-privilege control.
///
/// The value is never read before the checks pass, so no refusal message can contain
/// it — the file's bytes have not been touched at that point.
///
/// # Open once, validate the handle, read the handle (closes a TOCTOU)
///
/// The file is opened exactly ONCE, with [`std::fs::File::open`]. Its owner and mode
/// are checked with [`File::metadata`](std::fs::File::metadata) — an `fstat` on the
/// already-open descriptor — and the credential is then read through that SAME
/// handle. A path-based `std::fs::metadata(path)` followed by a separate
/// `std::fs::read_to_string(path)` resolves `path` TWICE, and anyone able to modify
/// the file's containing directory (a shared working directory, a configured
/// overlay) can repoint `path` at a different inode in the gap between the two
/// resolutions — the checks would then validate one file while the read pulls bytes
/// from another, never-validated one, defeating this function's fail-closed
/// contract. Validating the open handle instead of the path closes that window: the
/// owner and mode checked are, by construction, the owner and mode of the exact
/// inode [`read_to_string`](std::io::Read::read_to_string) is about to read, because
/// there is only one open of the file, not two independent resolutions of its path.
///
/// This deliberately does NOT reject symlinks. `File::open` follows a symlink to its
/// target, and the `fstat` above reports the TARGET's owner and mode, not the
/// link's — so a symlink whose target is owned by someone else, or has group/world
/// bits set, is refused by the very same ownership/mode checks below that a direct
/// file in that state would be refused by. Rejecting symlinks outright (or checking
/// them with `symlink_metadata`/`lstat`, which reports the LINK's own owner and mode
/// rather than the target's) would be a weaker check than this one, not a stronger
/// one: it would validate the wrong inode instead of the one actually read.
/// r3.s3.w5: the client-core's connection file (`mcp-connection.toml`) rides
/// the SAME vetted read — a visibility word plus this doc line, never a copy
/// of the logic (the memory-bank seam convention). The caller parses its own
/// keys out of the vetted text; the checks below are the only read path.
pub(crate) fn read_vetted_file(path: &Path) -> Result<Option<String>, CredentialFileRefusal> {
    read_vetted_file_with(path, running_uid())
}

/// [`read_vetted_file`] with the running uid injectable (the tests' seam).
fn read_vetted_file_with(
    path: &Path,
    running_uid: u32,
) -> Result<Option<String>, CredentialFileRefusal> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt as _, PermissionsExt};

    // `O_NONBLOCK` is what keeps this open from being a liveness hazard, and it is
    // needed precisely BECAUSE the checks now run on the handle rather than on the
    // path. Opening a FIFO for reading blocks until a writer appears, so a named
    // pipe left at a searched location — an overlay directory is operator-controlled
    // — would hang here forever, before `is_file()` ever got the chance to reject it
    // as a miss. That hang would reach the UI: `llm_credential_status` walks this
    // same chain and the banner calls it on paint. With the flag the open returns
    // immediately and the fstat below rejects the pipe as the ordinary miss it is.
    // On a regular file the flag is a no-op — regular files are always ready — so
    // the read below is unaffected.
    //
    // `O_CLOEXEC` keeps the descriptor out of any child process: a credential handle
    // has no business being inherited.
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        // A genuine absence is the ordinary miss the fall-through exists for.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // Every OTHER open failure (an inaccessible parent directory, an I/O error,
        // a symlink loop, ...) is NOT the same as "this location does not answer" —
        // silently falling through would hide the exposed/broken location from the
        // operator entirely. Refuse instead: the refusal carries the structured
        // reason, and the desktop resolver's wording (`desktop_message`) is what
        // this module has always said.
        Err(error) => {
            return Err(CredentialFileRefusal::Open {
                path: path.to_path_buf(),
                error,
            });
        }
    };

    // `fstat` on the handle we already hold — the owner and mode below describe the
    // exact inode `file` will be read from, never a second, separately-resolved path.
    let metadata = file
        .metadata()
        .map_err(|error| CredentialFileRefusal::Metadata {
            path: path.to_path_buf(),
            error,
        })?;
    if !metadata.is_file() {
        return Ok(None);
    }

    let owner = metadata.uid();
    if owner != running_uid {
        return Err(CredentialFileRefusal::Owner {
            path: path.to_path_buf(),
            owner,
            running_uid,
        });
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode & GROUP_AND_WORLD_BITS != 0 {
        return Err(CredentialFileRefusal::LooseMode {
            path: path.to_path_buf(),
            mode,
        });
    }

    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| CredentialFileRefusal::Read {
            path: path.to_path_buf(),
            error,
        })?;
    Ok(Some(text))
}

/// Validate one candidate credential file fail-closed, then read
/// `OLLAMA_API_KEY` out of it.
///
/// `Ok(None)` means "this location does not answer" — the file genuinely does not
/// exist (`File::open` fails with [`std::io::ErrorKind::NotFound`]), or it exists but
/// carries no `OLLAMA_API_KEY` line, or it is a directory rather than a file. All
/// three are ordinary misses that fall through to the next location.
fn read_credential_file(
    path: &Path,
    running_uid: u32,
) -> Result<Option<String>, CredentialFileRefusal> {
    let text = read_vetted_file_with(path, running_uid)?;
    Ok(text.and_then(|text| parse_dotenv(&text, OLLAMA_API_KEY_VAR)))
}

/// The error message for an exhausted search: it names every location actually
/// searched and says plainly that provisioning from inside the app is not yet
/// supported.
///
/// It deliberately does NOT mention `pulse setup-keys` — that verb does not exist
/// and is not being built, and pointing an operator at it is the orphaned pointer
/// `ADOPTION.md` §C2 records as a current-cut gap.
fn missing_credential_message(search: &CredentialSearch) -> String {
    let mut message = format!(
        "no {OLLAMA_API_KEY_VAR} credential found. Searched, in order:\n  \
         1. the process environment variable {OLLAMA_API_KEY_VAR}"
    );
    let mut step = 1;
    for (path, source) in search.file_locations() {
        step += 1;
        let label = match source {
            CredentialSource::Env => "the process environment",
            CredentialSource::ConfigDir => "${PULSE_CONFIG_DIR}",
            CredentialSource::CwdDotenv => "the working / manifest directory",
            CredentialSource::AppDataDir => "the application data directory",
            // `file_locations()` never emits a `Keychain` pair (the Keychain is not
            // one of its file locations), same as it never emits `Env` — this arm
            // exists only for match exhaustiveness, mirroring the `Env` arm above.
            CredentialSource::Keychain => "the macOS Keychain",
        };
        // `write!` into the String rather than `push_str(&format!(..))`: one
        // allocation instead of two, and what `clippy::format_push_string` asks for.
        // Writing to a String is infallible, so the Result is deliberately dropped.
        let _ = write!(message, "\n  {step}. {label}: {}", path.display());
    }
    if search.config_dir.is_none() {
        let _ = write!(
            message,
            // `{PULSE_CONFIG_DIR_ENV}` is an INLINE format capture, so this renders
            // the const's VALUE (`PULSE_CONFIG_DIR`), not its Rust name. The extra
            // brace pair matches the `${PULSE_CONFIG_DIR}` label the numbered list
            // above uses, so the message names the variable one way throughout.
            "\n  (${{{PULSE_CONFIG_DIR_ENV}}} is not set, so no overlay location was searched)"
        );
    }
    message.push_str(
        "\nSeeding a credential from inside the app is not yet supported. \
         Create a `.env` file containing `OLLAMA_API_KEY=<your key>` at one of the \
         locations above, owned by you and with mode 0600.",
    );
    message
}

/// Look up `var` in `.env` text (the first matching `KEY=VALUE` line; blank and
/// `#`-comment lines ignored). Surrounding quotes are trimmed.
///
/// Moved here from `src/cli/compose.rs` with the resolver rather than duplicated —
/// `src/cli/` keeps no resolution logic of its own.
fn parse_dotenv(text: &str, var: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == var {
            let value = value.trim().trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

/// Read a single Keychain secret, mapping every failure to a clear
/// [`LlmError::Config`].
///
/// Parameterized on `service` / `account` so a test can target a
/// guaranteed-absent name (the real `PulseTrader/glm_api_key` is seeded on the
/// owner's machine, so it can NOT stand in for the missing-entry path).
///
/// **Neither message names `pulse setup-keys`**, for the same reason
/// [`missing_credential_message`] does not: that verb does not exist and is not
/// being built (the orphaned pointer `ADOPTION.md` §C2 records as a current-cut
/// gap). Sending an operator to a command that answers "unrecognized" is worse
/// than telling them plainly that this path cannot be provisioned yet — and these
/// two functions are the only two credential-failure messages on this surface, so
/// them disagreeing is a contradiction a user would hit in one sitting.
fn read_secret(service: &str, account: &str) -> Result<String, LlmError> {
    let entry = Entry::new(service, account).map_err(|error| {
        LlmError::Config(format!(
            "could not open the macOS Keychain entry {service}/{account}: {error}. \
             Seeding a Keychain credential from inside the app is not yet supported."
        ))
    })?;
    entry.get_password().map_err(|error| {
        LlmError::Config(format!(
            "no GLM API key found in the macOS Keychain ({service}/{account}): {error}. \
             Seeding a Keychain credential from inside the app is not yet supported; \
             the composer reads a `.env` credential instead — see the resolver's own \
             message for the locations it searches."
        ))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{CredentialSearch, DOTENV_FILE, parse_dotenv, read_secret, resolve_llm_api_key_in};
    use crate::domain::{CredentialSource, LlmError};

    #[test]
    fn secrets_missing_entry_is_config_error() {
        // The REAL `PulseTrader/glm_api_key` entry is seeded (owner's live Z.AI
        // key), so this test targets a DISTINCT, guaranteed-absent account. A
        // missing item resolves to `NoEntry` immediately with NO Keychain prompt
        // (there is no ACL to authorize on a nonexistent item). The accessor must
        // surface a clear `Config` error, not panic — and must NOT point at
        // `pulse setup-keys`, a verb that does not exist (ADOPTION.md §C2).
        let err = read_secret(
            "PulseTrader",
            "glm_api_key_test_missing_9f3c1a7e_never_seed",
        )
        .expect_err("a guaranteed-absent keychain entry must return Err, not a value");
        assert!(
            matches!(&err, LlmError::Config(_)),
            "missing key must be a Config error, got {err:?}"
        );
        assert!(
            !err.to_string().contains("setup-keys"),
            "the message must not send the operator to a command that does not \
             exist, got: {err}"
        );
        assert!(
            err.to_string().contains("not yet supported"),
            "the message must say plainly that this path cannot be provisioned \
             yet, got: {err}"
        );
    }

    // ---- r1.s1.w2: the `.env` reader, moved here with the resolver -------------

    /// Moved verbatim from `src/cli/compose.rs` when `parse_dotenv` moved: the
    /// reader travels with the resolver rather than being duplicated.
    #[test]
    fn parse_dotenv_reads_key_ignoring_comments_and_quotes() {
        let env = "# a comment\n\nOLLAMA_API_KEY = \"abc123\"\nOTHER=nope\n";
        assert_eq!(
            parse_dotenv(env, "OLLAMA_API_KEY").as_deref(),
            Some("abc123")
        );
        assert_eq!(parse_dotenv(env, "OTHER").as_deref(), Some("nope"));
        assert!(parse_dotenv(env, "MISSING").is_none());
    }

    /// The out-of-crate suite cannot read a resolved key's VALUE (`expose()` is
    /// `pub(crate)` — the least-privilege control), so the value-correctness half of
    /// resolution is asserted HERE, in-crate, where `expose()` is reachable. Without
    /// this, "the right source answered" would be proven and "the right bytes came
    /// back" would not.
    #[test]
    fn resolver_returns_the_value_the_winning_location_held() {
        let key = resolve_llm_api_key_in(
            &CredentialSearch::empty().with_env_key(Some("env-value".to_owned())),
        )
        .expect("env key resolves");
        assert_eq!(key.expose(), "env-value");
        assert_eq!(key.source(), CredentialSource::Env);
    }

    /// An empty `OLLAMA_API_KEY` is treated as UNSET, not as an empty credential —
    /// otherwise an exported-but-blank shell variable would silently shadow a
    /// perfectly good `.env` file and fail at the transport instead of at resolution.
    #[test]
    fn the_production_search_never_names_a_build_machine_path() {
        // `CARGO_MANIFEST_DIR` is baked in at COMPILE time, so in a shipped `.app`
        // it names a directory on the machine that built it.
        // `missing_credential_message` lists every searched location verbatim and
        // that text reaches the Designer, so naming it would send an end user
        // hunting for a path they can neither find nor create.
        //
        // The assertion is deliberately about THAT path and not "every location
        // exists": the application-data directory legitimately does not exist yet on
        // a fresh install, and it MUST still be named — it is precisely where the
        // user is meant to create the file.
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let search = CredentialSearch::from_process_env();
        let named: Vec<_> = search
            .file_locations()
            .into_iter()
            .map(|(path, _)| path)
            .collect();

        let names_manifest_dir = named.iter().any(|path| path.parent() == Some(manifest_dir));
        assert_eq!(
            names_manifest_dir,
            manifest_dir.is_dir(),
            "the manifest directory must be searched when it exists (a developer \
             machine) and absent when it does not (a shipped app), but the search \
             {} it while it {}",
            if names_manifest_dir { "names" } else { "omits" },
            if manifest_dir.is_dir() {
                "exists"
            } else {
                "does not exist"
            },
        );

        // The app-data location is still named regardless — it is the one place a
        // user with no credential is actually able to create the file.
        assert!(
            !named.is_empty(),
            "the search must name at least one file location to point the user at"
        );
    }

    #[test]
    fn an_empty_env_var_is_not_a_credential() {
        let search = CredentialSearch::empty().with_env_key(Some(String::new()));
        assert!(resolve_llm_api_key_in(&search).is_err());
    }

    // ---- Codex P2 (`read_credential_file` TOCTOU): open-once/validate-handle/ ----
    // ---- read-handle must not break the ordinary, happy-path read ---------------

    /// `tests/credential_source.rs` cannot assert on a resolved key's VALUE
    /// (`expose()` is `pub(crate)`), so the guard that the open-once restructuring
    /// still reads the right BYTES off a normal, owned, `0600` file lives here,
    /// in-crate, alongside [`resolver_returns_the_value_the_winning_location_held`]
    /// (which covers the env-sourced case). Together they cover both branches of
    /// [`resolve_llm_api_key_in`]: a value handed straight through, and a value read
    /// off disk via [`read_credential_file`].
    #[test]
    fn a_normal_owned_0600_file_still_resolves_its_value() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(DOTENV_FILE);
        std::fs::write(
            &path,
            "OLLAMA_API_KEY=sk-HANDLEFIX1234abcd5678efgh9012ijkl\n",
        )
        .expect("write .env");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600 the .env");

        let search = CredentialSearch::empty().with_config_dir(Some(dir.path().to_path_buf()));
        let key = resolve_llm_api_key_in(&search)
            .expect("a normal owned 0600 file must still resolve after the restructuring");
        assert_eq!(key.source(), CredentialSource::ConfigDir);
        assert_eq!(key.expose(), "sk-HANDLEFIX1234abcd5678efgh9012ijkl");
    }

    // ---- r3.s3.w3: the profile, and the two refusal renderings -----------------

    /// Operator ruling (r3.s3.w3): the DESKTOP refusal text is byte-for-byte
    /// what this module always returned — only `pulse serve`'s startup line
    /// got its own wording. This pins the desktop wording so a future edit
    /// cannot silently drift it.
    #[test]
    fn the_desktop_mode_refusal_message_keeps_its_original_wording() {
        let refusal = super::CredentialFileRefusal::LooseMode {
            path: std::path::PathBuf::from("/tmp/example/.env"),
            mode: 0o644,
        };
        let message = refusal.desktop_message();
        assert!(
            message.contains("grants access to group or others"),
            "the desktop wording is unchanged; got: {message}"
        );
        assert!(
            message.contains("chmod 0600"),
            "the fix clause is unchanged; got: {message}"
        );
        assert!(
            !message.contains("group/world"),
            "the serve-only wording stays out of the desktop message; got: {message}"
        );
    }

    /// `pulse serve`'s Step-3 refusal body (operator ruling, r3.s3.w3): the
    /// exact wording AC-1(a) asserts on, naming the file and "group/world",
    /// never a value.
    #[test]
    fn the_serve_refusal_body_names_group_world_and_the_mode() {
        let refusal = super::CredentialFileRefusal::LooseMode {
            path: std::path::PathBuf::from("/tmp/example/.env"),
            mode: 0o644,
        };
        assert_eq!(
            refusal.to_string(),
            "/tmp/example/.env is group/world-readable (mode 0644); chmod 600 it"
        );
    }

    /// Under the server profile, the production search built from the process
    /// environment carries NO cwd or manifest dotenv slot at all — the file
    /// locations are the two deliberate ones only, and the exhausted-search
    /// message therefore never advertises a location the server would not
    /// read.
    ///
    /// (Process-global, like the suite's other profile cases: under the repo's
    /// nextest runner this test owns its own process.)
    #[test]
    fn the_server_profile_process_search_skips_cwd_and_manifest() {
        super::set_credential_profile(super::CredentialProfile::Server);
        let search = super::CredentialSearch::from_process_env();
        let cwd = std::env::current_dir().expect("cwd");
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for (path, _) in search.file_locations() {
            assert_ne!(
                path.parent(),
                Some(cwd.as_path()),
                "no cwd dotenv slot under Server: {path:?}"
            );
            assert_ne!(
                path.parent(),
                Some(manifest),
                "no manifest dotenv slot under Server: {path:?}"
            );
        }
    }
}
