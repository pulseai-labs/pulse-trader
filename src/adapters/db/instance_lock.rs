//! The per-database instance lock (#250, r4.s2.w2).
//!
//! `pulse serve` holds this lock for its process lifetime; `pulse import` and
//! `pulse restore` take it on their TARGET before any write. It is a
//! **non-blocking** exclusive `flock(2)` on `<db path>.serve.lock`, beside the
//! database — the same idiom [`migrate`](super::migrate)'s migration lock uses,
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
    /// The lock file could not be reached (parent directory, create, open).
    Io {
        /// The lock file the call failed on.
        lock: PathBuf,
        /// The underlying failure, in the operator's words.
        reason: String,
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
    #[must_use]
    pub(crate) fn lock_path(db_path: &Path) -> PathBuf {
        let resolved = canonical_identity(db_path);
        let mut name = resolved.file_name().map_or_else(
            || std::ffi::OsString::from("pulse.db"),
            std::ffi::OsStr::to_os_string,
        );
        name.push(LOCK_SUFFIX);
        resolved.with_file_name(name)
    }

    /// Take the exclusive, NON-BLOCKING instance lock on `db_path`.
    ///
    /// Creates the database's parent directory when it does not exist yet: the
    /// lock file is a sibling of the database, and a fresh install's target
    /// directory may be this call's to make (the same first-run case
    /// [`Db`](super::Db) covers for the database itself).
    ///
    /// # Errors
    ///
    /// [`InstanceLockError::Held`] when another process holds the lock;
    /// [`InstanceLockError::Io`] when the lock file cannot be created or locked.
    #[cfg(unix)]
    pub(crate) fn acquire(db_path: &Path) -> Result<Self, InstanceLockError> {
        use std::os::unix::io::AsRawFd;

        let lock_path = Self::lock_path(db_path);
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
        let lock_path = Self::lock_path(db_path);
        create_parent(&lock_path)?;
        Ok(Self {
            _file: open_lock_file(&lock_path)?,
        })
    }
}

/// `path` with every symlinked ancestor resolved and any not-yet-existing tail
/// preserved: the deepest ancestor that EXISTS is canonicalized and the
/// remaining components are appended unchanged. A path with nothing to resolve
/// against (a bare relative name that does not exist) resolves the cwd.
///
/// One copy of the rule, shared by the instance lock's [`InstanceLock::lock_path`]
/// and the import's same-target refusal (PR-354 fix C3b; it lived in
/// `cli::import` only, and the lock keyed on the path as typed).
pub(crate) fn canonical_identity(path: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut current = path.to_path_buf();
    let resolved = loop {
        if let Ok(root) = fs::canonicalize(&current) {
            break root;
        }
        match current.file_name().map(std::ffi::OsStr::to_os_string) {
            Some(name) => {
                tail.push(name);
                current = match current.parent() {
                    Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                    _ => PathBuf::from("."),
                };
            }
            None => break fs::canonicalize(".").unwrap_or_else(|_| PathBuf::from(".")),
        }
    };
    let mut out = resolved;
    for name in tail.iter().rev() {
        out.push(name);
    }
    out
}

/// Create the lock file's parent directory when it is not there yet.
fn create_parent(lock_path: &Path) -> Result<(), InstanceLockError> {
    if let Some(parent) = lock_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|error| InstanceLockError::Io {
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
    use super::{InstanceLock, InstanceLockError};
    use std::path::Path;
    use tempfile::TempDir;

    /// The lock file is the database's own path plus `.serve.lock`, wherever
    /// the database lives. A bare relative name resolves against the cwd — the
    /// same lock its absolute spelling takes (PR-354 fix C3b).
    #[test]
    fn the_lock_file_sits_beside_the_database() {
        assert_eq!(
            InstanceLock::lock_path(Path::new("/srv/pulse.db")),
            Path::new("/srv/pulse.db.serve.lock")
        );
        let cwd = std::fs::canonicalize(".").expect("the cwd resolves");
        assert_eq!(
            InstanceLock::lock_path(Path::new("pulse.db")),
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

        let lock = InstanceLock::lock_path(&db);
        assert_eq!(
            InstanceLock::lock_path(&file_link),
            lock,
            "a symlink to the database takes its lock"
        );
        assert_eq!(
            InstanceLock::lock_path(&dir_link.join("pulse.db")),
            lock,
            "a symlinked directory component takes it too"
        );
        // And a target that does not exist yet resolves through its deepest
        // EXISTING ancestor, so the first acquire and the second agree.
        let fresh = InstanceLock::lock_path(&real.join("fresh").join("pulse.db"));
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
            InstanceLock::lock_path(&db).exists(),
            "the lock file exists: {}",
            InstanceLock::lock_path(&db).display()
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
                assert_eq!(lock, &InstanceLock::lock_path(&db), "and the lock file");
            }
            other @ InstanceLockError::Io { .. } => {
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
}
