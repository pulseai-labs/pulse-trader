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
//! [`ensure_db_inside_data_dir`] is the one copy of that rule, and
//! `pulse serve --role` is its only caller.
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
    /// A `.` or `..` sits in the UNRESOLVED tail of a path (PR-354 fix Z1): the
    /// tail is appended verbatim, so it is refused rather than normalized.
    #[error(
        "the path {} holds a {component:?} component below its deepest existing ancestor; \
         refusing rather than normalizing it",
        path.display()
    )]
    DotComponent {
        /// The path as given.
        path: PathBuf,
        /// The offending component (`..` or `.`).
        component: String,
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

/// Create the marker EXCLUSIVELY for an absent-marker start (PR-354 fix C2b):
/// two overlapping starts both read no marker, and with a plain create both
/// would write — the loser would overwrite the winner's role. The publish is a
/// hard link of the synced temp file, so the second create fails with
/// `AlreadyExists` atomically, which [`check`] turns into "re-read and validate
/// the winning role".
///
/// # Errors
///
/// [`RoleRefused::MarkerWrite`] — with an `AlreadyExists` source when the
/// marker appeared between the read and this call.
fn create_exclusive(data_dir: &Path, role: ServerRole) -> Result<(), RoleRefused> {
    write_marker(data_dir, role)
}

/// The one marker write: one line, mode 0600, the data dir created first (0700,
/// PR-354 fix D1b), and the marker PUBLISHED from a temp file (PR-354 fix D1a)
/// by a hard link — the exclusive create (`link(2)` fails `AlreadyExists`
/// atomically, which [`check`] turns into "re-read and validate the winner";
/// there is no create-or-replace path left, PR-354 fix B1). A marker is
/// therefore never visible empty or partial: the old create-then-write let a
/// racing reader see `""` and refuse `MarkerUnreadable`, and a failed write left
/// an empty marker that refused every later start.
fn write_marker(data_dir: &Path, role: ServerRole) -> Result<(), RoleRefused> {
    let path = data_dir.join(ROLE_MARKER_NAME);
    crate::adapters::db::create_private_dir(data_dir).map_err(|source| {
        RoleRefused::MarkerWrite {
            path: path.clone(),
            source,
        }
    })?;
    let (temp, mut file) = create_temp_marker(data_dir, &path)?;
    let result = write_and_publish(&mut file, &temp, &path, data_dir, role);
    drop(file);
    // The temp name goes whatever happened: on success the published marker
    // holds the inode, on failure nothing at all is left behind.
    let _ = fs::remove_file(&temp);
    result
}

/// The unique temp file a marker write lands in first, beside the marker it
/// publishes: this process's pid plus a process-local counter, so neither two
/// writes in one process nor two processes can collide. A stale temp from a
/// crashed run with a recycled pid is stepped over (the next counter is free).
fn create_temp_marker(data_dir: &Path, marker: &Path) -> Result<(PathBuf, fs::File), RoleRefused> {
    let mut last: Option<std::io::Error> = None;
    for _ in 0..8 {
        let temp = temp_marker_path(data_dir);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&temp) {
            Ok(file) => return Ok((temp, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last = Some(error);
            }
            Err(source) => {
                return Err(RoleRefused::MarkerWrite {
                    path: marker.to_path_buf(),
                    source,
                });
            }
        }
    }
    Err(RoleRefused::MarkerWrite {
        path: marker.to_path_buf(),
        source: last.unwrap_or_else(|| std::io::Error::other("no marker temp name was free")),
    })
}

/// The temp marker's name under `data_dir`.
fn temp_marker_path(data_dir: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    data_dir.join(format!(".{ROLE_MARKER_NAME}.{}.{n}", std::process::id()))
}

/// Write the role into the temp file, sync it, then publish it under the
/// marker's name with a `hard_link` — and fsync the data dir so the published
/// name is durable too (PR-354 fix S2:
/// the marker was flushed but never synced, so a power loss could leave a start
/// believing a dir is marked when it is not).
///
/// The chain is the import's publish discipline, reused rather than copied:
/// `sync_file` fsyncs the temp file's bytes before the name that promises them
/// exists, and `sync_dir` fsyncs the directory entry the link/rename created.
fn write_and_publish(
    file: &mut fs::File,
    temp: &Path,
    path: &Path,
    data_dir: &Path,
    role: ServerRole,
) -> Result<(), RoleRefused> {
    writeln!(file, "{}", role.as_str()).map_err(|source| RoleRefused::MarkerWrite {
        path: path.to_path_buf(),
        source,
    })?;
    file.flush().map_err(|source| RoleRefused::MarkerWrite {
        path: path.to_path_buf(),
        source,
    })?;
    // The bytes must be ON DISK before the name that promises them exists.
    crate::cli::publish::sync_file(temp, path).map_err(|error| RoleRefused::MarkerWrite {
        path: path.to_path_buf(),
        source: std::io::Error::other(error.to_string()),
    })?;
    fs::hard_link(temp, path).map_err(|source| RoleRefused::MarkerWrite {
        path: path.to_path_buf(),
        source,
    })?;
    // And the directory entry that publishes it must be durable too.
    crate::cli::publish::sync_dir(data_dir).map_err(|error| RoleRefused::MarkerWrite {
        path: path.to_path_buf(),
        source: std::io::Error::other(error.to_string()),
    })
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

/// The path's raw components, `.` and `..` PRESERVED: `Path::components`
/// normalizes a `.` in the middle of a path away, which is exactly the component
/// the refusal below has to see. On unix the bytes are used directly, so a
/// non-UTF-8 path is handled too; elsewhere the components `Path` yields are
/// used (a middle `.` is invisible there, while `..` is still refused).
fn raw_components(path: &Path) -> Vec<OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
        let mut out: Vec<OsString> = Vec::new();
        if path.is_absolute() {
            out.push(OsString::from("/"));
        }
        out.extend(
            path.as_os_str()
                .as_bytes()
                .split(|byte| *byte == b'/')
                .filter(|segment| !segment.is_empty())
                .map(|segment| OsString::from_vec(segment.to_vec())),
        );
        out
    }
    #[cfg(not(unix))]
    {
        path.components()
            .map(|component| component.as_os_str().to_os_string())
            .collect()
    }
}

