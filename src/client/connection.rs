//! The connection files (r3.s3.w5, the `client-token-storage` fake; reshaped by
//! the dispatch-2 correction, and by r4.s3.w2).
//!
//! **TWO files**, and since r4.s3.w2 they have DIFFERENT shapes:
//!
//! - `<data dir>/server-connection.toml` — THE APP'S OWN connection: written by
//!   a successful `server_connect`, deleted by `server_disconnect`, loaded at
//!   start-up. Holds `{ url }` — the token lives in the Keychain, behind the
//!   token-store port. An older `{ url, token }` file is still READ (the G3
//!   migration's input); [`AppConnectionFile`] is `Deserialize`-only, so a
//!   token cannot be written back into it.
//! - `<data dir>/mcp-connection.toml` — BESIDE it: `pulse mcp login`'s artifact,
//!   read by the bare-`pulse mcp` relay. Holds `{ url, token }` (G4; #384
//!   tracks moving the agent token later).
//!
//! Shared rules (both files): written only after a passing handshake, through a
//! temporary file plus rename, with mode `0600` set BEFORE any token byte is
//! written; the temp is synced and the parent directory synced after the rename
//! (#252); read through `adapters::secrets`' vetted-file discipline — the SAME
//! `O_NONBLOCK`/`O_CLOEXEC`/fstat/owner/mode checks, reused, never a weaker
//! reimplementation. A group/world-readable file is refused on load with the
//! named reason. **Never** logged; the fake's replacement trigger stays in the
//! spine's `Fakes` ledger. `$PULSE_CONFIG_DIR` overrides the directory **for
//! tests only** — the same env var the credential resolver honours.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::adapters::secrets;

/// The app's own connection file.
const APP_FILE: &str = "server-connection.toml";
/// The relay's connection file (`pulse mcp login`'s output).
const MCP_FILE: &str = "mcp-connection.toml";

/// One stored connection: where the server is and the token that works.
///
/// Since r4.s3.w2 this is the RELAY's shape only — `mcp-connection.toml`, whose
/// reader and writer keep their meaning (G4). The app's own file is
/// [`AppConnectionFile`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionFile {
    /// The server's base URL.
    pub url: String,
    /// The bearer token — stored so the relay reconnects without retyping.
    /// Never logged, never in an error.
    pub token: String,
}

/// The app's own connection file, as READ (`server-connection.toml`).
///
/// `Deserialize` ONLY, deliberately: a token cannot be serialised into the app
/// file by construction — [`store_app`] takes the URL alone and writes
/// [`UrlOnlyBody`].
///
/// `token` exists for exactly one reason: an older `{ url, token }` file (the
/// `client-token-storage` era) must still be readable, because the one-time
/// verified migration (G3) reads the file token out of it. Every file this app
/// writes has no token key at all.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AppConnectionFile {
    /// The server's base URL.
    pub url: String,
    /// An older file's bearer token — the migration's input. Never logged,
    /// never in an error.
    #[serde(default)]
    pub token: Option<String>,
}

/// The URL-only body the app file is WRITTEN as. Private on purpose: it is the
/// only thing [`store_app`] can serialize, and it has no token field.
#[derive(Serialize)]
struct UrlOnlyBody<'a> {
    url: &'a str,
}

/// The directory both files live in: `$PULSE_CONFIG_DIR` (tests only) or the
/// platform data dir the database itself sits in.
///
/// # Errors
///
/// Names the failure when no directory can be resolved or the override is
/// unusable.
fn dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("PULSE_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        if dir.as_os_str().is_empty() {
            return Err("PULSE_CONFIG_DIR is set but empty".to_owned());
        }
        return Ok(dir);
    }
    let data_dir = crate::adapters::db::paths::default_data_dir()
        .map_err(|error| format!("no platform data directory: {error}"))?;
    Ok(data_dir)
}

/// # Errors
///
/// Directory resolution failed.
pub(crate) fn app_path() -> Result<PathBuf, String> {
    dir().map(|dir| dir.join(APP_FILE))
}

/// # Errors
///
/// Directory resolution failed.
pub(crate) fn mcp_path() -> Result<PathBuf, String> {
    dir().map(|dir| dir.join(MCP_FILE))
}

