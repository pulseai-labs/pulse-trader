//! The data-dir role marker (r4.s2.w3, C5 / ADR-0029).
//!
//! `pulse serve --role <prod|qa>` pins a data dir to a role in
//! `<data dir>/server-role` — one line, `prod` or `qa`, mode 0600. With no
//! `--role` nothing here runs and the server behaves exactly as it did before
//! the flag existed (dev and test servers keep working).
//!
//! **The refusal is the point.** QA's database is a copy of prod's, and QA or
//! prod serving the other's data would be the silent, written-to-the-wrong-
//! library failure the marker exists to prevent:
//!
//! - absent marker → write the given role, continue;
//! - same role → continue;
//! - different role → REFUSE, naming both roles and the data dir.
//!
//! The check runs in `pulse serve`'s composition root BEFORE the database is
//! opened (r4.s2.w3's plan-gate amendment), so a refused start touches nothing
//! at all: no database file created or migrated, no instance lock, no
//! start-log entry, no marker change. That order is the whole safety property
//! — a QA server started by mistake with prod's `--db` must never open prod's
//! database before it refuses.
//!
//! **With `--role`, the database must sit inside the data dir.** The marker
//! describes the dir the data lives in; a database outside it would make the
//! marker meaningless, so the containment check refuses by name first, and it
//! is pure (no writes) — a start refused for its paths leaves no marker behind.
//!
//! A marker this build cannot read (empty, torn, hand-edited) is a REFUSAL,
//! never an overwrite: the one thing a safety marker must not do is let a
//! different role take over a dir whose marker got corrupted.

use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The marker's file name, under the data dir.
pub const ROLE_MARKER_NAME: &str = "server-role";

/// The role a `pulse serve` run declares and a data dir is marked with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerRole {
    /// Production: the Mac Mini's server.
    Prod,
    /// QA: draco-desk's server.
    Qa,
}

impl ServerRole {
    /// The wire and marker spelling (`prod` / `qa`) — the same string in the
    /// marker, in `--role`, and in the handshake's additive `role` field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prod => "prod",
            Self::Qa => "qa",
        }
    }

    /// Parse the CLI/marker spelling.
    ///
    /// # Errors
    ///
    /// [`RoleRefused::Unknown`] for anything but `prod` or `qa`.
    pub fn parse(raw: &str) -> Result<Self, RoleRefused> {
        match raw {
            "prod" => Ok(Self::Prod),
            "qa" => Ok(Self::Qa),
            other => Err(RoleRefused::Unknown {
                raw: other.to_owned(),
            }),
        }
    }
}

