//! The per-database instance lock (#250, r4.s2.w2).
//!
//! `pulse serve` holds this lock for its process lifetime; `pulse import` and
//! `pulse restore` take it on their TARGET before any write. It is a
//! **non-blocking** exclusive `flock(2)` on `<db path>.serve.lock`, beside the
//! RESOLVED database (PR-354 fix C3b: the resolved identity, so every spelling
//! of one database takes one lock) — the same idiom
//! [`migrate`](super::migrate)'s migration lock uses,
//! except that a held lock is a REFUSAL with a named reason here, never a wait:
//! the operator stops the server (or the in-flight data op) and retries.
//!
//! Dropping the guard releases the lock: `flock` is per open-file-description,
//! so closing the file — a panicking path included — frees a waiter.

use std::fs;
use std::path::{Path, PathBuf};

/// The instance lock file's suffix, beside the database it guards.
const LOCK_SUFFIX: &str = ".serve.lock";

/// Why an instance lock could not be taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InstanceLockError {
    /// Another process holds the lock: a running `pulse serve`, or an in-flight
    /// import/restore that holds it until its install completes.
    Held {
        /// The database whose instance lock is held.
        target: PathBuf,
        /// The lock file that holds it.
        lock: PathBuf,
    },
    /// A path could not be resolved to an identity (PR-354 fix Z1): its
    /// unresolved tail holds a `.`/`..` component, or the walk could not reach
    /// an existing ancestor. Refused by name, never normalized — appending such
    /// a tail verbatim resolves to a DIFFERENT file than the path names.
    Unresolved {
        /// The path as given.
        target: PathBuf,
        /// Why it was refused.
        reason: String,
    },
    /// The lock file could not be reached (parent directory, create, open).
    Io {
        /// The lock file the call failed on.
        lock: PathBuf,
        /// The underlying failure, in the operator's words.
        reason: String,
    },
    /// The database file has MORE THAN ONE HARD LINK (PR-354 fix D6).
    /// Canonicalisation unifies symlinks but not hard links, so a hard-linked
    /// alias of a live database would take a different lock file and the two
    /// spellings would not exclude each other. One inode, one lock — refused
    /// before the lock is taken.
    HardLinked {
        /// The database path as given.
        target: PathBuf,
        /// How many links the file carries.
        links: u64,
    },
}

impl core::fmt::Display for InstanceLockError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Held { target, lock } => write!(
                f,
                "a running pulse serve holds it — the target {} is locked ({})",
                target.display(),
                lock.display()
            ),
            Self::Io { lock, reason } => write!(
                f,
                "cannot take the instance lock {}: {reason}",
                lock.display()
            ),
            Self::Unresolved { target, reason } => {
                write!(f, "cannot resolve {}: {reason}", target.display())
            }
            Self::HardLinked { target, links } => write!(
                f,
                "the database {} has {links} hard links — a hard-linked alias would take a \
                 different lock file, so this one cannot be locked safely; point --db at the \
                 one file",
                target.display()
            ),
        }
    }
}

impl std::error::Error for InstanceLockError {}

/// The held instance lock: the open file whose `flock` holds it. Dropping the
/// guard releases the lock (and closes the file).
#[derive(Debug)]
pub(crate) struct InstanceLock {
    /// The open file the lock lives on. The leading underscore is the point:
    /// holding it IS the guard's whole state.
    _file: fs::File,
}

impl InstanceLock {
    /// The lock file beside `db_path`: `<db name>.serve.lock` in the database's
    /// own directory — the directory being the RESOLVED identity's, so every
    /// spelling of one database takes one lock (PR-354 fix C3b): a symlink to
    /// the database, or a symlinked directory component, resolves to the same
    /// file as the real path, and `pulse serve pulse.db` and
    /// `pulse import --db link.db` can no longer take two different locks while
    /// the import renames over the live database.
    ///
    /// The rule is [`canonical_identity`]: the deepest existing ancestor is
    /// canonicalized and any not-yet-existing tail is appended unchanged, so a
    /// target the caller is about to create still resolves consistently with
    /// one that exists.
    /// # Errors
    ///
    /// [`InstanceLockError::Unresolved`] when the database path's unresolved
    /// tail holds a `.`/`..` component (PR-354 fix Z1).
    pub(crate) fn lock_path(db_path: &Path) -> Result<PathBuf, InstanceLockError> {
        let resolved = canonical_identity(db_path)?;
        let mut name = resolved.file_name().map_or_else(
            || std::ffi::OsString::from("pulse.db"),
            std::ffi::OsStr::to_os_string,
        );
        name.push(LOCK_SUFFIX);
        Ok(resolved.with_file_name(name))
    }

