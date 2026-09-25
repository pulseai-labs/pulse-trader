//! The connection files (r3.s3.w5, the `client-token-storage` fake; reshaped
//! by the dispatch-2 correction).
//!
//! **TWO files**, per spec lines 126 and 199 and reading-3 of the approved
//! ruling:
//!
//! - `<data dir>/server-connection.toml` — THE APP'S OWN connection: written
//!   by a successful `server_connect`, deleted by `server_disconnect`, loaded
//!   at start-up. Holds `{ url, token }`.
//! - `<data dir>/mcp-connection.toml` — BESIDE it: `pulse mcp login`'s
//!   artifact, read by the bare-`pulse mcp` relay. Same shape, same rules.
//!
//! Shared rules (both files): written only after a passing handshake, through
//! a temporary file plus rename, with mode `0600` set BEFORE the token is
//! written; read through `adapters::secrets`' vetted-file discipline — the
//! SAME `O_NONBLOCK`/`O_CLOEXEC`/fstat/owner/mode checks, reused, never a
//! weaker reimplementation. A group/world-readable file is refused on load
//! with the named reason. **Never** the Keychain, and never logged; the
//! fake's replacement trigger stays in the spine's `Fakes` ledger (r3.s4
//! revisits storage). `$PULSE_CONFIG_DIR` overrides the directory **for tests
//! only** — the same env var the credential resolver honours.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::adapters::secrets;

/// The app's own connection file.
const APP_FILE: &str = "server-connection.toml";
/// The relay's connection file (`pulse mcp login`'s output).
const MCP_FILE: &str = "mcp-connection.toml";

/// One stored connection: where the server is and the token that works.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ConnectionFile {
    /// The server's base URL.
    pub url: String,
    /// The bearer token — stored so the app (and the relay, in its own file)
    /// reconnects without retyping. Never logged, never in an error.
    pub token: String,
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

/// Parse a vetted file body into a connection. Both files carry the same
/// `{ url, token }` shape.
fn parse(text: &str, display: &std::path::Path) -> Result<ConnectionFile, String> {
    let parsed: ConnectionFile = toml::from_str(text)
        .map_err(|error| format!("{} is unreadable: {error}", display.display()))?;
    if parsed.url.is_empty() || parsed.token.is_empty() {
        return Err(format!("{} is missing url or token", display.display()));
    }
    Ok(parsed)
}

/// Load the file at `path`, if one exists and survives the vetting.
///
/// # Errors
///
/// A file that exists but fails the owner/mode/regular-file checks is a
/// refusal carrying the named reason — the caller records it as the
/// connection's refused state rather than silently ignoring a file it cannot
/// trust.
fn load_at(path: &std::path::Path) -> Result<Option<ConnectionFile>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let text = secrets::read_vetted_file(path).map_err(|refusal| refusal.to_string())?;
    let Some(text) = text else {
        // The vetted read says "not a regular file / vanished mid-check":
        // treated as absent — nothing to connect with.
        return Ok(None);
    };
    parse(&text, path).map(Some)
}

/// [`load_at`] on the app's `server-connection.toml`.
///
/// # Errors
///
/// The vetting refusal, named.
pub(crate) fn load_app() -> Result<Option<ConnectionFile>, String> {
    load_at(&app_path()?)
}

/// [`load_at`] on the relay's `mcp-connection.toml`.
///
/// # Errors
///
/// The vetting refusal, named.
pub(crate) fn load_mcp() -> Result<Option<ConnectionFile>, String> {
    load_at(&mcp_path()?)
}

/// Store the file at `path` the spec's way: a temporary file is opened with
/// mode `0600` set at creation — BEFORE any token byte is written — the body
/// goes to the temporary, and a rename makes the file atomic for every reader.
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
fn store_at(path: &std::path::Path, connection: &ConnectionFile) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let body = toml::to_string_pretty(connection)
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
        .map_err(|error| format!("cannot secure {}: {error}", tmp.display()))?;
    file.write_all(body.as_bytes())
        .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp);
        format!("cannot put {} in place: {error}", path.display())
    })?;
    Ok(())
}

/// [`store_at`] on the app's `server-connection.toml`.
///
/// # Errors
///
/// Directory creation or the write/rename failed; the reason names the path.
pub(crate) fn store_app(connection: &ConnectionFile) -> Result<(), String> {
    store_at(&app_path()?, connection)
}

/// [`store_at`] on the relay's `mcp-connection.toml`.
///
/// # Errors
///
/// Directory creation or the write/rename failed; the reason names the path.
pub(crate) fn store_mcp(connection: &ConnectionFile) -> Result<(), String> {
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
