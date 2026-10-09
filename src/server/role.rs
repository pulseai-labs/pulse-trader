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
//! [`ensure_db_inside_data_dir`] is the one copy of that rule; `pulse qa-seed`
//! reuses it (r4.s2 close-review F3).
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
/// Creates the data dir when it is not there yet (PR-354 fix C2a): the first
/// `--role` start on a fresh data dir must not fail with ENOENT before the
/// database's own open would have made the directory — launchd would relaunch
/// a server that never starts.
///
/// # Errors
///
/// [`RoleRefused::MarkerWrite`] when the file cannot be created or written.
pub fn write(data_dir: &Path, role: ServerRole) -> Result<(), RoleRefused> {
    write_marker(data_dir, role, false)
}

/// Create the marker EXCLUSIVELY for an absent-marker start (PR-354 fix C2b):
/// two overlapping starts both read no marker, and with a plain create both
/// would write — the loser would overwrite the winner's role. `create_new`
/// (`O_EXCL`) makes the second create fail with `AlreadyExists`, which
/// [`check`] turns into "re-read and validate the winning role".
///
/// # Errors
///
/// [`RoleRefused::MarkerWrite`] — with an `AlreadyExists` source when the
/// marker appeared between the read and this call.
fn create_exclusive(data_dir: &Path, role: ServerRole) -> Result<(), RoleRefused> {
    write_marker(data_dir, role, true)
}