    /// Take the exclusive, NON-BLOCKING instance lock on `db_path`.
    ///
    /// Creates the database's parent directory when it does not exist yet: the
    /// lock file is a sibling of the database, and a fresh install's target
    /// directory may be this call's to make (the same first-run case
    /// [`Db`](super::Db) covers for the database itself).
    ///
    /// A database file with MORE THAN ONE HARD LINK is refused before anything
    /// else (PR-354 fix D6): canonicalisation unifies symlinks but not hard
    /// links, so an alias of a live database would take a different lock file
    /// and the two spellings would not exclude each other.
    ///
    /// # Errors
    ///
    /// [`InstanceLockError::HardLinked`] when the database has more than one
    /// link; [`InstanceLockError::Held`] when another process holds the lock;
    /// [`InstanceLockError::Io`] when the lock file cannot be created or locked.
    #[cfg(unix)]
    pub(crate) fn acquire(db_path: &Path) -> Result<Self, InstanceLockError> {
        use std::os::unix::fs::MetadataExt as _;
        use std::os::unix::io::AsRawFd;

        // `metadata` follows symlinks, so the link count is the RESOLVED
        // file's: every spelling of the aliased database is refused.
        if let Ok(meta) = fs::metadata(db_path)
            && meta.nlink() > 1
        {
            return Err(InstanceLockError::HardLinked {
                target: db_path.to_path_buf(),
                links: meta.nlink(),
            });
        }

        let lock_path = Self::lock_path(db_path)?;
        create_parent(&lock_path)?;
        let file = open_lock_file(&lock_path)?;
        // SAFETY: `file` is a live open fd for the duration of the call, and the
        // returned guard keeps it open (and the lock held) until it drops.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            // `LOCK_NB` on a held lock is `EWOULDBLOCK`; anything else is a real
            // I/O failure, never a refusal.
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(InstanceLockError::Held {
                    target: db_path.to_path_buf(),
                    lock: lock_path,
                });
            }
            return Err(InstanceLockError::Io {
                lock: lock_path,
                reason: error.to_string(),
            });
        }
        Ok(Self { _file: file })
    }

    /// No `flock` on platforms without `unix`: the call site is identical and
    /// the guard still holds the file open, mirroring
    /// [`acquire_migration_lock`](super::migrate)'s posture — the instance
    /// refusal is a Unix-only control, and desktop parity is a separate item.
    ///
    /// # Errors
    ///
    /// [`InstanceLockError::Io`] when the lock file cannot be created or opened.
    #[cfg(not(unix))]
    pub(crate) fn acquire(db_path: &Path) -> Result<Self, InstanceLockError> {
        let lock_path = Self::lock_path(db_path)?;
        create_parent(&lock_path)?;
        Ok(Self {
            _file: open_lock_file(&lock_path)?,
        })
    }
}

/// The path's raw components, `.` and `..` PRESERVED: `Path::components`
/// normalizes a `.` in the middle of a path away, which is exactly the component
/// the refusal below has to see. On unix the bytes are used directly, so a
/// non-UTF-8 path is handled too; elsewhere the components `Path` yields are
/// used (a middle `.` is invisible there, while `..` is still refused).
fn raw_components(path: &Path) -> Vec<std::ffi::OsString> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
        let mut out: Vec<std::ffi::OsString> = Vec::new();
        if path.is_absolute() {
            out.push(std::ffi::OsString::from("/"));
        }
        out.extend(
            path.as_os_str()
                .as_bytes()
                .split(|byte| *byte == b'/')
                .filter(|segment| !segment.is_empty())
                .map(|segment| std::ffi::OsString::from_vec(segment.to_vec())),
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
fn is_dot_component(segment: &std::ffi::OsString) -> bool {
    segment == "." || segment == ".."
}

/// `path` with every symlinked ancestor resolved and any not-yet-existing tail
/// preserved: the deepest ancestor that EXISTS is canonicalized and the
/// remaining components are appended unchanged. The tail must be made of plain
/// names — a `.` or `..` below the deepest existing ancestor is REFUSED (PR-354
/// fix Z1), never normalized: appending it verbatim resolves to a DIFFERENT file
/// than the path names (which is how `/srv/new/../pulse.db` bypassed import's
/// #264 same-target refusal against `/srv/pulse.db`), and a path ENDING in `..`
/// used to fall back to the current directory, a silent pass.
///
/// One copy of the rule, shared by the instance lock's [`InstanceLock::lock_path`]
/// and the import's same-target refusal (PR-354 fix C3b).
///
/// # Errors
///
/// [`InstanceLockError::Unresolved`] when the unresolved tail holds a `.`/`..`
/// component, or when the walk cannot reach an existing ancestor.
pub(crate) fn canonical_identity(path: &Path) -> Result<PathBuf, InstanceLockError> {
    let refuse = |reason: String| InstanceLockError::Unresolved {
        target: path.to_path_buf(),
        reason,
    };
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| refuse(format!("the current directory cannot be resolved: {error}")))?
            .join(path)
    };
    let mut resolved: Option<PathBuf> = None;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
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
                    return Err(refuse(format!(
                        "the unresolved tail holds a {} component; refusing rather than \
                         normalizing the path",
                        segment.to_string_lossy()
                    )));
                }
                tail.push(segment);
            }
            Err(error) => return Err(refuse(error.to_string())),
        }
    }
    let Some(mut out) = resolved else {
        return Err(refuse("the path has no resolvable root".to_owned()));
    };
    for name in tail {
        out.push(name);
    }
    Ok(out)
}