impl std::fmt::Display for ServerRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a role-marked start was refused. Every variant names what it read, so
/// the operator's terminal carries the whole story.
#[derive(Debug, thiserror::Error)]
pub enum RoleRefused {
    /// The value given to `--role` is not a role.
    #[error("expected prod or qa, got {raw:?}")]
    Unknown {
        /// The rejected value.
        raw: String,
    },
    /// The database does not sit inside the data dir (the marker lives in the
    /// data dir, so the data it describes must live there too).
    #[error(
        "the database {} is outside the data dir {} (the role marker describes the data dir's data)",
        db.display(),
        data_dir.display()
    )]
    DbOutsideDataDir {
        /// The database path as given.
        db: PathBuf,
        /// The data dir as given.
        data_dir: PathBuf,
    },
    /// The dir carries the OTHER role.
    #[error(
        "this data dir is marked {marked}, --role {asked} (data dir {data_dir})",
        data_dir = data_dir.display()
    )]
    Mismatch {
        /// The role the marker holds.
        marked: ServerRole,
        /// The role this start asked for.
        asked: ServerRole,
        /// The data dir, as given.
        data_dir: PathBuf,
    },
    /// A path could not be resolved for the containment check.
    #[error("cannot resolve {}: {source}", path.display())]
    PathUnresolved {
        /// The path that did not resolve.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
    /// The marker could not be read.
    #[error("cannot read the role marker {}: {source}", path.display())]
    MarkerRead {
        /// The marker's path.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
    /// The marker holds something that is not a role.
    #[error("the role marker {} holds {content:?}, not prod or qa", path.display())]
    MarkerUnreadable {
        /// The marker's path.
        path: PathBuf,
        /// What the file held (trimmed).
        content: String,
    },
    /// The marker could not be written.
    #[error("cannot write the role marker {}: {source}", path.display())]
    MarkerWrite {
        /// The marker's path.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
}

/// The marker's role under `data_dir`; `Ok(None)` when the marker does not
/// exist (an unmarked dir — the first `--role` start marks it).
///
/// # Errors
///
/// [`RoleRefused::MarkerRead`] when the file exists but cannot be read, and
/// [`RoleRefused::MarkerUnreadable`] when it holds something that is not a
/// role (fail closed — never overwrite a marker this build cannot read).
pub fn read(data_dir: &Path) -> Result<Option<ServerRole>, RoleRefused> {
    let path = data_dir.join(ROLE_MARKER_NAME);
    match fs::read_to_string(&path) {
        Ok(content) => {
            let trimmed = content.trim();
            ServerRole::parse(trimmed)
                .map(Some)
                .map_err(|_| RoleRefused::MarkerUnreadable {
                    path,
                    content: trimmed.to_owned(),
                })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RoleRefused::MarkerRead { path, source }),
    }
}

/// Write the marker: one line, mode 0600 (the `serve-start-limit` discipline —
/// nothing else on the host reads the role, but the file sets the server's
/// safety boundary, so it is as private as the data it describes).
///
/// # Errors
///
/// [`RoleRefused::MarkerWrite`] when the file cannot be created or written.
pub fn write(data_dir: &Path, role: ServerRole) -> Result<(), RoleRefused> {
    let path = data_dir.join(ROLE_MARKER_NAME);
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|source| RoleRefused::MarkerWrite {
            path: path.clone(),
            source,
        })?;
    writeln!(file, "{}", role.as_str()).map_err(|source| RoleRefused::MarkerWrite {
        path: path.clone(),
        source,
    })?;
    Ok(())
}

/// Apply the marker for a `--role {role}` start: refuse a database outside the
/// data dir, refuse a dir marked with the other role, write the marker when it
/// is absent, and continue on the same role.
///
/// # Errors
///
/// [`RoleRefused`] — and NOTHING is written on any refusal.
pub fn check(data_dir: &Path, db: &Path, role: ServerRole) -> Result<(), RoleRefused> {
    let db_resolved = resolve(db)?;
    let dir_resolved = resolve(data_dir)?;
    if !db_resolved.starts_with(&dir_resolved) {
        return Err(RoleRefused::DbOutsideDataDir {
            db: db.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
        });
    }
    match read(data_dir)? {
        None => write(data_dir, role),
        Some(marked) if marked == role => Ok(()),
        Some(marked) => Err(RoleRefused::Mismatch {
            marked,
            asked: role,
            data_dir: data_dir.to_path_buf(),
        }),
    }
}

/// The absolute, symlink-resolved form of `path`, resolved through its deepest
/// EXISTING ancestor so a path whose final components do not exist yet (the
/// database a `--role` start is about to create) resolves consistently with one
/// that does (the data dir). The unresolved tail is appended verbatim.
///
/// # Errors
///
/// [`RoleRefused::PathUnresolved`] on any IO error while walking up — the
/// caller turns that into a named refusal, never a silent pass.
fn resolve(path: &Path) -> Result<PathBuf, RoleRefused> {
    let unresolved = |source| RoleRefused::PathUnresolved {
        path: path.to_path_buf(),
        source,
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(unresolved)?.join(path)
    };
    let mut cursor = absolute.as_path();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        match fs::canonicalize(cursor) {
            Ok(mut resolved) => {
                for name in tail.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name() else {
                    return Err(unresolved(error));
                };
                tail.push(name.to_os_string());
                match cursor.parent() {
                    Some(parent) => cursor = parent,
                    None => return Err(unresolved(error)),
                }
            }
            Err(error) => return Err(unresolved(error)),
        }
    }
}