/// Whether one raw component is `.` or `..`.
fn is_dot_component(segment: &OsString) -> bool {
    segment == "." || segment == ".."
}

/// The absolute, symlink-resolved form of `path`, resolved through its deepest
/// EXISTING ancestor so a path whose final components do not exist yet (the
/// database a `--role` start is about to create) resolves consistently with one
/// that does (the data dir). The unresolved tail is appended verbatim — and it
/// must be made of plain names: a `.` or `..` below the deepest existing
/// ancestor is REFUSED (PR-354 fix Z1), never normalized, because the verbatim
/// append would otherwise climb out of the data dir
/// (`<dir>/new/../../outside`) after the lexical containment check passed.
///
/// # Errors
///
/// [`RoleRefused::DotComponent`] for a `.`/`..` in the unresolved tail, and
/// [`RoleRefused::PathUnresolved`] on any IO error while walking — the caller
/// turns both into a named refusal, never a silent pass.
fn resolve(path: &Path) -> Result<PathBuf, RoleRefused> {
    let unresolved = |source| RoleRefused::PathUnresolved {
        path: path.to_path_buf(),
        source,
    };
    let dot = |segment: &OsString| RoleRefused::DotComponent {
        path: path.to_path_buf(),
        component: segment.to_string_lossy().into_owned(),
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(unresolved)?.join(path)
    };
    let mut resolved: Option<PathBuf> = None;
    let mut tail: Vec<OsString> = Vec::new();
    for segment in raw_components(&absolute) {
        // The candidate is the deepest existing prefix, the tail pushed so far
        // and this segment, joined TEXTUALLY: `PathBuf::join` drops a `.`
        // component before any syscall, which would hide exactly the case this
        // refusal exists for (PR-354 fix Z1).
        let mut joined = match &resolved {
            Some(root) => root.clone().into_os_string(),
            None => std::ffi::OsString::new(),
        };
        for name in &tail {
            joined.push(std::path::MAIN_SEPARATOR.to_string());
            joined.push(name);
        }
        if !joined.is_empty() {
            joined.push(std::path::MAIN_SEPARATOR.to_string());
        }
        joined.push(&segment);
        let prefix = PathBuf::from(joined);
        match fs::canonicalize(&prefix) {
            // Still inside the existing prefix: `.`/`..` here resolve against
            // real directories, so they are not the refusal's business.
            Ok(root) => resolved = Some(root),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if is_dot_component(&segment) {
                    return Err(dot(&segment));
                }
                tail.push(segment);
            }
            Err(error) => return Err(unresolved(error)),
        }
    }
    let Some(mut out) = resolved else {
        return Err(unresolved(std::io::Error::other(
            "the path has no resolvable root",
        )));
    };
    for name in tail {
        out.push(name);
    }
    Ok(out)
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

    /// PR-354 fix D1b: a data dir this code creates is 0700, never the process
    /// umask's 0755 — the marker's dir holds the database and the tokens (G10).
    #[cfg(unix)]
    #[test]
    fn a_fresh_data_dir_is_created_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("fresh").join("nested");

        check(&data_dir, &data_dir.join("pulse.db"), ServerRole::Prod)
            .expect("the first start marks a fresh dir");

        for path in [dir.path().join("fresh"), data_dir.clone()] {
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} is private", path.display());
        }
        let marker_mode = fs::metadata(data_dir.join(ROLE_MARKER_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(marker_mode, 0o600, "and the marker is too");
    }

    /// PR-354 fix D1a: the marker is published from a temp file, so a write that
    /// cannot land leaves NO marker at all — the old create-then-write left an
    /// empty one, which refused every later start as `MarkerUnreadable` — and no
    /// temp file behind.
    #[cfg(unix)]
    #[test]
    fn a_failed_marker_write_leaves_no_marker_and_no_temp() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        // A read-only dir: the temp file cannot be created at all.
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let error = create_exclusive(&data_dir, ServerRole::Qa)
            .expect_err("a read-only dir refuses the marker write");
        assert!(
            matches!(error, RoleRefused::MarkerWrite { .. }),
            "a MarkerWrite refusal: {error}"
        );
        assert!(
            !data_dir.join(ROLE_MARKER_NAME).exists(),
            "no marker exists"
        );
        let leftovers: Vec<_> = fs::read_dir(&data_dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(leftovers.is_empty(), "no temp left: {leftovers:?}");

        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// PR-354 fix S2: the marker's bytes and the directory entry that publishes
    /// it are fsynced — the import's publish chain, reused. A file-sync failure
    /// (before the publish) refuses and leaves NO marker; a dir-sync failure
    /// (after it) refuses too, and neither leaves a temp behind.
    #[test]
    fn a_failed_marker_sync_is_a_refusal_and_leaves_no_temp() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("data");

        // The file sync fails before the publish: no marker exists.
        crate::cli::publish::probe::fail_next_file_sync();
        let error =
            create_exclusive(&data_dir, ServerRole::Prod).expect_err("a failed file sync refuses");
        assert!(
            matches!(error, RoleRefused::MarkerWrite { .. }),
            "a MarkerWrite refusal: {error}"
        );
        assert!(
            !data_dir.join(ROLE_MARKER_NAME).exists(),
            "no marker was published"
        );
        assert!(
            no_temp(&data_dir),
            "and the temp is gone: {:?}",
            fs::read_dir(&data_dir).unwrap().flatten().count()
        );

        // The dir sync fails AFTER the publish: the marker is there, the run
        // still refuses (it cannot claim the name is durable), and the temp is
        // removed all the same.
        crate::cli::publish::probe::fail_next_sync_of(&data_dir);
        let error =
            create_exclusive(&data_dir, ServerRole::Prod).expect_err("a failed dir sync refuses");
        assert!(
            matches!(error, RoleRefused::MarkerWrite { .. }),
            "a MarkerWrite refusal: {error}"
        );
        assert_eq!(
            fs::read_to_string(data_dir.join(ROLE_MARKER_NAME)).unwrap(),
            "prod\n",
            "the marker itself is published"
        );
        assert!(no_temp(&data_dir), "and no temp is left");

        // With no injection armed the whole chain succeeds and leaves one file
        // (a FRESH dir: the marker above is already there, and the publish is
        // exclusive).
        let fresh = dir.path().join("fresh");
        create_exclusive(&fresh, ServerRole::Prod).expect("the publish works");
        assert_eq!(
            fs::read_to_string(fresh.join(ROLE_MARKER_NAME)).unwrap(),
            "prod\n"
        );
        assert!(no_temp(&fresh), "no temp after a successful publish");
    }

    /// Whether the data dir holds no marker temp (every entry is the marker).
    fn no_temp(data_dir: &std::path::Path) -> bool {
        fs::read_dir(data_dir)
            .unwrap()
            .flatten()
            .all(|entry| entry.file_name() == std::ffi::OsStr::new(ROLE_MARKER_NAME))
    }

    /// PR-354 fix Z1: a `..` or `.` component in the UNRESOLVED tail is refused
    /// by name, before anything is created. The tail is appended verbatim, so
    /// `<qa>/new/../../outside/pulse.db` passed the lexical containment check
    /// (`starts_with`) and then climbed out of the data dir once `new` existed;
    /// a `.` was silently appended twice.
    #[test]
    fn a_dot_component_in_the_unresolved_tail_is_refused() {
        let dir = TempDir::new().unwrap();
        let qa = dir.path().join("qa");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&qa).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let db = qa
            .join("new")
            .join("..")
            .join("..")
            .join("outside")
            .join("pulse.db");
        assert!(!qa.join("new").exists(), "the fixture's `new` is absent");
        let error = check(&qa, &db, ServerRole::Prod).expect_err("the dot-dot tail is refused");
        let message = error.to_string();
        assert!(
            message.contains("..") && message.contains(&db.display().to_string()),
            "the refusal names the component and the path: {message}"
        );
        assert!(!qa.join("new").exists(), "nothing was created");
        assert!(!qa.join(ROLE_MARKER_NAME).exists(), "no marker");
        assert!(
            !outside.join("pulse.db").exists(),
            "and no database outside the data dir"
        );

        // A `.` in the tail is refused too (`Path::join` would drop it, so the
        // path is built from its text).
        let dot = std::path::PathBuf::from(format!("{}/new/./pulse.db", qa.display()));
        let error = check(&qa, &dot, ServerRole::Prod).expect_err("the dot tail is refused");
        assert!(
            error.to_string().contains('.'),
            "the refusal names the component: {error}"
        );
        assert!(
            !qa.join("new").exists(),
            "nothing was created for the dot tail either"
        );
    }
}
