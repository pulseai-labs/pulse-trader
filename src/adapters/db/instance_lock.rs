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
    /// own directory. A bare relative `pulse.db` keeps its lock beside it in
    /// the current directory.
    #[must_use]
    pub(crate) fn lock_path(db_path: &Path) -> PathBuf {
        let mut name = db_path.file_name().map_or_else(
            || std::ffi::OsString::from("pulse.db"),
            std::ffi::OsStr::to_os_string,
        );
        name.push(LOCK_SUFFIX);
        db_path.with_file_name(name)
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
    /// the database lives (a bare relative name included).
    #[test]
    fn the_lock_file_sits_beside_the_database() {
        assert_eq!(
            InstanceLock::lock_path(Path::new("/srv/pulse.db")),
            Path::new("/srv/pulse.db.serve.lock")
        );
        assert_eq!(
            InstanceLock::lock_path(Path::new("pulse.db")),
            Path::new("pulse.db.serve.lock")
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