/// The one marker write: one line, mode 0600, the data dir created first.
/// `exclusive` picks `create_new` (the absent-marker race) over
/// create-or-replace (the callers that already validated the marker —
/// `qa-seed`'s re-run).
fn write_marker(data_dir: &Path, role: ServerRole, exclusive: bool) -> Result<(), RoleRefused> {
    let path = data_dir.join(ROLE_MARKER_NAME);
    fs::create_dir_all(data_dir).map_err(|source| RoleRefused::MarkerWrite {
        path: path.clone(),
        source,
    })?;
    let mut options = fs::OpenOptions::new();
    options.write(true);
    if exclusive {
        options.create_new(true);
    } else {
        options.create(true).truncate(true);
    }
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

/// The containment rule: the database must sit inside the data dir, because the
/// marker describes the data dir's data. Pure — a refusal writes nothing.
///
/// # Errors
///
/// [`RoleRefused::DbOutsideDataDir`] when `db` is not inside `data_dir`, and
/// [`RoleRefused::PathUnresolved`] when either path cannot be resolved.
pub(crate) fn ensure_db_inside_data_dir(data_dir: &Path, db: &Path) -> Result<(), RoleRefused> {
    let db_resolved = resolve(db)?;
    let dir_resolved = resolve(data_dir)?;
    if !db_resolved.starts_with(&dir_resolved) {
        return Err(RoleRefused::DbOutsideDataDir {
            db: db.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
        });
    }
    Ok(())
}

/// Apply the marker for a `--role {role}` start: refuse a database outside the
/// data dir, refuse a dir marked with the other role, create the marker when it
/// is absent, and continue on the same role.
///
/// The absent-marker path creates EXCLUSIVELY (PR-354 fix C2b): when two starts
/// overlap, the loser's create fails `AlreadyExists` and it re-reads the
/// winner's marker, validating it exactly as an existing marker is validated —
/// never overwriting it.
///
/// # Errors
///
/// [`RoleRefused`] — and NOTHING is written on any refusal.
pub fn check(data_dir: &Path, db: &Path, role: ServerRole) -> Result<(), RoleRefused> {
    ensure_db_inside_data_dir(data_dir, db)?;
    if let Some(marked) = read(data_dir)? {
        return same_role(marked, role, data_dir);
    }
    match create_exclusive(data_dir, role) {
        Ok(()) => Ok(()),
        Err(RoleRefused::MarkerWrite { path, source })
            if source.kind() == std::io::ErrorKind::AlreadyExists =>
        {
            lost_race(data_dir, role, path, source)
        }
        Err(error) => Err(error),
    }
}

/// The loser of the absent-marker race (PR-354 fix C2b): another start created
/// the marker between this one's read and its exclusive create. Re-read the
/// winner's marker and validate it exactly as an existing marker is validated —
/// the same role continues, the other refuses by name. A marker that vanished
/// between the two reads is refused by name, never overwritten.
fn lost_race(
    data_dir: &Path,
    role: ServerRole,
    path: PathBuf,
    source: std::io::Error,
) -> Result<(), RoleRefused> {
    match read(data_dir)? {
        Some(marked) => same_role(marked, role, data_dir),
        None => Err(RoleRefused::MarkerWrite { path, source }),
    }
}

/// The existing-marker rule, in one place: the same role continues, the other
/// refuses, naming both roles and the dir.
fn same_role(marked: ServerRole, role: ServerRole, data_dir: &Path) -> Result<(), RoleRefused> {
    if marked == role {
        return Ok(());
    }
    Err(RoleRefused::Mismatch {
        marked,
        asked: role,
        data_dir: data_dir.to_path_buf(),
    })
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{ROLE_MARKER_NAME, RoleRefused, ServerRole, check, create_exclusive, lost_race};
    use std::fs;
    use tempfile::TempDir;

    /// PR-354 fix C2a: the marker write creates the data dir when it is not
    /// there yet, so the first `--role` start on a fresh dir cannot fail with
    /// ENOENT before the database's own open would have made the directory.
    #[test]
    fn the_marker_write_creates_a_data_dir_that_does_not_exist_yet() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("fresh").join("nested");
        let db = data_dir.join("pulse.db");

        check(&data_dir, &db, ServerRole::Prod).expect("the first start marks a fresh dir");

        assert_eq!(
            fs::read_to_string(data_dir.join(ROLE_MARKER_NAME)).unwrap(),
            "prod\n"
        );
    }

    /// PR-354 fix C2b: the absent-marker path creates EXCLUSIVELY. The loser of
    /// a start race — its read saw no marker, the winner's create landed first
    /// — must re-read and validate the winner's role exactly as an existing
    /// marker is validated, never overwrite it. (Two overlapping starts cannot
    /// be interleaved deterministically in-process, so this drives the loser's
    /// own branch: the create fails `AlreadyExists` and the re-read decides.)
    #[test]
    fn a_lost_marker_race_re_reads_the_winner_instead_of_overwriting_it() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join(ROLE_MARKER_NAME);

        // The winner marked the dir `prod`; the loser asked for `qa`.
        fs::write(&marker, "prod\n").unwrap();
        let (path, source) = exclusive_create_lost(dir.path(), ServerRole::Qa);
        let refusal = lost_race(dir.path(), ServerRole::Qa, path, source)
            .expect_err("the other role refuses, exactly like an existing marker");
        assert!(
            matches!(
                &refusal,
                RoleRefused::Mismatch {
                    marked: ServerRole::Prod,
                    asked: ServerRole::Qa,
                    ..
                }
            ),
            "the refusal names both roles: {refusal}"
        );
        assert_eq!(
            fs::read_to_string(&marker).unwrap(),
            "prod\n",
            "the winner's marker is never overwritten"
        );

        // The winner marked it `prod` and the loser asked for `prod`: continue.
        let (path, source) = exclusive_create_lost(dir.path(), ServerRole::Prod);
        lost_race(dir.path(), ServerRole::Prod, path, source).expect("the same role continues");
        assert_eq!(fs::read_to_string(&marker).unwrap(), "prod\n");

        // A marker that VANISHED between the two reads — a dangling symlink is
        // the one shape a read sees as absent while the exclusive create still
        // refuses it — is refused by name, never overwritten.
        #[cfg(unix)]
        {
            fs::remove_file(&marker).unwrap();
            std::os::unix::fs::symlink(dir.path().join("gone"), &marker).unwrap();
            let (path, source) = exclusive_create_lost(dir.path(), ServerRole::Qa);
            let refusal = lost_race(dir.path(), ServerRole::Qa, path, source)
                .expect_err("a vanished marker is refused, never overwritten");
            assert!(
                matches!(&refusal, RoleRefused::MarkerWrite { source, .. }
                    if source.kind() == std::io::ErrorKind::AlreadyExists),
                "the refusal is the create's AlreadyExists: {refusal}"
            );
        }
    }

    /// The exclusive create's failure, as the loser of the race sees it: the
    /// marker path and the `AlreadyExists` source.
    fn exclusive_create_lost(
        data_dir: &std::path::Path,
        role: ServerRole,
    ) -> (std::path::PathBuf, std::io::Error) {
        let error = create_exclusive(data_dir, role)
            .expect_err("the exclusive create loses to the marker already there");
        match error {
            RoleRefused::MarkerWrite { path, source } => {
                assert_eq!(
                    source.kind(),
                    std::io::ErrorKind::AlreadyExists,
                    "the loser's create fails AlreadyExists, not another IO error"
                );
                (path, source)
            }
            other => panic!("the failure is the create's MarkerWrite: {other}"),
        }
    }
}