/// Create the lock file's parent directory when it is not there yet — 0700,
/// never the process umask's 0755 (PR-354 fix D1b: this call is the FIRST thing
/// to make the database's directory on a fresh install, so it is the one that
/// decides the isolation).
fn create_parent(lock_path: &Path) -> Result<(), InstanceLockError> {
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        super::create_private_dir(parent).map_err(|error| InstanceLockError::Io {
            lock: lock_path.to_path_buf(),
            reason: format!("create {}: {error}", parent.display()),
        })?;
    }
    Ok(())
}

/// Open (creating when absent) the lock file the `flock` needs.
fn open_lock_file(lock_path: &Path) -> Result<fs::File, InstanceLockError> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .map_err(|error| InstanceLockError::Io {
            lock: lock_path.to_path_buf(),
            reason: error.to_string(),
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{InstanceLock, InstanceLockError, canonical_identity};
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// The lock file is the database's own path plus `.serve.lock`, wherever
    /// the database lives. A bare relative name resolves against the cwd — the
    /// same lock its absolute spelling takes (PR-354 fix C3b).
    #[test]
    fn the_lock_file_sits_beside_the_database() {
        assert_eq!(
            InstanceLock::lock_path(Path::new("/srv/pulse.db")).expect("a plain path"),
            Path::new("/srv/pulse.db.serve.lock")
        );
        let cwd = std::fs::canonicalize(".").expect("the cwd resolves");
        assert_eq!(
            InstanceLock::lock_path(Path::new("pulse.db")).expect("a relative path"),
            cwd.join("pulse.db.serve.lock")
        );
    }

    /// PR-354 fix C3b: the lock is keyed on the RESOLVED identity, so every
    /// spelling of one database takes one lock — a symlink to the database and
    /// a symlinked directory component included. Keyed on the path as typed,
    /// `serve pulse.db` and `import --db link.db` took two different locks and
    /// the import renamed over the live database.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_spelling_takes_the_same_lock() {
        let tmp = TempDir::new().expect("tempdir");
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).expect("the real directory");
        let db = real.join("pulse.db");
        std::fs::write(&db, b"the database").expect("the database");
        let file_link = tmp.path().join("link.db");
        std::os::unix::fs::symlink(&db, &file_link).expect("the file symlink");
        let dir_link = tmp.path().join("alias");
        std::os::unix::fs::symlink(&real, &dir_link).expect("the directory symlink");

        let lock = InstanceLock::lock_path(&db).expect("the database");
        assert_eq!(
            InstanceLock::lock_path(&file_link).expect("the file symlink"),
            lock,
            "a symlink to the database takes its lock"
        );
        assert_eq!(
            InstanceLock::lock_path(&dir_link.join("pulse.db")).expect("the directory symlink"),
            lock,
            "a symlinked directory component takes it too"
        );
        // And a target that does not exist yet resolves through its deepest
        // EXISTING ancestor, so the first acquire and the second agree.
        let fresh =
            InstanceLock::lock_path(&real.join("fresh").join("pulse.db")).expect("a fresh target");
        assert_eq!(
            fresh,
            std::fs::canonicalize(&real)
                .expect("the real directory resolves")
                .join("fresh")
                .join("pulse.db.serve.lock")
        );
    }

    /// A fresh target's directory is this call's to make, and the lock file
    /// lands in it.
    #[test]
    fn acquiring_creates_the_targets_directory() {
        let tmp = TempDir::new().expect("tempdir");
        let db = tmp.path().join("fresh").join("pulse.db");
        let lock = InstanceLock::acquire(&db).expect("acquire on a fresh target");
        assert!(
            InstanceLock::lock_path(&db)
                .expect("the lock path")
                .exists(),
            "the lock file exists: {}",
            InstanceLock::lock_path(&db)
                .expect("the lock path")
                .display()
        );
        drop(lock);
    }

    /// A second acquire refuses while the first is held, and drop releases the
    /// lock for the next one — `flock` conflicts across open file descriptions,
    /// which is what makes that true across processes too.
    #[cfg(unix)]
    #[test]
    fn a_second_acquire_is_refused_and_drop_releases() {
        let tmp = TempDir::new().expect("tempdir");
        let db = tmp.path().join("pulse.db");

        let first = InstanceLock::acquire(&db).expect("the first acquire");
        let refused = InstanceLock::acquire(&db).expect_err("the second acquire refuses");
        match &refused {
            InstanceLockError::Held { target, lock } => {
                assert_eq!(target, &db, "the refusal names the database");
                assert_eq!(
                    lock,
                    &InstanceLock::lock_path(&db).expect("the lock path"),
                    "and the lock file"
                );
            }
            other @ (InstanceLockError::Io { .. }
            | InstanceLockError::HardLinked { .. }
            | InstanceLockError::Unresolved { .. }) => {
                panic!("expected Held, got {other:?}");
            }
        }
        assert!(
            refused
                .to_string()
                .contains("a running pulse serve holds it"),
            "the refusal reads as the operator needs it: {refused}"
        );

        drop(first);
        let again = InstanceLock::acquire(&db).expect("drop released the lock");
        drop(again);
    }

    /// PR-354 fix D6: a hard-linked alias of a database takes a DIFFERENT lock
    /// file (canonicalisation unifies symlinks, not hard links), so the two
    /// spellings would not exclude each other. A database with more than one
    /// link is refused before the lock — both spellings, by name and count —
    /// while a normal database still locks.
    #[cfg(unix)]
    #[test]
    fn a_hard_linked_database_is_refused() {
        let tmp = TempDir::new().expect("tempdir");
        let db = tmp.path().join("pulse.db");
        std::fs::write(&db, b"the database").expect("write the database");

        let lock = InstanceLock::acquire(&db).expect("a single-link database locks");
        drop(lock);

        let alias = tmp.path().join("alias.db");
        std::fs::hard_link(&db, &alias).expect("the hard link");
        for path in [&db, &alias] {
            let error = InstanceLock::acquire(path).expect_err("a hard-linked database is refused");
            match error {
                InstanceLockError::HardLinked { target, links } => {
                    assert_eq!(&target, path, "the refusal names the path as given");
                    assert_eq!(links, 2, "and the link count");
                }
                other => panic!("the refusal is HardLinked, not {other:?}"),
            }
        }
    }

    /// PR-354 fix Z1: the identity walk REFUSES a `.` or `..` in the unresolved
    /// tail — it used to append it verbatim (so `/srv/new/../pulse.db` did not
    /// equal `/srv/pulse.db`, bypassing import's #264 refusal) and a path
    /// ENDING in `..` fell back to the current directory, a silent pass.
    #[test]
    fn a_dot_component_in_the_unresolved_tail_is_refused() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("srv");
        std::fs::create_dir_all(&dir).expect("the fixture directory");
        let db = dir.join("pulse.db");
        std::fs::write(&db, b"the database").expect("the database");

        for (what, path) in [
            (
                "a `..` in the tail",
                dir.join("new").join("..").join("pulse.db"),
            ),
            ("a trailing `..`", dir.join("new").join("..")),
            (
                "a `.` in the tail",
                PathBuf::from(format!("{}/new/./pulse.db", dir.display())),
            ),
        ] {
            let error = canonical_identity(&path).expect_err("the dot tail is refused");
            let message = error.to_string();
            assert!(
                message.contains(&path.display().to_string()) && message.contains("cannot resolve"),
                "{what}: the refusal names the path: {message}"
            );
            assert!(
                message.contains("..") || message.contains('.'),
                "{what}: and the component: {message}"
            );
        }

        // A plain path still resolves, and so does a fresh tail of plain names.
        assert_eq!(
            canonical_identity(&db).expect("a plain path"),
            std::fs::canonicalize(&db).expect("the canonical path")
        );
        assert_eq!(
            canonical_identity(&dir.join("fresh").join("pulse.db")).expect("a fresh tail"),
            std::fs::canonicalize(&dir)
                .expect("the directory")
                .join("fresh")
                .join("pulse.db")
        );
    }
}