/// Parse a vetted relay-file body: both the URL and the token are required.
fn parse(text: &str, display: &Path) -> Result<ConnectionFile, String> {
    let parsed: ConnectionFile = toml::from_str(text)
        .map_err(|error| format!("{} is unreadable: {error}", display.display()))?;
    if parsed.url.is_empty() || parsed.token.is_empty() {
        return Err(format!("{} is missing url or token", display.display()));
    }
    Ok(parsed)
}

/// Parse a vetted app-file body: the URL is required, the token is an OLDER
/// file's (the migration's input) — and an empty one counts as absent.
fn parse_app(text: &str, display: &Path) -> Result<AppConnectionFile, String> {
    let parsed: AppConnectionFile = toml::from_str(text)
        .map_err(|error| format!("{} is unreadable: {error}", display.display()))?;
    if parsed.url.is_empty() {
        return Err(format!("{} is missing url", display.display()));
    }
    Ok(AppConnectionFile {
        url: parsed.url,
        token: parsed.token.filter(|token| !token.is_empty()),
    })
}

/// The vetted body of the file at `path`, or `None` when nothing answers there.
///
/// # Errors
///
/// A file that exists but fails the owner/mode/regular-file checks is a
/// refusal carrying the named reason — the caller records it as the
/// connection's refused state rather than silently ignoring a file it cannot
/// trust.
fn vetted_text(path: &Path) -> Result<Option<String>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let Some(text) = secrets::read_vetted_file(path).map_err(|refusal| refusal.to_string())? else {
        // The vetted read says "not a regular file / vanished mid-check":
        // treated as absent — nothing to connect with.
        return Ok(None);
    };
    Ok(Some(text))
}

/// Load the app's file at `path`, if one exists and survives the vetting.
///
/// # Errors
///
/// The vetting refusal, named.
fn load_app_at(path: &Path) -> Result<Option<AppConnectionFile>, String> {
    let Some(text) = vetted_text(path)? else {
        return Ok(None);
    };
    parse_app(&text, path).map(Some)
}

/// Load the relay's file at `path`, if one exists and survives the vetting.
///
/// # Errors
///
/// The vetting refusal, named.
fn load_mcp_at(path: &Path) -> Result<Option<ConnectionFile>, String> {
    let Some(text) = vetted_text(path)? else {
        return Ok(None);
    };
    parse(&text, path).map(Some)
}

/// [`load_app_at`] on the app's `server-connection.toml`.
///
/// # Errors
///
/// The vetting refusal, named.
pub fn load_app() -> Result<Option<AppConnectionFile>, String> {
    load_app_at(&app_path()?)
}

/// [`load_mcp_at`] on the relay's `mcp-connection.toml`.
///
/// # Errors
///
/// The vetting refusal, named.
pub fn load_mcp() -> Result<Option<ConnectionFile>, String> {
    load_mcp_at(&mcp_path()?)
}

/// A write-path failure before the rename: the temporary is removed (best
/// effort — a leftover temp is a file some later reader could meet, and for the
/// relay's file it can hold secret bytes) and the reason names the temporary.
fn temp_failure(tmp: &Path, action: &str, error: &std::io::Error) -> String {
    let _ = std::fs::remove_file(tmp);
    format!("cannot {action} {}: {error}", tmp.display())
}

/// Store `body` at `path` the spec's way: a temporary file is opened with mode
/// `0600` set at creation — BEFORE any byte is written — the body goes to the
/// temporary, the temporary is SYNCED, and a rename makes the file atomic for
/// every reader; the parent directory is synced afterwards, so the rename
/// itself survives a power loss (#252).
///
/// The temporary is deliberately NOT reused. `mode(0o600)` applies only when
/// the file is CREATED, so a temp left behind by a crashed write — or planted
/// by another local user — would keep its own mode while the fresh bearer token
/// is written into it and renamed over the good file. Whatever is at the temp
/// path is removed first, and the replacement is created with `create_new`,
/// which refuses a symlink (or anything else that reappeared between the
/// removal and the open: `O_CREAT|O_EXCL` never follows one) — so the bytes can
/// only land in a file this call created, and the explicit `set_permissions`
/// below pins the mode even if the process umask would have masked it at
/// creation.
fn store_at<T: Serialize>(path: &Path, body: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let body = toml::to_string_pretty(body)
        .map_err(|error| format!("cannot serialize the connection: {error}"))?;
    let tmp = path.with_extension("toml.tmp");
    // A symlink at the temp path is refused outright, never removed and never
    // written through: nothing legitimate creates one there, and following it
    // would put the bearer token in whatever file some other local user chose.
    if let Ok(metadata) = std::fs::symlink_metadata(&tmp)
        && metadata.file_type().is_symlink()
    {
        return Err(format!(
            "refusing to write {}: it is a symlink, not a temporary file",
            tmp.display()
        ));
    }
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot clear {}: {error}", tmp.display())),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|error| temp_failure(&tmp, "secure", &error))?;
    file.write_all(body.as_bytes())
        .map_err(|error| temp_failure(&tmp, "write", &error))?;
    // #252: the bytes must be on disk BEFORE the rename publishes them —
    // otherwise a power loss can leave the file zero-length or absent.
    file.sync_all()
        .map_err(|error| temp_failure(&tmp, "sync", &error))?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot put {} in place: {error}", path.display())
    })?;
    // #252: and the rename itself must be durable — the entry lives in the
    // parent directory, so that is what has to reach disk.
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("cannot sync {}: {error}", parent.display()))?;
    }
    Ok(())
}

/// [`store_at`] on the app's `server-connection.toml`, written as `{ url }`.
///
/// Takes the URL ALONE on purpose: the app file has no token field to write
/// (G2), and this is the only writer of it.
///
/// # Errors
///
/// Directory creation or the write/rename failed; the reason names the path.
pub fn store_app(url: &str) -> Result<(), String> {
    store_at(&app_path()?, &UrlOnlyBody { url })
}

/// [`store_at`] on the relay's `mcp-connection.toml` — still `{ url, token }`
/// (G4), now with the same durable rename (#252).
///
/// # Errors
///
/// Directory creation or the write/rename failed; the reason names the path.
pub fn store_mcp(connection: &ConnectionFile) -> Result<(), String> {
    store_at(&mcp_path()?, connection)
}

/// Delete the file at `path`. A missing file is already the goal.
///
/// # Errors
///
/// Any removal failure other than absence.
fn remove_at(path: &std::path::Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot remove {}: {error}", path.display())),
    }
}

/// [`remove_at`] on the app's `server-connection.toml`.
///
/// # Errors
///
/// Any removal failure other than absence.
pub(crate) fn remove_app() -> Result<(), String> {
    remove_at(&app_path()?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    // `PermissionsExt` comes in through `super::*` (the store path sets the mode).
    use super::*;

    fn connection() -> ConnectionFile {
        ConnectionFile {
            url: "http://100.64.0.1:7433".to_owned(),
            token: "secret-token".to_owned(),
        }
    }

    /// The stored file is 0600, and the temp is gone.
    #[test]
    fn a_stored_connection_is_owner_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(APP_FILE);
        store_at(&path, &connection()).expect("store");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the connection file is owner-only");
        assert!(
            !dir.path().join("server-connection.toml.tmp").exists(),
            "the temporary is renamed away"
        );
    }

    /// A temp left behind by a crashed write (here: group/world-readable) must
    /// not keep its mode while the fresh token is written into it.
    #[test]
    fn a_pre_existing_temp_never_keeps_its_own_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(APP_FILE);
        let tmp = dir.path().join("server-connection.toml.tmp");
        std::fs::write(&tmp, b"a crashed write left this here").expect("plant the temp");
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).expect("chmod");

        store_at(&path, &connection()).expect("store");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the token bytes went into an owner-only file");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.contains("secret-token"),
            "the connection landed: {text}"
        );
        assert!(
            !text.contains("crashed write"),
            "the old temp's bytes are gone, not renamed over the target"
        );
    }

    /// A symlinked temp is refused outright: the token must never be written
    /// through a link some other local user placed.
    #[test]
    fn a_symlinked_temp_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(APP_FILE);
        let elsewhere = dir.path().join("elsewhere");
        std::fs::write(&elsewhere, b"not mine").expect("target of the link");
        let tmp = dir.path().join("server-connection.toml.tmp");
        std::os::unix::fs::symlink(&elsewhere, &tmp).expect("plant the link");

        let error = store_at(&path, &connection()).expect_err("a symlink must be refused");
        assert!(error.contains("symlink"), "the reason names it: {error}");
        assert_eq!(
            std::fs::read_to_string(&elsewhere).expect("read the link target"),
            "not mine",
            "the link target was never written through"
        );
        assert!(!path.exists(), "nothing was stored");
    }
}
